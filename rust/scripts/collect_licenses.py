#!/usr/bin/env python3
"""Development-only: collect notices from Cargo's already-cached locked crates.

No downloads, network calls, or product runtime dependency on Python.
Run cargo metadata --locked --offline --format-version 1 first, then pass its JSON.
"""
import argparse
import hashlib
import json
import pathlib
import re
import shutil
import subprocess
import tomllib


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("metadata", type=pathlib.Path)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    workspace = pathlib.Path(__file__).resolve().parents[1]
    packages = json.loads(args.metadata.read_text())["packages"]
    lock = tomllib.loads((workspace / "Cargo.lock").read_text())
    checksums = {(p["name"], p["version"]): p.get("checksum") for p in lock["package"]}
    args.output.mkdir(parents=True, exist_ok=True)
    rows = []
    for package in sorted(packages, key=lambda p: (p["name"], p["version"])):
        if package["source"] is None:
            continue
        root = pathlib.Path(package["manifest_path"]).parent
        files = subprocess.check_output(
            ["rg", "--files", "--hidden", "--no-ignore", str(root)], text=True
        ).splitlines()
        candidates = {
            pathlib.Path(p) for p in files
            if (re.search(r"license|licence|copying|copyright|notice|^ofl(?:\.|$)|^ufl(?:\.|$)|^unlicense$", pathlib.Path(p).name, re.I)
                or (package["name"] == "epaint_default_fonts" and pathlib.Path(p).suffix.lower() == ".txt"))
            and pathlib.Path(p).is_file()
            and pathlib.Path(p).suffix.lower() not in {".rs", ".c", ".h", ".png", ".svg", ".json"}
        }
        if package.get("license_file"):
            candidates.add(root / package["license_file"])
        notices = []
        for path in sorted(candidates):
            if not path.is_file() or path.is_symlink():
                continue
            relative = path.relative_to(root)
            destination = args.output / f'{package["name"]}-{package["version"]}' / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, destination)
            notices.append({"file": str(destination.relative_to(args.output)), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()})
        rows.append({
            "name": package["name"], "version": package["version"],
            "license_declared": package.get("license"), "repository": package.get("repository"),
            "registry_checksum": checksums.get((package["name"], package["version"])),
            "notices": notices,
        })
    report = {"scope": "registry packages emitted by the supplied Cargo metadata, including its selected targets and development dependencies", "packages": rows}
    (args.output / "inventory.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    missing = [r["name"] + "@" + r["version"] for r in rows if not r["notices"]]
    print(json.dumps({"packages": len(rows), "notice_files": sum(len(r["notices"]) for r in rows), "without_bundled_notice_file": missing}, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
