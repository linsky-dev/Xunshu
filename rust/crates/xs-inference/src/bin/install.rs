//! 安装激活与回退都使用经过摘要校验及真实推理自检的独立版本目录。
use anyhow::{ensure, Context, Result};
use clap::{Parser, Subcommand};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MANIFEST: &str = "release.json";
const POINTER: &str = "active.json";
const RELEASE_OWNER: &str = ".xunshu-release-owner";
const PROBE_OWNER: &str = ".xunshu-self-test-owner";
const TEST_TEXT: &str = "本地文件";
const MAX_MANIFEST: u64 = 256 * 1024;
const PROGRAMS: [&str; 4] = ["xunshu", "xs-ui", "xs-content-worker", "xs-model-worker"];

#[derive(Parser)]
#[command(about = "寻枢本地安装、修复与回退")]
struct Args {
    #[arg(long)]
    root: PathBuf,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// 校验并安装新版本，也可用原载荷修复当前版本
    Install {
        payload: PathBuf,
        #[arg(long)]
        sha256: String,
    },
    /// 校验并启用上一个安装版本
    Rollback,
    /// 从有效安装指针启动原生界面
    Launch {
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// 仅在当前已授权目录下搜索，适用于目录菜单入口
        #[arg(long)]
        folder: Option<PathBuf>,
    },
    /// 清理历史安装版本及可核验归属的自检数据，不影响当前版/回退版
    Cleanup,
    /// 停用并移除安装版本，保留用户数据
    Uninstall,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: u32,
    version: String,
    platform: String,
    files: BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Version {
    directory: String,
    manifest_sha256: String,
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    active: Option<Version>,
    previous: Option<Version>,
    retained: Vec<Version>,
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    xs_platform::open_local_read(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "安装元数据超过大小限制");
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn safe_component(part: &str) -> bool {
    !part.is_empty()
        && !part.starts_with('.')
        && !part.ends_with('.')
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !matches!(
            part.split('.')
                .next()
                .unwrap_or_default()
                .to_ascii_uppercase()
                .as_str(),
            "CON"
                | "PRN"
                | "AUX"
                | "NUL"
                | "COM1"
                | "COM2"
                | "COM3"
                | "COM4"
                | "COM5"
                | "COM6"
                | "COM7"
                | "COM8"
                | "COM9"
                | "LPT1"
                | "LPT2"
                | "LPT3"
                | "LPT4"
                | "LPT5"
                | "LPT6"
                | "LPT7"
                | "LPT8"
                | "LPT9"
        )
}

fn executable(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn manifest(root: &Path, expected: &str) -> Result<(Manifest, Vec<u8>)> {
    let bytes = read_limited(&root.join(MANIFEST), MAX_MANIFEST)?;
    ensure!(
        digest(&bytes).eq_ignore_ascii_case(expected),
        "安装清单摘要不匹配"
    );
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    ensure!(
        manifest.schema == 1
            && manifest.platform == std::env::consts::OS
            && safe_component(&manifest.version)
            && (1..=128).contains(&manifest.files.len()),
        "安装清单版本、平台或文件数量无效"
    );
    let mut names = BTreeSet::new();
    for (name, entry) in &manifest.files {
        let folded = name.to_ascii_lowercase();
        ensure!(
            name.len() <= 160
                && name.split('/').all(safe_component)
                && folded != MANIFEST
                && names.insert(folded)
                && entry.sha256.len() == 64
                && entry.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                && entry.bytes <= 512 * 1024 * 1024,
            "安装载荷路径或摘要无效：{name}"
        );
    }
    for name in PROGRAMS.map(executable).into_iter().chain([
        "resources/inference.json".to_owned(),
        "resources/fonts/NotoSansSC-Regular.otf".to_owned(),
        "self-test/image.png".to_owned(),
        "self-test/scanned.pdf".to_owned(),
    ]) {
        ensure!(
            manifest.files.contains_key(&name),
            "安装载荷缺少必要文件：{name}"
        );
    }
    Ok((manifest, bytes))
}

fn checked_file(path: &Path, entry: &Entry, mut output: impl Write) -> Result<()> {
    let file = xs_platform::open_local_read(path)?;
    ensure!(
        file.metadata()?.len() == entry.bytes,
        "安装载荷长度不匹配：{}",
        path.display()
    );
    let mut file = file.take(entry.bytes + 1);
    let mut hash = Sha256::new();
    let mut bytes = [0_u8; 64 * 1024];
    let mut count = 0_u64;
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        count += n as u64;
        hash.update(&bytes[..n]);
        output.write_all(&bytes[..n])?;
    }
    ensure!(
        count == entry.bytes && hex(&hash.finalize()).eq_ignore_ascii_case(&entry.sha256),
        "安装载荷内容摘要不匹配：{}",
        path.display()
    );
    Ok(())
}

fn verify(root: &Path, expected: &str) -> Result<()> {
    let (manifest, _) = manifest(root, expected)?;
    for (name, entry) in manifest.files {
        checked_file(&root.join(name), &entry, io::sink())?;
    }
    xs_inference::Assets::verify(&root.join("resources"))?;
    Ok(())
}

fn version_root(root: &Path, version: &Version) -> Result<PathBuf> {
    ensure!(
        version.directory.starts_with("release-") && safe_component(&version.directory),
        "安装状态损坏"
    );
    ensure!(
        version.manifest_sha256.len() == 64
            && version
                .manifest_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "安装状态摘要无效"
    );
    Ok(root.join("versions").join(&version.directory))
}

fn read_state(root: &Path) -> Result<State> {
    match read_limited(&root.join(POINTER), MAX_MANIFEST) {
        Ok(bytes) => {
            let state: State = serde_json::from_slice(&bytes)?;
            validate_state(root, &state)?;
            Ok(state)
        },
        Err(error) if error.downcast_ref::<xs_platform::PlatformError>().is_some_and(|e| {
            matches!(e, xs_platform::PlatformError::Io { source, .. } if source.kind() == io::ErrorKind::NotFound)
        }) => Ok(State::default()),
        Err(error) => Err(error),
    }
}

fn validate_state(root: &Path, state: &State) -> Result<()> {
    let mut retained = BTreeSet::new();
    for version in &state.retained {
        version_root(root, version)?;
        ensure!(retained.insert(&version.directory), "安装状态包含重复版本");
    }
    for version in [&state.active, &state.previous].into_iter().flatten() {
        version_root(root, version)?;
        if let Some(entry) = state
            .retained
            .iter()
            .find(|v| v.directory == version.directory)
        {
            ensure!(
                entry.manifest_sha256 == version.manifest_sha256,
                "安装状态版本摘要冲突"
            );
        }
    }
    ensure!(
        !matches!((&state.active, &state.previous), (Some(a), Some(b)) if a.directory == b.directory),
        "当前和回退版本不能相同"
    );
    Ok(())
}

fn save_state(root: &Path, state: &State) -> Result<()> {
    validate_state(root, state)?;
    let outcome =
        xs_platform::atomic_write_local(&root.join(POINTER), &serde_json::to_vec(state)?, true)?;
    if let Some(warning) = outcome.warning() {
        anyhow::bail!("入口可能已切换，但持久化未确认；禁止继续删除旧版本：{warning}");
    }
    Ok(())
}

fn launch_checked(command: &mut Command) -> Result<()> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + Duration::from_secs(45);
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "安装自检失败：{status}");
            return Ok(());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("安装自检超过时间预算");
        }
        thread::sleep(Duration::from_millis(30));
    }
}

