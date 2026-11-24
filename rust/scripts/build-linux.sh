#!/usr/bin/env bash
# Build the two native binaries. This script does not install or download tools.
set -Eeuo pipefail
umask 077
export LC_ALL=C
export CARGO_NET_OFFLINE=true

usage() {
    cat <<'USAGE'
Usage: build-linux.sh [--output-root ABSOLUTE_DIRECTORY] [--max-glibc VERSION]

Run from any directory. The default output root is $HOME/Xunshu-builds.
Creates a new package-linux-x64.* directory containing only:
  xunshu  xs-ui  xs-content-worker  xs-model-worker  xs-install  README.txt
Existing package directories are never replaced or deleted.
--max-glibc rejects an artifact whose non-weak GLIBC version need exceeds VERSION.
This is a source build, not an installer or a UOS compatibility certification.
Dependencies and the Rust toolchain must already be installed and cached.
USAGE
}

fail() { printf 'BUILD FAILED: %s\n' "$*" >&2; exit 1; }

check_directory_path() {
    local candidate="$1"
    case "$candidate" in
        /*) ;;
        *) fail "Directory must be absolute: $candidate" ;;
    esac
    case "$candidate" in
        //*|*/../*|*/..|*/./*|*/.) fail "Ambiguous directory path: $candidate" ;;
    esac
    # lstat-style -L on "link/" can hide the link; inspect the component itself.
    while [[ "$candidate" != / && "$candidate" == */ ]]; do
        candidate="${candidate%/}"
    done
    while [[ "$candidate" != / ]]; do
        [[ ! -L "$candidate" ]] || fail "Refusing a symbolic-link directory: $candidate"
        if [[ -e "$candidate" && ! -d "$candidate" ]]; then
            fail "A directory component is not a directory: $candidate"
        fi
        candidate="$(dirname -- "$candidate")"
    done
}

output_root="$HOME/Xunshu-builds"
max_glibc=''
package_dir=''
while (($#)); do
    case "$1" in
        --output-root)
            (($# >= 2)) || fail "--output-root requires a directory"
            output_root="$2"; shift 2 ;;
        --max-glibc)
            (($# >= 2)) || fail "--max-glibc requires a version"
            max_glibc="$2"; shift 2 ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; fail "Unknown argument: $1" ;;
    esac
done

[[ "$(uname -s)" == Linux ]] || fail "Use build-windows.ps1 on Windows."
if [[ -n "$max_glibc" && ! "$max_glibc" =~ ^[0-9]+(\.[0-9]+)+$ ]]; then
    fail "--max-glibc must be a numeric version such as 2.35"
fi
for tool in cargo rustc readelf sha256sum sort awk sed tail mktemp; do
    command -v "$tool" >/dev/null || fail "Required build tool is missing: $tool"
done

workspace="$(cd -- "$(dirname -- "$0")/.." && pwd -P)"
[[ -f "$workspace/Cargo.toml" && -f "$workspace/Cargo.lock" ]] ||
    fail "Run this script from an intact rust/scripts source tree."
check_directory_path "$output_root"
cd -- "$workspace"

rust_details="$(rustc -vV)"
target="$(printf '%s\n' "$rust_details" | awk '/^host: / {print $2}')"
[[ "$target" == x86_64-unknown-linux-gnu ]] ||
    fail "This script packages native x86_64-unknown-linux-gnu builds; found $target."
target_dir="$workspace/target"
printf 'Building %s from %s (locked dependencies, offline).\n' "$target" "$workspace"
cargo build --locked --release -p xs-cli -p xs-ui -p xs-content -p xs-inference --offline \
    --target "$target" --target-dir "$target_dir"

mkdir -p -- "$output_root"
check_directory_path "$output_root"
output_root="$(cd -- "$output_root" && pwd -P)"
package_dir="$(mktemp -d -- "$output_root/package-linux-x64.XXXXXXXXXX")"
trap 'status=$?; if ((status != 0)); then printf "BUILD FAILED (exit %s). Incomplete output retained at: %s\n" "$status" "$package_dir" >&2; fi' EXIT

for binary in xunshu xs-ui xs-content-worker xs-model-worker xs-install; do
    source_file="$target_dir/$target/release/$binary"
    [[ -f "$source_file" && -x "$source_file" && ! -L "$source_file" ]] ||
        fail "Expected regular executable was not produced: $source_file"
    # The directory is new and private; noclobber also refuses pre-existing files.
    (set -o noclobber; cat -- "$source_file" > "$package_dir/$binary")
    chmod 755 -- "$package_dir/$binary"
done

report=''
for binary in xunshu xs-ui xs-content-worker xs-model-worker xs-install; do
    artifact="$package_dir/$binary"
    elf_header="$(readelf --wide --file-header "$artifact")"
    printf '%s\n' "$elf_header" | awk '/Machine:/ {print}' | \
        grep -q 'Advanced Micro Devices X86-64' ||
        fail "Unexpected ELF architecture in $binary"
    version_info="$(readelf --wide --version-info "$artifact")"
    dynamic_info="$(readelf --wide --dynamic "$artifact")"
    interpreter="$(readelf --wide --program-headers "$artifact" |
        sed -n 's/.*Requesting program interpreter: \(.*\)]/\1/p')"
    needed="$(printf '%s\n' "$dynamic_info" |
        sed -n 's/.*(NEEDED).*Shared library: \[\(.*\)\]/\1/p')"
    all_versions="$(printf '%s\n' "$version_info" |
        awk '/Name: GLIBC_[0-9]/ {for (i=1;i<=NF;i++) if ($i=="Name:") print $(i+1)}' |
        sort -Vu)"
    strong_versions="$(printf '%s\n' "$version_info" |
        awk '/Name: GLIBC_[0-9]/ && !/Flags: WEAK/ {for (i=1;i<=NF;i++) if ($i=="Name:") print $(i+1)}' |
        sort -Vu)"
    weak_versions="$(printf '%s\n' "$version_info" |
        awk '/Name: GLIBC_[0-9]/ && /Flags: WEAK/ {for (i=1;i<=NF;i++) if ($i=="Name:") print $(i+1)}' |
        sort -Vu)"
    if printf '%s\n' "$version_info" | grep -q 'Name: GLIBC_PRIVATE'; then
        fail "$binary references private GLIBC interfaces; no compatibility result is issued."
    fi
    strong_max="$(printf '%s\n' "$strong_versions" | tail -n 1)"
    all_max="$(printf '%s\n' "$all_versions" | tail -n 1)"
    if [[ -z "$all_versions" ]]; then
        [[ -z "$needed" && -z "$interpreter" ]] ||
            fail "Cannot determine the dynamic GLIBC requirements of $binary."
        strong_max='none (static ELF)'
        all_max='none (static ELF)'
    elif [[ -z "$strong_versions" ]]; then
        fail "GLIBC declarations found without a determinable non-weak requirement: $binary"
    fi
    if [[ -n "$max_glibc" && -n "$strong_versions" ]]; then
        limit="GLIBC_$max_glibc"
        highest="$(printf '%s\n%s\n' "$strong_max" "$limit" | sort -V | tail -n 1)"
        [[ "$highest" == "$limit" ]] ||
            fail "$binary requires $strong_max, exceeding the requested $limit ceiling."
    fi
    digest="$(sha256sum -- "$artifact" | awk '{print $1}')"
    report+="
