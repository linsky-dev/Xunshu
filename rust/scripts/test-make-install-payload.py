"""Offline integrity contracts. Fixtures are not runnable neural models."""
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("payload", Path(__file__).with_name("make-install-payload.py"))
payload = importlib.util.module_from_spec(spec)
spec.loader.exec_module(payload)


class PayloadContracts(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.resources = self.root / "resources"
        self.resources.mkdir()
        self.models = {"libonnxruntime.so", "bge.onnx", "bge-tokenizer.json", "ppocr-det.onnx",
                       "ppocr-rec.onnx", "ppocr-dictionary.txt"}
        names = self.models | {"BGE-LICENSE.txt", "PP-OCRv5-LICENSE.txt", "onnxruntime-LICENSE",
                               "onnxruntime-ThirdPartyNotices.txt", "BGE-MODEL-CARD.md",
                               "PP-OCRv5-det-MODEL-CARD.md", "PP-OCRv5-rec-MODEL-CARD.md"}
        for name in names:
            (self.resources / name).write_text("synthetic contract fixture: " + name, encoding="utf-8")
        self.inference = {"schema": 1, "files": {name: {"bytes": (self.resources / name).stat().st_size,
                           "sha256": payload.digest(self.resources / name), "source": "test", "license": "test"}
                           for name in self.models}}
        self.write_json("inference.json", self.inference)
        self.refresh_provenance()

    def write_json(self, name, data):
        (self.resources / name).write_text(json.dumps(data), encoding="utf-8")

    def refresh_provenance(self):
        self.write_json("build-provenance.json", {"outputs_sha256": {p.name: payload.digest(p)
                        for p in self.resources.iterdir() if p.name != "build-provenance.json"}})

    def test_consistent_fixture_integrity_only(self):
        self.assertEqual(len(payload.validate_resources(self.resources, "linux")), 15)

    def test_tampered_model_and_license(self):
        for name in ("bge.onnx", "BGE-LICENSE.txt"):
            p = self.resources / name
            old = p.read_bytes()
            p.write_bytes(old + b"tampered")
            with self.assertRaises(ValueError):
                payload.validate_resources(self.resources, "linux")
            p.write_bytes(old)

    def test_recomputed_provenance_does_not_hide_inference_mismatch(self):
        (self.resources / "bge.onnx").write_bytes(b"changed")
        self.refresh_provenance()
        with self.assertRaisesRegex(ValueError, "内置推理清单记录"):
            payload.validate_resources(self.resources, "linux")

    def test_missing_extra_and_wrong_platform(self):
        with self.assertRaises(ValueError):
            payload.validate_resources(self.resources, "windows")
        extra = self.resources / "unexpected.dll"
        extra.write_bytes(b"extra")
        with self.assertRaises(ValueError):
            payload.validate_resources(self.resources, "linux")
        extra.unlink()
        (self.resources / "build-provenance.json").unlink()
        with self.assertRaises(ValueError):
            payload.validate_resources(self.resources, "linux")

    def test_duplicate_json_key_rejected(self):
        (self.resources / "inference.json").write_text('{"schema":1,"schema":1,"files":{}}')
        self.refresh_provenance()
        with self.assertRaisesRegex(ValueError, "重复字段"):
            payload.validate_resources(self.resources, "linux")

    def test_failed_assembly_never_publishes_payload(self):
        output = self.root / "payload"
        def fail(*args):
            staged = args[4]
            staged.mkdir()
            (staged / "release.json").write_bytes(b"partial")
            raise ValueError("injected failure")
        with patch.object(payload, "assemble", side_effect=fail):
            with self.assertRaisesRegex(ValueError, "injected failure"):
                payload.prepare(self.root, self.resources, self.root, self.root, output, "linux", "test")
        self.assertFalse(output.exists())
        self.assertFalse(list(self.root.glob(".xunshu-payload-*")))


if __name__ == "__main__":
    unittest.main(verbosity=2)
