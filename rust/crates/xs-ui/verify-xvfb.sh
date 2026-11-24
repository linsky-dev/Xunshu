#!/usr/bin/env bash
set -euo pipefail

if (( $# != 3 )) || (( EUID == 0 )); then
    printf '请以普通用户运行：verify-xvfb.sh <xs-ui> <xunshu> <合成样本父目录>\n' >&2
    exit 2
fi

ui=$(realpath -- "$1")
cli=$(realpath -- "$2")
root=$(realpath -- "$3")
if [[ $(dirname -- "$ui") != "$(dirname -- "$cli")" || ! -x "$ui" || ! -x "$cli" ]]; then
    printf 'xs-ui、xunshu 和 xs-content-worker 必须位于同一构建目录。\n' >&2
    exit 2
fi

fixture=$(mktemp -d "$root/ui-xvfb-XXXXXXXX")
profile=$fixture/profile
documents=$fixture/documents
mkdir -- "$profile" "$documents"
printf '这是只用于 Linux 原生窗口验收的合成文档。原生正文验收。\n' >"$documents/合成报告.txt"
"$cli" --data-dir "$profile" init --root "$documents" >"$fixture/init.json"
"$cli" --data-dir "$profile" scan >"$fixture/scan.json"
"$cli" --data-dir "$profile" content-configure --root "$documents" >"$fixture/content-configure.json"
"$cli" --data-dir "$profile" content-rebuild >"$fixture/content-rebuild.json"

env -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS LIBGL_ALWAYS_SOFTWARE=1 \
    timeout 30s xvfb-run -a -s '-screen 0 1280x800x24' "$ui" --data-dir "$profile" \
    --content --smoke-test --query '原生正文验收' >"$fixture/native.log" 2>&1 || {
        cat -- "$fixture/native.log" >&2
        exit 1
    }

if ! grep -Fq 'XS_UI_CONTENT_SMOKE_OK cjk_font=true' "$fixture/native.log"; then
    cat -- "$fixture/native.log" >&2
    exit 1
fi
printf '通过：Xvfb 原生窗口、独立正文 IPC、预览和中文字体。证据目录 %s\n' "$fixture"
