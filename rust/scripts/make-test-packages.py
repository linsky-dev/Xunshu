"""以已验证的原生程序与模型资源组装离线测试候选包。"""

import argparse
import json
import os
import re
import runpy
import shutil
import stat
import subprocess
import sys
import tempfile
import tomllib
import zipfile
from pathlib import Path

from PIL import Image


SCRIPTS = Path(__file__).resolve().parent
WORKSPACE = SCRIPTS.parent
PROGRAMS = ('xunshu', 'xs-ui', 'xs-content-worker', 'xs-model-worker', 'xs-install')
WINDOWS_CRT = {'msvcp140.dll', 'msvcp140_1.dll', 'vcruntime140.dll', 'vcruntime140_1.dll'}


def write(path, content):
    path.write_text(content, encoding='utf-8', newline='\n')


def copy_verified(source, target, digest):
    if source.is_symlink() or not source.is_file():
        raise ValueError(f'缺少普通文件：{source}')
    expected = digest(source)
    shutil.copy2(source, target)
    if digest(target) != expected:
        raise ValueError(f'复制期间文件发生变化：{source}')


def metadata(args, stage, payload, digest):
    licenses = stage / 'licenses'
    shutil.copytree(WORKSPACE / 'licenses', licenses)
    copy_verified(WORKSPACE / 'LICENSE', licenses / 'Xunshu-LICENSE.txt', digest)
    runtime_notice = licenses / 'runtime'
    if args.runtime_notice:
        runtime_notice.mkdir()
        for source in args.runtime_notice:
            destination = runtime_notice / source.name
            if destination.exists():
                raise ValueError(f'重复运行库许可文件：{source.name}')
            copy_verified(source, destination, digest)

    info = {'status': 'internal_test_candidate', 'signed': False, 'platform': args.platform,
            'version': args.version, 'source_commit': args.source_commit, 'architecture': args.architecture,
            'cargo_lock_sha256': digest(WORKSPACE / 'Cargo.lock'),
            'payload_manifest_sha256': digest(payload / 'release.json')}
    if args.platform == 'linux':
        info.update({'target': args.linux_kind, 'target_glibc_ceiling': args.max_glibc})
    write(stage / 'VERSION.json', json.dumps(info, ensure_ascii=False, indent=2) + '\n')
    purpose = (
        'Windows 10/11 x64 实机测试候选版。\n'
        if args.platform == 'windows' else (
            f'统信 UOS V20 {args.architecture} 测试候选版。\n' if args.linux_kind == 'uos' else
            f'通用 Linux {args.architecture} 测试候选版，尚未取得统信 UOS 实机结论。\n'))
    steps = (
        '安装包：运行 Setup-Test.exe，选择本机安装目录。管理员权限用于配置应用程序的离线防火墙规则。\n'
        '升级或修复：关闭寻枢后再次运行安装器，使用原安装目录。\n'
        '回退：在管理员 PowerShell 中运行 .\\xs-install.exe --root "安装目录的绝对路径" rollback。\n'
        '卸载：从 Windows 已安装应用列表操作，或运行安装目录中的卸载程序。用户搜索数据会保留。\n'
        '便携包：完整解压后双击 xs-ui.exe；可用 --data-dir 指定独立本地数据目录。\n'
        if args.platform == 'windows' else
        '安装或升级：使用 sudo dpkg -i ./安装包.deb。依赖须已从系统包源预装。\n'
        '打开：在应用菜单中选择寻枢测试候选版。也可运行 /opt/xunshu-test/xs-ui。\n'
        '卸载：使用 sudo dpkg -r xunshu-test，应用数据目录保留。\n'
        '便携包：完整解压后运行 ./xs-ui；可用 --data-dir 指定独立本地数据目录。\n')
    write(stage / 'README.txt', f'''寻枢 Xunshu {args.version}

{purpose}内部测试包，未签名。请先核对交付目录中的 SHA256SUMS。

{steps}
首次运行在设置中选择明确授权的本地目录，程序才会建立索引。
请勿把 --data-dir 指向旧版数据目录。模型、字体和对应许可证随包提供。
模型计算、文件名和正文索引均在本机执行。

源码提交 {args.source_commit}
载荷 release.json SHA-256 {info['payload_manifest_sha256']}
''')
    return info


