"""从已构建程序、内置资源和官方字体生成供 xs-install 验收的载荷。"""
import argparse
import hashlib
import json
import re
import shutil
import struct
import tempfile
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()



# Noto CJK Sans2.004, upstream tree 523d033d6cb47f4a80c58a35753646f5c3608a78.
# Git blob IDs authenticate the exact pinned repository objects; release.json
# separately records SHA-256 of the delivered bytes.
FONT_OBJECTS = {
    "font": (16437364, "dc15562470b4f842321894787a0d066879ccff8b"),
    "license": (4301, "d952d62c065f3f35fb83a173496e90b21525aef3"),
}

FONT_LICENSE_SHA256 = "6a73f9541c2de74158c0e7cf6b0a58ef774f5a780bf191f2d7ec9cc53efe2bf2"
MODEL_LICENSE_SHA256 = {
    "BGE-LICENSE.txt": "587a673933425dbc36ec61268d3b954051b2d3ef3c9b322ede357976055ffdd5",
    "PP-OCRv5-LICENSE.txt": "3840c5c0c61c294264d2dd77b8777be6ddd90121ef4e0e64abcd22edea581d6e",
}


def verify_font_object(path, role):
    size, expected = FONT_OBJECTS[role]
    if path.is_symlink() or not path.is_file() or path.stat().st_size != size:
        raise ValueError(f"字体来源文件长度或类型不匹配：{path.name}")
    data = path.read_bytes()
    if role == "license" and hashlib.sha256(data).hexdigest() != FONT_LICENSE_SHA256:
        raise ValueError(f"字体许可证来源 SHA-256 不匹配：{path.name}")
    if len(data) != size or hashlib.sha1(f"blob {size}\0".encode() + data).hexdigest() != expected:
        raise ValueError(f"字体来源 Git 对象摘要不匹配：{path.name}")


def verify_native_header(path, platform, library=False):
    """Reject mistaken platform/architecture before assembling x64 payloads.

    Header checks are not code signing, loading or native execution tests.
    """
    def invalid():
        raise ValueError(f"原生程序平台、架构或文件类型不匹配：{path.name}")

    with path.open("rb") as stream:
        header = stream.read(64)
        if len(header) != 64:
            invalid()
        if platform == "linux":
            if header[:7] != b"\x7fELF\x02\x01\x01":
                invalid()
            kind, machine, version = struct.unpack_from("<HHI", header, 16)
            if machine != 62 or version != 1 or kind not in ((3,) if library else (2, 3)):
                invalid()
            if struct.unpack_from("<H", header, 52)[0] != 64:
                invalid()
        elif platform == "windows":
            if header[:2] != b"MZ":
                invalid()
            offset = struct.unpack_from("<I", header, 60)[0]
            if not 64 <= offset <= 1024 * 1024:
                invalid()
            stream.seek(offset)
            pe = stream.read(26)
            if len(pe) != 26 or pe[:4] != b"PE\0\0":
                invalid()
            machine, sections = struct.unpack_from("<HH", pe, 4)
            optional_size, flags, magic = struct.unpack_from("<HHH", pe, 20)
            if (machine != 0x8664 or not sections or optional_size < 112 or magic != 0x20b
                    or not flags & 2 or bool(flags & 0x2000) != library):
                invalid()
            if path.stat().st_size < offset + 24 + optional_size + 40 * sections:
                invalid()
        else:
            invalid()


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
    for name, expected in MODEL_LICENSE_SHA256.items():
        if hashes[name] != expected:
            raise ValueError(f"模型许可证来源 SHA-256 不匹配：{name}")
    if provenance.get("outputs_sha256") != hashes:
        raise ValueError("构建来源清单与模型资源摘要不一致")
    for name, record in inference["files"].items():
        if (type(record.get("bytes")) is not int or record["bytes"] != paths[name].stat().st_size
                or record.get("sha256") != hashes[name]
                or not record.get("source") or not record.get("license")):
            raise ValueError(f"内置推理清单记录不一致：{name}")
    hashes["build-provenance.json"] = digest(paths["build-provenance.json"])
    return hashes

def copy_runtime(output, platform, libraries):
    """将明确列出的应用本地 DLL 纳入原生安装器的完整性清单。"""
    used = {path.name.lower() for path in output.iterdir()}
    for source in libraries:
        name = source.name.lower()
        if (platform != "windows" or not re.fullmatch(r"[a-z0-9][a-z0-9_.-]*\.dll", name)
                or name in used or source.is_symlink() or not source.is_file()):
            raise ValueError(f"无效的应用本地运行库：{source.name}")
        verify_native_header(source, platform, library=True)
        expected = digest(source)
        destination = output / name
        shutil.copyfile(source, destination)
        if digest(destination) != expected:
            raise ValueError(f"运行库在复制期间发生变化：{source.name}")
        used.add(name)


def assemble(binaries, resources, font, font_license, output, platform, version, expected_hashes, libraries=()):
    suffix = ".exe" if platform == "windows" else ""
    names = [name + suffix for name in ("xunshu", "xs-ui", "xs-content-worker", "xs-model-worker")]
    expected = [binaries / name for name in names] + [resources / "inference.json", font, font_license]
    if any(not path.is_file() or path.is_symlink() for path in expected):
        raise ValueError("构建程序、模型清单或字体文件缺失")
    verify_font_object(font, "font")
    verify_font_object(font_license, "license")
    output.mkdir(parents=True, exist_ok=False)
    for name in names:
        source = binaries / name
        verify_native_header(source, platform)
        before = digest(source)
        shutil.copy2(source, output / name)
        if digest(output / name) != before:
            raise ValueError(f"程序在复制期间发生变化：{name}")
        verify_native_header(output / name, platform)
    copy_runtime(output, platform, libraries)
    resource_copy = output / "resources"
    resource_copy.mkdir()
    for path in resources.iterdir():
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"资源包中有无效文件：{path.name}")
        shutil.copyfile(path, resource_copy / path.name)
    if validate_resources(resource_copy, platform) != expected_hashes:
        raise ValueError("模型资源在复制期间发生变化")
    verify_native_header(resource_copy / ("onnxruntime.dll" if platform == "windows" else "libonnxruntime.so"), platform, library=True)
    fonts = resource_copy / "fonts"
    fonts.mkdir()
    shutil.copyfile(font, fonts / "NotoSansSC-Regular.otf")
    shutil.copyfile(font_license, fonts / "Noto-CJK-LICENSE.txt")
    verify_font_object(fonts / "NotoSansSC-Regular.otf", "font")
    verify_font_object(fonts / "Noto-CJK-LICENSE.txt", "license")
    # Render with the exact staged font, not a mutable external input.
    font = fonts / "NotoSansSC-Regular.otf"

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



def prepare(binaries, resources, font, font_license, output, platform, version, libraries=()):
    if output.exists() or output.is_symlink():
        raise FileExistsError("载荷输出目录必须是新的")
    expected_hashes = validate_resources(resources, platform)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".xunshu-payload-", dir=output.parent) as tmp:
        staged = Path(tmp) / "payload"
        sha, count = assemble(binaries, resources, font, font_license, staged, platform, version, expected_hashes, libraries)
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
    parser.add_argument("--runtime-dll", type=Path, action="append", default=[])
    args = parser.parse_args()
    prepare(args.binaries, args.resources, args.font, args.font_license, args.output, args.platform, args.version, args.runtime_dll)


if __name__ == "__main__":
    main()
