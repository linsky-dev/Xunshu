"""Offline integrity contracts. Fixtures are not runnable neural models."""
import importlib.util
import hashlib
import struct
import sys
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
        # Fixtures are not upstream licenses; override only the independently
        # pinned expected hashes in this isolated test scope.
        pins = patch.dict(payload.MODEL_LICENSE_SHA256, {name: payload.digest(self.resources / name)
                          for name in payload.MODEL_LICENSE_SHA256})
        pins.start()
        self.addCleanup(pins.stop)

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

    def test_recomputed_provenance_cannot_replace_pinned_license(self):
        (self.resources / "BGE-LICENSE.txt").write_bytes(b"substituted")
        self.refresh_provenance()
        with self.assertRaisesRegex(ValueError, "模型许可证来源 SHA-256"):
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

    def test_pinned_font_and_license_bytes(self):
        for role in ("font", "license"):
            data = ("synthetic-" + role).encode()
            fixture = self.root / role
            fixture.write_bytes(data)
            sha = hashlib.sha1(f"blob {len(data)}\0".encode() + data).hexdigest()
            with patch.dict(payload.FONT_OBJECTS, {role: (len(data), sha)}), patch.object(payload, "FONT_LICENSE_SHA256", hashlib.sha256(data).hexdigest()):
                payload.verify_font_object(fixture, role)
                fixture.write_bytes(b"x" * len(data))
                with self.assertRaises(ValueError):
                    payload.verify_font_object(fixture, role)

    def test_elf_architecture_and_type(self):
        path = self.root / "elf"
        header = bytearray(64)
        header[:7] = b"\x7fELF\x02\x01\x01"
        struct.pack_into("<HHI", header, 16, 2, 62, 1)
        struct.pack_into("<H", header, 52, 64)
        path.write_bytes(header)
        payload.verify_native_header(path, "linux")
        with self.assertRaises(ValueError):
            payload.verify_native_header(path, "linux", library=True)
        with self.assertRaises(ValueError):
            payload.verify_native_header(path, "windows")
        struct.pack_into("<H", header, 18, 183)  # AArch64 is not x64.
        path.write_bytes(header)
        with self.assertRaises(ValueError):
            payload.verify_native_header(path, "linux")
        path.write_bytes(b"\x7fELF")
        with self.assertRaises(ValueError):
            payload.verify_native_header(path, "linux")

    def test_pe_architecture_and_dll_type(self):
        path = self.root / "pe"
        header = bytearray(512)
        header[:2] = b"MZ"
        struct.pack_into("<I", header, 60, 64)
        header[64:68] = b"PE\0\0"
        struct.pack_into("<HH", header, 68, 0x8664, 1)
        struct.pack_into("<HHH", header, 84, 112, 2, 0x20b)
        path.write_bytes(header)
        payload.verify_native_header(path, "windows")
        with self.assertRaises(ValueError):
            payload.verify_native_header(path, "windows", library=True)
        struct.pack_into("<H", header, 86, 0x2002)
        path.write_bytes(header)
        payload.verify_native_header(path, "windows", library=True)
        with self.assertRaises(ValueError):
            payload.verify_native_header(path, "windows")
        struct.pack_into("<H", header, 68, 0xaa64)
        path.write_bytes(header)
        with self.assertRaises(ValueError):
            payload.verify_native_header(path, "windows", library=True)

    @unittest.skipUnless(sys.platform.startswith("linux"), "real ELF host executable only")
    def test_real_linux_executable_header(self):
        import platform
        if platform.machine() != "x86_64":
            self.skipTest("x64-only packaging contract")
        payload.verify_native_header(Path(sys.executable).resolve(), "linux")

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
