"""Synthetic CLI review: never scans a real directory or opens catalog paths."""
from pathlib import Path
import csv
import hashlib
import io
import json
import os
import subprocess
import tempfile

REPO = Path(__file__).resolve().parents[1]
CLI = Path(os.environ.get("XS_REVIEW_CLI", REPO / "rust/target/release/xunshu"))
LABEL = os.environ.get("XS_REVIEW_LABEL", "rerun")
if LABEL and (not LABEL.replace("-", "").isalnum()):
    raise ValueError("label must be alphanumeric or hyphen")
PREFIX = Path("/tmp/xunshu-rust-validation-036c199d3807")
PREFIX.mkdir(exist_ok=True)
base = Path(tempfile.mkdtemp(prefix="rust-review-cli-", dir=PREFIX))
data = base / "data"
events = []

def run(*args):
    p = subprocess.run([str(CLI), "--data-dir", str(data), *map(str, args)], capture_output=True, text=True, timeout=30)
    event = {"args": list(map(str, args)), "returncode": p.returncode, "stdout": p.stdout, "stderr": p.stderr}
    events.append(event)
    return json.loads(p.stdout) if p.returncode == 0 else None

original = (
    "Filename,Size,Date Modified,Date Created,Attributes,Project\n"
    '"C:\\synthetic\\precise.txt",123,133444736000000001,133444736123456789,34,项目甲\n'
    '"C:\\synthetic\\unknown.txt",,,,,\n'
)
catalog = base / "original.efu"
catalog.write_text(original, encoding="utf-8")
run("import-efu", catalog, "--name", "review")
exported = base / "exported.efu"
run("export-efu", exported)
roundtrip = exported.read_text(encoding="utf-8") if exported.exists() else None
empty = base / "empty.efu"
empty.write_text("Filename,Size,Date Modified,Date Created,Attributes\n", encoding="utf-8")
run("import-efu", empty, "--name", "review")
after_empty = run("search", "source:offline:review")
extreme = base / "extreme.efu"
extreme.write_text("Filename,Date Modified\n/tmp/synthetic.txt,-170141183460469231731687303715884105728\n", encoding="utf-8")
run("import-efu", extreme, "--name", "extreme")
result = {
    "binary": str(CLI), "binary_sha256": hashlib.sha256(CLI.read_bytes()).hexdigest(),
    "fixture_dir": str(base), "source_unchanged": catalog.read_text(encoding="utf-8") == original,
    "input_efu": original, "output_efu": roundtrip,
    "roundtrip_rows": list(csv.DictReader(io.StringIO(roundtrip))) if roundtrip else None,
    "empty_reimport_total": after_empty.get("total") if after_empty else None,
    "events": events,
}
result["roundtrip_values_exact"] = list(csv.DictReader(io.StringIO(original))) == result["roundtrip_rows"]
target = REPO / ("migration-audit/rust-review-cli" + ("-" + LABEL if LABEL else "") + ".json")
target.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
print(json.dumps({"report": str(target), "fixture_dir": str(base), "source_unchanged": result["source_unchanged"], "input_efu": original, "output_efu": roundtrip, "empty_reimport_total": result["empty_reimport_total"], "returncodes": [e["returncode"] for e in events]}, ensure_ascii=False, indent=2))
if os.environ.get("XS_REVIEW_EXPECT_FIXED") == "1":
    assert result["source_unchanged"] and result["roundtrip_values_exact"]
    assert result["empty_reimport_total"] == 0
    assert events[-1]["returncode"] == 1 and "panicked" not in events[-1]["stderr"]
