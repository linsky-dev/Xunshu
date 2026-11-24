#!/usr/bin/env python3
"""Audit the deterministic filename benchmark, without turning partial results into passes."""
import argparse
import json
import math
from pathlib import Path


# The dataset definition is fixed in xs-cli's benchmark(): five filenames cycle.
EXPECTED = {
    "never_present_xyz": 0,
    "0001234": 1,
    "预算决算不存在": 0,
    "干部": 600_000,
    "project_budget_final_0000121": 1,
    "name:report ext:pdf": 0,
    "readme ext:rs": 600_000,
    "budget | meeting": 1_200_000,
}


def evaluate(report):
    errors = []
    targets = []
    # A standalone benchmark-read file does not establish the dataset identity.
    if report.get("entries") != 3_000_000 or report.get("dataset") != (
        "deterministic synthetic mixed Chinese/English filenames; no real files scanned"
    ):
        errors.append("Expected the built-in 3,000,000-entry synthetic dataset")
    read = report.get("read_process", {})
    if type(read.get("iterations")) is not int or read["iterations"] < 30:
        errors.append("At least 30 iterations per query are required")
    rows = read.get("queries", [])
    seen = set()
    for row in rows:
        query = row.get("query")
        if query not in EXPECTED or query in seen:
            errors.append(f"Unexpected or duplicate query: {query!r}")
            continue
        seen.add(query)
        if row.get("complete") is not True or type(row.get("total")) is not int or row["total"] != EXPECTED[query]:
            errors.append(f"Incorrect or incomplete result: {query}")
        values = [row.get(key) for key in ("p50_ms", "p95_ms", "p99_ms")]
        if any(type(x) not in (int, float) or not math.isfinite(x) or x < 0 for x in values) or values != sorted(values):
            errors.append(f"Invalid percentile evidence: {query}")
            continue
        if values[1] > 50:
            targets.append({"query": query, "p95_ms": values[1], "target_ms": 50})
    if seen != set(EXPECTED):
        errors.append("Missing expected queries")
    return {"correctness": "FAIL" if errors else "PASS",
            "latency": "NOT_EVALUATED" if errors else ("FAIL" if targets else "PASS"),
            "errors": errors, "over_budget": targets,
            "scope": "synthetic filename benchmark only; not native disk, desktop or release acceptance"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--strict-latency", action="store_true")
    args = parser.parse_args()
    result = evaluate(json.loads(args.report.read_text(encoding="utf-8")))
    print(json.dumps(result, ensure_ascii=False, indent=2))
    return int(result["correctness"] != "PASS" or (args.strict_latency and result["latency"] != "PASS"))


if __name__ == "__main__":
    raise SystemExit(main())