fn self_test(directory: &Path, data: &Path) -> Result<()> {
    let model = directory.join(executable("xs-model-worker"));
    launch_checked(
        Command::new(model)
            .args(["self-test", TEST_TEXT])
            .stdin(Stdio::from(xs_platform::open_local_read(
                &directory.join("self-test/image.png"),
            )?)),
    )?;
    let cli = directory.join(executable("xunshu"));
    let mut configure = Command::new(&cli);
    launch_checked(
        configure
            .arg("--data-dir")
            .arg(data)
            .args(["content-configure", "--ocr", "--root"])
            .arg(directory.join("self-test")),
    )?;
    launch_checked(
        Command::new(&cli)
            .arg("--data-dir")
            .arg(data)
            .arg("content-rebuild"),
    )?;
    // 此入口核对扫描 PDF 的实际命中，而不是只核对查询进程退出码。
    launch_checked(
        Command::new(&cli)
            .arg("--data-dir")
            .arg(data)
            .args(["content-self-test", TEST_TEXT]),
    )?;
    Ok(())
}

/// 仅清理安装器创建且与发布 SHA 一致的自检目录，绝不清理用户数据目录。
fn clean_probe(root: &Path, version: &Version) -> Result<()> {
    let probe = root.join("self-test-data").join(&version.directory);
    if !probe.exists() {
        return Ok(());
    }
    xs_platform::validate_local_path(&probe, false)?;
    let owner = probe.join(PROBE_OWNER);
    match read_limited(&owner, 128) {
        Ok(bytes) if bytes == version.manifest_sha256.as_bytes() => {
            std::fs::remove_dir_all(&probe)?;
            Ok(())
        }
        Ok(_) => anyhow::bail!("自检目录归属摘要不匹配，拒绝清理：{}", probe.display()),
        Err(_) => anyhow::bail!(
            "旧版自检目录缺少归属标记，保留供人工确认：{}",
            probe.display()
        ),
    }
}

