"""Offline build-contract tests, not genuine neural model inference tests."""
import hashlib
import importlib.util
import io
import sys
import tempfile
import unittest
import urllib.error
import zipfile
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("prepare_models", Path(__file__).with_name("prepare-models.py"))
models = importlib.util.module_from_spec(spec)
spec.loader.exec_module(models)


def sha(data):
    return hashlib.sha256(data).hexdigest()


class BuildContracts(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_pinned_cache_reuses_only_matching_bytes(self):
        path = self.root / "weights"
        path.write_bytes(b"verified")
        with patch.object(models.urllib.request, "urlopen", side_effect=AssertionError("network")):
            self.assertEqual(models.fetch("https://example.invalid/a", path, sha(b"verified")), path)
        path.write_bytes(b"wrong")
        with patch.object(models.urllib.request, "urlopen", return_value=io.BytesIO(b"verified")):
            models.fetch("https://example.invalid/a", path, sha(b"verified"))
        self.assertEqual(path.read_bytes(), b"verified")

    def test_unpinned_cache_is_refreshed_and_never_used_offline(self):
        path = self.root / "metadata"
        path.write_bytes(b"old")
        with patch.object(models.urllib.request, "urlopen", return_value=io.BytesIO(b"new")) as request:
            models.fetch("https://example.invalid/a", path)
            request.assert_called_once()
        self.assertEqual(path.read_bytes(), b"new")
        with patch.object(models.urllib.request, "urlopen", side_effect=urllib.error.URLError("offline")), patch.object(models.time, "sleep"):
            with self.assertRaises(urllib.error.URLError):
                models.fetch("https://example.invalid/a", path)
        self.assertEqual(path.read_bytes(), b"new")
        self.assertEqual(list(self.root.iterdir()), [path])

    def test_bad_digest_and_empty_download_do_not_publish(self):
        path = self.root / "asset"
        for data, expected in ((b"wrong", sha(b"good")), (b"", None)):
            with patch.object(models.urllib.request, "urlopen", return_value=io.BytesIO(data)):
                with self.assertRaises(ValueError):
                    models.fetch("https://example.invalid/a", path, expected)
            self.assertFalse(path.exists())
            self.assertEqual(list(self.root.iterdir()), [])

    def test_bge_staging_excludes_old_derived_models_and_config(self):
        cache, work = self.root / "cache", self.root / "work"
        (cache / "bge").mkdir(parents=True)
        work.mkdir()
        (cache / "bge" / "bge-quantized.onnx").write_bytes(b"stale")
        (cache / "bge" / "config.json").write_bytes(b"stale")
        (cache / "bge" / "model.safetensors").write_bytes(b"pinned-test-weights")
        with patch.object(models, "BGE_WEIGHTS_SHA256", sha(b"pinned-test-weights")), patch.object(models.urllib.request, "urlopen", side_effect=lambda *a, **kw: io.BytesIO(b"fresh-metadata")) as request:
            staged, records = models.stage_bge_inputs(cache, work)
        self.assertEqual(request.call_count, 6)
        self.assertEqual((staged / "config.json").read_bytes(), b"fresh-metadata")
        self.assertFalse((staged / "bge-quantized.onnx").exists())
        self.assertEqual(records["model.safetensors"], sha(b"pinned-test-weights"))
        self.assertEqual(len(records), 7)

    def test_checked_copy_rejects_mismatch(self):
        source, target = self.root / "source", self.root / "target"
        source.write_bytes(b"modified")
        with self.assertRaises(ValueError):
            models.checked_copy(source, target, sha(b"expected"))

    def test_runtime_archive_requires_unique_library_and_licenses(self):
        cache, work, output = (self.root / name for name in ("cache", "work", "output"))
        for directory in (cache, work, output):
            directory.mkdir()
        archive = cache / "runtime.zip"
        for names, valid in [(["lib/onnxruntime.dll", "LICENSE", "ThirdPartyNotices.txt"], True),
                             (["lib/onnxruntime.dll", "LICENSE"], False),
                             (["lib/onnxruntime.dll", "other/onnxruntime.dll", "LICENSE", "ThirdPartyNotices.txt"], False)]:
            with zipfile.ZipFile(archive, "w") as package:
                for name in names:
                    package.writestr(name, b"test fixture, not runtime")
            with patch.dict(models.ORT_ARCHIVES, {"windows": (archive.name, models.digest(archive), "onnxruntime.dll", "onnxruntime.dll")}):
                if valid:
                    self.assertEqual(models.add_runtime(cache, work, output, "windows")[0], "onnxruntime.dll")
                    self.assertEqual(len(list(output.iterdir())), 3)
                else:
                    with self.assertRaises(ValueError):
                        models.add_runtime(cache, work, output, "windows")
            for path in output.iterdir():
                path.unlink()

    def test_failed_build_never_publishes_partial_directory(self):
        output = self.root / "resources"
        def fail(cache, work, staged, platform):
            staged.mkdir()
            (staged / "inference.json").write_text("partial")
            raise ValueError("injected failure")
        with patch.object(models, "build_resources", side_effect=fail), patch.object(sys, "argv", ["prepare-models", "--cache", str(self.root / "cache"), "--output", str(output), "--platform", "linux"]):
            with self.assertRaisesRegex(ValueError, "injected failure"):
                models.main()
        self.assertFalse(output.exists())
        self.assertFalse(list(self.root.glob(".xunshu-model-build-*")))

    def test_numeric_gate_rejects_shape_zero_nan_and_infinity(self):
        import numpy as np
        expected = np.ones((4, 512), dtype=np.float32)
        for actual in [np.ones((4, 511)), np.zeros_like(expected), np.full_like(expected, np.nan), np.full_like(expected, np.inf)]:
            with self.assertRaises(ValueError):
                models.embedding_cosines(expected, actual, 4)
        self.assertTrue(np.allclose(models.embedding_cosines(expected, expected, 4), 1.0))
        self.assertTrue(np.allclose(models.embedding_cosines(expected, -expected, 4), -1.0))
        huge = np.full_like(expected, np.finfo(np.float32).max)
        self.assertTrue(np.allclose(models.embedding_cosines(huge, huge, 4), 1.0))


if __name__ == "__main__":
    unittest.main(verbosity=2)
