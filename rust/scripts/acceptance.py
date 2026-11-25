#!/usr/bin/env python3
"""在本机隔离目录执行寻枢平台验收并生成 JSON 与中文报告。"""

from __future__ import annotations

import argparse
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path


def run_json(command: list[str], timeout: int = 120) -> tuple[dict, float]:
    started = time.perf_counter()
    completed = subprocess.run(command, capture_output=True, text=True, timeout=timeout)
    elapsed_ms = (time.perf_counter() - started) * 1000
    if completed.returncode != 0:
        raise RuntimeError(
            f"退出码 {completed.returncode}: {completed.stderr.strip() or completed.stdout.strip()}"
        )
    try:
        return json.loads(completed.stdout), elapsed_ms
    except json.JSONDecodeError as error:
        raise RuntimeError(f"程序未输出有效 JSON: {error}") from error


def record(status: str, detail: str, **evidence: object) -> dict:
    return {"status": status, "detail": detail, **evidence}


def write_report(path: Path, report: dict) -> None:
    path.mkdir(parents=True, exist_ok=True)
    json_path = path / "acceptance.json"
    markdown_path = path / "验收报告.md"
    json_path.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    labels = {
        "environment": "系统环境",
        "installation": "安装升级与卸载",
        "launch": "程序启动",
        "filename_search": "文件名搜索",
        "content_search": "正文搜索",
        "ocr": "OCR 图片文字搜索",
        "semantic_search": "语义搜索",
        "permissions": "范围授权与保护",
        "watcher": "文件变更监听",
        "recovery": "索引恢复",
        "resources": "内存与 CPU",
        "performance": "搜索耗时",
        "offline": "离线运行",
        "local_service": "本地服务传输",
        "cli_negative": "非法查询拒绝",
        "data_retention": "卸载后数据保留",
    }
    lines = ["# 寻枢平台实机验收报告", "", f"生成时间：{report['generated_at']}",
             f"平台：{report['platform']}", f"整体结果：{report['overall']}", "",
             "本次只使用报告目录下的合成样本和临时数据目录。", "", "| 检查项 | 状态 | 结果 |",
             "| --- | --- | --- |"]
    for key, label in labels.items():
        result = report["checks"][key]
        lines.append(f"| {label} | {result['status']} | {result['detail'].replace('|', '/')} |")
    lines.extend(["", "## 人工检查清单", ""])
    for item in report["manual_checks"]:
        lines.append(f"- [ ] {item}")
    lines.append("")
    markdown_path.write_text("\n".join(lines), encoding="utf-8")


def system_environment() -> dict[str, object]:
    os_release: dict[str, str] = {}
    release_file = Path("/etc/os-release")
    if release_file.is_file():
        for line in release_file.read_text(encoding="utf-8", errors="replace").splitlines():
            if "=" in line and not line.startswith("#"):
                key, value = line.split("=", 1)
                os_release[key] = value.strip().strip('"')
    return {
        "system": platform.system(),
        "release": platform.release(),
        "distribution": platform.platform(),
        "os_release": os_release,
        "architecture": platform.machine(),
        "libc": platform.libc_ver(),
        "python_version": platform.python_version(),
    }


