"""Inject publication faults into an isolated CLI process on synthetic tmpfs."""
from pathlib import Path
import json
import os
import subprocess
import tempfile

REPO = Path(__file__).resolve().parents[1]
CLI = Path(os.environ.get("XS_REVIEW_CLI", REPO / "rust/target/release/xunshu"))
LABEL = os.environ.get("XS_REVIEW_LABEL", "rerun")
if LABEL and (not LABEL.replace("-", "").isalnum()):
    raise ValueError("label must be alphanumeric or hyphen")
SHIM = REPO / "migration-audit/fs_faults.so"
BASE = Path(tempfile.mkdtemp(prefix="rust-review-faults-", dir="/tmp/xunshu-rust-validation-036c199d3807"))
first = BASE / "first.efu"
second = BASE / "second.efu"
first.write_text("Filename,Size\nC:\\synthetic\\first.txt,1\n", encoding="utf-8")
second.write_text("Filename,Size\nC:\\synthetic\\second.txt,2\n", encoding="utf-8")

def invoke(data, file, env=None):
    p=subprocess.run([str(CLI),"--data-dir",str(data),"import-efu",str(file),"--name","fault-review"],capture_output=True,text=True,timeout=30,env=env)
    return {"returncode":p.returncode,"stdout":p.stdout,"stderr":p.stderr}

fsync_data=BASE / "fsync-data"
initial_fsync=invoke(fsync_data,first)
current=fsync_data / "index/CURRENT.json"
before=current.read_text()
env=os.environ.copy();env["LD_PRELOAD"]=str(SHIM);env["XS_REVIEW_FAIL_DIR_FSYNC"]=str(current.parent)
failed_fsync=invoke(fsync_data,second,env)
after=current.read_text()
loaded=subprocess.run([str(CLI),"--data-dir",str(fsync_data),"search","source:offline:fault-review"],capture_output=True,text=True,timeout=30)
loaded_result={"returncode":loaded.returncode,"stdout":loaded.stdout,"stderr":loaded.stderr}

race_data=BASE / "race-data"
initial_race=invoke(race_data,first)
parent=race_data / "index"
saved=race_data / "index-saved"
redirect=BASE / "unauthorized-target"
redirect.mkdir()
env=os.environ.copy();env.update({"LD_PRELOAD":str(SHIM),"XS_REVIEW_SWAP_PARENT":str(parent),"XS_REVIEW_SAVED_PARENT":str(saved),"XS_REVIEW_REDIRECT_PARENT":str(redirect)})
failed_race=invoke(race_data,second,env)
result={"fixture_dir":str(BASE),"fsync":{"initial":initial_fsync,"injected":failed_fsync,"before":json.loads(before),"after":json.loads(after),"current_changed":before!=after,"current_changed_despite_error":before!=after and failed_fsync["returncode"]!=0,"loaded_after_commit":loaded_result},"parent_swap":{"initial":initial_race,"injected":failed_race,"injection_triggered":"XS_REVIEW_SWAPPED_PARENT" in failed_race["stderr"],"redirected_files":[p.name for p in redirect.iterdir()],"index_is_symlink":parent.is_symlink(),"saved_current":json.loads((saved/"CURRENT.json").read_text()) if saved.exists() else None}}
(REPO/("migration-audit/rust-review-faults"+("-"+LABEL if LABEL else "")+".json")).write_text(json.dumps(result,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
print(json.dumps(result,ensure_ascii=False,indent=2))
if os.environ.get("XS_REVIEW_EXPECT_FIXED") == "1":
    published=json.loads(failed_fsync["stdout"])
    assert failed_fsync["returncode"] == 0 and published["generation"] == 2
    assert published["durable"] is False and published["durability_warning"]
    assert result["fsync"]["after"]["generation"] == 2
    loaded=json.loads(loaded_result["stdout"])
    assert loaded_result["returncode"] == 0 and loaded["generation"] == 2
    assert loaded["total"] == 1 and loaded["results"][0]["name"] == "second.txt"
    assert result["parent_swap"]["injection_triggered"] and not result["parent_swap"]["redirected_files"]
    assert failed_race["returncode"] == 1 and result["parent_swap"]["saved_current"]["generation"] == 1
