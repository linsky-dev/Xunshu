"""在独立测试目录验证带模型的安装生命周期，Windows 运行需能管理本机规则。"""
import argparse
import hashlib
import json
import subprocess
import tempfile
from pathlib import Path


def verify(installer, payload, root):
    manifest = payload / "release.json"
    sha = hashlib.sha256(manifest.read_bytes()).hexdigest()
    active = root / "active.json"
    user_data = root / "data-preserve-test.txt"
    user_data.write_text("保留的合成用户数据", encoding="utf-8")

    def run(*args, expect_success=True):
        result = subprocess.run([str(installer), "--root", str(root), *args], capture_output=True,
                                encoding="utf-8", errors="replace", timeout=180)
        if (result.returncode == 0) != expect_success:
            raise AssertionError(f"安装命令退出码异常 {args}: {result.returncode}\n{result.stdout}\n{result.stderr}")
        print(json.dumps({"action": args[0], "exit_code": result.returncode,
                          "stdout": result.stdout, "stderr": result.stderr}, ensure_ascii=False), flush=True)

    def state():
        return json.loads(active.read_text(encoding="utf-8"))

    def install(expected=sha, success=True):
        run("install", str(payload), "--sha256", expected, expect_success=success)
        return state()["active"]["directory"]

    try:
        first = install()
        assert install("0" * 64, success=False) == first
        second = install()
        assert second != first and state()["previous"]["directory"] == first
        assert len(list((root / "versions").glob("release-*"))) <= 2
        run("cleanup")
        assert len(list((root / "versions").glob("release-*"))) <= 2
        run("rollback")
        assert state()["active"]["directory"] == first
        # 只改动本次隔离安装复制的模型，保留原始载荷。
        model = root / "versions" / first / "resources" / "bge.onnx"
        with model.open("r+b") as stream:
            stream.seek(0)
            first_byte = stream.read(1)
            stream.seek(0)
            stream.write(bytes([first_byte[0] ^ 1]))
        repaired = install()
        assert repaired not in (first, second)
        # 两版保留：当前可用版 + 可能需要回退验证的旧版；第二版应清理。
        assert not (root / "versions" / second).exists()
        assert len(list((root / "versions").glob("release-*"))) <= 2
        run("cleanup")
        run("rollback", expect_success=False)
        assert state()["active"]["directory"] == repaired
    finally:
        if active.is_file():
            run("uninstall")
            assert state()["active"] is None and user_data.is_file()
            assert not list((root / "versions").glob("release-*"))
            assert not list((root / "self-test-data").glob("release-*"))
    print(json.dumps({"ok": True, "verified": ["install_with_models", "manifest_rejection", "same_version_install", "rollback", "corruption_repair", "damaged_rollback_rejected", "uninstall_preserves_data", "cleanup_keeps_two_versions", "cleanup_removes_owned_probe_data"]}, ensure_ascii=False))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--installer", required=True, type=Path)
    parser.add_argument("--payload", required=True, type=Path)
    parser.add_argument("--temp-root", required=True, type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="xunshu-install-", dir=args.temp_root.resolve()) as directory:
        verify(args.installer.resolve(), args.payload.resolve(), Path(directory))


if __name__ == "__main__":
    main()
