#!/usr/bin/env bash
# Rust-only regression and CLI contracts; never scans a pre-existing user root.
set -Eeuo pipefail
umask 077
export CARGO_NET_OFFLINE=true
export LC_ALL=C

usage() {
    cat <<'USAGE'
Usage: verify.sh [--temp-root ABSOLUTE_DIRECTORY] [--evidence-root ABSOLUTE_DIRECTORY] [--bin-dir ABSOLUTE_DIRECTORY]

Default temporary base: rust/test-tmp
Default evidence base:  rust/evidence
Each run creates its own random subdirectory. Only that owned temporary directory
is cleaned; logs and earlier evidence remain. Overlay/network fixtures are refused.
The approved cloud fallback must be explicit:
  --temp-root /tmp/xunshu-rust-validation-036c199d3807
Requires an installed Rust toolchain, cached dependencies, Python 3 (JSON checks
only), GNU utilities and findmnt. Does not install/download/elevate privileges.
Runs Rust workspace tests and a small synthetic CLI flow. No old Python suite,
real-disk benchmark, global scan, or desktop acceptance is performed.
With --bin-dir, runs only CLI contracts against the supplied local binaries.
USAGE
}
fail() { printf 'VERIFY FAILED: %s\n' "$*" >&2; exit 1; }
check_directory_path() {
    local candidate="$1"
    case "$candidate" in /*) ;; *) fail "Directory must be absolute: $candidate" ;; esac
    case "$candidate" in //*|*/../*|*/..|*/./*|*/.) fail "Ambiguous path: $candidate" ;; esac
    while [[ "$candidate" != / && "$candidate" == */ ]]; do
        candidate="${candidate%/}"
    done
    while [[ "$candidate" != / ]]; do
        [[ ! -L "$candidate" ]] || fail "Refusing a symbolic-link directory: $candidate"
        [[ ! -e "$candidate" || -d "$candidate" ]] ||
            fail "Not a directory: $candidate"
        candidate="$(dirname -- "$candidate")"
    done
}