def portable_pack(stage, payload, manager, output):
    root = stage / 'portable'
    shutil.copytree(payload, root)
    shutil.copytree(stage / 'licenses', root / 'licenses')
    for source in (manager, stage / 'README.txt', stage / 'VERSION.json'):
        shutil.copy2(source, root / source.name)
    abi = stage / 'linux-abi.json'
    if abi.is_file():
        shutil.copy2(abi, root / abi.name)
    if output.suffix == '.zip':
        shutil.copy2(manager.with_name('xunshu.ico'), root / 'xunshu.ico')
        with zipfile.ZipFile(output, 'x', zipfile.ZIP_DEFLATED, compresslevel=3) as archive:
            for path in sorted(root.rglob('*')):
                if path.is_file():
                    archive.write(path, f'Xunshu/{path.relative_to(root).as_posix()}')
        with zipfile.ZipFile(output) as archive:
            failed = archive.testzip()
            if failed:
                raise ValueError(f'便携包 ZIP 完整性检查失败：{failed}')
    else:
        subprocess.run(['tar', '--create', '--gzip', '--file', str(output),
                        r'--transform=s,^\./,Xunshu/,', '--directory', str(root), '.'], check=True)
        subprocess.run(['gzip', '--test', str(output)], check=True)


def windows_pack(args, stage, manager, info, output, digest):
    managers = manager.parent
    launcher = managers / 'xs-launch.exe'
    copy_verified(args.bin_dir / launcher.name, launcher, digest)
    runpy.run_path(str(SCRIPTS / 'make-install-payload.py'))['verify_native_header'](launcher, 'windows')
    copy_verified(WORKSPACE / 'assets/xunshu.ico', managers / 'xunshu.ico', digest)
    # 同一套应用本地运行库也支撑安装管理程序本身的 CRT 或 MinGW 导入。
    for path in args.runtime_dll:
        shutil.copy2(stage / 'payload' / path.name.lower(), managers / path.name.lower())
    command = [str(args.inno_compiler), f'/O{output.parent}', f'/F{output.stem}',
               f'/DStageDir={stage}', f'/DAppVersionText={args.version}',
               f"/DPayloadSha256={info['payload_manifest_sha256']}", str(SCRIPTS / 'xunshu-test.iss')]
    subprocess.run(command, check=True)