$binary
  SHA-256: $digest
  Highest non-weak GLIBC version need: $strong_max
  Highest declared GLIBC tag, including weak references: $all_max
  ELF interpreter: $interpreter
  DT_NEEDED (direct dependencies only):
$needed
  Weak GLIBC version needs (not a hard minimum by themselves):
$weak_versions
"
    printf '%s: non-weak %s; all declared tags %s\n' "$binary" "$strong_max" "$all_max"
done

lock_digest="$(sha256sum -- "$workspace/Cargo.lock" | awk '{print $1}')"
build_time="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
host_system="$(uname -srmo)"
(
    set -o noclobber
    cat > "$package_dir/README.txt" <<README
Xunshu Rust - locally compiled Linux x64 binaries

This directory is a source-build output, not an installer or a signed release.
包含命令行、原生界面、正文抽取、模型推理、安装工具和本说明。
No Python, Node.js, PyQt, WebView, toolchain, model, or user index is bundled.
The desktop still needs the host's graphics stack, fonts, and desktop services.

Built UTC: $build_time
Build host: $host_system
Rust target: $target
Cargo.lock SHA-256: $lock_digest
Build command:
  cargo build --locked --release -p xs-cli -p xs-ui -p xs-content -p xs-inference --offline --target $target

$rust_details

Start:
  ./xunshu --help
  ./xs-ui
Or pass --data-dir /absolute/local/new-data-directory to either program.
Use a separate new directory; do not point this version at the old schema4 store.
Startup does not scan a disk. Save explicit roots before choosing a scan.
This Linux build includes filename search, native UI and lexical content search.
OCR/semantic require separately prepared, verified model resources. Windows-only
MFT/USN and desktop integration are not certified by this Linux build.
See the current QA report for exact validation scope.

ELF ABI inspection of the copied binaries:
$report
These are measured version requirements, not full platform certification.
The non-weak maximum is a necessary symbol-version condition. Other shared
libraries, dynamically loaded graphics libraries, kernel features and drivers
must also work on the destination system. Weak version tags are listed separately.
A Debian build is not thereby compatible with UOS V20 1070.
Build against the target system's userspace and run its desktop acceptance tests.

Build prerequisites are development-only. Cargo was run offline and locked;
this script did not install dependencies, elevate privileges, or install a service.
See the accompanying source tree's docs/PLATFORM_BUILD.md, docs/FEATURE_PARITY.md
and VALIDATION.md for migration limits and the recorded validation status.
This directory is intended for local validation; final distributable packaging
and the complete locked-dependency license notices remain separate release work.
README
)
printf 'Source-build package created: %s\n' "$package_dir"
