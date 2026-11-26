"""Native installer failure contracts; no executable payload or real model required.

Never reports successful installation or inference. Uses only a new synthetic
root on a supported local filesystem; every attempted launch must be rejected.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import struct
import shutil
import tempfile


def digest(data):
    return hashlib.sha256(data).hexdigest()


def write_manifest(payload, files):
    manifest = {"schema": 1, "version": "test", "platform": "windows" if os.name == "nt" else "linux",
                "files": files}
    raw = json.dumps(manifest, ensure_ascii=False, sort_keys=True).encode("utf-8")
    (payload / "release.json").write_bytes(raw)
    return digest(raw)


def verify(installer, work):
    root = work / "install"
    versions = root / "versions"
    versions.mkdir(parents=True)
    payload = work / "payload"
    payload.mkdir()
    suffix = ".exe" if os.name == "nt" else ""
    names = [name + suffix for name in ("xunshu", "xs-ui", "xs-content-worker", "xs-model-worker")]
    names += ["resources/inference.json", "resources/fonts/NotoSansSC-Regular.otf",
              "self-test/image.png", "self-test/scanned.pdf"]
    # Non-executable synthetic bytes; inference manifest is deliberately invalid.
    data = b"synthetic invalid payload\n"
    files = {name: {"bytes": len(data), "sha256": digest(data)} for name in names}
    for name in names:
        path = payload / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    sha = write_manifest(payload, files)
    retained = []
    for name in ("release-test-old", "release-test-previous"):
        directory = versions / name
        directory.mkdir()
        (directory / ".xunshu-release-owner").write_text("a" * 64, encoding="ascii")
        retained.append({"directory": name, "manifest_sha256": "a" * 64})
    pointer = root / "active.json"
    # Legacy state, deliberately missing retained entries.
    pointer.write_text(json.dumps({"active": retained[0], "previous": retained[1], "retained": []}), encoding="utf-8")
    user = root / "user-data.txt"
    user.write_bytes(b"synthetic user data")
    checked = []

    def run(label, *args, expected):
        proc = subprocess.run([str(installer), "--root", str(root), *args], capture_output=True,
                              encoding="utf-8", errors="replace", timeout=30)
        if proc.returncode == 0 or expected not in proc.stderr:
            raise AssertionError(f"{label}: unexpected result {proc.returncode}\n{proc.stdout}\n{proc.stderr}")
        current = json.loads(pointer.read_text(encoding="utf-8"))
        assert current["active"] == retained[0] and current["previous"] == retained[1], label
        assert user.read_bytes() == b"synthetic user data", label
        assert {p.name for p in versions.iterdir()} == {v["directory"] for v in retained}, label
        for v in retained:
            assert (versions / v["directory"] / ".xunshu-release-owner").read_text() == "a" * 64, label
        checked.append(label)
        print(json.dumps({"case": label, "exit_code": proc.returncode, "stderr": proc.stderr}, ensure_ascii=False), flush=True)

    run("wrong_manifest_digest", "install", str(payload), "--sha256", "0" * 64, expected="安装清单摘要不匹配")
    conflict = dict(files)
    conflict["Resources/extra.bin"] = files[names[0]]
    invalid = write_manifest(payload, conflict)
    run("case_collision", "install", str(payload), "--sha256", invalid, expected="安装目录大小写冲突")
    sha = write_manifest(payload, files)
    (payload / names[0]).write_bytes(b"changed")
    run("corrupt_program", "install", str(payload), "--sha256", sha, expected="安装载荷长度不匹配")
    (payload / names[0]).write_bytes(data)
    run("invalid_model_manifest", "install", str(payload), "--sha256", sha, expected="内置模型清单损坏")
    current = json.loads(pointer.read_text(encoding="utf-8"))
    assert current["retained"] == retained, "legacy identities not preserved"
    # An explicit cleanup can preserve all live pointers without valid models.
    proc = subprocess.run([str(installer), "--root", str(root), "cleanup"], capture_output=True,
                          encoding="utf-8", errors="replace", timeout=30)
    assert proc.returncode == 0, proc.stderr
    assert user.read_bytes() == b"synthetic user data"
    assert {p.name for p in versions.iterdir()} == {v["directory"] for v in retained}
    checked.append("cleanup_preserves_active_and_previous")
    # Simulate a process interrupted after journaling its rollback probe. This
    # exercises the native cleanup command, not actual inference or rollback.
    pending = {"directory": "release-rollback-" + "1" * 32, "manifest_sha256": "b" * 64}
    unknown = {"directory": "release-old-unknown", "manifest_sha256": "c" * 64}
    probe = root / "self-test-data" / pending["directory"]
    probe.mkdir(parents=True)
    (probe / ".xunshu-self-test-owner").write_text("b" * 64, encoding="ascii")
    (probe / "synthetic.txt").write_bytes(b"owned synthetic probe")
    unowned = root / "self-test-data" / unknown["directory"]
    unowned.mkdir()
    (unowned / "keep.txt").write_bytes(b"unowned synthetic data")
    # A similarly named release must not be deleted via the probe journal.
    unrelated = versions / pending["directory"]
    unrelated.mkdir()
    (unrelated / "keep.txt").write_bytes(b"unrelated release data")
    current = json.loads(pointer.read_text(encoding="utf-8"))
    current["retained"].append(unknown)
    current["rollback_probes"] = [pending]
    pointer.write_text(json.dumps(current), encoding="utf-8")

    def cleanup(label, success):
        proc = subprocess.run([str(installer), "--root", str(root), "cleanup"], capture_output=True,
                              encoding="utf-8", errors="replace", timeout=30)
        assert (proc.returncode == 0) == success, (label, proc.stderr)
        assert ("历史清理完成" if success else "历史清理部分完成") in proc.stderr, (label, proc.stderr)
        state = json.loads(pointer.read_text(encoding="utf-8"))
        assert state["active"] == retained[0] and state["previous"] == retained[1], label
        assert user.read_bytes() == b"synthetic user data", label
        assert (unrelated / "keep.txt").read_bytes() == b"unrelated release data", label
        assert not probe.exists() and not state.get("rollback_probes"), label
        checked.append(label)
        print(json.dumps({"case": label, "exit_code": proc.returncode, "stderr": proc.stderr}, ensure_ascii=False), flush=True)
        return state

    current = cleanup("partial_cleanup_preserves_unknown_and_reclaims_journaled_probe", False)
    assert current["retained"] == retained + [unknown]
    assert (unowned / "keep.txt").read_bytes() == b"unowned synthetic data"
    # Only the test harness removes the synthetic unowned directory.
    shutil.rmtree(unowned)
    current = cleanup("cleanup_retry_completes", True)
    assert current["retained"] == retained
    cleanup("cleanup_is_idempotent", True)
    # A separate synthetic release has matching outer digests, but is not a
    # valid product. Verify launch and rollback reject it before execution.
    launch_root = work / "launch"
    release = launch_root / "versions" / "release-test-broken"
    shutil.copytree(payload, release)
    version = {"directory": release.name, "manifest_sha256": sha}
    launch_pointer = launch_root / "active.json"
    launch_state = {"active": version, "previous": None, "retained": [version]}
    launch_pointer.write_text(json.dumps(launch_state), encoding="utf-8")
    (launch_root / "install.lock").touch()

    def refuse(label, action, expected):
        before = launch_pointer.read_bytes()
        proc = subprocess.run([str(installer), "--root", str(launch_root), action], capture_output=True,
                              encoding="utf-8", errors="replace", timeout=30)
        assert proc.returncode != 0 and expected in proc.stderr, (label, proc.stderr)
        assert launch_pointer.read_bytes() == before, label
        checked.append(label)
        print(json.dumps({"case": label, "exit_code": proc.returncode, "stderr": proc.stderr}, ensure_ascii=False), flush=True)

    unknown = release / "resources" / "unlisted.dll"
    unknown.write_bytes(b"not a real library")
    refuse("launch_rejects_unlisted_file", "launch", "安装目录包含未登记文件或链接")
    assert unknown.read_bytes() == b"not a real library"
    unknown.unlink()
    resource = release / "resources" / "inference.json"
    resource.write_bytes(b"X" * len(data))
    refuse("launch_rejects_tampered_resource", "launch", "安装载荷内容摘要不匹配")
    resource.write_bytes(data)
    refuse("launch_rejects_invalid_model_manifest", "launch", "内置模型清单损坏")
    launch_state.update(active=None, previous=version)
    launch_pointer.write_text(json.dumps(launch_state), encoding="utf-8")
    refuse("rollback_rejects_invalid_model_manifest", "rollback", "内置模型清单损坏")
    print(json.dumps({"ok": True, "verified": checked, "scope": "native negative installation contracts only; no valid model or successful install"}, ensure_ascii=False))


def verify_windows_launcher(installer):
    # 快捷方式双击时 Windows 只会给 CUI 可执行文件分配控制台。
    with installer.open("rb") as executable:
        assert executable.read(2) == b"MZ"
        executable.seek(0x3C)
        pe = struct.unpack("<I", executable.read(4))[0]
        executable.seek(pe)
        assert executable.read(4) == b"PE\0\0"
        # 签名 4 字节 + COFF 头 20 字节 + 可选头的 Subsystem 偏移 68。
        executable.seek(pe + 4 + 20 + 68)
        subsystem = struct.unpack("<H", executable.read(2))[0]
        assert subsystem == 2, f"Windows launcher must use the GUI subsystem, got {subsystem}"
    print(json.dumps({"case": "windows_launcher_has_no_console_subsystem", "subsystem": subsystem}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", type=Path, required=True)
    parser.add_argument("--temp-root", type=Path, required=True)
    args = parser.parse_args()
    if os.name == "nt":
        verify_windows_launcher(args.installer)
    with tempfile.TemporaryDirectory(prefix="xunshu-negative-install-", dir=args.temp_root.resolve()) as directory:
        verify(args.installer.resolve(), Path(directory))


if __name__ == "__main__":
    main()
