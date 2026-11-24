#requires -Version 7.0
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$Ui,
    [Parameter(Mandatory)][string]$Cli,
    [switch]$SkipContent,
    [switch]$CheckAccessibility,
    [switch]$CheckDiagnostics,
    [switch]$CheckWizard,
    [string]$FixtureRoot = ''
)

$ErrorActionPreference = 'Stop'
if (!$IsWindows -or ![Environment]::UserInteractive) {
    throw '请在 Windows 交互桌面运行本验收。'
}
$uiPath = (Resolve-Path -LiteralPath $Ui).Path
$cliPath = (Resolve-Path -LiteralPath $Cli).Path
if ([IO.Path]::GetDirectoryName($uiPath) -ne [IO.Path]::GetDirectoryName($cliPath)) {
    throw 'xs-ui.exe 和 xunshu.exe 必须位于同一构建目录。'
}
if ($CheckAccessibility -or $CheckDiagnostics -or $CheckWizard) {
    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
}

if (!('XunshuNativeFixture' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class XunshuNativeFixture {
    private delegate bool EnumProc(IntPtr window, IntPtr data);
    [DllImport("user32.dll")] private static extern bool EnumWindows(EnumProc callback, IntPtr data);
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] private static extern int GetWindowText(IntPtr window, StringBuilder text, int size);
    [DllImport("user32.dll", CharSet=CharSet.Unicode)] private static extern int GetClassName(IntPtr window, StringBuilder text, int size);
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr window, uint message, IntPtr wparam, IntPtr lparam);
    public static IntPtr Find(int process, string title, string className) {
        IntPtr match = IntPtr.Zero;
        EnumWindows(delegate(IntPtr window, IntPtr data) {
            uint owner;
            GetWindowThreadProcessId(window, out owner);
            if (owner != (uint)process) return true;
            var value = new StringBuilder(256);
            if (!String.IsNullOrEmpty(className)) {
                GetClassName(window, value, value.Capacity);
                if (value.ToString() != className) return true;
            }
            if (!String.IsNullOrEmpty(title)) {
                value.Clear();
                GetWindowText(window, value, value.Capacity);
                if (value.ToString() != title) return true;
            }
            match = window;
            return false;
        }, IntPtr.Zero);
        return match;
    }
}
'@
}

function Start-UiFixture([string[]]$Arguments, [string]$TestHome = '') {
    $start = [Diagnostics.ProcessStartInfo]::new($uiPath)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    if ($TestHome) { $start.Environment['USERPROFILE'] = $TestHome }
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    [Diagnostics.Process]::Start($start)
}

function Wait-Fixture([scriptblock]$Check, [string]$Description) {
    $deadline = [Diagnostics.Stopwatch]::StartNew()
    do {
        $value = & $Check
        if ($value) { return $value }
        Start-Sleep -Milliseconds 120
    } while ($deadline.Elapsed.TotalSeconds -lt 18)
    throw "等待超时：$Description"
}

function Find-AccessibleItem([IntPtr]$Window, [string]$Name) {
    $condition = [System.Windows.Automation.PropertyCondition]::new(
        [System.Windows.Automation.AutomationElement]::NameProperty, $Name)
    try {
        $root = [System.Windows.Automation.AutomationElement]::FromHandle($Window)
        if ($root) { $root.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $condition) }
    } catch [System.Windows.Automation.ElementNotAvailableException] { }
}

function Invoke-WizardButton([IntPtr]$Window, [string]$Label) {
    $item = Wait-Fixture { Find-AccessibleItem $Window $Label } "向导按钮 $Label"
    $item.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
}

