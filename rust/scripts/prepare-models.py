"""构建阶段准备内置模型。产品运行时只加载生成的本地目录。"""
import argparse
import hashlib
import json
import os
import shutil
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.request
import zipfile
from pathlib import Path


BGE = "https://huggingface.co/BAAI/bge-small-zh-v1.5/resolve/7999e1d3359715c523056ef9478215996d62a620"
BGE_WEIGHTS_SHA256 = "354763b9b1357bc9c44f62c6be2276321081ed2567773608c0d0785b61d5a026"
DET = "https://huggingface.co/PaddlePaddle/PP-OCRv5_mobile_det_onnx/resolve/e6f4fa85f00e168c862bc462aebca69eef9b3d3d"
REC = "https://huggingface.co/PaddlePaddle/PP-OCRv5_mobile_rec_onnx/resolve/ed152b8b495f84de93cda5709d768548a9127622"
OCR_SHA256 = {
    "det": "a431985659dc921974177a95adcfbb90fd9e51989a5e04d70d0b75f597b6e61d",
    "rec": "da72dc72ca4dc220df0dfde68c1dedc31c58d3e76a25871122e5056227d50092",
}
BGE_LICENSE = "https://raw.githubusercontent.com/FlagOpen/FlagEmbedding/d04f7989eeec2c6db4ead7c77ddef656f9c9d3aa/LICENSE"
OCR_LICENSE = "https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/a00f633cef33a8192f853fdaf8decf24ce2240b0/LICENSE"
LICENSE_OBJECTS = {
    "BGE-LICENSE.txt": (1065, "360931513aa6c02f933a403202afa99ac2c5bc88"),
    "PP-OCRv5-LICENSE.txt": (11376, "430edfa0cfcb59f15b7aa5457427d0faf2e40dac"),
}
LICENSE_SHA256 = {
    "BGE-LICENSE.txt": "587a673933425dbc36ec61268d3b954051b2d3ef3c9b322ede357976055ffdd5",
    "PP-OCRv5-LICENSE.txt": "3840c5c0c61c294264d2dd77b8777be6ddd90121ef4e0e64abcd22edea581d6e",
}
ORT_RELEASE = "https://github.com/microsoft/onnxruntime/releases/download/v1.24.4"
ORT_ARCHIVES = {
    "windows": ("onnxruntime-win-x64-1.24.4.zip", "d2319fddfb6ea4db99ccc4b60c85c517bcd855721f5daa6a06d40d7cb2ee2357", "onnxruntime.dll", "onnxruntime.dll"),
    "linux": ("onnxruntime-linux-x64-1.24.4.tgz", "3a211fbea252c1e66290658f1b735b772056149f28321e71c308942cdb54b747", "libonnxruntime.so.1.24.4", "libonnxruntime.so"),
}


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def fetch(url, dest, expected=None):
    # Only independently pinned bytes may be reused. A commit-pinned URL alone
    # does not authenticate bytes previously placed in a local cache.
    if dest.is_symlink():
        raise ValueError(f"拒绝符号链接缓存：{dest.name}")
    if expected is not None and dest.is_file() and digest(dest) == expected:
        return dest
    dest.parent.mkdir(parents=True, exist_ok=True)
    fd, filename = tempfile.mkstemp(prefix=f".{dest.name}.", suffix=".part", dir=dest.parent)
    os.close(fd)
    temp = Path(filename)
    try:
        for attempt in range(2):
            try:
                request = urllib.request.Request(url, headers={"User-Agent": "Xunshu-build-assets/1"})
                with urllib.request.urlopen(request, timeout=60) as response, temp.open("wb") as output:
                    shutil.copyfileobj(response, output)
                break
            except urllib.error.HTTPError as error:
                if attempt or (error.code != 429 and not 500 <= error.code <= 599):
                    raise
                time.sleep(20 if error.code == 429 else 2)
            except (TimeoutError, urllib.error.URLError):
                if attempt:
                    raise
                time.sleep(2)
        if expected is not None and digest(temp) != expected:
            raise ValueError(f"上游下载摘要不匹配：{dest.name}")
        if not temp.stat().st_size:
            raise ValueError(f"上游下载为空：{dest.name}")
        temp.replace(dest)
        return dest
    finally:
        temp.unlink(missing_ok=True)


