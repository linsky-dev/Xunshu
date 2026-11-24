use anyhow::{ensure, Context, Result};

fn run() -> Result<()> {
    xs_runtime::enforce_offline()?;
    let mut args = std::env::args_os().skip(1);
    let extension = args.next().context("缺少正文格式参数")?;
    let extension = extension.to_str().context("正文格式参数编码无效")?;
    if extension == "--serve" {
        ensure!(args.next().is_none(), "正文服务参数无效");
        return xs_content::extract_worker_serve(std::io::stdin().lock(), std::io::stdout().lock());
    }
    let flag = args.next();
    let ocr = flag.as_deref().is_some_and(|flag| flag == "--ocr");
    ensure!(
        (ocr || flag.is_none()) && args.next().is_none(),
        "正文抽取参数无效"
    );
    xs_content::extract_worker_with_ocr(
        extension,
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        ocr,
    )
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
