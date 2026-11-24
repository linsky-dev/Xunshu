"""以合成目录验证恢复后的 CLI、协议、授权和正文入口。"""
import argparse
import json
import os
import queue
import shutil
import subprocess
import tempfile
import threading
import time
from pathlib import Path


class Session:
    def __init__(self, command):
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        text=True, encoding="utf-8")
        self.output = queue.Queue()

        def read():
            for line in self.process.stdout:
                self.output.put(line)
            self.output.put(None)

        threading.Thread(target=read, daemon=True).start()

    def send(self, value):
        self.process.stdin.write(json.dumps(value, ensure_ascii=False) + "\n")
        self.process.stdin.flush()

    def recv(self):
        line = self.output.get(timeout=20)
        assert line is not None, "协议进程提前退出"
        return json.loads(line)

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=4)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=4)
        self.process.stdout.close()


def run(bin_dir, workspace, with_models):
    suffix = ".exe" if os.name == "nt" else ""
    cli = str(bin_dir / ("xunshu" + suffix))
    fixtures = workspace / "files"
    data = workspace / "data"
    fixtures.mkdir()
    (fixtures / "报告.txt").write_text("本地文档全文搜索，支持正文索引。\nhello evidence", encoding="utf-8")
    base = [cli, "--data-dir", str(data)]

    def command(*args):
        process = subprocess.run(base + list(args), capture_output=True, text=True, encoding="utf-8", timeout=30)
        assert process.returncode == 0, f"{args}: {process.stderr}\n{process.stdout}"
        return json.loads(process.stdout)

    assert command("init", "--root", str(fixtures))["ok"]
    assert command("scan")["published"]
    assert command("search", "报告")["total"] == 1
    with Session(base + ["serve-v2"]) as server:
        server.send({"id": 1, "op": "hello", "protocol": 2})
        assert server.recv()["protocol"] == 2
        server.send({"id": 2, "op": "search", "query": "报告", "preview": True})
        found = server.recv()
        if found.get("phase") == "preview":
            found = server.recv()
        assert found["id"] == 2 and found["ok"] and found["result"]["total"] == 1, found

    watcher = subprocess.Popen(base + ["watch"], stdout=subprocess.DEVNULL)
    try:
        time.sleep(1)
        (fixtures / "监听新增.txt").write_text("synthetic watcher", encoding="utf-8")
        for _ in range(40):
            if command("search", "监听新增")["total"] == 1:
                break
            time.sleep(0.25)
        else:
            raise AssertionError("监听新增未进入当前索引")
    finally:
        watcher.terminate()
        watcher.wait(timeout=5)

    efu = workspace / "fixture.efu"
    edited = workspace / "edited.efu"
    efu.write_text("Filename,Size,Date Modified,Date Created,Attributes,Custom\nZ:/unknown.pdf,,,,,old\n", encoding="utf-8")
    approval = command("efu-inspect", str(efu), "--row", "1")
    assert command("efu-edit", str(efu), str(edited), "--row", "1", "--revision", approval["revision"], "--set", "Custom=new")["ok"]
    assert command("efu-compare", str(efu), str(edited))["changed"] == 1
    assert command("import-efu", str(edited), "--name", "synthetic")["imported"] == 1
    assert command("search", "source:offline:synthetic !size:>0")["total"] == 0
    assert command("search", "source:offline:synthetic unknown")["total"] == 1

    assert command("content-configure", "--root", str(fixtures))["ok"]
    command("content-rebuild")
    hit = command("content-search", "全文搜索")
    assert hit["indexed_total"] == 1 and "全文搜索" in hit["hits"][0]["snippet"]

    def initialize(server):
        server.send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-11-25"}})
        assert server.recv()["result"]["protocolVersion"] == "2025-11-25"
        server.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def invoke(server, request_id, query, approval_id=None):
        args = {"query": query}
        if approval_id:
            args["approval_id"] = approval_id
        server.send({"jsonrpc": "2.0", "id": request_id, "method": "tools/call",
                     "params": {"name": "search_content", "arguments": args}})
        result = server.recv()["result"]
        return result["isError"], json.loads(result["content"][0]["text"])

    with Session(base + ["mcp"]) as mcp:
        initialize(mcp)
        error, request = invoke(mcp, 2, "全文搜索")
        assert error and request["state"] == "approval_required"
        approval_id = request["approval_id"]
        assert command("agent-inspect", approval_id)["query"] == "全文搜索"
        assert command("agent-approve", approval_id)["approved"]
        assert invoke(mcp, 3, "另一查询", approval_id)[0]
        with Session(base + ["mcp"]) as other:
            initialize(other)
            assert invoke(other, 4, "全文搜索", approval_id)[0]
        error, accepted = invoke(mcp, 5, "全文搜索", approval_id)
        assert not error and accepted["indexed_total"] == 1
        assert invoke(mcp, 6, "全文搜索", approval_id)[0]
        _, request = invoke(mcp, 7, "全文搜索")
        assert command("agent-approve", request["approval_id"])["approved"]
        assert command("content-configure", "--disable")["ok"]
        assert invoke(mcp, 8, "全文搜索", request["approval_id"])[0]
        assert command("content-configure", "--root", str(fixtures))["ok"]
        assert invoke(mcp, 9, "全文搜索", request["approval_id"])[0]

    checked = ["scope_scan", "resident_stdio_v2", "watch_update", "efu_edit_compare", "nullable_metadata", "content_worker", "agent_once_and_revocation"]
    if with_models:
        assets = command("status", "--verify-assets")
        assert assets["model_resources"]["state"] == "verified", assets["model_resources"]
        for name in ("image.png", "scanned.pdf"):
            shutil.copyfile(bin_dir / "self-test" / name, fixtures / name)
        (fixtures / "长文.txt").write_text("春季花园开满了鲜花。" * 90 + "量子纠缠实验在两个粒子之间建立强关联。", encoding="utf-8")
        (fixtures / "对照.txt").write_text("蓝鲸以磷虾为食，生活在海洋中。", encoding="utf-8")
        assert command("content-configure", "--root", str(fixtures), "--ocr", "--semantic")["ok"]
        content = command("content-rebuild")
        assert content["published"] and content["semantic_vectors"] > 0 and content["failed"] == 0, content
        ocr = command("content-search", "本地文件")
        assert any(Path(hit["path"]).name == "scanned.pdf" for hit in ocr["hits"]), ocr
        result = command("content-search", "寻找量子纠缠实验记录", "--semantic")
        top = result["hits"][0]
        assert Path(top["path"]).name == "长文.txt" and "量子纠缠" in top["snippet"], top
        checked += ["installed_ocr", "scanned_pdf", "semantic_tail"]
    print(json.dumps({"ok": True, "platform": os.name, "verified": checked}, ensure_ascii=False))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--temp-root", type=Path, required=True)
    parser.add_argument("--with-models", action="store_true")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="xunshu-recovery-", dir=args.temp_root.resolve()) as directory:
        run(args.bin_dir.resolve(), Path(directory), args.with_models)


if __name__ == "__main__":
    main()
