#!/bin/sh
set -eu

ui=$1
evidence=$2
mkdir -p "$evidence"
evidence=$(cd "$evidence" && pwd -P)
profile=$(mktemp -d "$evidence/profile.XXXXXXXX")

if [ "$(uname -m)" != aarch64 ]; then
    echo 'EGL 原生窗口回归需要 ARM64 Linux。' >&2
    exit 1
fi

# 在 X11 关闭 GLX 扩展，以软件 EGL 启动原生窗口并记录实际加载的库。
if ! env -u WAYLAND_DISPLAY -u WAYLAND_SOCKET WINIT_UNIX_BACKEND=x11 \
    LIBGL_ALWAYS_SOFTWARE=1 xvfb-run -a -s '-screen 0 1280x900x24 -extension GLX' \
    timeout 120s env LD_DEBUG=libs,files LD_DEBUG_OUTPUT="$profile/loader" \
    "$ui" --smoke-test --data-dir "$profile/settings" > "$profile/ui.log" 2>&1; then
    cat "$profile/ui.log"
    exit 1
fi

cat "$profile/ui.log"
grep -F 'XS_UI_SMOKE_OK' "$profile/ui.log"
grep -E 'calling init: .*libEGL\.so' "$profile"/loader.*
if grep -E 'libGLX\.so|libGLX_[[:alnum:]_]+\.so' "$profile"/loader.*; then
    echo '窗口启动曾尝试加载 GLX。' >&2
    exit 1
fi
echo 'XS_ARM64_X11_EGL_OK'
