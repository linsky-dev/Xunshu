// XXX keep in sync with glutin's build.rs.

use cfg_aliases::cfg_aliases;

fn main() {
    // Setup alias to reduce `cfg` boilerplate.
    cfg_aliases! {
        // Systems.
        android_platform: { target_os = "android" },
        wasm_platform: { target_family = "wasm" },
        macos_platform: { target_os = "macos" },
        ios_platform: { target_os = "ios" },
        apple: { any(ios_platform, macos_platform) },
        free_unix: { all(unix, not(apple), not(android_platform)) },

        // Native displays.
        x11_platform: { all(feature = "x11", free_unix, not(wasm_platform)) },
        wayland_platform: { all(feature = "wayland", free_unix, not(wasm_platform)) },

        // Backends.
        egl_backend: { all(feature = "egl", any(windows, unix), not(apple), not(wasm_platform)) },
        // UOS ARM64 的旧 GLVND 会在 GLX 符号解析时以 SIGSEGV 退出。
        // 在 Linux ARM64 上仅选择 EGL，X11 窗口仍由 winit 管理。
        linux_arm64: { all(target_os = "linux", target_arch = "aarch64") },
        glx_backend: { all(feature = "glx", x11_platform, not(wasm_platform), not(linux_arm64)) },
        wgl_backend: { all(feature = "wgl", windows, not(wasm_platform)) },
        cgl_backend: { all(macos_platform, not(wasm_platform)) },
    }
}