def linux_pack(args, stage, payload, manager, output):
    audit = runpy.run_path(str(SCRIPTS / 'audit-linux-abi.py'))['inspect']
    binaries = [payload / name for name in PROGRAMS[:-1]]
    audited = [audit(path, args.max_glibc, args.architecture) for path in
               (*binaries, manager, payload / 'resources/libonnxruntime.so')]
    over = [entry for entry in audited if entry['ceiling_status'] == 'FAIL']
    if over:
        detail = ', '.join(f"{Path(entry['file']).name}={entry['max_strong_glibc']}" for entry in over)
        raise ValueError(f'超出目标 GLIBC {args.max_glibc} 上限：{detail}')
    for entry in audited:
        entry['file'] = Path(entry['file']).name
    write(stage / 'linux-abi.json', json.dumps(audited, ensure_ascii=False, indent=2) + '\n')
    glibc = max((entry['max_strong_glibc'] for entry in audited if entry['max_strong_glibc']),
                key=lambda version: tuple(map(int, version.split('.'))))

    package = stage / 'debian'
    application = package / 'opt/xunshu-test'
    shutil.copytree(payload, application)
    shutil.copy2(manager, application / manager.name)
    for name in PROGRAMS:
        (application / name).chmod(0o755)
    share = package / 'usr/share'
    docs = share / 'doc/xunshu-test'
    docs.mkdir(parents=True)
    for name in ('README.txt', 'VERSION.json', 'linux-abi.json'):
        shutil.copy2(stage / name, docs / name)
    shutil.copytree(stage / 'licenses', docs / 'licenses')
    shutil.copy2(stage / 'licenses/Xunshu-LICENSE.txt', docs / 'copyright')
    desktop = share / 'applications'
    desktop.mkdir()
    write(desktop / 'xunshu-test.desktop', '''[Desktop Entry]
Type=Application
Name=寻枢测试候选版
Name[en]=Xunshu Test Candidate
Comment=在本地搜索文件和正文
Exec=/opt/xunshu-test/xs-ui
Icon=xunshu-test
Terminal=false
Categories=Utility;FileTools;
StartupNotify=true
''')
    icons = share / 'icons/hicolor/64x64/apps'
    icons.mkdir(parents=True)
    with Image.open(WORKSPACE / 'assets/xunshu.png') as icon:
        icon.resize((64, 64), Image.Resampling.LANCZOS).save(icons / 'xunshu-test.png')

    control = package / 'DEBIAN'
    control.mkdir()
    # 图形后端按需加载的常用库无法从 ELF DT_NEEDED 完整推导。
    dependencies = (f'libc6 (>= {glibc}), libgcc-s1 | libgcc1, libstdc++6, libx11-6, '
                    'libx11-xcb1, libxcursor1, libxi6, libxkbcommon0, libxkbcommon-x11-0, '
                    'libwayland-client0, libegl1, libgl1, libfontconfig1')
    version = args.version.replace('-alpha.', '~alpha.') + '-1'
    installed_kib = sum(path.stat().st_size for path in package.rglob('*') if path.is_file())
    write(control / 'control', f'''Package: xunshu-test
Version: {version}
Architecture: {args.architecture}
Section: utils
Priority: optional
Maintainer: Xunshu contributors
Homepage: https://github.com/linsky-dev/Xunshu
Installed-Size: {(installed_kib + 1023) // 1024}
Depends: {dependencies}
Description: Xunshu offline desktop search test candidate
 Native desktop search with local indexing, optical character recognition
 and semantic retrieval. This build is intended for internal acceptance.
''')
    for path in package.rglob('*'):
        # 包内权限不继承打包者的私有目录 umask。
        mode = path.stat().st_mode
        path.chmod(0o755 if path.is_dir() or mode & stat.S_IXUSR else 0o644)
    # UOS V20 1070 自带旧版 dpkg；gzip 避免依赖较新 dpkg 的 zstd 支持。
    subprocess.run(['dpkg-deb', '--root-owner-group', '-Zgzip', '--build', str(package), str(output)], check=True)
    subprocess.run(['dpkg-deb', '--info', str(output)], check=True)


def publish(dist, results, digest):
    with tempfile.TemporaryDirectory(prefix='.xunshu-publish-', dir=dist) as temporary:
        publication = Path(temporary)
        output = []
        for result in results:
            source = result
            if result.stat().st_dev != publication.stat().st_dev:
                source = publication / result.name
                copy_verified(result, source, digest)
            output.append((source, dist / result.name, digest(source)))
        published = []
        try:
            for source, destination, _ in output:
                # 同卷原子发布；并发调用也不会覆盖已经生成的候选包。
                destination.hardlink_to(source)
                published.append((source, destination))
        except OSError:
            for source, destination in published:
                if destination.exists() and destination.samefile(source):
                    destination.unlink()
            raise
        for _, destination, sha in output:
            print(f'{sha}  {destination.name}', flush=True)


