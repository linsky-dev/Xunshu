#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn run() -> anyhow::Result<()> {
    use anyhow::{ensure, Context};
    use std::{
        os::windows::process::CommandExt,
        process::{Command, Stdio},
    };
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let executable = std::env::current_exe()?;
    let root = executable.parent().context("无法定位寻枢安装目录")?;
    let manager = root.join("xs-install.exe");
    xs_platform::validate_local_path(&manager, false)
        .context("安装管理程序缺失，请重新运行安装包")?;
    let status = Command::new(manager)
        .arg("--root")
        .arg(root)
        .args(["launch", "--interactive"])
        // 支持在隔离数据目录验证桌面入口。安装根目录以启动器位置为准。
        .args(std::env::args_os().skip(1))
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("无法启动寻枢安装管理程序")?;
    // 退出码 1 由安装管理程序在关闭诊断对话框后返回。
    ensure!(
        matches!(status.code(), Some(0 | 1)),
        "安装管理程序启动失败（退出码 0x{:08X}）",
        status.code().unwrap_or(-1) as u32
    );
    if status.code() == Some(1) {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(windows)]
fn main() {
    if let Err(error) = run() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR};
        let title: Vec<u16> = "寻枢启动失败".encode_utf16().chain([0]).collect();
        let message: Vec<u16> = format!("{error:#}").encode_utf16().chain([0]).collect();
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                message.as_ptr(),
                title.as_ptr(),
                MB_ICONERROR,
            )
        };
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("请使用 xs-ui 启动寻枢");
    std::process::exit(1);
}
