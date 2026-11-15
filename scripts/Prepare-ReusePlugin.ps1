#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$Destination = (Join-Path $PSScriptRoot '..\release\v0.1.0\plugins'),
    [string]$RuntimeDirectory = (Join-Path $PSScriptRoot '..\runtime\speech'),
    [string]$ModelDirectory = (Join-Path $PSScriptRoot '..\runtime\models'),
    [switch]$IncludeLaughter
)
$ErrorActionPreference = 'Stop'
$projectRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$pluginDestination = [IO.Path]::GetFullPath($Destination)
$runtimeSource = Join-Path ([IO.Path]::GetFullPath($RuntimeDirectory)) 'speech.dll'
$modelsSource = [IO.Path]::GetFullPath($ModelDirectory)
if (-not (Test-Path -LiteralPath $runtimeSource -PathType Leaf)) { throw 'speech.dllがありません。Prepare-Speech.ps1で準備してください。' }
$models = @(
    @{Id='reuse'; Name='RE-USE'; File='NVIDIA-NemotronLabs-RE-USE-10M-F32.gguf'; Sha256='827faf209c6988b9cfd07b08ee7f212a7b2eeb7870a25380df671d55338b3879'},
    @{Id='reuse-laughter'; Name='RE-USE laughter'; File='NVIDIA-NemotronLabs-RE-USE-10M-laughter-F32.gguf'; Sha256='15715fbb45e1ba3ae45da52c3018f60ef32f4fd48ff278cb14b402f9e12a32bf'}
) | Where-Object { $_.Id -eq 'reuse' -or $IncludeLaughter }
foreach ($model in $models) {
    $source = Join-Path $modelsSource $model.File
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "モデルがありません: $source" }
    if ((Get-FileHash -LiteralPath $source).Hash -ine $model.Sha256) { throw "モデルのSHA-256が一致しません: $source" }
}
$manifest = Join-Path $projectRoot 'native-plugins\reuse\Cargo.toml'
$buildTarget = Join-Path $projectRoot 'local\plugin-build\reuse\target'
& cargo build --locked --offline --release -j 2 --manifest-path $manifest --target-dir $buildTarget
if ($LASTEXITCODE -ne 0) { throw 'RE-USEプラグインのビルドに失敗しました' }
$dll = Join-Path $buildTarget 'release\talkrecorder_reuse.dll'
foreach ($model in $models) {
    $folder = Join-Path $pluginDestination $model.Id
    New-Item -ItemType Directory -Path $folder -Force | Out-Null
    Copy-Item -LiteralPath $dll -Destination (Join-Path $folder 'talkrecorder_reuse.dll')
    Copy-Item -LiteralPath $runtimeSource -Destination (Join-Path $folder 'speech.dll')
    Copy-Item -LiteralPath (Join-Path $modelsSource $model.File) -Destination (Join-Path $folder $model.File)
    [ordered]@{schema_version=1;id=$model.Id;name=$model.Name;library='talkrecorder_reuse.dll';resources=@{model=$model.File;runtime='speech.dll'}} |
        ConvertTo-Json -Depth 4 | Set-Content -LiteralPath (Join-Path $folder 'talkrecorder-plugin.json') -Encoding utf8
    Copy-Item -LiteralPath (Join-Path $projectRoot 'native-plugins\reuse\NOTICE.md') -Destination $folder
    Get-FileHash -LiteralPath (Join-Path $folder 'talkrecorder_reuse.dll')
}