function Test-Wizard([string]$Fixture) {
    $testHome = Join-Path $Fixture 'wizard-home'
    $docs = Join-Path $testHome 'Documents'
    $data = Join-Path $Fixture 'wizard-profile'
    $settings = Join-Path $data 'settings.json'
    [void][IO.Directory]::CreateDirectory($docs)
    [IO.File]::WriteAllText((Join-Path $docs '估算合成报告.txt'), '只用于向导验收。')
    $watch = [Diagnostics.Stopwatch]::StartNew()
    $process = Start-UiFixture @('--standalone', '--data-dir', $data) $testHome
    try {
        $window = Wait-Fixture {
            $handle = [XunshuNativeFixture]::Find($process.Id, '寻枢 · 本地文件搜索', $null)
            if ($handle -ne [IntPtr]::Zero) { $handle }
        } '新配置向导窗口'
        Invoke-WizardButton $window '使用推荐范围'
        Invoke-WizardButton $window '试扫并估算'
        $estimate = Wait-Fixture {
            try {
                $root = [System.Windows.Automation.AutomationElement]::FromHandle($window)
                $items = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants,
                    [System.Windows.Automation.Condition]::TrueCondition)
                foreach ($item in $items) {
                    if ($item.Current.Name -like '试扫检查 *预计收录 *') { return $item.Current.Name }
                }
            } catch [System.Windows.Automation.ElementNotAvailableException] { }
        } '明确显示已估算条目与时间'
        if ((Test-Path -LiteralPath $settings) -or
            (Test-Path -LiteralPath (Join-Path $data 'index/CURRENT.json'))) {
            throw '试扫提前保存了设置或发布了索引。'
        }
        Invoke-WizardButton $window '保存并开始扫描'
        $null = Wait-Fixture {
            if (Test-Path -LiteralPath (Join-Path $data 'index/CURRENT.json')) {
                $result = & $cliPath --data-dir $data search '估算合成报告' | ConvertFrom-Json
                $LASTEXITCODE -eq 0 -and $result.total -eq 1
            }
        } '确认后的首个合成命中'
        $saved = Get-Content -LiteralPath $settings -Raw -Encoding UTF8 | ConvertFrom-Json
        if ($saved.mode -ne 'standard' -or $saved.content_enabled -or
            @($saved.roots).Count -ne 1 -or $saved.roots[0].path -ne $docs) {
            throw '推荐设置偏离了显式确认的标准范围。'
        }
        [pscustomobject]@{ estimate = $estimate; synthetic_first_hit_ms = $watch.ElapsedMilliseconds }
    } finally {
        if (!$process.HasExited) { $process.Kill(); $process.WaitForExit() }
        $process.Dispose()
    }
}

function Find-Renderer([int]$HostId, [string]$Title) {
    $children = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$HostId" |
        Where-Object { $_.ExecutablePath -eq $uiPath -and $_.CommandLine -like '*--desktop-render*' })
    foreach ($child in $children) {
        $window = [XunshuNativeFixture]::Find($child.ProcessId, $Title, $null)
        if ($window -ne [IntPtr]::Zero) { return [pscustomobject]@{ Id = [int]$child.ProcessId; Window = $window } }
    }
}

function Invoke-Activation([string[]]$Arguments) {
    $activation = Start-UiFixture $Arguments
    try {
        if (!$activation.WaitForExit(7000) -or $activation.ExitCode -ne 0) { throw '同配置二次启动没有退出并移交请求。' }
    } finally {
        if (!$activation.HasExited) { $activation.Kill() }
        $activation.Dispose()
    }
}

function Invoke-UiDiagnostic([string[]]$Arguments, [string]$ExpectedTitle = '') {
    $start = [Diagnostics.ProcessStartInfo]::new($uiPath)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardError = $true
    foreach ($argument in $Arguments) { $start.ArgumentList.Add($argument) }
    $process = [Diagnostics.Process]::Start($start)
    try {
        $stderr = $process.StandardError.ReadToEndAsync()
        if ($ExpectedTitle) {
            $null = Wait-Fixture {
                $window = [XunshuNativeFixture]::Find($process.Id, $ExpectedTitle, $null)
                if ($window -ne [IntPtr]::Zero) { $window }
            } $ExpectedTitle
        }
        if (!$process.WaitForExit(20000)) { throw '原生界面诊断未退出。' }
        [pscustomobject]@{ Exit = $process.ExitCode; ErrorText = $stderr.GetAwaiter().GetResult() }
    } finally {
        if (!$process.HasExited) { $process.Kill() }
        $process.Dispose()
    }
}

