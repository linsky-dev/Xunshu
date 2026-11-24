"""Synthetic contract tests; not a benchmark result."""
import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location("gate", Path(__file__).with_name("check-benchmark-gate.py"))
gate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(gate)


def fixture():
    return {"entries": 3_000_000,
            "dataset": "deterministic synthetic mixed Chinese/English filenames; no real files scanned",
            "read_process": {"iterations": 30, "queries": [
                {"query": query, "total": count, "complete": True, "verified_samples": 30,
                 "oracle_verified_samples": 30, "samples_ms": [10.0] * 30,
                 "p50_ms": 10.0, "p95_ms": 10.0, "p99_ms": 10.0}
                for query, count in gate.EXPECTED.items()]}}


class GateContracts(unittest.TestCase):
    def test_complete_raw_evidence(self):
        self.assertEqual(gate.evaluate(fixture())["latency"], "PASS")

    def test_recomputed_percentile_catches_outlier(self):
        report = fixture()
        row = report["read_process"]["queries"][-1]
        row["samples_ms"][-2:] = [70.0, 80.0]
        self.assertEqual(gate.evaluate(report)["correctness"], "FAIL")
        row.update(p95_ms=70.0, p99_ms=80.0)
        self.assertEqual(gate.evaluate(report)["latency"], "FAIL")

    def test_old_or_incomplete_evidence_rejected(self):
        for field in ("samples_ms", "oracle_verified_samples"):
            report = fixture()
            del report["read_process"]["queries"][0][field]
            self.assertEqual(gate.evaluate(report)["correctness"], "FAIL")

    def test_invalid_raw_sample_types(self):
        for value in (float("nan"), float("inf"), True, -1.0, "10"):
            report = fixture()
            report["read_process"]["queries"][0]["samples_ms"][0] = value
            self.assertEqual(gate.evaluate(report)["correctness"], "FAIL")


if __name__ == "__main__":
    unittest.main(verbosity=2)