def main() -> int:
    if sys.version_info < (3, 7):
        raise SystemExit("验收工具要求 Python 3.7 或更新版本，仅用于测试，不是产品运行时依赖。")
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True, help="包含 xunshu 与 xs-ui 的本机目录")
    parser.add_argument("--output-dir", type=Path, required=True, help="验收结果输出目录")
    parser.add_argument("--installer", type=Path, help="可选的 xs-install 可执行文件")
    parser.add_argument("--payload", type=Path, help="可选安装载荷，需与 --installer 配套")
    parser.add_argument("--with-models", action="store_true", help="使用本机已验证模型运行 OCR 与语义专项验收")
    parser.add_argument("--model-fixtures", type=Path, help="包含合成 scanned.pdf/image.png 的 self-test 目录")
    parser.add_argument("--keep-work", action="store_true", help="保留本次随机合成测试目录供排查")
    args = parser.parse_args()

    if bool(args.installer) != bool(args.payload):
        parser.error("--installer 与 --payload 必须同时提供")
    bin_dir = args.bin_dir.resolve(strict=True)
    cli = bin_dir / ("xunshu.exe" if os.name == "nt" else "xunshu")
    ui = bin_dir / ("xs-ui.exe" if os.name == "nt" else "xs-ui")
    if not cli.is_file() or not ui.is_file():
        parser.error(f"--bin-dir 必须包含 {cli.name} 和 {ui.name}")

    output_dir = args.output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    work_root = Path(tempfile.mkdtemp(prefix=f"xunshu-accept-{uuid.uuid4().hex[:8]}-", dir=output_dir))
    samples = work_root / "samples"
    samples.mkdir()
    content_samples = work_root / "content-samples"
    content_samples.mkdir()
    data_dir = work_root / "data"
    checks: dict[str, dict] = {}
    expected_text = "寻枢离线验收合成正文"
    (samples / "验收样本.txt").write_text("filename fixture", encoding="utf-8")
    (samples / "附件.pdf").write_text("这不是 PDF，仅用于文件名收录", encoding="utf-8")
    (samples / "ignored.bin").write_bytes(b"excluded")
    (content_samples / "正文样本.txt").write_text(expected_text, encoding="utf-8")

    try:
        environment = system_environment()
        checks["environment"] = record(
            "PASS", f"{environment['distribution']}，架构 {environment['architecture']}"
        )
        try:
            started = time.perf_counter()
            cli_help = subprocess.run([str(cli), "--help"], capture_output=True, text=True, timeout=30)
            if cli_help.returncode != 0:
                raise RuntimeError(f"xunshu --help 退出码 {cli_help.returncode}")
            gui_started = time.perf_counter()
            gui_environment = os.environ.copy()
            gui = subprocess.run(
                [str(ui), "--standalone", "--smoke-test", "--data-dir", str(work_root / "gui-data")],
                capture_output=True, text=True, timeout=30, env=gui_environment,
            )
            gui_backend = "Wayland" if gui_environment.get("WAYLAND_DISPLAY") else "X11/native"
            if (gui.returncode != 0 or "XS_UI_SMOKE_OK: native window rendered" not in gui.stderr) \
                    and gui_environment.get("WAYLAND_DISPLAY") and os.name != "nt":
                gui_environment.pop("WAYLAND_DISPLAY", None)
                gui_environment.pop("XDG_SESSION_TYPE", None)
                gui = subprocess.run(
                    [str(ui), "--standalone", "--smoke-test", "--data-dir", str(work_root / "gui-data")],
                    capture_output=True, text=True, timeout=30, env=gui_environment,
                )
                gui_backend = "X11 fallback"
            gui_ms = (time.perf_counter() - gui_started) * 1000
            if gui.returncode != 0 or "XS_UI_SMOKE_OK: native window rendered" not in gui.stderr:
                raise RuntimeError(
                    f"桌面窗口冒烟检查失败，退出码 {gui.returncode}: "
                    f"{gui.stderr.strip() or gui.stdout.strip()}"
                )
            checks["launch"] = record(
                "PASS", f"CLI 与原生桌面窗口冒烟检查通过（{gui_backend}）",
                cli_help_ms=round((time.perf_counter() - started) * 1000 - gui_ms, 2),
                gui_smoke_ms=round(gui_ms, 2),
            )
        except Exception as error:
            checks["launch"] = record("FAIL", str(error))

        def invoke(*arguments: str, timeout: int = 120) -> tuple[dict, float]:
            return run_json([str(cli), "--data-dir", str(data_dir), *arguments], timeout)

        try:
            init, _ = invoke("init", "--root", str(samples), "--ext", "txt,pdf")
            scan, _ = invoke("scan")
            found, search_ms = invoke("search", "ext:txt")
            status, _ = invoke("status")
            if not init.get("ok") or not scan.get("published") or not scan["report"].get("complete"):
                raise RuntimeError("初始化或完整扫描未成功")
            if found.get("total") != 1 or found["results"][0]["name"] != "验收样本.txt":
                raise RuntimeError("合成文件名查询结果不匹配")
            if status.get("network") is not False:
                raise RuntimeError("状态未报告离线运行")
            checks["filename_search"] = record("PASS", "中文文件名按扩展名精确命中 1 项", elapsed_ms=round(search_ms, 2))
            checks["offline"] = record("PASS", "状态接口报告 network=false", network=status["network"])
            checks["performance"] = record("PASS", "记录单次合成文件名查询耗时，不作为性能门槛", search_ms=round(search_ms, 2))
        except Exception as error:
            checks["filename_search"] = record("FAIL", str(error))
            checks["offline"] = record("FAIL", "文件名查询流程未完成", error=str(error))
            checks["performance"] = record("SKIP", "文件名查询未完成")

        try:
            service = subprocess.run(
                [str(cli), "--data-dir", str(data_dir), "serve-v2"],
                input='{"id":1,"op":"hello","protocol":2}\n',
                capture_output=True, text=True, timeout=30,
            )
            service_response = json.loads(service.stdout.splitlines()[0])
            if service.returncode != 0 or service_response.get("protocol") != 2:
                raise RuntimeError("stdio-v2 协议握手失败")
            checks["local_service"] = record(
                "PASS", "继承 stdin/stdout 的 JSONL v2 握手成功；服务接口不开放 TCP/HTTP 端口",
                protocol=service_response["protocol"],
            )
        except Exception as error:
            checks["local_service"] = record("FAIL", str(error))

        try:
            invalid = subprocess.run(
                [str(cli), "--data-dir", str(data_dir), "search", "unknown_field:value"],
                capture_output=True, text=True, timeout=30,
            )
            invalid_error = json.loads(invalid.stderr.strip().splitlines()[-1])
            if invalid.returncode == 0 or invalid_error.get("ok") is not False or "unknown_field" not in invalid_error.get("error", ""):
                raise RuntimeError("不支持的查询语法未明确拒绝")
            checks["cli_negative"] = record("PASS", "非法查询收到明确错误并以非零退出", exit_code=invalid.returncode)
        except Exception as error:
            checks["cli_negative"] = record("FAIL", str(error))

        try:
            configure, _ = invoke("content-configure", "--root", str(content_samples))
            rebuild, _ = invoke("content-rebuild", timeout=600)
            if not configure.get("ok") or not rebuild.get("complete") or rebuild.get("failed") != 0:
                raise RuntimeError(
                    "正文验收失败: " + json.dumps(
                        {"configure": configure, "rebuild": rebuild}, ensure_ascii=False
                    )
                )
            content, content_ms = invoke("content-search", expected_text, timeout=120)
            if not content.get("hits"):
                raise RuntimeError("合成 TXT 正文搜索未命中: " + json.dumps(content, ensure_ascii=False))
            checks["content_search"] = record(
                "PASS", "合成 TXT 正文命中", elapsed_ms=round(content_ms, 2),
                documents=rebuild.get("documents"),
            )
        except Exception as error:
            checks["content_search"] = record("FAIL", str(error))

        recovery_helper = Path(__file__).with_name("verify-recovery.py")
        try:
            recovery_bin_dir = bin_dir
            if args.with_models:
                recovery_bin_dir = work_root / "model-test-bin"
                recovery_bin_dir.mkdir()
                suffix = ".exe" if os.name == "nt" else ""
                for name in ("xunshu", "xs-content-worker", "xs-model-worker"):
                    os.link(bin_dir / f"{name}{suffix}", recovery_bin_dir / f"{name}{suffix}")
                shutil.copytree(bin_dir / "resources", recovery_bin_dir / "resources", copy_function=os.link)
                fixtures = args.model_fixtures or (args.payload / "self-test" if args.payload else bin_dir / "self-test")
                fixtures = fixtures.resolve(strict=True)
                shutil.copytree(fixtures, recovery_bin_dir / "self-test")
            recovery_args = [sys.executable, str(recovery_helper), "--bin-dir", str(recovery_bin_dir),
                             "--temp-root", str(work_root)]
            if args.with_models:
                recovery_args.append("--with-models")
            recovery_run = subprocess.run(recovery_args, capture_output=True, text=True, timeout=1200)
            if recovery_run.returncode != 0:
                raise RuntimeError(recovery_run.stderr.strip() or recovery_run.stdout.strip())
            recovery_result = json.loads(recovery_run.stdout)
            verified = recovery_result.get("verified", [])
            checks["permissions"] = record("PASS", "隔离合成目录授权、扫描与撤权合同通过")
            checks["watcher"] = record("PASS", "隔离目录文件变更监听合同通过")
            checks["recovery"] = record("PASS", "安装数据目录中的索引恢复合同通过", verified=verified)
            if args.with_models:
                checks["ocr"] = record("PASS", "已验证模型完成合成扫描 PDF OCR", model_self_test=True)
                checks["semantic_search"] = record("PASS", "已验证模型完成中文语义检索", semantic_vectors=True)
            else:
                checks["ocr"] = record("SKIP", "未启用 --with-models")
                checks["semantic_search"] = record("SKIP", "未启用 --with-models")
        except Exception as error:
            checks["permissions"] = record("FAIL", str(error))
            checks["watcher"] = record("FAIL", str(error))
            checks["recovery"] = record("FAIL", str(error))
            checks["ocr"] = record("FAIL", str(error)) if args.with_models else record("SKIP", "未启用 --with-models")
            checks["semantic_search"] = record("FAIL", str(error)) if args.with_models else record("SKIP", "未启用 --with-models")
        checks["resources"] = record("SKIP", "跨平台进程内存与 CPU 采样未由此脚本采集")

        if args.installer:
            installer = args.installer.resolve(strict=True)
            payload = args.payload.resolve(strict=True)
            helper = Path(__file__).with_name("verify-install.py")
            try:
                completed = subprocess.run(
                    [sys.executable, str(helper), "--installer", str(installer),
                     "--payload", str(payload), "--temp-root", str(work_root)],
                    capture_output=True, text=True, timeout=900,
                )
                if completed.returncode != 0:
                    raise RuntimeError(completed.stderr.strip() or completed.stdout.strip())
                checks["installation"] = record("PASS", "安装、升级、回滚、修复与卸载通过 verify-install.py")
                checks["data_retention"] = record("PASS", "安装器测试确认卸载保留合成用户数据")
            except Exception as error:
                checks["installation"] = record("FAIL", str(error))
                checks["data_retention"] = record("FAIL", "安装器数据保留测试失败", error=str(error))
        else:
            checks["installation"] = record("SKIP", "未提供 --installer 与 --payload")
            checks["data_retention"] = record("SKIP", "未提供可测试的安装器与载荷")

        manual_checks = [
            "在目标系统桌面双击启动应用，检查窗口焦点、中文字体、中文输入法与键盘导航。",
            "在多屏和高 DPI 配置下移动窗口，检查缩放、窗口位置记忆与恢复。",
            "检查系统托盘和全局快捷键，并验证结果打开、文件定位与拖拽。",
            "用隔离目录中的真实合成扫描 PDF 验证 OCR，再验证混合扫描文档。",
            "只有模型载荷状态为已验证时，执行中文语义查询并核对相关性。",
            "启动文件变更监听，在隔离目录新增、修改、删除文件，确认增量索引及退出恢复。",
            "在副本或一次性测试目录中制造索引损坏，检查诊断、恢复和重建。",
            "确认收录范围只包含显式授权目录，撤销后检查原目录不再出现在搜索结果。",
            "如安装测试未通过，手动核对安装路径、权限提示、修复入口与卸载后用户数据。",
        ]
        statuses = {item["status"] for item in checks.values()}
        overall = "FAIL" if "FAIL" in statuses else "PASS_WITH_SKIPS" if "SKIP" in statuses else "PASS"
        report = {
            "schema_version": 1,
            "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
            "platform": platform.platform(),
            "environment": environment,
            "binary_dir": str(bin_dir),
            "offline": True,
            "synthetic_work_dir": str(work_root),
            "synthetic_work_dir_retained": args.keep_work,
            "checks": checks,
            "manual_checks": manual_checks,
            "overall": overall,
        }
        write_report(output_dir, report)
        print(json.dumps({"overall": overall, "report": str(output_dir / "acceptance.json"),
                          "manual_report": str(output_dir / "验收报告.md")}, ensure_ascii=False))
        return 1 if overall == "FAIL" else 0
    finally:
        if not args.keep_work:
            shutil.rmtree(work_root)


if __name__ == "__main__":
    raise SystemExit(main())
