//! MCP 仅通过继承的标准输入输出提供工具。实际查询前须经本地独立入口逐次授权。
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use xs_core::operations;

const PROTOCOL: &str = "2025-11-25";
const MAX_FRAME: usize = 64 * 1024;
const MAX_PENDING: usize = 16;
const MAX_QUERY: usize = 1024;
const TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Pending {
    request_id: String,
    session: String,
    scope: ApprovalScope,
    query: String,
    limit: usize,
    expires_at: u64,
}

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ApprovalScope {
    token: String,
    settings_revision: xs_platform::FileRevision,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Arguments {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
    approval_id: Option<String>,
}

fn default_limit() -> usize {
    10
}

fn now() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn new_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("无法生成本地授权标识：{error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn request_path(dir: &Path, kind: &str, id: &str) -> Result<PathBuf> {
    ensure!(
        id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "授权请求标识无效"
    );
    Ok(dir.join("agent").join(kind).join(format!("{id}.json")))
}

fn read_record(dir: &Path, kind: &str, id: &str) -> Result<Pending> {
    let path = request_path(dir, kind, id)?;
    let mut bytes = Vec::new();
    xs_platform::open_local_read(&path)?
        .take(MAX_FRAME as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_FRAME, "本地授权文件超出长度限制");
    let record: Pending = serde_json::from_slice(&bytes).context("本地授权文件损坏")?;
    ensure!(record.request_id == id, "授权文件标识不一致");
    Ok(record)
}

fn save_record(dir: &Path, kind: &str, record: &Pending) -> Result<()> {
    let path = request_path(dir, kind, &record.request_id)?;
    xs_platform::create_local_dir_all(path.parent().context("授权目录不存在")?)?;
    // 只创建新文件，禁止重复授权或覆盖消费审计。
    let outcome = xs_platform::atomic_write_local(&path, &serde_json::to_vec(record)?, false)?;
    ensure!(outcome.is_durable(), "本地授权写入未确认持久化");
    Ok(())
}

fn current_scope(dir: &Path) -> Result<ApprovalScope> {
    let settings_path = operations::settings_path(dir);
    let settings_revision = xs_platform::FileRevision::at_path(&settings_path)?;
    let settings = operations::load_settings(dir)?;
    ensure!(settings.content_enabled, "正文功能已关闭");
    ensure!(
        settings_revision == xs_platform::FileRevision::at_path(&settings_path)?,
        "授权范围正在变化，请重试"
    );
    Ok(ApprovalScope {
        token: settings.content_scope_token()?,
        settings_revision,
    })
}

pub(crate) fn inspect(dir: &Path, id: &str) -> Result<Value> {
    let pending = read_record(dir, "pending", id)?;
    ensure!(
        pending.expires_at >= now()? && pending.scope == current_scope(dir)?,
        "授权请求已失效"
    );
    Ok(
        json!({"request_id":id,"tool":"search_content","query":pending.query,"limit":pending.limit,"expires_at":pending.expires_at}),
    )
}

pub(crate) fn approve(dir: &Path, id: &str) -> Result<Value> {
    let pending = read_record(dir, "pending", id)?;
    ensure!(
        pending.expires_at >= now()? && pending.scope == current_scope(dir)?,
        "授权请求已失效"
    );
    save_record(dir, "approved", &pending)?;
    Ok(json!({"approved":true,"request_id":id,"expires_at":pending.expires_at}))
}

struct Session<'a> {
    dir: &'a Path,
    id: String,
    pending: HashMap<String, (Pending, Instant)>,
    reader: xs_content::ContentReader,
}

