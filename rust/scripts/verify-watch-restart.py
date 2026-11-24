"""Native filename watcher rename/delete/restart/scope-revocation contracts.

Only creates and searches synthetic files under the caller's temporary root.
A forced exit is a process-kill test, not power-loss or 72-hour stability.
"""
import argparse
import json
import os
import subprocess
import tempfile
import time
from pathlib import Path


def verify(cli, work):
    roots = [work / name for name in ("scope-one", "scope-two")]
    for root in roots:
        root.mkdir()
    data = work / "data"
    original = roots[0] / "alpha-restart.txt"
    original.write_text("synthetic", encoding="utf-8")
    (roots[1] / "outside-restart.txt").write_text("synthetic", encoding="utf-8")
    base = [str(cli), "--data-dir", str(data)]
    checked = []
    process = None
    log = None

    def run(*args, allow_failure=False):
        result = subprocess.run(base + list(args), capture_output=True, encoding="utf-8", timeout=30)
        if result.returncode:
            if allow_failure:
                return None
            raise AssertionError(f"{args}: {result.returncode}: {result.stderr}")
        return json.loads(result.stdout)

    def counts(expected, allow_failure=False):
        for name, expectation in expected.items():
            result = run("search", name, allow_failure=allow_failure)
            count = expectation if isinstance(expectation, int) else 1
            if result is None or not result["complete"] or result["total"] != count:
                return False
            if isinstance(expectation, Path):
                actual = os.path.normcase(os.path.normpath(result["results"][0]["path"]))
                if actual != os.path.normcase(os.path.normpath(str(expectation))):
                    return False
        return True

    def published(events):
        for line in events.splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("event") == "reconcile" and event.get("outcome", {}).get("published"):
                return True
        return False

    def wait(label, expected, ready=False):
        start = time.monotonic()
        deadline = start + 30
        while time.monotonic() < deadline:
            assert process is not None and process.poll() is None, "watch process terminated"
            events = log.read_text(encoding="utf-8", errors="replace")
            if (not ready or published(events)) and counts(expected, allow_failure=True):
                checked.append({"case": label, "elapsed_ms": round((time.monotonic() - start) * 1000, 2)})
                return
            time.sleep(0.15)
        raise AssertionError(f"{label}: {expected}; watcher events: {events[-4000:]}")

    def start(label):
        nonlocal process, log
        log = work / (label + ".log")
        with log.open("wb") as stream:
            process = subprocess.Popen(base + ["watch"], stdout=stream, stderr=stream)

    def stop():
        nonlocal process
        if process is not None:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=10)
            process = None

    try:
        assert run("init", "--root", str(roots[0]))["ok"]
        assert run("scan")["published"]
        assert counts({"alpha-restart": 1, "outside-restart": 0})
        start("initial")
        wait("initial_subscription", {"alpha-restart": 1, "outside-restart": 0}, ready=True)
        renamed = roots[0] / "beta-restart.txt"
        original.rename(renamed)
        wait("rename_removes_old_name", {"alpha-restart": 0, "beta-restart": renamed})
        nested = roots[0] / "old-folder"
        nested.mkdir()
        (nested / "nested-restart.txt").write_text("synthetic", encoding="utf-8")
        wait("new_subdirectory", {"nested-restart": nested / "nested-restart.txt", "old-folder": nested})
        replacement = roots[0] / "new-folder"
        nested.rename(replacement)
        wait("directory_rename_reconciles_descendants", {"old-folder": 0, "new-folder": replacement, "nested-restart": replacement / "nested-restart.txt"})
        renamed.unlink()
        wait("delete_removes_index_record", {"beta-restart": 0})
        stop()
        # No notifications are available while the process is dead.
        (replacement / "nested-restart.txt").unlink()
        (roots[0] / "offline-restart.txt").write_text("synthetic", encoding="utf-8")
        start("restarted")
        wait("forced_exit_restart_reconciles_missed_changes", {"nested-restart": 0, "offline-restart": roots[0] / "offline-restart.txt"}, ready=True)
        assert run("init", "--root", str(roots[1]), "--replace")["ok"]
        # The former scope must not leak while the watcher switches generations.
        for _ in range(5):
            former = run("search", "offline-restart", allow_failure=True)
            assert former is None or former["total"] == 0, former
        wait("live_scope_revocation", {"offline-restart": 0, "outside-restart": 1})
        (roots[0] / "revoked-restart.txt").write_text("synthetic", encoding="utf-8")
        (roots[1] / "allowed-restart.txt").write_text("synthetic", encoding="utf-8")
        wait("revoked_scope_stays_excluded", {"revoked-restart": 0, "allowed-restart": 1})
        assert (roots[0] / "offline-restart.txt").is_file(), "source files must not be modified by index"
    finally:
        stop()
    return {"ok": True, "platform": os.name, "verified": checked,
            "scope": "real native processes and synthetic local filesystem; forced exit is not power loss"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--temp-root", type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="xunshu-watch-restart-", dir=args.temp_root.resolve()) as directory:
        result = verify(args.cli.resolve(), Path(directory))
    print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
