#!/usr/bin/env python3
"""Read-only, offline preflight gate for publishing an Xunshu source archive.

Run from any directory: python3 rust/scripts/check_release_consistency.py
Does not claim runtime compatibility, signatures, or benchmark acceptance.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import sys
import tomllib
from pathlib import Path


def inspect(root: Path, expected: str | None = None) -> dict:
    project = tomllib.loads((root / 'Cargo.toml').read_text('utf-8'))
    lock_bytes = (root / 'Cargo.lock').read_bytes()
    lock = tomllib.loads(lock_bytes.decode('utf-8'))
    version = project['workspace']['package']['version']
    members = project['workspace']['members']
    problems: list[str] = []
    if expected and version != expected:
        problems.append(f'Expected version {expected}, found {version}')
    if len(members) != len(set(members)):
        problems.append('Duplicate workspace member')
    lock_packages = {(item['name'], item['version']) for item in lock['package']}
    for member in members:
        manifest = root / member / 'Cargo.toml'
        if not manifest.is_file():
            problems.append(f'Missing manifest {member}')
            continue
        item = tomllib.loads(manifest.read_text('utf-8'))
        name = item['package']['name']
        member_version = item['package'].get('version')
        if member_version != {'workspace': True} and member_version != version:
            problems.append(f'Wrong version in {member}: {member_version}')
        if (name, version) not in lock_packages:
            problems.append(f'Cargo.lock missing {name} {version}')
    toolchain = tomllib.loads((root / 'rust-toolchain.toml').read_text('utf-8'))
    tool = toolchain['toolchain']['channel']
    return {
        'status': 'pass' if not problems else 'fail',
        'version': version,
        'workspace_member_count': len(members),
        'toolchain': tool,
        'cargo_lock_sha256': hashlib.sha256(lock_bytes).hexdigest(),
        'problems': problems,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument('--expected-version')
    args = parser.parse_args()
    result = inspect(args.root.resolve(), args.expected_version)
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return 0 if result['status'] == 'pass' else 1


if __name__ == '__main__':
    sys.exit(main())
