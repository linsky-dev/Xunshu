#requires -Version 5.1
<#
.SYNOPSIS
Builds local Rust CLI/UI binaries without installing or downloading tools.
.DESCRIPTION
Run from any directory. Creates a new directory below USERPROFILE\Xunshu-builds
(or -OutputRoot) containing the five executable files and README.txt.
This is a source-build output, not an installer or a signed release.
#>
[CmdletBinding()]
param(
    [string]$OutputRoot = '',
    [string]$Target = '',
    [switch]$Help
)
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false

function Assert-LocalDirectoryPath([string]$Path) {
    if ($Path -notmatch '^[A-Za-z]:[\\/]' -or $Path -match '(^|[\\/])\.\.?([\\/]|$)') {
        throw "Expected an explicit local drive-letter directory: $Path"
    }
    $full = [System.IO.Path]::GetFullPath($Path)
    $root = [System.IO.Path]::GetPathRoot($full)
    $drive = [System.IO.DriveInfo]::new($root)
    if ($drive.DriveType -notin @([System.IO.DriveType]::Fixed,
        [System.IO.DriveType]::Removable, [System.IO.DriveType]::Ram)) {
        throw "Network, unavailable or non-writable drive type refused: $root"
    }
    $current = $full
    if ($current.Length -gt $root.Length) { $current = $current.TrimEnd([char[]]'\/') }
    while ($true) {
        $item = $null
        try {
            $item = Get-Item -LiteralPath $current -Force -ErrorAction Stop
        } catch [System.Management.Automation.ItemNotFoundException] {
            $item = $null
        }
        if ($null -ne $item) {
            if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Reparse-point directory refused: $current"
            }
            if (-not $item.PSIsContainer) { throw "Not a directory: $current" }
        }
        if ($current.TrimEnd([char[]]'\/') -eq $root.TrimEnd([char[]]'\/')) { break }
        $current = [System.IO.Path]::GetDirectoryName($current.TrimEnd([char[]]'\/'))
        if ([string]::IsNullOrEmpty($current)) { throw "Invalid parent traversal: $full" }
    }
    return $full
}

function Assert-PeX64([string]$Path) {
    $file = [System.IO.File]::OpenRead($Path)
    $reader = [System.IO.BinaryReader]::new($file)
    try {
        if ($reader.ReadUInt16() -ne 0x5A4D) { throw "Missing DOS header: $Path" }
        [void]$file.Seek(60, [System.IO.SeekOrigin]::Begin)
        $offset = $reader.ReadInt32()
        if ($offset -lt 64 -or $offset -gt ($file.Length - 6)) { throw "Invalid PE offset: $Path" }
        [void]$file.Seek($offset, [System.IO.SeekOrigin]::Begin)
        if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne 0x8664) {
            throw "Expected a Windows x64 PE binary: $Path"
        }
    } finally {
        $reader.Dispose()
    }
}

if ($Help) {
    Write-Output 'Usage: build-windows.ps1 [-OutputRoot C:\Users\you\Xunshu-builds] [-Target x86_64-pc-windows-msvc|x86_64-pc-windows-gnu]'
    Write-Output 'Requires an installed Rust/linker toolchain and cached Cargo.lock dependencies. No download, elevation, installation or deletion is performed.'
    exit 0
}