fn prepare_probe(root: &Path, version: &Version) -> Result<PathBuf> {
    let probe = root.join("self-test-data").join(&version.directory);
    xs_platform::create_local_dir_all(&root.join("self-test-data"))?;
    xs_platform::create_local_dir_new(&probe)?;
    xs_platform::atomic_write_local(
        &probe.join(PROBE_OWNER),
        version.manifest_sha256.as_bytes(),
        false,
    )?;
    Ok(probe)
}

/// 版本目录只能删除清单拥有的文件，目录必须确认为空后才移除。
fn remove_empty_release_dirs(directory: &Path, names: impl Iterator<Item = String>) -> Result<()> {
    let mut parents: Vec<PathBuf> = Vec::new();
    for name in names {
        let mut parent = Path::new(&name).parent();
        while let Some(relative) = parent {
            if relative.as_os_str().is_empty() {
                break;
            }
            parents.push(directory.join(relative));
            parent = relative.parent();
        }
    }
    parents.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    parents.dedup();
    for path in parents {
        match std::fs::remove_dir(&path) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

/// 在指针持久化后清理既非当前也非回退的版本。
/// 个别文件被占用时保留记录并报告，而不是误报清理成功或阻断已完成安装。
fn cleanup_old_versions(root: &Path, state: &mut State) -> Result<usize> {
    // Ensure the current pointer is durable before any destructive operation.
    save_state(root, state)?;
    let mut keep = Vec::new();
    let mut deleted = 0;
    for version in &state.retained {
        if state
            .active
            .as_ref()
            .is_some_and(|v| v.directory == version.directory)
            || state
                .previous
                .as_ref()
                .is_some_and(|v| v.directory == version.directory)
        {
            keep.push(version.clone());
            continue;
        }
        match remove_version(root, version) {
            Ok(()) => {
                deleted += 1;
                if let Err(error) = clean_probe(root, version) {
                    // 旧版自检数据没有归属标识，不能自动删除，但不应阻塞继续升级。
                    eprintln!(
                        "历史自检数据未清理，请人工核验 {}：{error:#}",
                        version.directory
                    );
                }
            }
            Err(error) => {
                eprintln!(
                    "历史安装目录保留待下次重试 {}：{error:#}",
                    version.directory
                );
                keep.push(version.clone());
            }
        }
    }
    state.retained = keep;
    save_state(root, state)?;
    Ok(deleted)
}

fn install(root: &Path, payload: &Path, expected: &str, state: &mut State) -> Result<()> {
    let (manifest, raw) = manifest(payload, expected)?;
    let mut nonce = [0_u8; 8];
    getrandom::fill(&mut nonce).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let version = Version {
        directory: format!("release-{}-{}", manifest.version, hex(&nonce)),
        manifest_sha256: digest(&raw),
    };
    let destination = version_root(root, &version)?;
    xs_platform::create_local_dir_all(&root.join("versions"))?;
    xs_platform::create_local_dir_new(&destination)?;
    xs_platform::atomic_write_local(
        &destination.join(RELEASE_OWNER),
        version.manifest_sha256.as_bytes(),
        false,
    )?;
    // 先持久化新版本归属，支持安装中断后的 Cleanup 补偿回收。
    let result = (|| -> Result<()> {
        state.retained.push(version.clone());
        save_state(root, state)?;
        // Commit the exact manifest before copying payload files. A crash now
        // leaves an allowlist for safe compensation, not an arbitrary tree.
        ensure!(
            xs_platform::atomic_write_local(&destination.join(MANIFEST), &raw, false)?.is_durable(),
            "安装清单持久化未确认"
        );
        for (name, entry) in &manifest.files {
            let source = payload.join(name);
            let target = destination.join(name);
            xs_platform::create_local_dir_all(target.parent().context("无效的载荷目录")?)?;
            let outcome = xs_platform::write_local_new(&target, |file| {
                checked_file(&source, entry, file).map_err(io::Error::other)
            })?;
            ensure!(outcome.is_durable(), "安装载荷写入未确认持久化");
            #[cfg(unix)]
            if PROGRAMS.contains(&name.as_str()) {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))?;
            }
        }
        verify(&destination, &version.manifest_sha256)?;
        policy(&destination, "install")?;
        let probe = prepare_probe(root, &version)?;
        self_test(&destination, &probe)?;
        state.previous = state.active.replace(version.clone());
        // 激活指针持久化完成后才能清理旧版本。
        save_state(root, state)?;
        cleanup_old_versions(root, state)?;
        Ok(())
    })();
    if let Err(error) = result {
        // 只按磁盘持久化状态判断是否可回收，绝不能误删刚刚成功激活的版本。
        match read_state(root) {
            Ok(mut persisted) => {
                if !persisted
                    .active
                    .as_ref()
                    .is_some_and(|v| v.directory == version.directory)
                {
                    match remove_version(root, &version) {
                        Ok(()) => {
                            if let Err(problem) = clean_probe(root, &version) {
                                eprintln!("失败安装的自检数据无法自动清理：{problem:#}");
                            }
                            persisted
                                .retained
                                .retain(|v| v.directory != version.directory);
                            if let Err(problem) = save_state(root, &persisted) {
                                eprintln!("失败安装清理后写回状态失败：{problem:#}");
                            }
                        }
                        Err(problem) => {
                            eprintln!("失败安装暂存目录未清理，可重试 cleanup：{problem:#}")
                        }
                    }
                }
                *state = persisted;
            }
            Err(problem) => eprintln!("安装状态无法核验，保留临时文件以防误删：{problem:#}"),
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(not(windows))]
fn policy(_directory: &Path, _action: &str) -> Result<()> {
    Ok(())
}

#[cfg(windows)]
fn policy(directory: &Path, action: &str) -> Result<()> {
    let system =
        PathBuf::from(std::env::var_os("SystemRoot").context("无法定位 Windows 系统目录")?);
    let powershell = system.join("System32/WindowsPowerShell/v1.0/powershell.exe");
    for name in PROGRAMS.map(executable) {
        let program = directory.join(name);
        let rule = format!("Xunshu-{}", digest(program.to_string_lossy().as_bytes()));
        let script = r#"
$ErrorActionPreference = 'Stop'
$name = $env:XUNSHU_INSTALL_RULE
$program = $env:XUNSHU_INSTALL_PROGRAM
$rule = Get-NetFirewallRule -PolicyStore PersistentStore -Name $name -ErrorAction SilentlyContinue
if ($rule) {
    $application = $rule | Get-NetFirewallApplicationFilter
    if ($application.Program -ne $program -or $rule.Group -ne 'Xunshu Offline') { throw '防火墙规则归属不符' }
}
if ($env:XUNSHU_INSTALL_ACTION -eq 'remove') {
    if ($rule) {
        if (Get-CimInstance Win32_Process | Where-Object ExecutablePath -eq $program) { throw '请先退出寻枢' }
        $rule | Remove-NetFirewallRule
    }
    exit 0
}
if ($env:XUNSHU_INSTALL_ACTION -eq 'install' -and -not $rule) {
    New-NetFirewallRule -PolicyStore PersistentStore -Name $name -DisplayName '寻枢离线运行' -Group 'Xunshu Offline' -Program $program -Direction Outbound -Action Block -Profile Any -Enabled True | Out-Null
}
$effective = Get-NetFirewallRule -PolicyStore ActiveStore -Name $name
if ($effective.Action -ne 'Block' -or $effective.Direction -ne 'Outbound' -or $effective.Enabled -ne 'True' -or ($effective | Get-NetFirewallApplicationFilter).Program -ne $program) { throw '离线规则没有生效' }
if (Get-NetFirewallProfile -PolicyStore ActiveStore | Where-Object Enabled -ne 'True') { throw '防火墙未对所有配置启用' }
"#;
        let result = Command::new(&powershell)
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("XUNSHU_INSTALL_RULE", rule)
            .env("XUNSHU_INSTALL_PROGRAM", &program)
            .env("XUNSHU_INSTALL_ACTION", action)
            .output()?;
        ensure!(
            result.status.success(),
            "Windows 应用离线规则操作失败：{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    Ok(())
}

fn remove_version(root: &Path, version: &Version) -> Result<()> {
    let directory = version_root(root, version)?;
    if !directory.exists() {
        return Ok(());
    }
    xs_platform::validate_local_path(&directory, false)?;
    let owner_path = directory.join(RELEASE_OWNER);
    let owned = read_limited(&owner_path, 128)
        .is_ok_and(|bytes| bytes == version.manifest_sha256.as_bytes());
    let (manifest, _) = match manifest(&directory, &version.manifest_sha256) {
        Ok(found) => found,
        Err(_error) if owned && !directory.join(MANIFEST).exists() => {
            // With no manifest there is no proof that other files belong to us.
            // Only an owner-marker-only interrupted directory may be removed.
            for item in std::fs::read_dir(&directory)? {
                ensure!(
                    item?.file_name() == RELEASE_OWNER,
                    "缺少清单且包含未知文件，保留待人工确认"
                );
            }
            xs_platform::remove_local_file(&owner_path)?;
            std::fs::remove_dir(&directory)?;
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    policy(&directory, "remove")?;
    for name in manifest.files.keys() {
        let path = directory.join(name);
        match xs_platform::remove_local_file(&path) {
            Ok(()) => (),
            Err(xs_platform::PlatformError::Io { source, .. })
                if source.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    // 先清理子目录，未知文件将导致拒绝删除整个版本目录。
    remove_empty_release_dirs(&directory, manifest.files.keys().cloned())?;
    // 在删除归属清单前确认顶层没有不属于安装器的其他文件。
    for entry in std::fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name();
        ensure!(
            name.to_str()
                .is_some_and(|n| n == MANIFEST || (owned && n == RELEASE_OWNER)),
            "安装目录出现未受管文件，拒绝删除：{}",
            entry.path().display()
        );
    }
    xs_platform::remove_local_file(&directory.join(MANIFEST))?;
    if owned {
        xs_platform::remove_local_file(&owner_path)?;
    }
    std::fs::remove_dir(&directory)?;
    Ok(())
}

fn run() -> Result<()> {
    let args = Args::parse();
    let root = std::path::absolute(args.root)?;
    xs_platform::create_local_dir_all(&root)?;
    let lock: File = xs_platform::open_or_create_local_lock(&root.join("install.lock"))?;
    FileExt::lock_exclusive(&lock)?;
    let mut state = read_state(&root)?;
    match args.command {
        Action::Install { payload, sha256 } => {
            install(&root, &std::path::absolute(payload)?, &sha256, &mut state)
        }
        Action::Rollback => {
            let previous = state.previous.as_ref().context("没有可回退的安装版本")?;
            let directory = version_root(&root, previous)?;
            verify(&directory, &previous.manifest_sha256)?;
            policy(&directory, "verify")?;
            self_test(
                &directory,
                &root.join("self-test-data").join(&previous.directory),
            )?;
            std::mem::swap(&mut state.active, &mut state.previous);
            save_state(&root, &state)
        }
        Action::Launch { data_dir, folder } => {
            let version = state.active.as_ref().context("寻枢尚未安装")?;
            let directory = version_root(&root, version)?;
            policy(&directory, "verify")?;
            let (manifest, _) = manifest(&directory, &version.manifest_sha256)?;
            let ui = executable("xs-ui");
            let program = directory.join(&ui);
            for name in PROGRAMS.map(executable) {
                checked_file(&directory.join(&name), &manifest.files[&name], io::sink())?;
            }
            let mut command = Command::new(program);
            if let Some(data) = data_dir {
                let data = std::path::absolute(data)?;
                if let Some(folder) = folder {
                    let folder = std::path::absolute(folder)?;
                    let settings = xs_platform::Settings::load(data.join("settings.json"))?;
                    ensure!(
                        settings.allows_filename(&folder, true)?,
                        "目录未获当前配置授权"
                    );
                    xs_platform::validate_local_path(&folder, false)?;
                    ensure!(folder.is_dir(), "所选路径不是本地目录");
                    let query = folder
                        .to_str()
                        .filter(|value| !value.contains('"'))
                        .context("目录无法表示为安全查询")?;
                    command.arg("--query").arg(format!("ancestor:\"{query}\""));
                }
                command.arg("--data-dir").arg(data);
            } else {
                ensure!(folder.is_none(), "目录入口需要明确的 --data-dir");
            }
            command.spawn()?;
            Ok(())
        }
        Action::Cleanup => {
            let cleaned = cleanup_old_versions(&root, &mut state)?;
            eprintln!("历史版本清理完成：{cleaned} 个；当前版及回退版均已保留");
            Ok(())
        }
        Action::Uninstall => {
            // 兼容老版本缺少 retained 记录但 active/previous 仍有效的情况。
            let mut versions = state.retained.clone();
            for version in [&state.active, &state.previous].into_iter().flatten() {
                if !versions.iter().any(|v| v.directory == version.directory) {
                    versions.push(version.clone());
                }
            }
            state.active = None;
            state.previous = None;
            state.retained = versions.clone();
            save_state(&root, &state)?;
            for version in &versions {
                remove_version(&root, version)?;
                if let Err(error) = clean_probe(&root, version) {
                    // 旧安装目录缺标记时不可误删；卸载继续，并明确残留。
                    eprintln!("用户数据已保留，自检目录需手动确认：{error:#}");
                }
            }
            save_state(&root, &State::default())
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("安装操作失败：{error:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    fn version() -> Version {
        Version {
            directory: "release-test-123".into(),
            manifest_sha256: "a".repeat(64),
        }
    }

    #[test]
    fn state_validation_refuses_conflicting_or_duplicate_version_records() {
        let root = Path::new("/test");
        let v = version();
        let mut state = State {
            active: Some(v.clone()),
            previous: None,
            retained: vec![v.clone()],
        };
        assert!(validate_state(root, &state).is_ok());
        state.previous = Some(v.clone());
        assert!(validate_state(root, &state).is_err());
        state.previous = None;
        state.retained.push(v.clone());
        assert!(validate_state(root, &state).is_err());
        state.retained.pop();
        state.retained[0].manifest_sha256 = "b".repeat(64);
        assert!(validate_state(root, &state).is_err());
        state.retained.clear();
        assert!(
            validate_state(root, &state).is_ok(),
            "legacy retained records may be absent"
        );
        state.active.as_mut().unwrap().manifest_sha256 = "not-a-hash".into();
        assert!(validate_state(root, &state).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn interrupted_install_without_manifest_preserves_unknown_files() {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().join(format!("xs-install-test-{}", hex(&random)));
        xs_platform::create_local_dir_all(&root).unwrap();
        let v = version();
        let directory = version_root(&root, &v).unwrap();
        xs_platform::create_local_dir_all(&directory).unwrap();
        let marker = directory.join(RELEASE_OWNER);
        std::fs::write(&marker, v.manifest_sha256.as_bytes()).unwrap();
        let unknown = directory.join("keep.txt");
        std::fs::write(&unknown, "not owned by the installer").unwrap();
        assert!(remove_version(&root, &v).is_err());
        assert_eq!(
            std::fs::read_to_string(&unknown).unwrap(),
            "not owned by the installer"
        );
        std::fs::remove_file(&unknown).unwrap();
        remove_version(&root, &v).unwrap();
        assert!(!directory.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