workspace="$(cd -- "$(dirname -- "$0")/.." && pwd -P)"
temp_root="$workspace/test-tmp"
evidence_root="$workspace/evidence"
bin_dir=''
run_temp=''
run_identity=''
run_marker=''
log_file=''
while (($#)); do
    case "$1" in
        --temp-root)
            (($# >= 2)) || fail "--temp-root requires a directory"
            temp_root="$2"; shift 2 ;;
        --evidence-root)
            (($# >= 2)) || fail "--evidence-root requires a directory"
            evidence_root="$2"; shift 2 ;;
        --bin-dir)
            (($# >= 2)) || fail "--bin-dir requires a directory"
            bin_dir="$2"; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; fail "Unknown argument: $1" ;;
    esac
done
[[ "$(uname -s)" == Linux ]] || fail "Use verify.ps1 on Windows."
((EUID != 0)) || fail "Run as an ordinary user; permission-denial tests must not run as root."
tools=(python3 findmnt mktemp stat tee sha256sum)
if [[ -z "$bin_dir" ]]; then tools+=(cargo rustc); fi
for tool in "${tools[@]}"; do
    command -v "$tool" >/dev/null || fail "Required development tool is missing: $tool"
done
[[ -f "$workspace/Cargo.toml" && -f "$workspace/Cargo.lock" ]] ||
    fail "Source tree or Cargo.lock is missing."
check_directory_path "$temp_root"
check_directory_path "$evidence_root"
existing="$temp_root"
while [[ ! -d "$existing" ]]; do existing="$(dirname -- "$existing")"; done
fixture_fs="$(findmnt --noheadings --output FSTYPE --target "$existing")"
case "$fixture_fs" in
    ext2|ext3|ext4|btrfs|xfs|tmpfs|ramfs|zfs|vfat|msdos|exfat|ntfs|ntfs3|f2fs|jfs|reiserfs|hfs|hfsplus|ubifs|bcachefs) ;;
    *) fail "Fixture filesystem '$fixture_fs' is not an approved local writable filesystem. Choose an explicit local --temp-root; the policy is not bypassed." ;;
esac

mkdir -p -- "$temp_root" "$evidence_root"
check_directory_path "$temp_root"
check_directory_path "$evidence_root"
temp_root="$(cd -- "$temp_root" && pwd -P)"
evidence_root="$(cd -- "$evidence_root" && pwd -P)"
evidence_dir="$(mktemp -d -- "$evidence_root/verify-linux.XXXXXXXXXX")"
log_file="$evidence_dir/verify.log"
run_temp="$(mktemp -d -- "$temp_root/xunshu-verify.XXXXXXXXXX")"
run_identity="$(stat -c '%d:%i' -- "$run_temp")"
run_marker="$(basename -- "$run_temp")"
printf '%s\n' "$run_marker" > "$run_temp/.xunshu-verification-owner"

cleanup() {
    local status="$?"
    trap - EXIT
    if [[ -n "$run_temp" && -d "$run_temp" && ! -L "$run_temp" &&
          "$(dirname -- "$run_temp")" == "$temp_root" &&
          "$(stat -c '%d:%i' -- "$run_temp")" == "$run_identity" &&
          -f "$run_temp/.xunshu-verification-owner" &&
          ! -L "$run_temp/.xunshu-verification-owner" &&
          "$(cat -- "$run_temp/.xunshu-verification-owner")" == "$run_marker" ]]; then
        if ! find -P "$run_temp" -xdev -depth -delete; then
            printf 'Owned temporary cleanup failed: %s\n' "$run_temp" >&2
            status=1
        fi
    else
        printf 'Cleanup refused: owned directory identity changed; retained %s\n' "$run_temp" >&2
        status=1
    fi
    printf 'verification_exit_code=%s\n' "$status" | tee -a "$log_file"
    printf 'Evidence retained: %s\n' "$evidence_dir"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
export TMPDIR="$run_temp"
export TMP="$run_temp"
export TEMP="$run_temp"

log_command() {
    { printf '$'; printf ' %q' "$@"; printf '\n'; } | tee -a "$log_file"
}
run() {
    log_command "$@"
    "$@" 2>&1 | tee -a "$log_file"
}
run_json() {
    local destination="$1"
    shift
    log_command "$@"
    local status=0
    if "$@" > "$destination" 2> "$destination.stderr"; then
        status=0
    else
        status="$?"
    fi
    cat -- "$destination" "$destination.stderr" | tee -a "$log_file"
    return "$status"
}

cd -- "$workspace"
printf 'fixture_filesystem=%s\nfixture_directory=%s\n' "$fixture_fs" "$run_temp" |
    tee -a "$log_file"
if [[ -z "$bin_dir" ]]; then
    run rustc -vV
    target="$(rustc -vV | awk '/^host: / {print $2}')"
    [[ "$target" == x86_64-unknown-linux-gnu ]] ||
        fail "This verification script currently targets native Linux x64 GNU."
    run cargo test --locked --offline --workspace --target "$target" --target-dir "$workspace/target"
    run cargo build --locked --offline -p xs-cli -p xs-ui --target "$target" --target-dir "$workspace/target"
    bin_dir="$workspace/target/$target/debug"
else
    check_directory_path "$bin_dir"
    printf '%s\n' 'Using supplied local binaries. This invocation checks CLI contracts only.' | tee -a "$log_file"
fi
cli="$bin_dir/xunshu"
ui="$bin_dir/xs-ui"
for binary in "$cli" "$ui"; do
    [[ -f "$binary" && ! -L "$binary" && -x "$binary" ]] || fail "Expected a regular executable: $binary"
done
run sha256sum -- "$cli" "$ui"
run "$cli" --help
run "$ui" --help

files="$run_temp/files"
data="$files/app-private"
catalog_data="$run_temp/catalog-data"
mkdir -p -- "$files/subdir" "$files/.git"
printf 'synthetic report\n' > "$files/report.txt"
printf 'synthetic notes\n' > "$files/subdir/notes.txt"
printf 'synthetic metadata sample; not a real PDF\n' > "$files/预算报告.pdf"
printf 'excluded type\n' > "$files/ignored.bin"
printf 'excluded smart directory\n' > "$files/.git/hidden.txt"
run_json "$evidence_dir/init.json" "$cli" --data-dir "$data" init --root "$files" --ext txt,pdf
run_json "$evidence_dir/scan.json" "$cli" --data-dir "$data" scan
run_json "$evidence_dir/all.json" "$cli" --data-dir "$data" search ''
run_json "$evidence_dir/txt.json" "$cli" --data-dir "$data" search ext:txt
run_json "$evidence_dir/pdf.json" "$cli" --data-dir "$data" search ext:pdf
run_json "$evidence_dir/status.json" "$cli" --data-dir "$data" status
run_json "$evidence_dir/doctor.json" "$cli" --data-dir "$data" doctor --json --export "$run_temp/diagnostics.json"
run_json "$evidence_dir/export.json" "$cli" --data-dir "$data" export-efu "$run_temp/fixture.efu"
run_json "$evidence_dir/import.json" "$cli" --data-dir "$catalog_data" import-efu "$run_temp/fixture.efu" --name fixture
run_json "$evidence_dir/offline.json" "$cli" --data-dir "$catalog_data" search ext:pdf
if run_json "$evidence_dir/invalid.json" "$cli" --data-dir "$data" search unknown_field:value; then
    fail "Unsupported query unexpectedly succeeded."
fi

python3 - "$evidence_dir" "$files" "$data" <<'PY' | tee -a "$log_file"
import json
import sys
from pathlib import Path

evidence, files, data = map(Path, sys.argv[1:])
def load(name):
    return json.loads((evidence / name).read_text(encoding="utf-8"))
def require(condition, message):
    if not condition:
        raise SystemExit("CLI CONTRACT FAILED: " + message)

require(load("init.json")["ok"] is True, "init")
scan = load("scan.json")
require(scan["published"] is True and scan["report"]["complete"] is True, "complete publication")
require(not scan["report"]["errors"] and not scan["report"]["cancelled"], "scan must be error-free")
for name, expected in [("txt.json", {"report.txt", "notes.txt"}), ("pdf.json", {"预算报告.pdf"})]:
    result = load(name)
    require(result["complete"] is True and result["total"] == len(expected), name + " totals")
    require({row["name"] for row in result["results"]} == expected, name + " exact results")
    require(all(row["source"] == "local" for row in result["results"]), "local source")
all_rows = load("all.json")["results"]
require(all(Path(row["path"]) != data and data not in Path(row["path"]).parents for row in all_rows), "self exclusion")
require(all(row["name"] not in {"ignored.bin", ".git", "hidden.txt"} for row in all_rows), "scope filtering")
require(load("status.json")["network"] is False, "offline status")
doctor = load("doctor.json")
require(doctor["index"]["state"] == "readable" and doctor["index"]["coverage_complete"], "diagnostic index status")
require(doctor["settings"]["configured_roots"]["accessible"] == 1 and doctor["network"] is False, "diagnostic scope and offline status")
diagnostics = (files.parent / "diagnostics.json").read_text(encoding="utf-8")
require(json.loads(diagnostics) == doctor and all(word not in diagnostics for word in ("report.txt", "app-private", files.parent.name)), "diagnostic privacy")
offline = load("offline.json")
require(offline["complete"] is True and offline["total"] == 1, "offline import result")
require(offline["results"][0]["source"] == "offline:fixture", "import must grant no local access")
error = json.loads((evidence / "invalid.json.stderr").read_text(encoding="utf-8"))
require(error["ok"] is False and "unknown_field" in error["error"], "explicit unsupported-query error")
print("PASS: Synthetic CLI contracts.")
PY
