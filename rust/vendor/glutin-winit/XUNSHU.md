# glutin-winit 0.5.0

源自 crates.io 的 MIT 版本，对应上游提交 `0811b6c753334f1681c7db8a48e407596cfa9366`。保留上游许可证、库代码和 Cargo 清单。

寻枢只修改 `build.rs` 中的后端选择。Linux ARM64 可用 EGL 与 X11 或 Wayland，不会启用 GLX 回退。这样可以避免 UOS 20 的旧 GLVND 在解析 GLX 函数时退出。Windows 和其他 Linux 架构继续采用上游选择。

升级 eframe 或 glutin-winit 时，用 ARM64 原生窗口回归检查 GLX 是否仍被排除，再决定是否移除此补丁。
