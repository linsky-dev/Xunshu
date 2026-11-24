"""从已构建程序、内置资源和官方字体生成供 xs-install 验收的载荷。"""
import argparse
import hashlib
import json
import shutil
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def prepare(binaries, resources, font, font_license, output, platform, version):
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
    print(json.dumps({"manifest": str(manifest), "sha256": digest(manifest), "files": len(files)}, ensure_ascii=False))


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
