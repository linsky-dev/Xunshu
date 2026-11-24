#!/usr/bin/env bash
# 在本机以合成样本执行指定生产解析器的模糊测试。
set -Eeuo pipefail
umask 077
cd -- "$(dirname -- "$0")/.."

target="${1:-}"
duration="${2:-28800}"
case "$target" in query|efu_csv|usn_decode) ;; *)
    echo '用法：bash scripts/fuzz.sh {query|efu_csv|usn_decode} [持续秒数]' >&2
    exit 2 ;;
esac
[[ "$duration" =~ ^[1-9][0-9]{0,5}$ ]] || { echo '持续秒数必须为正整数。' >&2; exit 2; }

export CARGO_NET_OFFLINE=true
output_root="${XUNSHU_FUZZ_OUTPUT_ROOT:-$(pwd -P)}"
if [[ "$output_root" != /* || ! -d "$output_root" || -L "$output_root" ]]; then
    echo '模糊测试输出根必须是现有的本地绝对目录。' >&2
    exit 2
fi
output_root="$(cd -- "$output_root" && pwd -P)"
filesystem="$(findmnt -n -o FSTYPE -T "$output_root")"
case "$filesystem" in
    ext2|ext3|ext4|btrfs|xfs|tmpfs|ramfs|zfs|ntfs|ntfs3) ;;
    9p)
        source="$(findmnt -n -o SOURCE -T "$output_root")"
        [[ -n "${WSL_DISTRO_NAME:-}" && "$source" =~ ^[A-Za-z]:\\$ ]] || exit 2
        ;;
    *) printf '模糊测试拒绝使用此文件系统：%s\n' "$filesystem" >&2; exit 2 ;;
esac
[[ ! -L "$output_root/test-tmp" && ! -L "$output_root/evidence" ]] || exit 2
mkdir -p -- "$output_root/test-tmp" "$output_root/evidence"
work="$(mktemp -d -- "$output_root/test-tmp/fuzz-$target.XXXXXXXX")"
evidence="$(mktemp -d -- "$output_root/evidence/fuzz-$target.XXXXXXXX")"
export TMPDIR="$work"
mkdir -- "$work/corpus" "$evidence/artifacts"
if [[ -d "fuzz/seeds/$target" ]]; then
    cp -R -- "fuzz/seeds/$target/." "$work/corpus/"
fi
if [[ "$target" == usn_decode ]]; then
    python3 - "$work/corpus" <<'PY'
import struct
import sys
from pathlib import Path

corpus = Path(sys.argv[1])
for version, id_size, fixed, length in [(2, 8, 60, 64), (3, 16, 76, 80)]:
    row = bytearray(length)
    struct.pack_into('<IHH', row, 0, length, version, 0)
    row[8:8 + id_size] = bytes([0x23]) * id_size
    row[8 + id_size:8 + 2 * id_size] = bytes([0x45]) * id_size
    struct.pack_into('<q', row, 8 + 2 * id_size, 33)
    struct.pack_into('<HH', row, fixed - 4, 2, fixed)
    row[fixed:fixed + 2] = '文'.encode('utf-16-le')
    (corpus / f'usn-v{version}').write_bytes(struct.pack('<Q', 99) + row)
PY
fi

status=0
{
    printf 'target=%s requested_seconds=%s started_utc=%s\n' "$target" "$duration" "$(date -u +%FT%TZ)"
    cargo +nightly-2026-11-09 fuzz run "$target" "$work/corpus" -- \
        -max_total_time="$duration" -timeout=5 -max_len=65536 \
        -artifact_prefix="$evidence/artifacts/" || status=$?
    printf 'exit=%s finished_utc=%s\n' "$status" "$(date -u +%FT%TZ)"
} > "$evidence/run.log" 2>&1
printf '证据：%s/run.log\n' "$evidence"
exit "$status"
