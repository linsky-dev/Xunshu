param([string] $Cargo = 'cargo')

$ErrorActionPreference = 'Stop'
$workspace = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\..\..')).Path
$manifest = Join-Path $workspace 'rust\Cargo.toml'
$fixtureDir = Join-Path $workspace '.recovery-tools\windows\platform-fixtures'
New-Item -ItemType Directory -Force -Path $fixtureDir | Out-Null
$nonce = [guid]::NewGuid().ToString('N')
$image = Join-Path $fixtureDir "native-ntfs-$nonce.vhdx"
$originalLetters = @(Get-CimInstance Win32_LogicalDisk | ForEach-Object { $_.DeviceID.TrimEnd(':') })
$attached = $false
try {
    & $Cargo build --manifest-path $manifest --locked -p xs-platform --example verify_native_vhd
    if ($LASTEXITCODE -ne 0) { throw "构建 NTFS 回归程序失败。" }

    New-VHD -Path $image -SizeBytes 160MB -Dynamic | Out-Null
    Mount-VHD -Path $image -Passthru | Out-Null
    $attached = $true
    $info = Get-VHD -Path $image
    if (!$info.Attached -or [int]$info.DiskNumber -lt 1) { throw "无法确定新建虚拟盘。" }
    $disk = Get-Disk -Number $info.DiskNumber
    if ($disk.IsBoot -or $disk.IsSystem -or $disk.Size -ne 160MB -or $disk.PartitionStyle -ne 'RAW' -or $disk.BusType -notmatch 'Virtual') {
        throw "磁盘校验失败。仅允许初始化本次创建的文件型虚拟盘。"
    }
    Write-Output "合成虚拟磁盘 $($disk.Number)，类型 $($disk.BusType)，容量 $($disk.Size) 字节"
    Initialize-Disk -Number $disk.Number -PartitionStyle GPT | Out-Null
    $partition = New-Partition -DiskNumber $disk.Number -UseMaximumSize -AssignDriveLetter
    $letter = [string]$partition.DriveLetter
    if ($letter -notmatch '^[E-Z]$' -or $originalLetters -contains $letter) { throw "虚拟盘盘符与现有驱动器冲突。" }
    $confirmed = Get-Partition -DriveLetter $letter
    if ($confirmed.DiskNumber -ne $disk.Number) { throw "盘符不属于本次创建的虚拟盘。" }
    Format-Volume -Partition $partition -FileSystem NTFS -NewFileSystemLabel XunshuSynth -Confirm:$false | Out-Null
    $device = $letter + ':'
    & fsutil.exe usn createjournal m=1048576 a=1048576 $device
    if ($LASTEXITCODE -ne 0) { throw "合成虚拟盘创建 USN 日志失败。" }
    $root = $device + '\'
    [IO.File]::WriteAllText((Join-Path $root 'xunshu-synthetic-marker'), $nonce)
    & $Cargo run --manifest-path $manifest --locked -p xs-platform --example verify_native_vhd -- $root $nonce
    if ($LASTEXITCODE -ne 0) { throw "原生 NTFS 回归失败。" }
} finally {
    if ($attached) { Dismount-VHD -Path $image }
    if (Test-Path -LiteralPath $image -PathType Leaf) { Remove-Item -LiteralPath $image }
}
