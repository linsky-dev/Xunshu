//! 应用进程的本地运行约束。Linux 策略在启动线程或加载推理库前生效。

#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod linux;

/// 对当前进程及后续子进程应用平台支持的出站策略。
/// 返回值区分本进程已启用过滤器和由安装器管理策略的平台。
pub fn enforce_offline() -> std::io::Result<bool> {
    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    {
        linux::enforce()?;
        Ok(true)
    }
    #[cfg(windows)]
    {
        Ok(false)
    }
    #[cfg(not(any(
        windows,
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    )))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "此平台尚无应用离线策略",
        ))
    }
}