def verify_license(path):
    size, expected = LICENSE_OBJECTS[path.name]
    if path.is_symlink() or not path.is_file() or path.stat().st_size != size:
        raise ValueError(f"许可证来源长度或类型不匹配：{path.name}")
    data = path.read_bytes()
    if hashlib.sha256(data).hexdigest() != LICENSE_SHA256[path.name]:
        raise ValueError(f"许可证来源 SHA-256 不匹配：{path.name}")
    if len(data) != size or hashlib.sha1(f"blob {size}\0".encode() + data).hexdigest() != expected:
        raise ValueError(f"许可证来源 Git 对象摘要不匹配：{path.name}")


def checked_copy(source, target, expected):
    shutil.copyfile(source, target)
    if digest(target) != expected:
        raise ValueError(f"复制后摘要不匹配：{target.name}")


def embedding_cosines(expected, actual, count):
    import numpy as np

    shape = (count, 512)
    if expected.shape != shape or actual.shape != shape:
        raise ValueError("BGE 输出维度不匹配")
    # Float64 keeps the norm calculation from overflowing for finite float32 values.
    left, right = expected.astype(np.float64), actual.astype(np.float64)
    if not np.isfinite(left).all() or not np.isfinite(right).all():
        raise ValueError("BGE 输出存在非有限数值")
    left_norm = np.linalg.norm(left, axis=1)
    right_norm = np.linalg.norm(right, axis=1)
    if (left_norm <= 1e-12).any() or (right_norm <= 1e-12).any():
        raise ValueError("BGE 输出存在零向量")
    cosine = ((left / left_norm[:, None]) * (right / right_norm[:, None])).sum(axis=1)
    if not np.isfinite(cosine).all():
        raise ValueError("BGE 余弦相似度无效")
    return cosine


def stage_bge_inputs(cache, work):
    root = work / "bge"
    root.mkdir()
    weights = fetch(f"{BGE}/model.safetensors", cache / "bge" / "model.safetensors", BGE_WEIGHTS_SHA256)
    checked_copy(weights, root / "model.safetensors", BGE_WEIGHTS_SHA256)
    for name in ("README.md", "config.json", "tokenizer.json", "tokenizer_config.json", "special_tokens_map.json", "vocab.txt"):
        fetch(f"{BGE}/{name}", root / name)
    inputs_provenance = {path.name: digest(path) for path in sorted(root.iterdir())}
    return root, inputs_provenance


