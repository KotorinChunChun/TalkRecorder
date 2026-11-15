#Requires -Version 7.0
[CmdletBinding()]
param(
    [string] $Destination = (Join-Path $PSScriptRoot '..\release\v0.1.0\plugins\rnnoise')
)

$ErrorActionPreference = 'Stop'
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$sourceRoot = Join-Path $projectRoot 'native-plugins\rnnoise'
$manifestPath = Join-Path $sourceRoot 'Cargo.toml'
$targetPath = Join-Path $projectRoot 'local\plugin-build\rnnoise\target'
$destinationPath = [IO.Path]::GetFullPath($Destination)
if (-not (Test-Path -LiteralPath (Join-Path $sourceRoot 'Cargo.lock'))) {
    throw '固定済みCargo.lockがありません。ソース一式を取得してください。'
}
& cargo build --locked --manifest-path $manifestPath --target-dir $targetPath --release
if ($LASTEXITCODE -ne 0) { throw 'RNNoiseプラグインのビルドに失敗しました。' }
$builtDll = Join-Path $targetPath 'release\talkrecorder_rnnoise.dll'
if (-not (Test-Path -LiteralPath $builtDll -PathType Leaf)) {
    throw 'Windows x64 DLLが生成されませんでした。Windows x64のRust環境で実行してください。'
}
New-Item -ItemType Directory -Path $destinationPath -Force | Out-Null
Copy-Item -LiteralPath $builtDll -Destination (Join-Path $destinationPath 'talkrecorder_rnnoise.dll') -Force
foreach ($fileName in @('talkrecorder-plugin.json', 'README.md', 'LICENSE.txt', 'LICENSE-nnnoiseless.txt')) {
    Copy-Item -LiteralPath (Join-Path $sourceRoot $fileName) -Destination (Join-Path $destinationPath $fileName) -Force
}
$sourceHash = (Get-FileHash -LiteralPath $builtDll -Algorithm SHA256).Hash
$installedHash = (Get-FileHash -LiteralPath (Join-Path $destinationPath 'talkrecorder_rnnoise.dll') -Algorithm SHA256).Hash
if ($sourceHash -ne $installedHash) { throw '配置したDLLのSHA-256が一致しません。' }
Write-Output "RNNoiseプラグインを準備しました: $destinationPath"
Write-Output '内蔵モデルを使用します。アプリの設定を変更せず、選択するまで動作しません。'
Get-FileHash -LiteralPath (Join-Path $destinationPath 'talkrecorder_rnnoise.dll') -Algorithm SHA256
