"""构建阶段准备内置模型。产品运行时只加载生成的本地目录。"""
import argparse
import hashlib
import json
import os
import shutil
import sys
import tarfile
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
ORT_RELEASE = "https://github.com/microsoft/onnxruntime/releases/download/v1.24.4"
ORT_ARCHIVES = {
    "windows": ("onnxruntime-win-x64-1.24.4.zip", "d2319fddfb6ea4db99ccc4b60c85c517bcd855721f5daa6a06d40d7cb2ee2357", "onnxruntime.dll", "onnxruntime.dll"),
    "linux": ("onnxruntime-linux-x64-1.24.4.tgz", "3a211fbea252c1e66290658f1b735b772056149f28321e71c308942cdb54b747", "libonnxruntime.so.1.24.4", "libonnxruntime.so"),
}


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def fetch(url, dest, expected=None):
    if dest.is_file() and (expected is None or digest(dest) == expected):
        return dest
    dest.parent.mkdir(parents=True, exist_ok=True)
    temp = dest.with_suffix(dest.suffix + ".part")
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
    if expected and digest(temp) != expected:
        raise ValueError(f"上游下载摘要不匹配：{dest.name}")
    temp.replace(dest)
    return dest


def export_bge(cache):
    # Python 转换工具属于开发依赖，模型加载不执行仓库远程代码。
    import numpy as np
    import onnx
    import onnxruntime as ort
    import torch
    from onnxruntime.quantization import QuantType, quantize_dynamic
    from transformers import AutoModel, AutoTokenizer

    root = cache / "bge"
    for name in ("README.md", "config.json", "model.safetensors", "tokenizer.json", "tokenizer_config.json", "special_tokens_map.json", "vocab.txt"):
        fetch(f"{BGE}/{name}", root / name, BGE_WEIGHTS_SHA256 if name == "model.safetensors" else None)
    quantized = root / "bge-quantized.onnx"
    if quantized.is_file():
        onnx.checker.check_model(str(quantized))
        return quantized, root / "tokenizer.json"
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
    if not np.allclose(fp32, expected, rtol=0.002, atol=0.002):
        raise ValueError("ONNX 浮点导出与官方权重数值不一致")
    quant_temp = root / "bge-quantized.pending.onnx"
    quantize_dynamic(str(baseline), str(quant_temp), per_channel=True, weight_type=QuantType.QInt8,
                     op_types_to_quantize=["MatMul"])
    onnx.checker.check_model(str(quant_temp))
    actual = ort.InferenceSession(str(quant_temp), sess_options=options, providers=["CPUExecutionProvider"]).run(
        None, inputs)[0]
    cosine = (expected * actual).sum(axis=1) / (np.linalg.norm(expected, axis=1) * np.linalg.norm(actual, axis=1))
    if actual.shape != (len(samples), 512) or not np.isfinite(actual).all() or cosine.min() < 0.98:
        print(f"量化未通过数值门槛，采用已校验的浮点模型：{cosine.tolist()}", flush=True)
        return baseline, root / "tokenizer.json"
    quant_temp.replace(quantized)
    print(f"BGE 转换校验，最小余弦相似度 {cosine.min():.6f}", flush=True)
    return quantized, root / "tokenizer.json"


def prepare_ocr(cache, name, upstream):
    import yaml

    root = cache / name
    for file in ("README.md", "inference.yml"):
        fetch(f"{upstream}/{file}", root / file)
    model = fetch(f"{upstream}/inference.onnx", root / "inference.onnx", OCR_SHA256[name])
    config = yaml.safe_load((root / "inference.yml").read_text(encoding="utf-8"))
    if name == "rec":
        dictionary = root / "dictionary.txt"
        keys = config["PostProcess"]["character_dict"]
        if not keys or any(not isinstance(key, str) or "\n" in key or "\r" in key for key in keys):
            raise ValueError("官方 OCR 字典格式异常")
        dictionary.write_text("\n".join(keys) + "\n", encoding="utf-8", newline="\n")
        return model, dictionary
    return model


def add_runtime(cache, output, platform):
    archive, expected, source_name, local_name = ORT_ARCHIVES[platform]
    url = f"{ORT_RELEASE}/{archive}"
    path = fetch(url, cache / archive, expected)
    license_names = {"LICENSE", "ThirdPartyNotices.txt"}
    if platform == "windows":
        with zipfile.ZipFile(path) as package:
            for item in package.infolist():
                name = Path(item.filename).name
                if name == source_name or name in license_names:
                    target = local_name if name == source_name else "onnxruntime-" + name
                    with package.open(item) as stream, (output / target).open("wb") as file:
                        shutil.copyfileobj(stream, file)
    else:
        with tarfile.open(path, "r:gz") as package:
            for item in package:
                name = Path(item.name).name
                if item.isfile() and (name == source_name or name in license_names):
                    target = local_name if name == source_name else "onnxruntime-" + name
                    with package.extractfile(item) as stream, (output / target).open("wb") as file:
                        shutil.copyfileobj(stream, file)
    return local_name, url


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--platform", choices=ORT_ARCHIVES, required=True)
    args = parser.parse_args()
    os.environ["HF_HUB_OFFLINE"] = "1"
    os.environ["TRANSFORMERS_OFFLINE"] = "1"
    if args.output.exists():
        raise FileExistsError("资源输出目录必须是新的")
    args.cache.mkdir(parents=True, exist_ok=True)
    bge, tokenizer = export_bge(args.cache)
    detection = prepare_ocr(args.cache, "det", DET)
    recognition, dictionary = prepare_ocr(args.cache, "rec", REC)
    notices = {
        "BGE-LICENSE.txt": fetch("https://raw.githubusercontent.com/FlagOpen/FlagEmbedding/master/LICENSE", args.cache / "BGE-LICENSE.txt"),
        "PP-OCRv5-LICENSE.txt": fetch("https://raw.githubusercontent.com/PaddlePaddle/PaddleOCR/v3.0.3/LICENSE", args.cache / "PP-OCRv5-LICENSE.txt"),
    }
    args.output.mkdir(parents=True)
    records = {}

    def record(name, source, license_id):
        path = args.output / name
        records[name] = {"bytes": path.stat().st_size, "sha256": digest(path), "source": source, "license": license_id}

    for source, name, url, license_id in (
        (bge, "bge.onnx", BGE, "MIT"),
        (tokenizer, "bge-tokenizer.json", BGE, "MIT"),
        (detection, "ppocr-det.onnx", DET, "Apache-2.0"),
        (recognition, "ppocr-rec.onnx", REC, "Apache-2.0"),
        (dictionary, "ppocr-dictionary.txt", REC, "Apache-2.0"),
    ):
        shutil.copyfile(source, args.output / name)
        record(name, url, license_id)
    runtime, source = add_runtime(args.cache, args.output, args.platform)
    record(runtime, source, "MIT")
    for folder, name in [("bge", "BGE"), ("det", "PP-OCRv5-det"), ("rec", "PP-OCRv5-rec")]:
        shutil.copyfile(args.cache / folder / "README.md", args.output / f"{name}-MODEL-CARD.md")
    for name, notice in notices.items():
        shutil.copyfile(notice, args.output / name)
    (args.output / "inference.json").write_text(json.dumps({"schema": 1, "files": records}, indent=2, sort_keys=True) + "\n",
                                               encoding="utf-8", newline="\n")
    print(f"内置推理资源已准备：{args.output}", flush=True)


if __name__ == "__main__":
    sys.exit(main())
