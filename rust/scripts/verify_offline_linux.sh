#!/usr/bin/env bash
# Run Xunshu Rust verification without weakening the local-filesystem guard.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export CARGO_NET_OFFLINE=true
cargo check --workspace --offline
# The data protection layer rejects overlay filesystems. Use a genuine local
# tmpfs mount for test data (for example /dev/shm in a container).
if [[ "$(stat -f -c %T /dev/shm)" != "tmpfs" ]]; then
  echo 'ERROR: /dev/shm must be a real tmpfs for these integration tests' >&2
  exit 2
fi
TEST_ROOT="$(mktemp -d /dev/shm/xunshu-test.XXXXXXXX)"
trap 'rm -rf -- "$TEST_ROOT"' EXIT
export TMPDIR="$TEST_ROOT"
export XS_CONTENT_TEST_ROOT="$TEST_ROOT"
# The worker must also reside on an allowed filesystem. Do not modify the
# production path policy or symlink to an overlay-backed executable.
cargo build --locked --offline -p xs-content --bin xs-content-worker
WORKER="${CARGO_TARGET_DIR:-$ROOT/target}/debug/xs-content-worker"
cp -- "$WORKER" "$TEST_ROOT/xs-content-worker"
chmod 700 "$TEST_ROOT/xs-content-worker"
export XS_CONTENT_TEST_WORKER_PATH="$TEST_ROOT/xs-content-worker"
cargo test --workspace --locked --offline --no-fail-fast
