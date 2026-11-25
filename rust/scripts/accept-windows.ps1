# Windows x64 实机验收入口，测试数据与报告输出到用户指定目录。
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$BinDir,
    [Parameter(Mandatory = $true)][string]$OutputDir,
    [string]$Installer = '',
    [string]$Payload = '',
    [string]$ModelFixtures = '',
    [switch]$WithModels,
    [switch]$KeepWork
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
    throw '此入口需要在 Windows 10/11 x64 上运行。'
}
if ([string]::IsNullOrWhiteSpace($Installer) -ne [string]::IsNullOrWhiteSpace($Payload)) {
    throw 'Installer 和 Payload 必须同时提供。'
}
$script = Join-Path $PSScriptRoot 'acceptance.py'
$arguments = @($script, '--bin-dir', $BinDir, '--output-dir', $OutputDir)
if ($Installer) { $arguments += @('--installer', $Installer, '--payload', $Payload) }
if ($WithModels) { $arguments += '--with-models' }
if ($ModelFixtures) { $arguments += @('--model-fixtures', $ModelFixtures) }
if ($KeepWork) { $arguments += '--keep-work' }
& python @arguments
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
