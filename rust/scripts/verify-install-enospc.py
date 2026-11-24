"""Linux native ENOSPC installer regression using a dedicated empty small mount.

The caller must create an isolated disposable local mount, e.g. tmpfs in a
private mount namespace. Never fill an existing user volume or shared /dev/shm.
This is a failure-only test, not a successful model installation.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def digest(data):
    return hashlib.sha256(data).hexdigest()


def verify(installer, work, payload):
    root = work / "install"
    versions = root / "versions"
    versions.mkdir(parents=True)
    previous = {"directory": "release-test-old", "manifest_sha256": "a" * 64}
    old = versions / previous["directory"]
    old.mkdir()
    (old / ".xunshu-release-owner").write_text("a" * 64, encoding="ascii")
    state = {"active": previous, "previous": None, "retained": [previous]}
    pointer = root / "active.json"
    pointer.write_text(json.dumps(state), encoding="utf-8")
    user = root / "user-data.txt"
    user.write_bytes(b"synthetic preserved user data")
    names = ["xunshu", "xs-ui", "xs-content-worker", "xs-model-worker",
             "resources/inference.json", "resources/fonts/NotoSansSC-Regular.otf",
             "self-test/image.png", "self-test/scanned.pdf"]
    stat = os.statvfs(work)
    available = stat.f_bavail * stat.f_frsize
    # At most 16 MiB allocated on the separate source volume.
    data = b"x" * (available + 65536)
    files = {}
    for name in names:
        content = data if name == "xunshu" else b"invalid synthetic content"
        path = payload / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content)
        files[name] = {"bytes": len(content), "sha256": digest(content)}
    raw = json.dumps({"schema": 1, "version": "enospc-test", "platform": "linux", "files": files}, sort_keys=True).encode()
    (payload / "release.json").write_bytes(raw)
    command = [str(installer), "--root", str(root)]
    result = subprocess.run(command + ["install", str(payload), "--sha256", digest(raw)],
                            capture_output=True, encoding="utf-8", errors="replace", timeout=30,
                            env={**os.environ, "LC_ALL": "C"})
    assert result.returncode != 0, "invalid payload must not activate"
    assert "No space left on device" in result.stderr or "os error 28" in result.stderr, result.stderr
    after = json.loads(pointer.read_text(encoding="utf-8"))
    assert after == state, after
    assert user.read_bytes() == b"synthetic preserved user data"
    assert list(versions.iterdir()) == [old], list(versions.iterdir())
    assert (old / ".xunshu-release-owner").read_text() == "a" * 64
    cleanup = subprocess.run(command + ["cleanup"], capture_output=True,
                             encoding="utf-8", errors="replace", timeout=30)
    assert cleanup.returncode == 0, cleanup.stderr
    assert json.loads(pointer.read_text(encoding="utf-8")) == state
    assert user.read_bytes() == b"synthetic preserved user data"
    final = os.statvfs(work)
    print(json.dumps({"ok": True, "scope": "isolated Linux tmpfs native ENOSPC; no successful installation",
                      "mount_bytes": stat.f_blocks * stat.f_frsize, "available_before": available,
                      "attempted_file_bytes": len(data), "available_after": final.f_bavail * final.f_frsize,
                      "exit_code": result.returncode, "stderr": result.stderr,
                      "verified": ["kernel_enospc", "active_preserved", "user_data_preserved", "failed_version_removed", "cleanup_retries"]}, ensure_ascii=False))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, required=True)
    parser.add_argument("--empty-mount", type=Path, required=True)
    parser.add_argument("--payload-temp-root", type=Path, required=True)
    args = parser.parse_args()
    mount = args.empty_mount.resolve(strict=True)
    source = args.payload_temp_root.resolve(strict=True)
    if sys.platform != "linux" or mount in (Path("/"), Path("/dev/shm")) or not os.path.ismount(mount):
        raise SystemExit("Requires a dedicated disposable Linux mount")
    if any(mount.iterdir()) or os.stat(mount).st_dev == os.stat(source).st_dev:
        raise SystemExit("Mount must be empty and separate from the payload volume")
    stat = os.statvfs(mount)
    total = stat.f_blocks * stat.f_frsize
    if not 512 * 1024 <= total <= 8 * 1024 * 1024:
        raise SystemExit("Dedicated mount must be 512 KiB to 8 MiB")
    with tempfile.TemporaryDirectory(prefix="xs-enospc-", dir=mount) as work:
        with tempfile.TemporaryDirectory(prefix="xs-enospc-source-", dir=source) as payload:
            verify(args.installer.resolve(), Path(work), Path(payload))


if __name__ == "__main__":
    main()
