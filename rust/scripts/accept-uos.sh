#!/usr/bin/env bash
set -euo pipefail

if [[ -r /etc/os-release ]]; then
    . /etc/os-release
else
    printf '%s\n' '无法读取 /etc/os-release，不能确认 UOS 目标环境。' >&2
    exit 2
fi
if [[ "${ID:-}" != uos && "${ID_LIKE:-}" != *uos* ]]; then
    printf '当前系统不是已识别的 UOS 环境：ID=%s ID_LIKE=%s\n' "${ID:-unknown}" "${ID_LIKE:-unknown}" >&2
    exit 2
fi
if [[ "$(uname -m)" != x86_64 ]]; then
    printf '需要 x86_64，实际为 %s\n' "$(uname -m)" >&2
    exit 2
fi

if [[ $# -lt 2 ]]; then
    printf '用法：%s <bin-dir> <output-dir> [installer payload]\n' "$0" >&2
    exit 2
fi
script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
bin_dir="$1"
output_dir="$2"
shift 2
args=("$script_dir/acceptance.py" --bin-dir "$bin_dir" --output-dir "$output_dir")
if [[ $# -ge 2 ]]; then
    args+=(--installer "$1" --payload "$2")
    shift 2
    args+=("$@")
elif [[ $# -eq 1 ]]; then
    printf '%s\n' 'installer 与 payload 必须同时提供。' >&2
    exit 2
fi
exec python3 "${args[@]}"