if (!$FixtureRoot) { $FixtureRoot = Join-Path $PSScriptRoot '../../test-tmp' }
$fixtureParent = Get-Item -LiteralPath (Resolve-Path -LiteralPath $FixtureRoot).Path -Force
if (!$fixtureParent.PSIsContainer -or $fixtureParent.FullName -notmatch '^[A-Za-z]:\\' -or
    [IO.DriveInfo]::new($fixtureParent.PSDrive.Root).DriveType -ne [IO.DriveType]::Fixed) {
    throw '验收样本父目录必须在本机固定磁盘上。'
}
for ($current = $fixtureParent; $null -ne $current; $current = $current.Parent) {
    if ($current.Attributes -band [IO.FileAttributes]::ReparsePoint) {
        throw '验收样本路径包含重解析点。'
    }
}
$fixture = Join-Path $fixtureParent.FullName ('ui-host-' + [Guid]::NewGuid().ToString('N'))
$documents = Join-Path $fixture 'documents'
$profile = Join-Path $fixture 'profile'
$null = New-Item -ItemType Directory -Path $documents, $profile
[IO.File]::WriteAllText((Join-Path $documents '合成报告.txt'), '只用于 Windows 原生界面回归。原生正文验收。')
$null = & $cliPath --data-dir $profile init --root $documents
if ($LASTEXITCODE -ne 0) { throw '无法建立合成设置。' }
$null = & $cliPath --data-dir $profile scan
if ($LASTEXITCODE -ne 0) { throw '无法建立合成文件名索引。' }

