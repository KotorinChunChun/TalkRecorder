#Requires -Version 7.0
[CmdletBinding()]
param(
    [string] $Destination = (Join-Path $PSScriptRoot '..\runtime')
)

$ErrorActionPreference = 'Stop'
# FFmpeg.org の Windows builds 案内に掲載されている Gyan essentials。
# 可変の latest URL ではなく、バージョンと公式掲載の SHA-256 を固定する。
$encoderVersion = '9.0.2'
$archiveName = "ffmpeg-$encoderVersion-essentials_build.zip"
$archiveUri = "https://www.gyan.dev/ffmpeg/builds/packages/$archiveName"
$archiveHash = '60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba'
$runtimePath = [IO.Path]::GetFullPath($Destination)
$encoderPath = Join-Path $runtimePath 'ffmpeg'
$downloadsPath = Join-Path $runtimePath 'downloads'
$archivePath = Join-Path $downloadsPath $archiveName
$manifestPath = Join-Path $runtimePath 'encoder-manifest.json'
New-Item -ItemType Directory -Force -Path $runtimePath,$downloadsPath | Out-Null

if (-not (Test-Path -LiteralPath $archivePath) -or
    (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash -ine $archiveHash) {
    $partialPath = "$archivePath.partial"
    Invoke-WebRequest -Uri $archiveUri -OutFile $partialPath -TimeoutSec 600 -MaximumRetryCount 2 -RetryIntervalSec 3
    if ((Get-FileHash -LiteralPath $partialPath -Algorithm SHA256).Hash -ine $archiveHash) {
        throw 'FFmpeg アーカイブの SHA-256 が固定値と一致しません。展開を中止します。'
    }
    Move-Item -LiteralPath $partialPath -Destination $archivePath -Force
}

$stagePath = Join-Path $runtimePath ('.ffmpeg-extract-' + [Guid]::NewGuid().ToString('N'))
$stageFullPath = [IO.Path]::GetFullPath($stagePath)
$stagePrefix = $stageFullPath + [IO.Path]::DirectorySeparatorChar
$archive = [IO.Compression.ZipFile]::OpenRead($archivePath)
try {
    foreach ($entry in $archive.Entries) {
        $entryPath = [IO.Path]::GetFullPath([IO.Path]::Combine($stageFullPath, $entry.FullName))
        if (-not $entryPath.StartsWith($stagePrefix, [StringComparison]::OrdinalIgnoreCase)) {
            throw "展開先の範囲外を指すエントリがあります: $($entry.FullName)"
        }
    }
} finally {
    $archive.Dispose()
}

try {
    [IO.Compression.ZipFile]::ExtractToDirectory($archivePath, $stageFullPath)
    $packagePath = Join-Path $stageFullPath "ffmpeg-$encoderVersion-essentials_build"
    $packageExe = Join-Path $packagePath 'bin\ffmpeg.exe'
    if (-not (Test-Path -LiteralPath $packageExe -PathType Leaf)) {
        throw '想定した FFmpeg のパッケージ構成ではありません。'
    }
    $expectedExeHash = (Get-FileHash -LiteralPath $packageExe -Algorithm SHA256).Hash.ToLowerInvariant()
    $installedExe = Join-Path $encoderPath 'bin\ffmpeg.exe'
    if (Test-Path -LiteralPath $encoderPath) {
        if (-not (Test-Path -LiteralPath $installedExe -PathType Leaf) -or
            (Get-FileHash -LiteralPath $installedExe -Algorithm SHA256).Hash -ine $expectedExeHash) {
            throw "既存のエンコーダーは固定版と異なります。別の -Destination を指定してください: $encoderPath"
        }
    } else {
        Move-Item -LiteralPath $packagePath -Destination $encoderPath
    }
    $versionOutput = & $installedExe -version
    if ($LASTEXITCODE -ne 0) { throw 'FFmpeg の実行確認に失敗しました。' }
    [ordered]@{
        version = $encoderVersion
        source = $archiveUri
        archive_sha256 = $archiveHash
        executable_sha256 = $expectedExeHash
        executable = 'ffmpeg/bin/ffmpeg.exe'
        license = 'GPL-3.0-or-later; packaged license and build configuration are retained in runtime/ffmpeg'
        version_line = $versionOutput[0]
    } | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath $manifestPath -Encoding utf8
    Write-Output "エンコーダーを準備しました: $installedExe"
    Write-Output $versionOutput[0]
    Write-Output "SHA-256: $archiveHash"
} finally {
    # 再帰削除の対象は、この実行で生成した runtime 配下の一時展開先だけ。
    $runtimePrefix = $runtimePath.TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if ((Test-Path -LiteralPath $stageFullPath) -and
        $stageFullPath.StartsWith($runtimePrefix, [StringComparison]::OrdinalIgnoreCase) -and
        [IO.Path]::GetFileName($stageFullPath).StartsWith('.ffmpeg-extract-', [StringComparison]::Ordinal)) {
        Remove-Item -LiteralPath $stageFullPath -Recurse -Force
    }
}