def export_bge(cache, work):
    # Python 转换工具属于开发依赖，模型加载不执行仓库远程代码。
    import numpy as np
    import onnx
    import onnxruntime as ort
    import torch
    from onnxruntime.quantization import QuantType, quantize_dynamic
    from transformers import AutoModel, AutoTokenizer

    root, inputs_provenance = stage_bge_inputs(cache, work)
    quantized = root / "bge-quantized.onnx"
    # Never trust a derived ONNX left by an earlier build. Re-export from the
    # verified weights and fresh metadata in this isolated workspace.
    torch.set_num_threads(2)
    tokenizer = AutoTokenizer.from_pretrained(root, local_files_only=True, trust_remote_code=False)
    encoder = AutoModel.from_pretrained(root, local_files_only=True, trust_remote_code=False,
                                        use_safetensors=True, attn_implementation="eager").eval()

    class SentenceEncoder(torch.nn.Module):
        def forward(self, input_ids, attention_mask, token_type_ids):
            outputs = self.encoder(input_ids=input_ids, attention_mask=attention_mask, token_type_ids=token_type_ids)
            return outputs.last_hidden_state[:, 0]

    names = ["input_ids", "attention_mask", "token_type_ids"]
    samples = ["寻枢离线搜索", "文件检索与整理", "蓝鲸在海洋中生活", "Windows local file search"]
    model = SentenceEncoder()
    model.add_module("encoder", encoder)
    model.eval()
    batch = tokenizer(samples, padding=True, return_tensors="pt")
    baseline = root / "bge-fp32.onnx"
    with torch.no_grad():
        torch.onnx.export(model, tuple(batch[name] for name in names), str(baseline), dynamo=False,
                          opset_version=17, input_names=names, output_names=["embedding"],
                          dynamic_axes={**{name: {0: "batch", 1: "tokens"} for name in names},
                                        "embedding": {0: "batch"}})
        expected = model(*(batch[name] for name in names)).numpy()
    options = ort.SessionOptions()
    options.intra_op_num_threads = 2
    inputs = {name: batch[name].numpy() for name in names}
    fp32 = ort.InferenceSession(str(baseline), sess_options=options, providers=["CPUExecutionProvider"]).run(None, inputs)[0]
    embedding_cosines(expected, fp32, len(samples))
    if not np.allclose(fp32, expected, rtol=0.002, atol=0.002):
        raise ValueError("ONNX 浮点导出与官方权重数值不一致")
    quant_temp = root / "bge-quantized.pending.onnx"
    quantize_dynamic(str(baseline), str(quant_temp), per_channel=True, weight_type=QuantType.QInt8,
                     op_types_to_quantize=["MatMul"])
    onnx.checker.check_model(str(quant_temp))
    actual = ort.InferenceSession(str(quant_temp), sess_options=options, providers=["CPUExecutionProvider"]).run(
        None, inputs)[0]
    cosine = embedding_cosines(expected, actual, len(samples))
    minimum = float(cosine.min())
    provenance = {"upstream": BGE, "inputs_sha256": inputs_provenance,
                  "sample_count": len(samples), "minimum_quantized_cosine": minimum,
                  "fp32_max_abs_error": float(np.max(np.abs(fp32 - expected))),
                  "versions": {"torch": str(torch.__version__), "onnx": onnx.__version__,
                               "onnxruntime": ort.__version__, "numpy": np.__version__}}
    if minimum < 0.98:
        print(f"量化未通过数值门槛，采用已校验的浮点模型：{cosine.tolist()}", flush=True)
        provenance["selected"] = "fp32"
        return baseline, root / "tokenizer.json", provenance
    quant_temp.replace(quantized)
    provenance["selected"] = "int8"
    print(f"BGE 转换校验，最小余弦相似度 {minimum:.6f}", flush=True)
    return quantized, root / "tokenizer.json", provenance


def prepare_ocr(cache, work, name, upstream):
    import yaml

    root = work / name
    for file in ("README.md", "inference.yml"):
        fetch(f"{upstream}/{file}", root / file)
    model = root / "inference.onnx"
    cached = fetch(f"{upstream}/inference.onnx", cache / name / "inference.onnx", OCR_SHA256[name])
    checked_copy(cached, model, OCR_SHA256[name])
    config = yaml.safe_load((root / "inference.yml").read_text(encoding="utf-8"))
    if name == "rec":
        dictionary = root / "dictionary.txt"
        keys = config["PostProcess"]["character_dict"]
        if not keys or any(not isinstance(key, str) or "\n" in key or "\r" in key for key in keys):
            raise ValueError("官方 OCR 字典格式异常")
        dictionary.write_text("\n".join(keys) + "\n", encoding="utf-8", newline="\n")
        return model, dictionary
    return model


def add_runtime(cache, work, output, platform):
    archive, expected, source_name, local_name = ORT_ARCHIVES[platform]
    url = f"{ORT_RELEASE}/{archive}"
    cached = fetch(url, cache / archive, expected)
    path = work / archive
    checked_copy(cached, path, expected)
    license_names = {"LICENSE", "ThirdPartyNotices.txt"}
    required = {source_name, *license_names}
    seen = set()

    def target_for(name):
        if name in seen:
            raise ValueError(f"ORT 压缩包中存在重复文件：{name}")
        seen.add(name)
        return output / (local_name if name == source_name else "onnxruntime-" + name)
    if platform == "windows":
        with zipfile.ZipFile(path) as package:
            for item in package.infolist():
                name = Path(item.filename).name
                if name == source_name or name in license_names:
                    target = target_for(name)
                    with package.open(item) as stream, target.open("xb") as file:
                        shutil.copyfileobj(stream, file)
    else:
        with tarfile.open(path, "r:gz") as package:
            for item in package:
                name = Path(item.name).name
                if item.isfile() and (name == source_name or name in license_names):
                    target = target_for(name)
                    with package.extractfile(item) as stream, target.open("xb") as file:
                        shutil.copyfileobj(stream, file)
    if seen != required:
        raise ValueError(f"ORT 压缩包缺少必需文件：{sorted(required - seen)}")
    return local_name, url