$workspace = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$previousLocation = Get-Location
$previousOffline = [Environment]::GetEnvironmentVariable('CARGO_NET_OFFLINE', 'Process')
$packageDirectory = ''
$exitCode = 1
try {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        throw 'Use build-linux.sh on Linux. This script builds on Windows.'
    }
    $workspace = Assert-LocalDirectoryPath $workspace
    if (-not (Test-Path -LiteralPath (Join-Path $workspace 'Cargo.lock') -PathType Leaf)) {
        throw 'Cargo.lock is missing from the Rust workspace.'
    }
    if ([string]::IsNullOrEmpty($OutputRoot)) {
        if ([string]::IsNullOrEmpty($env:USERPROFILE)) { throw 'USERPROFILE is unavailable; pass -OutputRoot.' }
        $OutputRoot = Join-Path $env:USERPROFILE 'Xunshu-builds'
    }
    $OutputRoot = Assert-LocalDirectoryPath $OutputRoot
    $cargo = (Get-Command cargo -CommandType Application -ErrorAction Stop).Source
    $rustc = (Get-Command rustc -CommandType Application -ErrorAction Stop).Source
    [Environment]::SetEnvironmentVariable('CARGO_NET_OFFLINE', 'true', 'Process')
    Set-Location -LiteralPath $workspace
    $rustDetails = @(& $rustc -vV)
    if ($LASTEXITCODE -ne 0) { throw "rustc failed with exit code $LASTEXITCODE." }
    $rustHost = @($rustDetails | Where-Object { $_ -like 'host: *' })
    if ($rustHost.Count -ne 1) { throw 'Cannot determine the installed Rust host.' }
    $rustHost = $rustHost[0].Substring(6).Trim()
    if ([string]::IsNullOrEmpty($Target)) { $Target = $rustHost }
    if ($Target -notin @('x86_64-pc-windows-msvc', 'x86_64-pc-windows-gnu')) {
        throw "Only Windows x64 targets are supported by this script: $Target"
    }
    $targetDirectory = Join-Path $workspace 'target'
    $buildArguments = @('build', '--locked', '--release', '-p', 'xs-cli', '-p', 'xs-ui', '-p', 'xs-content', '-p', 'xs-inference',
        '--offline', '--target', $Target, '--target-dir', $targetDirectory)
    Write-Host "Building $Target from $workspace (locked dependencies, offline)."
    & $cargo @buildArguments
    if ($LASTEXITCODE -ne 0) { throw "Cargo build failed with exit code $LASTEXITCODE." }

    if (-not (Test-Path -LiteralPath $OutputRoot)) {
        [void][System.IO.Directory]::CreateDirectory($OutputRoot)
    }
    $OutputRoot = Assert-LocalDirectoryPath $OutputRoot
    $packageName = 'package-windows-x64-' + [Guid]::NewGuid().ToString('N')
    $packageDirectory = Join-Path $OutputRoot $packageName
    [void](New-Item -ItemType Directory -Path $packageDirectory -ErrorAction Stop)
    $releaseDirectory = Join-Path (Join-Path $targetDirectory $Target) 'release'
    foreach ($name in @('xunshu.exe', 'xs-ui.exe', 'xs-content-worker.exe', 'xs-model-worker.exe', 'xs-install.exe')) {
        $source = Join-Path $releaseDirectory $name
        $item = Get-Item -LiteralPath $source -Force -ErrorAction Stop
        if ($item.PSIsContainer -or ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint)) {
            throw "Expected a regular executable: $source"
        }
        $destination = Join-Path $packageDirectory $name
        [System.IO.File]::Copy($source, $destination, $false)
        Assert-PeX64 $destination
    }
    $cliPath = Join-Path $packageDirectory 'xunshu.exe'
    $uiPath = Join-Path $packageDirectory 'xs-ui.exe'
    $workerPath = Join-Path $packageDirectory 'xs-content-worker.exe'
    $modelPath = Join-Path $packageDirectory 'xs-model-worker.exe'
    $installPath = Join-Path $packageDirectory 'xs-install.exe'
    $cliHash = (Get-FileHash -LiteralPath $cliPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $uiHash = (Get-FileHash -LiteralPath $uiPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $workerHash = (Get-FileHash -LiteralPath $workerPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $modelHash = (Get-FileHash -LiteralPath $modelPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $installHash = (Get-FileHash -LiteralPath $installPath -Algorithm SHA256).Hash.ToLowerInvariant()
    $lockHash = (Get-FileHash -LiteralPath (Join-Path $workspace 'Cargo.lock') -Algorithm SHA256).Hash.ToLowerInvariant()
    $buildTime = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ')
    $rustText = $rustDetails -join [Environment]::NewLine
    $readme = @"
Xunshu Rust - locally compiled Windows x64 binaries

This directory is source-build output, not an installer or a signed release.
包含命令行、原生界面、正文抽取、模型推理、安装工具和本说明。
The executables are Rust programs; Python, Node.js, PyQt and WebView are not
application runtime dependencies. No toolchain, model, or user index is bundled.
Native Windows/CRT and graphics dependencies still require target-machine checks.

Built UTC: $buildTime
Rust target: $Target
Cargo.lock SHA-256: $lockHash
Build command:
  cargo build --locked --release -p xs-cli -p xs-ui -p xs-content -p xs-inference --offline --target $Target

$rustText

SHA-256 xunshu.exe: $cliHash
SHA-256 xs-ui.exe:  $uiHash
SHA-256 xs-content-worker.exe: $workerHash
SHA-256 xs-model-worker.exe: $modelHash
SHA-256 xs-install.exe: $installHash
已检查五个程序的 x64 PE 头部。
Building/inspecting a PE file is not Windows 10/11 desktop acceptance.

Start in PowerShell:
  .\xunshu.exe --help
  .\xs-ui.exe
Both accept --data-dir C:\absolute\local\new-data-directory.
Use a separate new directory; do not point this version at the old schema4 store.
Startup does not scan disks. Save explicit roots before choosing a scan.
This build contains filename search, platform-specific acceleration paths,
content search and optional OCR/semantic code. Model files are not bundled here.
Native availability, features requiring administrator permissions and system
integration must be tested on the actual Windows machine before any release.
This binary-only directory is not a signed installer or a verified complete kit.

This script did not install software, download resources, elevate privileges,
change the registry, register services or delete an earlier package.
Missing build dependencies fail explicitly and must be prepared separately.
See source docs/PLATFORM_BUILD.md, docs/FEATURE_PARITY.md and VALIDATION.md.
Final distributable packaging, signatures and complete dependency license
notices are separate release work; this folder is for local source validation.
"@
    $readmePath = Join-Path $packageDirectory 'README.txt'
    $utf8 = [System.Text.UTF8Encoding]::new($false)
    $stream = [System.IO.File]::Open($readmePath, [System.IO.FileMode]::CreateNew,
        [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
    try {
        $bytes = $utf8.GetBytes($readme)
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush()
    } finally { $stream.Dispose() }
    Write-Host "Source-build package created: $packageDirectory"
    $exitCode = 0
} catch {
    [Console]::Error.WriteLine("BUILD FAILED: " + $_.Exception.Message)
    if ($packageDirectory) {
        [Console]::Error.WriteLine("Incomplete output retained, not a successful package: " + $packageDirectory)
    }
} finally {
    [Environment]::SetEnvironmentVariable('CARGO_NET_OFFLINE', $previousOffline, 'Process')
    Set-Location -LiteralPath $previousLocation.Path
}
exit $exitCode