$hostProcess = Start-UiFixture @('--data-dir', $profile, '--quick', '--query', '合成')
try {
    $hostWindow = Wait-Fixture {
        $window = [XunshuNativeFixture]::Find($hostProcess.Id, $null, 'Xunshu.DesktopHost')
        if ($window -ne [IntPtr]::Zero) { $window }
    } 'Windows 托盘宿主'
    $quick = Wait-Fixture { Find-Renderer $hostProcess.Id '寻枢 · 快捷搜索' } '快捷搜索原生窗口'
    Invoke-Activation @('--data-dir', $profile, '--query', '合成报告')
    $main = Wait-Fixture { Find-Renderer $hostProcess.Id '寻枢 · 本地文件搜索' } '现有进程切换到主窗口'
    if ($main.Id -ne $quick.Id) { throw '切换视图启动了重复渲染进程。' }
    $accessibility = $null
    if ($CheckAccessibility) {
        $resultLabel = '合成报告.txt，' + (Join-Path $documents '合成报告.txt')
        foreach ($name in @('本地文件搜索', $resultLabel)) {
            $null = Wait-Fixture { Find-AccessibleItem $main.Window $name } "可访问性节点 $name"
        }
        $accessibility = $true
    }
    $diagnosticsPanel = $null
    $diagnosticsCliExport = $null
    if ($CheckDiagnostics) {
        $openDiagnostics = Wait-Fixture { Find-AccessibleItem $main.Window '索引状态' } '索引状态入口'
        $openDiagnostics.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke()
        $null = Wait-Fixture { Find-AccessibleItem $main.Window '索引　可读' } '索引状态面板显示诊断结果'
        $null = Wait-Fixture { Find-AccessibleItem $main.Window '导出脱敏报告' } '诊断面板导出入口'
        $diagnosticsPanel = $true

        # 导出调用由 CLI 单独取证，UIA 只验证面板及其入口。
        $reportPath = Join-Path $fixture 'diagnostics-export.json'
        $null = & $cliPath --data-dir $profile doctor --json --export $reportPath
        if ($LASTEXITCODE -ne 0) { throw '无法导出本机诊断报告。' }
        $raw = [IO.File]::ReadAllText($reportPath)
        $report = $raw | ConvertFrom-Json
        if ($report.schema -ne 1 -or $report.network -ne $false -or $report.index.state -ne 'readable' -or
            $raw.Contains('合成报告') -or $raw.Contains((Split-Path -Leaf $fixture))) {
            throw '诊断报告状态或脱敏校验失败。'
        }
        $diagnosticsCliExport = $true
    }
    # 驱动已识别的热键消息，实体键盘的低级钩子仍需实机人工输入。
    if (![XunshuNativeFixture]::PostMessage($hostWindow, 0x8002, [IntPtr]::Zero, [IntPtr]::Zero)) { throw '无法发送快捷框切换消息。' }
    $quick = Wait-Fixture { Find-Renderer $hostProcess.Id '寻枢 · 快捷搜索' } '热键分派切回快捷框'
    if (![XunshuNativeFixture]::PostMessage($hostWindow, 0x8002, [IntPtr]::Zero, [IntPtr]::Zero)) { throw '无法发送快捷框隐藏消息。' }
    $null = Wait-Fixture { !(Get-Process -Id $quick.Id -ErrorAction SilentlyContinue) } '隐藏后渲染进程退出'
    $watch = Wait-Fixture {
        Get-CimInstance Win32_Process -Filter "ParentProcessId=$($hostProcess.Id)" |
            Where-Object { $_.ExecutablePath -eq $cliPath -and $_.CommandLine -like '* watch' } |
            Select-Object -First 1
    } '隐藏时后台监听仍在运行'
    [IO.File]::WriteAllText((Join-Path $documents '隐藏新增合成条目.txt'), '本地监听回归。')
    $null = Wait-Fixture {
        $result = & $cliPath --data-dir $profile search '隐藏新增合成条目' | ConvertFrom-Json
        $LASTEXITCODE -eq 0 -and $result.total -eq 1
    } '隐藏期间新建文件已发布'
    $privateBytes = (Get-Process -Id $hostProcess.Id, $watch.ProcessId | Measure-Object -Sum PrivateMemorySize64).Sum
    Invoke-Activation @('--data-dir', $profile, '--quick')
    $reopened = Wait-Fixture { Find-Renderer $hostProcess.Id '寻枢 · 快捷搜索' } '关闭后重新唤起'
    if ($reopened.Id -eq $quick.Id) { throw '重新唤起没有创建独立渲染进程。' }
    if (![XunshuNativeFixture]::PostMessage($hostWindow, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero)) { throw '无法退出托盘宿主。' }
    if (!$hostProcess.WaitForExit(7000) -or $hostProcess.ExitCode -ne 0) { throw '托盘宿主退出失败。' }
    $null = Wait-Fixture {
        !(Get-Process -Id $reopened.Id, $watch.ProcessId -ErrorAction SilentlyContinue)
    } '宿主退出回收渲染和监听进程'
    $contentPreview = $null
    $cjkFont = $null
    if (!$SkipContent) {
        $null = & $cliPath --data-dir $profile content-configure --root $documents
        if ($LASTEXITCODE -ne 0) { throw '无法为合成样本授权正文范围。' }
        $null = & $cliPath --data-dir $profile content-rebuild
        if ($LASTEXITCODE -ne 0) { throw '无法建立合成正文索引。' }
        $content = Invoke-UiDiagnostic @('--data-dir', $profile, '--content', '--smoke-test', '--query', '原生正文验收') '寻枢 · 本地文件搜索'
        if ($content.Exit -ne 0 -or $content.ErrorText -notmatch 'XS_UI_CONTENT_SMOKE_OK cjk_font=true') {
            throw "正文预览或中文字体校验失败：$($content.ErrorText)"
        }
        $badFont = Join-Path $fixture '损坏字体.otf'
        [IO.File]::WriteAllText($badFont, '合成损坏字体')
        $invalid = Invoke-UiDiagnostic @('--data-dir', $profile, '--smoke-test', '--font', $badFont)
        if ($invalid.Exit -eq 0 -or $invalid.ErrorText -notmatch '无法识别字体格式') { throw '损坏字体没有返回明确的中文错误。' }
        $contentPreview = $true
        $cjkFont = $true
    }
    $wizard = if ($CheckWizard) { Test-Wizard $fixture } else { $null }
    [pscustomobject]@{ result = '通过'; fixture = $fixture; hidden_private_bytes = $privateBytes; native_window = $true; single_instance = $true; hotkey_dispatch = $true; physical_ctrl = $false; hidden_watch = $true; exit_cleanup = $true; content_preview = $contentPreview; cjk_font = $cjkFont; windows_uia = $accessibility; diagnostics_panel_uia = $diagnosticsPanel; diagnostics_cli_export = $diagnosticsCliExport; synthetic_wizard = $wizard } | ConvertTo-Json
} finally {
    if (!$hostProcess.HasExited) { $hostProcess.Kill(); $hostProcess.WaitForExit() }
    $hostProcess.Dispose()
}
