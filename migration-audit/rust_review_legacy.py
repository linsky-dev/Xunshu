"""Read-only schema4 import contract using only synthetic SQLite databases."""
from pathlib import Path
import hashlib
import json
import os
import sqlite3
import subprocess
import tempfile

REPO=Path(__file__).resolve().parents[1]
CLI=Path(os.environ.get("XS_REVIEW_CLI", REPO/"rust/target/release/xunshu"))
LABEL=os.environ.get("XS_REVIEW_LABEL", "rerun")
if LABEL and (not LABEL.replace("-", "").isalnum()):
    raise ValueError("label must be alphanumeric or hyphen")
BASE=Path(tempfile.mkdtemp(prefix="rust-review-legacy-",dir="/tmp/xunshu-rust-validation-036c199d3807"))
db=BASE/"legacy.db"
with sqlite3.connect(db) as c:
    c.executescript("""
      PRAGMA user_version=4;
      CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT,size INTEGER,mtime REAL,is_dir INTEGER,source TEXT,mtime_filetime TEXT,birthtime_filetime TEXT,attributes INTEGER,extra_json TEXT);
      CREATE TABLE filelists(source TEXT PRIMARY KEY,path TEXT,enabled INTEGER);
      INSERT INTO filelists VALUES('efu:disabled','C:\\synthetic\\disabled.efu',0);
      INSERT INTO filelists VALUES('efu:enabled','C:\\synthetic\\enabled.efu',1);
      INSERT INTO files VALUES(1,'C:\\synthetic\\disabled.txt',1,1700000000,0,'efu:disabled','133444736000000001','133444736123456789',34,'{"Project":"disabled"}');
      INSERT INTO files VALUES(2,'C:\\synthetic\\enabled.txt',2,1700000000,0,'efu:enabled','133444736000000002','133444736123456790',32,'{"Project":"enabled"}');
      INSERT INTO files VALUES(3,'C:\\synthetic\\unknown.txt',NULL,NULL,0,'local',NULL,NULL,NULL,NULL);
    """)

def snap():
    return {p.name:hashlib.sha256(p.read_bytes()).hexdigest() for p in BASE.glob("legacy.db*")}
def run(data,*args):
    p=subprocess.run([str(CLI),"--data-dir",str(data),*map(str,args)],capture_output=True,text=True,timeout=30)
    return {"args":list(map(str,args)),"returncode":p.returncode,"stdout":p.stdout,"stderr":p.stderr}

before=snap()
data=BASE/"data"
imported=run(data,"import-legacy",db)
disabled=run(data,"search","source:offline:schema4:efu:disabled")
enabled_before_removal=run(data,"search","source:offline:schema4:efu:enabled")
after=snap()
with sqlite3.connect(db) as c:
    c.execute("DELETE FROM files WHERE source='efu:enabled'")
reimported=run(data,"import-legacy",db)
after_removal=run(data,"search","source:offline:schema4:efu:enabled")

wal_db=BASE/"pending.db"
c=sqlite3.connect(wal_db)
c.executescript("PRAGMA journal_mode=WAL; PRAGMA user_version=4; CREATE TABLE files(id INTEGER PRIMARY KEY,path TEXT,size INTEGER,mtime REAL,is_dir INTEGER,source TEXT); INSERT INTO files VALUES(1,'C:\\synthetic\\wal.txt',1,0,0,'local');")
c.commit()
wal=Path(str(wal_db)+"-wal")
wal_nonempty=wal.exists() and wal.stat().st_size>0
wal_result=run(BASE/"wal-data","import-legacy",wal_db)
c.close()
result={"fixture_dir":str(BASE),"source_before":before,"source_after":after,"byte_identical_and_no_new_sidecars":before==after,"import":imported,"disabled_catalog_search":disabled,"removed_source":"efu:enabled","enabled_before_removal":enabled_before_removal,"reimport_after_source_removal":reimported,"removed_source_search":after_removal,"nonempty_wal":wal_nonempty,"pending_wal_import":wal_result}
(REPO/("migration-audit/rust-review-legacy"+("-"+LABEL if LABEL else "")+".json")).write_text(json.dumps(result,ensure_ascii=False,indent=2)+"\n",encoding="utf-8")
print(json.dumps(result,ensure_ascii=False,indent=2))
if os.environ.get("XS_REVIEW_EXPECT_FIXED") == "1":
    assert result["byte_identical_and_no_new_sidecars"]
    assert disabled["returncode"] == 0 and json.loads(disabled["stdout"])["total"] == 0
    assert enabled_before_removal["returncode"] == 0 and json.loads(enabled_before_removal["stdout"])["total"] == 1
    assert after_removal["returncode"] == 0 and json.loads(after_removal["stdout"])["total"] == 0
    assert wal_nonempty and wal_result["returncode"] == 1 and "WAL" in wal_result["stderr"]
