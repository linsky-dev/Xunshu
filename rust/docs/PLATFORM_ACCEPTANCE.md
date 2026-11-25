# Windows 与统信 UOS 实机验收

## 适用范围

此工具在 Windows 10/11 x64 或统信 UOS x86_64 本机运行，使用给定的 Rust CLI 和桌面程序二进制。它创建随机命名的隔离目录，只在其中放置合成文件。工具不会扫描用户目录，也不会下载模型或访问网络。报告分别写入 `acceptance.json` 和 `验收报告.md`。

接受测试的系统版本和架构来自实际运行主机。UOS 脚本检查 `/etc/os-release` 与 `uname -m`，它的结果不能替代指定 UOS V20 镜像上的测试。

## Windows

在 PowerShell 中运行：

```powershell
pwsh -File scripts/accept-windows.ps1 `
  -BinDir C:\Xunshu\bin `
  -OutputDir C:\Xunshu\acceptance
```

可选传入配套的安装器和已构建载荷，运行安装、升级、回滚、修复、卸载与用户数据保留合同测试：

```powershell
pwsh -File scripts/accept-windows.ps1 `
  -BinDir C:\Xunshu\bin `
  -OutputDir C:\Xunshu\acceptance `
  -Installer C:\Xunshu\bin\xs-install.exe `
  -Payload C:\Xunshu\payload
```

若本机已安装模型，可添加 `-WithModels` 执行 OCR 与中文语义专项。默认从 `Payload\self-test` 读取打包时生成的合成扫描件，也可用 `-ModelFixtures` 指定合成样本目录。

## 统信 UOS

在目标 UOS 终端运行：

```sh
bash scripts/accept-uos.sh /opt/xunshu/bin /opt/xunshu/acceptance
```

可选传入本机 `xs-install` 和载荷目录：

```sh
bash scripts/accept-uos.sh /opt/xunshu/bin /opt/xunshu/acceptance \
  /opt/xunshu/bin/xs-install /opt/xunshu/payload
```

若本机已安装模型，在安装参数之后添加 `--with-models`。默认读取载荷目录下打包的 `self-test` 合成扫描件。

## 结果解释

`acceptance.json` 每项包含 `status`、结果说明及可用的证据数值。整体为 `FAIL` 时脚本以非零退出。含有尚未自动验证项目时整体为 `PASS_WITH_SKIPS`。未安装 OCR 与语义模型的环境会如实记录为跳过。未提供安装载荷时，安装生命周期和数据保留也会跳过。搜索耗时是单次样本测量值，不构成性能门槛。

脚本用报告目录内本轮随机目录作为临时测试根目录，成功后默认清理。使用 `python scripts/acceptance.py --help` 可查看通用入口参数。需要保留合成目录以调查失败时，通用入口支持 `--keep-work`。报告目录和其父目录应由验收人员专用，避免与其他任务共用。

Windows 调用遵循本机当前 PowerShell 执行策略。UOS 1070 的 Python 3.7.3 可运行本脚本所用语法与标准库接口。验收脚本及复用的合同脚本只用于测试，不属于产品运行时依赖。启用 `--with-models` 的 OCR 及语义测试还需要 Python Pillow 来合成混合 PDF 样本。

Linux 使用系统统计接口记录 CPU 时间和内存。Windows 已安装 `psutil` 时会采集扫描进程的 CPU 时间和工作集，未安装或未采到样本会显示跳过。资源读数仅代表该轮合成验收。

请逐项完成人工清单中的桌面窗口、中文输入法、多屏与高 DPI、快捷键、托盘、文件拖拽、真实合成扫描件 OCR、变更监听、索引恢复和范围撤销检查。只有安装经核验模型的本机才执行语义搜索。人工检查结果应在验收报告副本中逐项勾选并注明目标系统版本。
