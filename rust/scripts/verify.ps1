#requires -Version 5.1
<#
.SYNOPSIS
Runs Rust workspace tests and a synthetic CLI flow on Windows x64.
.DESCRIPTION
Only this run's new random temporary directory is eligible for cleanup.
No old Python test suite, user-root scan, full-disk benchmark or GUI acceptance
is performed. Requires installed tools and cached locked Cargo dependencies.
#>
[CmdletBinding()]
param(
    [string]$TempRoot = '',
    [string]$EvidenceRoot = '',
    [string]$BinDir = '',
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
function Write-Log([string]$Text) {
    Write-Host $Text
    [System.IO.File]::AppendAllText($script:logPath, $Text + [Environment]::NewLine, $script:utf8)
}
function Invoke-LoggedNative([string]$Binary, [string[]]$NativeArguments) {
    Write-Log ('$ ' + $Binary + ' ' + ($NativeArguments -join ' '))
    $previousAction = $ErrorActionPreference
    try {
        # Windows PowerShell 5.1 wraps redirected native stderr in ErrorRecord.
        # Check the native exit code explicitly, without treating build progress
        # on stderr as a failed PowerShell command.
        $ErrorActionPreference = 'Continue'
        & $Binary @NativeArguments 2>&1 | ForEach-Object { Write-Log ([string]$_) }
        $nativeExit = $LASTEXITCODE
    } finally { $ErrorActionPreference = $previousAction }
    if ($nativeExit -ne 0) { throw "$Binary failed with exit code $nativeExit." }
}
function Invoke-CliJson([string]$Name, [string[]]$NativeArguments) {
    Write-Log ('$ xunshu ' + ($NativeArguments -join ' '))
    $lines = @(& $script:cli @NativeArguments)
    $nativeExit = $LASTEXITCODE
    if ($nativeExit -ne 0) { throw "CLI $Name failed with exit code $nativeExit." }
    $json = $lines -join [Environment]::NewLine
    [System.IO.File]::WriteAllText((Join-Path $script:evidenceDirectory $Name), $json, $script:utf8)
    return ($json | ConvertFrom-Json -ErrorAction Stop)
}
function Require([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw "CLI CONTRACT FAILED: $Message" }
}
function Remove-OwnedTemporaryDirectory {
    $item = Get-Item -LiteralPath $script:runTemporary -Force -ErrorAction Stop
    if (-not $item.PSIsContainer -or
        ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
        [System.IO.Path]::GetDirectoryName($item.FullName) -ne $script:TempRoot.TrimEnd([char[]]'\/')) {
        throw 'Temporary directory identity changed; cleanup refused.'
    }
    $marker = Join-Path $script:runTemporary '.xunshu-verification-owner'
    $markerItem = Get-Item -LiteralPath $marker -Force -ErrorAction Stop
    if (($markerItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0 -or
        [System.IO.File]::ReadAllText($marker) -ne $script:ownerToken) {
        throw 'Temporary ownership marker changed; cleanup refused.'
    }
    # Inspect without recursive traversal through reparse points. If an aborted
    # test left a link/junction, keep the owned tree for review instead of risking
    # recursive deletion through the link on older PowerShell/.NET versions.
    $pending = [System.Collections.Generic.Stack[string]]::new()
    $pending.Push($script:runTemporary)
    while ($pending.Count -gt 0) {
        $directory = $pending.Pop()
        foreach ($entry in [System.IO.Directory]::GetFileSystemEntries($directory)) {
            $attributes = [System.IO.File]::GetAttributes($entry)
            if (($attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Temporary tree contains a reparse point; retained for review: $entry"
            }
            if (($attributes -band [System.IO.FileAttributes]::Directory) -ne 0) {
                $pending.Push($entry)
            }
        }
    }
    [System.IO.Directory]::Delete($script:runTemporary, $true)
}

if ($Help) {
    Write-Output 'Usage: verify.ps1 [-TempRoot D:\local\synthetic-tests] [-EvidenceRoot D:\local\evidence] [-BinDir D:\local\built-binaries]'
    Write-Output 'Defaults are rust\test-tmp and rust\evidence. Each run uses a new random directory. No downloads, elevation, user-root scans or old Python tests.'
    exit 0
}
$workspace = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$previousLocation = Get-Location
$savedEnvironment = @{}
foreach ($key in @('TMPDIR', 'TEMP', 'TMP', 'CARGO_NET_OFFLINE')) {
    $savedEnvironment[$key] = [Environment]::GetEnvironmentVariable($key, 'Process')
}
$previousConsoleEncoding = [Console]::OutputEncoding
$previousOutputEncoding = $OutputEncoding
$script:utf8 = [System.Text.UTF8Encoding]::new($false)
$script:runTemporary = ''
$script:ownerToken = ''
$script:logPath = ''
$script:evidenceDirectory = ''
$script:cli = ''
$exitCode = 1
try {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) {
        throw 'Use verify.sh on Linux; this script runs native Windows tests.'
    }
    $workspace = Assert-LocalDirectoryPath $workspace
    if ([string]::IsNullOrEmpty($TempRoot)) { $TempRoot = Join-Path $workspace 'test-tmp' }
    if ([string]::IsNullOrEmpty($EvidenceRoot)) { $EvidenceRoot = Join-Path $workspace 'evidence' }
    $TempRoot = Assert-LocalDirectoryPath $TempRoot
    $EvidenceRoot = Assert-LocalDirectoryPath $EvidenceRoot
    [void][System.IO.Directory]::CreateDirectory($TempRoot)
    [void][System.IO.Directory]::CreateDirectory($EvidenceRoot)
    $TempRoot = Assert-LocalDirectoryPath $TempRoot
    $EvidenceRoot = Assert-LocalDirectoryPath $EvidenceRoot
    $script:evidenceDirectory = Join-Path $EvidenceRoot ('verify-windows-' + [Guid]::NewGuid().ToString('N'))
    [void](New-Item -ItemType Directory -Path $script:evidenceDirectory -ErrorAction Stop)
    $script:logPath = Join-Path $script:evidenceDirectory 'verify.log'
    $script:ownerToken = [Guid]::NewGuid().ToString('N')
    $script:runTemporary = Join-Path $TempRoot ('xunshu-verify-' + $script:ownerToken)
    [void](New-Item -ItemType Directory -Path $script:runTemporary -ErrorAction Stop)
    [System.IO.File]::WriteAllText(
        (Join-Path $script:runTemporary '.xunshu-verification-owner'), $script:ownerToken, $script:utf8)
    foreach ($key in @('TMPDIR', 'TEMP', 'TMP')) {
        [Environment]::SetEnvironmentVariable($key, $script:runTemporary, 'Process')
    }
    [Environment]::SetEnvironmentVariable('CARGO_NET_OFFLINE', 'true', 'Process')
    [Console]::OutputEncoding = $script:utf8
    $OutputEncoding = $script:utf8
    Set-Location -LiteralPath $workspace
    if ([string]::IsNullOrEmpty($BinDir)) {
        $cargo = (Get-Command cargo -CommandType Application -ErrorAction Stop).Source
        $rustc = (Get-Command rustc -CommandType Application -ErrorAction Stop).Source
        $details = @(& $rustc -vV)
        if ($LASTEXITCODE -ne 0) { throw "rustc failed with exit code $LASTEXITCODE." }
        Write-Log ($details -join [Environment]::NewLine)
        $hostLines = @($details | Where-Object { $_ -like 'host: *' })
        if ($hostLines.Count -ne 1) { throw 'Cannot determine Rust host target.' }
        $target = $hostLines[0].Substring(6).Trim()
        if ($target -notin @('x86_64-pc-windows-msvc', 'x86_64-pc-windows-gnu')) {
            throw "Native Windows x64 Rust host required; found $target."
        }
        $targetDirectory = Join-Path $workspace 'target'
        Invoke-LoggedNative $cargo @('test', '--locked', '--offline', '--workspace',
            '--target', $target, '--target-dir', $targetDirectory)
        Invoke-LoggedNative $cargo @('build', '--locked', '--offline', '-p', 'xs-cli', '-p', 'xs-ui',
            '--target', $target, '--target-dir', $targetDirectory)
        $BinDir = Join-Path (Join-Path $targetDirectory $target) 'debug'
    } else {
        Write-Log 'Using supplied local binaries. This invocation checks CLI contracts only.'
        $BinDir = Assert-LocalDirectoryPath $BinDir
    }
    $script:cli = Join-Path $BinDir 'xunshu.exe'
    $ui = Join-Path $BinDir 'xs-ui.exe'
    foreach ($binary in @($script:cli, $ui)) {
        $item = Get-Item -LiteralPath $binary -Force
        if ($item.PSIsContainer -or ($item.Attributes -band [IO.FileAttributes]::ReparsePoint)) {
            throw "Expected a regular local binary: $binary"
        }
        Write-Log ('SHA256 ' + $item.Name + ' ' + (Get-FileHash -LiteralPath $binary -Algorithm SHA256).Hash)
    }
    Invoke-LoggedNative $script:cli @('--help')
    Invoke-LoggedNative $ui @('--help')

    $files = Join-Path $script:runTemporary 'files'
    $data = Join-Path $files 'app-private'
    $catalogData = Join-Path $script:runTemporary 'catalog-data'
    [void][System.IO.Directory]::CreateDirectory((Join-Path $files 'subdir'))
    [void][System.IO.Directory]::CreateDirectory((Join-Path $files '.git'))
    $pdfName = ([string][char]0x9884) + [char]0x7B97 + [char]0x62A5 + [char]0x544A + '.pdf'
    [System.IO.File]::WriteAllText((Join-Path $files 'report.txt'), 'synthetic report', $script:utf8)
    [System.IO.File]::WriteAllText((Join-Path $files 'subdir\notes.txt'), 'synthetic notes', $script:utf8)
    [System.IO.File]::WriteAllText((Join-Path $files $pdfName), 'metadata fixture, not a real PDF', $script:utf8)
    [System.IO.File]::WriteAllText((Join-Path $files 'ignored.bin'), 'excluded type', $script:utf8)
    [System.IO.File]::WriteAllText((Join-Path $files '.git\hidden.txt'), 'excluded smart directory', $script:utf8)
    $init = Invoke-CliJson 'init.json' @('--data-dir', $data, 'init', '--root', $files, '--ext', 'txt,pdf')
    $scan = Invoke-CliJson 'scan.json' @('--data-dir', $data, 'scan')
    # file:|folder: avoids PowerShell 5.1's loss of empty native arguments.
    $all = Invoke-CliJson 'all.json' @('--data-dir', $data, 'search', 'file:|folder:')
    $txt = Invoke-CliJson 'txt.json' @('--data-dir', $data, 'search', 'ext:txt')
    $pdf = Invoke-CliJson 'pdf.json' @('--data-dir', $data, 'search', 'ext:pdf')
    $status = Invoke-CliJson 'status.json' @('--data-dir', $data, 'status')
    $diagnostics = Join-Path $script:runTemporary 'diagnostics.json'
    $doctor = Invoke-CliJson 'doctor.json' @('--data-dir', $data, 'doctor', '--json', '--export', $diagnostics)
    $efu = Join-Path $script:runTemporary 'fixture.efu'
    $export = Invoke-CliJson 'export.json' @('--data-dir', $data, 'export-efu', $efu)
    $import = Invoke-CliJson 'import.json' @('--data-dir', $catalogData, 'import-efu', $efu, '--name', 'fixture')
    $offline = Invoke-CliJson 'offline.json' @('--data-dir', $catalogData, 'search', 'ext:pdf')
    Require ($init.ok -eq $true) 'init'
    Require ($scan.published -and $scan.report.complete -and -not $scan.report.cancelled) 'complete publication'
    Require (@($scan.report.errors).Count -eq 0) 'error-free scan'
    Require ($txt.complete -and $txt.total -eq 2 -and
        (($txt.results.name | Sort-Object) -join '|') -eq 'notes.txt|report.txt') 'exact text results'
    Require ($pdf.complete -and $pdf.total -eq 1 -and $pdf.results[0].name -eq $pdfName) 'Unicode PDF result'
    foreach ($row in $all.results) {
        Require ($row.source -eq 'local') 'local source'
        Require ($row.path -ne $data -and -not $row.path.StartsWith(
            $data.TrimEnd([char[]]'\/') + '\', [StringComparison]::OrdinalIgnoreCase)) 'self exclusion'
        Require ($row.name -notin @('ignored.bin', '.git', 'hidden.txt')) 'scope filtering'
    }
    Require ($status.network -eq $false) 'offline status'
    Require ($doctor.index.state -eq 'readable' -and $doctor.index.coverage_complete) 'diagnostic index status'
    Require ($doctor.settings.configured_roots.accessible -eq 1 -and $doctor.network -eq $false) 'diagnostic scope and offline status'
    $doctorText = [System.IO.File]::ReadAllText($diagnostics)
    Require (!$doctorText.Contains('report.txt') -and !$doctorText.Contains('app-private') -and
        !$doctorText.Contains($script:ownerToken)) 'diagnostic privacy'
    Require ($offline.complete -and $offline.total -eq 1 -and
        $offline.results[0].source -eq 'offline:fixture') 'import grants no local access'
    $previousAction = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $invalidLines = @(& $script:cli --data-dir $data search unknown_field:value 2>&1)
        $invalidExit = $LASTEXITCODE
    } finally { $ErrorActionPreference = $previousAction }
    $invalidText = ($invalidLines | ForEach-Object { [string]$_ }) -join [Environment]::NewLine
    [System.IO.File]::WriteAllText((Join-Path $script:evidenceDirectory 'invalid.stderr.json'),
        $invalidText, $script:utf8)
    $invalid = $invalidText | ConvertFrom-Json -ErrorAction Stop
    Require ($invalidExit -ne 0 -and $invalid.ok -eq $false -and
        $invalid.error.Contains('unknown_field')) 'explicit unsupported-query error'
    Write-Log 'PASS: Synthetic CLI contracts.'
    $exitCode = 0
} catch {
    [Console]::Error.WriteLine('VERIFY FAILED: ' + $_.Exception.Message)
    if ($script:logPath) {
        [System.IO.File]::AppendAllText($script:logPath,
            'VERIFY FAILED: ' + $_.Exception.Message + [Environment]::NewLine, $script:utf8)
    }
} finally {
    foreach ($key in $savedEnvironment.Keys) {
        [Environment]::SetEnvironmentVariable($key, $savedEnvironment[$key], 'Process')
    }
    [Console]::OutputEncoding = $previousConsoleEncoding
    $OutputEncoding = $previousOutputEncoding
    Set-Location -LiteralPath $previousLocation.Path
    if ($script:runTemporary) {
        try { Remove-OwnedTemporaryDirectory }
        catch {
            [Console]::Error.WriteLine('Cleanup refused or failed; owned fixture retained: ' +
                $script:runTemporary + '. ' + $_.Exception.Message)
            $exitCode = 1
        }
    }
    if ($script:logPath) {
        [System.IO.File]::AppendAllText($script:logPath,
            "verification_exit_code=$exitCode" + [Environment]::NewLine, $script:utf8)
        Write-Host ('Evidence retained: ' + $script:evidenceDirectory)
    }
}
exit $exitCode
