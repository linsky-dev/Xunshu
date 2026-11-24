use anyhow::{bail, ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    io::{BufRead, Read, Write},
    sync::atomic::AtomicBool,
};
use xs_inference::{Assets, Embedder, Ocr};

const MAX_FRAME: usize = 128 * 1024;
const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: u64,
    text: String,
}

fn send(output: &mut impl Write, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() <= 8 * 1024 * 1024, "模型响应超出长度上限");
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn serve(assets: &Assets, mut input: impl BufRead, mut output: impl Write) -> Result<()> {
    let mut model = Embedder::new(assets)?;
    let cancel = AtomicBool::new(false);
    let mut frame = Vec::new();
    send(
        &mut output,
        &json!({"protocol":1,"model":"bge-small-zh-v1.5","dimension":512,"fingerprint":assets.embedding_fingerprint()}),
    )?;
    loop {
        frame.clear();
        if Read::take(&mut input, MAX_FRAME as u64 + 1).read_until(b'\n', &mut frame)? == 0 {
            return Ok(());
        }
        ensure!(frame.len() <= MAX_FRAME, "模型请求超出长度上限");
        let response = match serde_json::from_slice::<Request>(&frame) {
            Ok(request) => match model.embed(&request.text, &cancel) {
                Ok(windows) => json!({"id":request.id,"ok":true,"windows":windows}),
                Err(error) => json!({"id":request.id,"ok":false,"error":error.to_string()}),
            },
            Err(error) => json!({"id":null,"ok":false,"error":format!("模型请求无效：{error}")}),
        };
        send(&mut output, &response)?;
    }
}

fn read_image(input: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input
        .take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_IMAGE_BYTES, "图片超出长度上限");
    Ok(bytes)
}

fn self_test(assets: &Assets, expected: &str) -> Result<Value> {
    let cancel = AtomicBool::new(false);
    let mut model = Embedder::new(assets)?;
    let mut vec = |text: &str| -> Result<Vec<f32>> {
        Ok(model
            .embed(text, &cancel)?
            .pop()
            .context("语义自检没有输出")?
            .vector)
    };
    let query = vec("查找本地文档")?;
    let related = vec("如何在电脑上搜索文件和文档")?;
    let unrelated = vec("蓝鲸以磷虾为食，生活在海洋中")?;
    let cosine = |other: &[f32]| query.iter().zip(other).map(|(a, b)| a * b).sum::<f32>();
    let related_score = cosine(&related);
    let unrelated_score = cosine(&unrelated);
    ensure!(related_score > unrelated_score, "中文语义自检排序异常");
    let long = format!("{}正文末尾关键词", "正文前段。".repeat(140));
    let windows = model.embed(&long, &cancel)?;
    ensure!(
        windows.len() > 1
            && windows
                .last()
                .is_some_and(|window| window.end == long.len()),
        "长文自检没有覆盖尾部"
    );
    drop(model);

    let mut ocr = Ocr::new(assets)?;
    let lines = ocr.recognize(&read_image(std::io::stdin().lock())?, &cancel)?;
    let text = lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    ensure!(
        !expected.is_empty() && text.contains(expected) && text.contains("XUNSHU OFFLINE"),
        "OCR 自检未识别预期文本：{text}"
    );
    Ok(
        json!({"ok":true,"semantic":{"related":related_score,"unrelated":unrelated_score,"windows":windows.len()},"ocr":{"text":text,"lines":lines.len()}}),
    )
}

fn run() -> Result<()> {
    xs_runtime::enforce_offline()?;
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--help"] {
        println!("寻枢模型工作进程\n  serve  从标准输入接受语义请求\n  ocr  从标准输入读取图片\n  self-test <预期识别文本>  执行内置模型自检");
        return Ok(());
    }
    let assets = Assets::installed()?;
    match args.as_slice() {
        [mode] if mode == "serve" => {
            serve(&assets, std::io::stdin().lock(), std::io::stdout().lock())
        }
        [mode] if mode == "ocr" => {
            let mut ocr = Ocr::new(&assets)?;
            let lines = ocr.recognize(
                &read_image(std::io::stdin().lock())?,
                &AtomicBool::new(false),
            )?;
            send(
                &mut std::io::stdout().lock(),
                &json!({"ok":true,"lines":lines}),
            )
        }
        [mode, expected] if mode == "self-test" => send(
            &mut std::io::stdout().lock(),
            &self_test(&assets, expected)?,
        ),
        _ => bail!("模型进程参数无效，运行 --help 查看用法"),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