impl Session<'_> {
    fn clear_request(&self, id: &str) {
        for kind in ["pending", "approved"] {
            if let Ok(path) = request_path(self.dir, kind, id) {
                let _ = xs_platform::remove_local_file(&path);
            }
        }
    }

    fn call(&mut self, arguments: Arguments) -> Result<Value> {
        ensure!(
            !arguments.query.trim().is_empty() && arguments.query.len() <= MAX_QUERY,
            "正文查询长度无效"
        );
        ensure!(
            (1..=25).contains(&arguments.limit),
            "授权查询每次允许 1 至 25 条结果"
        );
        let scope = current_scope(self.dir)?;
        if let Some(id) = arguments.approval_id {
            let (pending, expiry) = self
                .pending
                .get(&id)
                .context("授权请求不属于当前会话，或已经消费")?;
            ensure!(
                expiry > &Instant::now() && pending.expires_at >= now()?,
                "授权请求已过期"
            );
            ensure!(
                pending.scope == scope
                    && pending.query == arguments.query
                    && pending.limit == arguments.limit,
                "当前范围或查询参数与授权请求不一致"
            );
            let approved = read_record(self.dir, "approved", &id).context("尚未收到本地授权")?;
            ensure!(
                approved == *pending && approved.session == self.id,
                "本地授权与会话不一致"
            );
            // 先持久化消费，再执行查询。任何异常退出后的新进程都使用不同会话。
            self.pending.remove(&id);
            let consumed = save_record(self.dir, "consumed", &approved);
            self.clear_request(&id);
            consumed?;
            let response = self.reader.search(
                &arguments.query,
                &xs_content::SearchOptions {
                    limit: arguments.limit,
                    offset: 0,
                },
                &AtomicBool::new(false),
            )?;
            xs_content::validate_response(self.dir, &response)?;
            ensure!(
                current_scope(self.dir)? == scope,
                "授权范围在查询期间已变化"
            );
            let hits = response.hits.iter().map(|hit| json!({"path":hit.path,"location":hit.location,"snippet":hit.snippet,"highlights":hit.highlights})).collect::<Vec<_>>();
            return Ok(tool_result(
                &json!({"indexed_total":response.indexed_total,"omitted_stale":response.omitted_stale,"hits":hits}),
                false,
            ));
        }

        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, (_, expiry))| *expiry <= Instant::now())
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired {
            self.pending.remove(&id);
            self.clear_request(&id);
        }
        ensure!(self.pending.len() < MAX_PENDING, "此会话的待授权请求已满");
        let id = new_id()?;
        let pending = Pending {
            request_id: id.clone(),
            session: self.id.clone(),
            scope,
            query: arguments.query,
            limit: arguments.limit,
            expires_at: now()?.saturating_add(TTL.as_secs()),
        };
        save_record(self.dir, "pending", &pending)?;
        self.pending
            .insert(id.clone(), (pending, Instant::now() + TTL));
        Ok(tool_result(
            &json!({"state":"approval_required","approval_id":id,
            "message":"请由用户在本地核对并授权，再用相同参数及 approval_id 重试。"}),
            true,
        ))
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        for id in self.pending.keys() {
            self.clear_request(id);
        }
    }
}

fn tool_result(value: &Value, error: bool) -> Value {
    json!({"content":[{"type":"text","text":value.to_string()}],"isError":error})
}

fn send(output: &mut impl Write, response: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(response)?;
    ensure!(bytes.len() <= 1024 * 1024, "MCP 响应超出长度限制");
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn error(id: Value, code: i32, message: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

pub(crate) fn serve(dir: &Path, mut input: impl BufRead, mut output: impl Write) -> Result<()> {
    let mut session = Session {
        dir,
        id: new_id()?,
        pending: HashMap::new(),
        reader: xs_content::ContentReader::new(dir),
    };
    let mut negotiated = false;
    let mut ready = false;
    let mut line = Vec::new();
    loop {
        line.clear();
        if Read::take(&mut input, MAX_FRAME as u64 + 1).read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.len() > MAX_FRAME {
            bail!("MCP 输入超出长度限制");
        }
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            send(&mut output, &error(Value::Null, -32700, "JSON 无效"))?;
            continue;
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str);
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_none() {
            send(
                &mut output,
                &error(id.unwrap_or(Value::Null), -32600, "请求无效"),
            )?;
            continue;
        }
        let method = method.unwrap_or_default();
        let Some(id) = id else {
            if negotiated && method == "notifications/initialized" {
                ready = true;
            }
            continue;
        };
        let params = &message["params"];
        let result = match method {
            "initialize" if !negotiated && params["protocolVersion"].is_string() => {
                negotiated = true;
                json!({"protocolVersion":PROTOCOL,"capabilities":{"tools":{}},
                    "serverInfo":{"name":"xunshu","version":env!("CARGO_PKG_VERSION")}})
            }
            "ping" => json!({}),
            _ if !ready => {
                send(&mut output, &error(id, -32002, "请先初始化会话"))?;
                continue;
            }
            "tools/list" => json!({"tools":[{
                "name":"search_content","description":"在本地已授权正文范围内搜索片段，每次查询须由用户独立授权。",
                "inputSchema":{"type":"object","properties":{"query":{"type":"string","maxLength":MAX_QUERY},
                    "limit":{"type":"integer","minimum":1,"maximum":25},"approval_id":{"type":"string"}},
                    "required":["query"],"additionalProperties":false}
            }]}),
            "tools/call" if params["name"] == "search_content" => {
                let result = serde_json::from_value(params["arguments"].clone())
                    .map_err(anyhow::Error::from)
                    .and_then(|arguments| session.call(arguments));
                result.unwrap_or_else(|error| {
                    tool_result(&json!({"message":error.to_string()}), true)
                })
            }
            _ => {
                send(&mut output, &error(id, -32601, "方法不受支持"))?;
                continue;
            }
        };
        send(
            &mut output,
            &json!({"jsonrpc":"2.0","id":id,"result":result}),
        )?;
    }
}
