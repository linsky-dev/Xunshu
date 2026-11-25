"""Inspect ELF ABI requirements without executing the inspected programs.

A pass is a direct-symbol ceiling check, not target OS certification.
"""
import argparse
import hashlib
import json
import os
import re
import subprocess
from pathlib import Path

VERSION = re.compile(r"\bName: (GLIBC_\S+)\s+Flags: (.*?)\s+Version: \d+")
NEEDED = re.compile(r"\(NEEDED\).*Shared library: \[([^\]]+)\]")
INTERP = re.compile(r"Requesting program interpreter: ([^\]]+)\]")


def numeric(tag):
    if not re.fullmatch(r"\d+(?:\.\d+)+", tag):
        raise ValueError(f"Non-numeric version: {tag}")
    parts = tuple(map(int, tag.split(".")))
    # Treat e.g. 2.34.0 and 2.34 as equal.
    while len(parts) > 2 and parts[-1] == 0:
        parts = parts[:-1]
    return parts


def parse(header, versions, dynamic, program, max_glibc=None):
    if not re.search(r"^\s*Class:\s+ELF64\s*$", header, re.M) or not re.search(
            r"^\s*Machine:\s+Advanced Micro Devices X86-64\s*$", header, re.M):
        raise ValueError("Not an x86_64 ELF64 object")
    if not re.search(r"^\s*Data:\s+.*little endian\s*$", header, re.M):
        raise ValueError("Not a little-endian ELF object")
    if not re.search(r"^\s*Type:\s+(?:DYN|EXEC)\b", header, re.M):
        raise ValueError("Not a dynamic/shared object or executable")
    needed = sorted(set(NEEDED.findall(dynamic)))
    interpreters = INTERP.findall(program)
    if len(interpreters) > 1:
        raise ValueError("Multiple ELF interpreters")
    # Only dependency version requirements, not versions exported by an ELF.
    need_section = versions.split("Version needs section", 1)
    strong, weak, other = set(), set(), set()
    if len(need_section) == 2:
        for tag, flags in VERSION.findall(need_section[1]):
            if tag == "GLIBC_PRIVATE":
                raise ValueError("GLIBC_PRIVATE dependency is not a public ABI")
            suffix = tag.removeprefix("GLIBC_")
            if not re.fullmatch(r"\d+(?:\.\d+)+", suffix):
                other.add(tag)
                continue
            if flags == "none":
                strong.add(suffix)
            elif flags == "WEAK":
                weak.add(suffix)
            else:
                raise ValueError(f"Unrecognized version flags: {flags}")
    if (needed or interpreters) and not strong:
        raise ValueError("Dynamic object has no determinable non-weak GLIBC requirements")
    if other:
        raise ValueError(f"Non-numeric GLIBC ABI requirements: {sorted(other)}")
    maximum = max(strong, key=numeric, default=None)
    exceeds = max_glibc is not None and maximum is not None and numeric(maximum) > numeric(max_glibc)
    return {"direct_needed": needed, "interpreter": interpreters[0] if interpreters else None,
            "strong_glibc": sorted(strong, key=numeric), "weak_glibc": sorted(weak, key=numeric),
            "max_strong_glibc": maximum, "ceiling": max_glibc,
            "ceiling_status": "NOT_REQUESTED" if max_glibc is None else ("FAIL" if exceeds else "PASS")}


def inspect(path, max_glibc):
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"Missing or non-regular object: {path}")
    def readelf(option):
        return subprocess.run(["readelf", "--wide", option, "--", str(path.resolve())],
                              check=True, capture_output=True, text=True, timeout=30,
                              env={**os.environ, "LC_ALL": "C"}).stdout
    def digest():
        with path.open("rb") as stream:
            return hashlib.file_digest(stream, "sha256").hexdigest()
    before = digest()
    result = parse(readelf("--file-header"), readelf("--version-info"),
                   readelf("--dynamic"), readelf("--program-headers"), max_glibc)
    if digest() != before:
        raise ValueError("Object changed during inspection")
    return {"file": str(path), "sha256": before, **result}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("files", type=Path, nargs="+")
    parser.add_argument("--max-glibc")
    args = parser.parse_args()
    if args.max_glibc is not None:
        try:
            numeric(args.max_glibc)
        except ValueError as error:
            parser.error(str(error))
    results, errors = [], []
    for path in args.files:
        try:
            results.append(inspect(path, args.max_glibc))
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            errors.append({"file": str(path), "error": str(error)})
    failed = bool(errors) or any(item["ceiling_status"] == "FAIL" for item in results)
    print(json.dumps({"status": "FAIL" if failed else "PASS", "files": results, "errors": errors,
                      "scope": "direct ELF ABI inspection only; not transitive dependencies, dlopen, native installation or OS certification"}, ensure_ascii=False, indent=2))
    return int(failed)


if __name__ == "__main__":
    raise SystemExit(main())