def build_resources(cache, work, output, platform):
    bge, tokenizer, provenance = export_bge(cache, work)
    detection = prepare_ocr(cache, work, "det", DET)
    recognition, dictionary = prepare_ocr(cache, work, "rec", REC)
    notices = {
        "BGE-LICENSE.txt": fetch(BGE_LICENSE, work / "BGE-LICENSE.txt"),
        "PP-OCRv5-LICENSE.txt": fetch(OCR_LICENSE, work / "PP-OCRv5-LICENSE.txt"),
    }
    for notice in notices.values():
        verify_license(notice)
    output.mkdir()
    records = {}

    def record(name, source, license_id):
        path = output / name
        records[name] = {"bytes": path.stat().st_size, "sha256": digest(path), "source": source, "license": license_id}

    for source, name, url, license_id in (
        (bge, "bge.onnx", BGE, "MIT"),
        (tokenizer, "bge-tokenizer.json", BGE, "MIT"),
        (detection, "ppocr-det.onnx", DET, "Apache-2.0"),
        (recognition, "ppocr-rec.onnx", REC, "Apache-2.0"),
        (dictionary, "ppocr-dictionary.txt", REC, "Apache-2.0"),
    ):
        shutil.copyfile(source, output / name)
        record(name, url, license_id)
    runtime, source = add_runtime(cache, work, output, platform)
    record(runtime, source, "MIT")
    for folder, name in [("bge", "BGE"), ("det", "PP-OCRv5-det"), ("rec", "PP-OCRv5-rec")]:
        shutil.copyfile(work / folder / "README.md", output / f"{name}-MODEL-CARD.md")
    for name, notice in notices.items():
        shutil.copyfile(notice, output / name)
        verify_license(output / name)
    (output / "inference.json").write_text(json.dumps({"schema": 1, "files": records}, indent=2, sort_keys=True) + "\n",
                                               encoding="utf-8", newline="\n")
    provenance["ocr"] = {name: {"source": url, "config_sha256": digest(work / name / "inference.yml"),
                              "model_sha256": OCR_SHA256[name]}
                         for name, url in (("det", DET), ("rec", REC))}
    provenance["license_sources"] = {"BGE-LICENSE.txt": BGE_LICENSE, "PP-OCRv5-LICENSE.txt": OCR_LICENSE}
    provenance["license_git_objects"] = {name: {"bytes": size, "git_blob_sha1": sha, "sha256": LICENSE_SHA256[name]}
                                         for name, (size, sha) in LICENSE_OBJECTS.items()}
    provenance["outputs_sha256"] = {p.name: digest(p) for p in sorted(output.iterdir())}
    (output / "build-provenance.json").write_text(json.dumps(provenance, indent=2, sort_keys=True) + "\n",
                                                  encoding="utf-8", newline="\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--platform", choices=ORT_ARCHIVES, required=True)
    args = parser.parse_args()
    os.environ["HF_HUB_OFFLINE"] = "1"
    os.environ["TRANSFORMERS_OFFLINE"] = "1"
    if args.output.exists() or args.output.is_symlink():
        raise FileExistsError("资源输出目录必须是新的")
    args.cache.mkdir(parents=True, exist_ok=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    # Publish only a complete resource directory, on the same filesystem.
    with tempfile.TemporaryDirectory(prefix=".xunshu-model-build-", dir=args.output.parent) as tmp:
        work = Path(tmp)
        staged = work / "resources"
        build_resources(args.cache, work, staged, args.platform)
        if args.output.exists() or args.output.is_symlink():
            raise FileExistsError("资源输出目录已存在")
        staged.rename(args.output)
    print(f"内置推理资源已准备：{args.output}", flush=True)


if __name__ == "__main__":
    sys.exit(main())
