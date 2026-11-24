"""从已构建程序、内置资源和官方字体生成供 xs-install 验收的载荷。"""
import argparse
import hashlib
import json
import shutil
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()



def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError(f"资源清单含重复字段：{key}")
        result[key] = value
    return result


def read_manifest(path):
    if path.stat().st_size > 1024 * 1024:
        raise ValueError(f"资源清单过大：{path.name}")
    return json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=unique_object)


def validate_resources(resources, platform):
    """Cross-check build provenance and inference manifest before packaging.

    This is integrity checking, not an independent signature or model acceptance.
    """
    if not resources.is_dir() or resources.is_symlink():
        raise ValueError("无效模型资源目录")
    library = "onnxruntime.dll" if platform == "windows" else "libonnxruntime.so"
    models = {library, "bge.onnx", "bge-tokenizer.json", "ppocr-det.onnx",
              "ppocr-rec.onnx", "ppocr-dictionary.txt"}
    required = models | {"inference.json", "build-provenance.json", "BGE-LICENSE.txt",
                         "PP-OCRv5-LICENSE.txt", "onnxruntime-LICENSE",
                         "onnxruntime-ThirdPartyNotices.txt", "BGE-MODEL-CARD.md",
                         "PP-OCRv5-det-MODEL-CARD.md", "PP-OCRv5-rec-MODEL-CARD.md"}
    paths = {path.name: path for path in resources.iterdir()}
    if set(paths) != required:
        raise ValueError(f"模型资源集合不匹配：缺少 {sorted(required - paths.keys())}；额外 {sorted(paths.keys() - required)}")
    for path in paths.values():
        if path.is_symlink() or not path.is_file() or not 0 < path.stat().st_size <= 512 * 1024 * 1024:
            raise ValueError(f"无效模型资源：{path.name}")
    inference = read_manifest(paths["inference.json"])
    provenance = read_manifest(paths["build-provenance.json"])
    if inference.get("schema") != 1 or set(inference.get("files", {})) != models:
        raise ValueError("内置推理清单资源集合不匹配")
    hashes = {name: digest(paths[name]) for name in required - {"build-provenance.json"}}
    if provenance.get("outputs_sha256") != hashes:
        raise ValueError("构建来源清单与模型资源摘要不一致")
    for name, record in inference["files"].items():
        if (type(record.get("bytes")) is not int or record["bytes"] != paths[name].stat().st_size
                or record.get("sha256") != hashes[name]
                or not record.get("source") or not record.get("license")):
            raise ValueError(f"内置推理清单记录不一致：{name}")
    hashes["build-provenance.json"] = digest(paths["build-provenance.json"])
    return hashes

def assemble(binaries, resources, font, font_license, output, platform, version, expected_hashes):
    suffix = ".exe" if platform == "windows" else ""
    names = [name + suffix for name in ("xunshu", "xs-ui", "xs-content-worker", "xs-model-worker")]
    expected = [binaries / name for name in names] + [resources / "inference.json", font, font_license]
    if any(not path.is_file() or path.is_symlink() for path in expected):
        raise ValueError("构建程序、模型清单或字体文件缺失")
    output.mkdir(parents=True, exist_ok=False)
    for name in names:
        shutil.copy2(binaries / name, output / name)
    resource_copy = output / "resources"
    resource_copy.mkdir()
    for path in resources.iterdir():
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"资源包中有无效文件：{path.name}")
        shutil.copyfile(path, resource_copy / path.name)
    if validate_resources(resource_copy, platform) != expected_hashes:
        raise ValueError("模型资源在复制期间发生变化")
    fonts = resource_copy / "fonts"
    fonts.mkdir()
    shutil.copyfile(font, fonts / "NotoSansSC-Regular.otf")
    shutil.copyfile(font_license, fonts / "Noto-CJK-LICENSE.txt")

    # 纯合成文字图像及只嵌入栅格图像的 PDF，用于检验真实 OCR。
    tests = output / "self-test"
    tests.mkdir()
    image = Image.new("RGB", (820, 360), "white")
    drawer = ImageDraw.Draw(image)
    drawer.text((64, 45), "本地文件", font=ImageFont.truetype(str(font), 86), fill="black")
    drawer.text((64, 190), "XUNSHU OFFLINE", font=ImageFont.truetype(str(font), 62), fill="black")
    image.save(tests / "image.png")
    image.save(tests / "scanned.pdf", resolution=144.0)

    files = {
        path.relative_to(output).as_posix(): {"bytes": path.stat().st_size, "sha256": digest(path)}
        for path in sorted(output.rglob("*")) if path.is_file()
    }
    manifest = output / "release.json"
    manifest.write_text(json.dumps({"schema": 1, "version": version, "platform": platform, "files": files}, indent=2,
                                   sort_keys=True) + "\n", encoding="utf-8", newline="\n")
    return digest(manifest), len(files)



def prepare(binaries, resources, font, font_license, output, platform, version):
    if output.exists() or output.is_symlink():
        raise FileExistsError("载荷输出目录必须是新的")
    expected_hashes = validate_resources(resources, platform)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".xunshu-payload-", dir=output.parent) as tmp:
        staged = Path(tmp) / "payload"
        sha, count = assemble(binaries, resources, font, font_license, staged, platform, version, expected_hashes)
        if output.exists() or output.is_symlink():
            raise FileExistsError("载荷输出目录已存在")
        staged.rename(output)
    print(json.dumps({"manifest": str(output / "release.json"), "sha256": sha, "files": count}, ensure_ascii=False))

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binaries", "resources", "font", "font-license", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--platform", choices=("windows", "linux"), required=True)
    parser.add_argument("--version", required=True)
    args = parser.parse_args()
    prepare(args.binaries, args.resources, args.font, args.font_license, args.output, args.platform, args.version)


if __name__ == "__main__":
    main()
