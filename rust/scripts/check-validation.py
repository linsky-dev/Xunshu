#!/usr/bin/env python3
"""核对本地验证页的证据链接和已声明的 CLI 能力开关。"""

import argparse
import json
import re
from pathlib import Path
from urllib.parse import unquote


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--status", type=Path, help="verify 脚本生成的 status.json")
    args = parser.parse_args()
    workspace = Path(__file__).resolve().parents[1]
    document = (workspace / "VALIDATION.md").read_text(encoding="utf-8")
    for link in re.findall(r"\]\((evidence/[^)]+)\)", document):
        path = (workspace / unquote(link)).resolve()
        if not path.is_relative_to(workspace / "evidence") or not path.is_file():
            parser.error(f"验收证据不存在或路径超出范围：{link}")

    expected = dict(re.findall(r"^\| `([a-z0-9_]+)` \| `(true|false)` \|$", document, re.M))
    if not expected:
        parser.error("验证页没有 CLI 能力开关表")
    if args.status:
        actual = json.loads(args.status.read_text(encoding="utf-8"))["capabilities"]
        for name, value in expected.items():
            if actual.get(name) is not (value == "true"):
                parser.error(f"能力声明与当前程序不符：{name}，文档 {value}，程序 {actual.get(name)}")
    print("验证页证据链接与能力声明检查通过。")


if __name__ == "__main__":
    main()