def package(args):
    payload_api = runpy.run_path(str(SCRIPTS / 'make-install-payload.py'))
    digest = payload_api['digest']
    suffix = '.exe' if args.platform == 'windows' else ''
    manager_name = PROGRAMS[-1] + suffix
    package_label = 'Windows-x64' if args.platform == 'windows' else (
        f'UOS-{args.architecture}' if args.linux_kind == 'uos' else f'Linux-{args.architecture}')
    names = ([f'Xunshu-{package_label}-Setup-Test.exe', f'Xunshu-{package_label}-Portable-Test.zip']
             if args.platform == 'windows' else
             [f'Xunshu-{package_label}-Test.deb', f'Xunshu-{package_label}-Portable-Test.tar.gz'])
    args.dist.mkdir(parents=True, exist_ok=True)
    if any(os.path.lexists(args.dist / name) for name in names):
        raise FileExistsError('目标候选包已存在，请使用新的输出目录。')
    with tempfile.TemporaryDirectory(prefix='.xunshu-pack-', dir=args.work_dir or args.dist) as temporary:
        stage = Path(temporary)
        payload = stage / 'payload'
        payload_api['prepare'](args.bin_dir, args.resources, args.font, args.font_license,
                               payload, args.platform, args.version, args.runtime_dll, args.architecture)
        managers = stage / 'manager'
        managers.mkdir()
        manager = managers / manager_name
        copy_verified(args.bin_dir / manager_name, manager, digest)
        payload_api['verify_native_header'](manager, args.platform, architecture=args.architecture)
        info = metadata(args, stage, payload, digest)
        first, second = (stage / name for name in names)
        if args.platform == 'windows':
            windows_pack(args, stage, manager, info, first, digest)
        else:
            linux_pack(args, stage, payload, manager, first)
        portable_pack(stage, payload, manager, second)
        publish(args.dist, (first, second), digest)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('bin-dir', 'resources', 'font', 'font-license', 'dist'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--platform', choices=('windows', 'linux'), required=True)
    parser.add_argument('--source-commit', required=True, help='原生编译所用源码的完整 Git SHA')
    parser.add_argument('--inno-compiler', type=Path, help='Windows 打包时使用 Inno Setup 6.7+ 的 ISCC.exe')
    parser.add_argument('--runtime-dll', type=Path, action='append', default=[])
    parser.add_argument('--runtime-notice', type=Path, action='append', default=[])
    parser.add_argument('--architecture', choices=('amd64', 'arm64'), default='amd64')
    parser.add_argument('--linux-kind', choices=('generic', 'uos'), default='generic')
    parser.add_argument('--max-glibc', help='已核验目标系统提供的 GLIBC 最高版本')
    parser.add_argument('--work-dir', type=Path, help='大文件暂存父目录；WSL 下为 DEB 打包指定 Linux 文件系统')
    args = parser.parse_args()
    if not re.fullmatch(r'[a-f0-9]{40}', args.source_commit):
        parser.error('--source-commit 必须是完整的小写 Git SHA')
    args.version = tomllib.loads((WORKSPACE / 'Cargo.toml').read_text(encoding='utf-8'))['workspace']['package']['version']
    args.dist = args.dist.resolve()
    if args.platform == 'windows':
        valid = args.inno_compiler and args.runtime_notice and WINDOWS_CRT <= {path.name.lower() for path in args.runtime_dll}
        if os.name != 'nt' or not valid or args.architecture != 'amd64' or args.linux_kind != 'generic' or args.max_glibc:
            parser.error('Windows 打包需要 Windows、Inno 编译器、ONNX 所需 x64 CRT DLL 和许可证')
    elif sys.platform != 'linux' or args.inno_compiler or args.runtime_dll:
        parser.error('Linux 打包需要 Linux 及其原生打包工具')
    elif args.linux_kind == 'uos' and args.max_glibc != '2.28':
        parser.error('UOS V20 候选包须以 GLIBC 2.28 作为审核上限')
    package(args)


if __name__ == '__main__':
    main()
