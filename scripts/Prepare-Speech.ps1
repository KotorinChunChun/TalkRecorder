#Requires -Version 7.0
[CmdletBinding()]
param(
    [string] $Destination = (Join-Path $PSScriptRoot '..\runtime'),
    [switch] $IncludeHaas,
    [switch] $IncludeDenoise,
    [switch] $IncludeLaughter
)
$ErrorActionPreference = 'Stop'
$runtimePath = [IO.Path]::GetFullPath($Destination)
$downloadPath = Join-Path $runtimePath 'downloads'
$speechPath = Join-Path $runtimePath 'speech'
$modelPath = Join-Path $runtimePath 'models'
New-Item -ItemType Directory -Force -Path $downloadPath,$speechPath,$modelPath | Out-Null

# speech.cpp v0.10.1 (C API 4.1)、公式 SHA256SUMS と同じ値を固定。
$releaseTag = 'v0.10.1'
$archiveName = 'speech-0.10.1-windows-x64-vulkan.zip'
$archiveHash = 'a8fe0e4b4f463c460e7ace9d38c363bec08b254cac5a7f764b7967af35d4fa1a'

function Get-VerifiedFile {
    param([string] $Uri, [string] $Path, [string] $Hash, [long] $Size = 0)
    if (Test-Path -LiteralPath $Path) {
        $existingHash = (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash
        if ($existingHash -ieq $Hash -and ($Size -eq 0 -or (Get-Item -LiteralPath $Path).Length -eq $Size)) { return }
    }
    $partialPath = "$Path.partial"
    Invoke-WebRequest -Uri $Uri -OutFile $partialPath
    if ((Get-FileHash -LiteralPath $partialPath -Algorithm SHA256).Hash -ine $Hash) { throw "SHA-256 が一致しません: $Path" }
    if ($Size -ne 0 -and (Get-Item -LiteralPath $partialPath).Length -ne $Size) { throw "ファイルサイズが一致しません: $Path" }
    Move-Item -LiteralPath $partialPath -Destination $Path -Force
}

$archivePath = Join-Path $downloadPath $archiveName
Get-VerifiedFile -Uri "https://github.com/nyosegawa/speech.cpp/releases/download/$releaseTag/$archiveName" -Path $archivePath -Hash $archiveHash
Expand-Archive -LiteralPath $archivePath -DestinationPath $speechPath -Force

# 固定タグ tools/catalog/catalog.json の revision / size / sha256 を採用。
$models = @(
    @{ Name='silero-vad-309K-v6.2-F32.gguf'; Repo='sakasegawa/silero-vad-GGUF'; Revision='7fcb7aa2a70c312f8579fc987b3e1cdecf9321ca'; Size=1238624; Hash='2fc312514e5fc4fa4ffd02b25c76a4853fa4130e8fb9777d01502183b01238cb'; Enabled=$true },
    @{ Name='Haas-616M-ja-v1.0-Q8_0.gguf'; Repo='sakasegawa/Haas-ja-v1.0-GGUF'; Revision='6b965d5007ba780585de51549d57a8c1c0d67c7e'; Size=656381472; Hash='cd5f60b2f1c411404b83fca4a65e3ce30a26d9e951b6a346529e65f22758afeb'; Enabled=$IncludeHaas },
    @{ Name='NVIDIA-NemotronLabs-RE-USE-10M-F32.gguf'; Repo='sakasegawa/NVIDIA-NemotronLabs-RE-USE-GGUF'; Revision='117011d06bba5522006f5fcfeadfdb714b58b236'; Size=38519136; Hash='827faf209c6988b9cfd07b08ee7f212a7b2eeb7870a25380df671d55338b3879'; Enabled=$IncludeDenoise },
    @{ Name='NVIDIA-NemotronLabs-RE-USE-10M-laughter-F32.gguf'; Repo='sakasegawa/NVIDIA-NemotronLabs-RE-USE-laughter-GGUF'; Revision='452f2af2f689e3b273bd9c550170217ec4fba978'; Size=38519200; Hash='15715fbb45e1ba3ae45da52c3018f60ef32f4fd48ff278cb14b402f9e12a32bf'; Enabled=$IncludeLaughter }
)
foreach ($model in $models | Where-Object Enabled) {
    $uri = "https://huggingface.co/$($model.Repo)/resolve/$($model.Revision)/$($model.Name)"
    Get-VerifiedFile -Uri $uri -Path (Join-Path $modelPath $model.Name) -Hash $model.Hash -Size $model.Size
}
@{ version=$releaseTag; api='4.1'; archive_sha256=$archiveHash; models=@($models | Where-Object Enabled | Select-Object Name,Revision,Hash,Size) } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $runtimePath 'speech-manifest.json') -Encoding utf8
Write-Output "音声ランタイムとモデルを準備しました: $runtimePath"
Write-Output 'Windows GPU ドライバーの Vulkan loader が必要です。録音や OS 設定変更は行っていません。'
