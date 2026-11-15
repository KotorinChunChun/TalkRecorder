#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$Destination = (Join-Path $PSScriptRoot '..\release\v0.1.0\plugins\deepfilternet3'),
    [switch]$SkipBuild
)
$ErrorActionPreference = 'Stop'
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
if (-not [IO.Path]::IsPathFullyQualified($Destination)) { throw 'Destinationは絶対パスで指定してください' }
$destinationPath = [IO.Path]::GetFullPath($Destination)
$buildRoot = Join-Path $projectRoot 'local\plugin-build\deepfilter'
$targetRoot = Join-Path $buildRoot 'target'
$manifestPath = Join-Path $projectRoot 'native-plugins\deepfilter\Cargo.toml'
$revision = '7b14b895fe7fc42cab72b4a4c1e49c982bd6404b'
$modelName = 'DeepFilterNet3_onnx.tar.gz'
$modelHash = 'c94d91f70911001c946e0fabb4aa9adc37045f45a03b56008cb0c8244cb63616'
New-Item -ItemType Directory -Path $buildRoot -Force | Out-Null

function Get-VerifiedPluginFile {
    param([string]$RelativeSource, [string]$Name, [string]$SHA256)
    $filePath = Join-Path $buildRoot $Name
    if (-not (Test-Path -LiteralPath $filePath) -or (Get-FileHash -LiteralPath $filePath).Hash -ine $SHA256) {
        $temporaryPath = "$filePath.partial"
        Invoke-WebRequest -Uri "https://raw.githubusercontent.com/Rikorose/DeepFilterNet/$revision/$RelativeSource" -OutFile $temporaryPath -TimeoutSec 300
        if ((Get-FileHash -LiteralPath $temporaryPath).Hash -ine $SHA256) {
            throw "SHA-256が一致しません: $Name"
        }
        Move-Item -LiteralPath $temporaryPath -Destination $filePath -Force
    }
    return $filePath
}

$modelPath = Get-VerifiedPluginFile "models/$modelName" $modelName $modelHash
$mitPath = Get-VerifiedPluginFile 'LICENSE-MIT' 'LICENSE-MIT' '24e6bb09c928af8d8e56268082f87413247ce36b39dd5d33add2f9893968065e'
$apachePath = Get-VerifiedPluginFile 'LICENSE-APACHE' 'LICENSE-APACHE' '1eaee808c5fb6b4e895ba30425285a5cdc5dd25bba2cd230f264c2200c331aec'
if (-not $SkipBuild) {
    & cargo build --locked --release --jobs 2 --manifest-path $manifestPath --target-dir $targetRoot
    if ($LASTEXITCODE -ne 0) { throw 'DeepFilterNet3プラグインのビルドに失敗しました' }
}
$library = Join-Path $targetRoot 'release\talkrecorder_deepfilter.dll'
if (-not (Test-Path -LiteralPath $library -PathType Leaf)) { throw "DLLがありません: $library" }
New-Item -ItemType Directory -Path $destinationPath -Force | Out-Null
Copy-Item -LiteralPath $library -Destination (Join-Path $destinationPath 'talkrecorder_deepfilter.dll')
Copy-Item -LiteralPath $modelPath -Destination (Join-Path $destinationPath $modelName)
Copy-Item -LiteralPath $mitPath -Destination (Join-Path $destinationPath 'LICENSE-MIT')
Copy-Item -LiteralPath $apachePath -Destination (Join-Path $destinationPath 'LICENSE-APACHE')
@{
    schema_version = 1
    id = 'deepfilternet3'
    name = 'DeepFilterNet3'
    library = 'talkrecorder_deepfilter.dll'
    resources = @{ model = $modelName }
} | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath (Join-Path $destinationPath 'talkrecorder-plugin.json') -Encoding utf8
@{
    source = 'https://github.com/Rikorose/DeepFilterNet'
    revision = $revision
    model_sha256 = $modelHash
    library_sha256 = (Get-FileHash -LiteralPath (Join-Path $destinationPath 'talkrecorder_deepfilter.dll')).Hash.ToLowerInvariant()
    api_version = 1
    processing = 'mono 48000 Hz; independent windows; upstream STFT and lookahead delay compensated'
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $destinationPath 'provenance.json') -Encoding utf8
Write-Output "DeepFilterNet3プラグインを準備しました: $destinationPath"
Write-Output '配置しただけでは有効になりません。TalkRecorderのデバイス枠で選択してください。'
