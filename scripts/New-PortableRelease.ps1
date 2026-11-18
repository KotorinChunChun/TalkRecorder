#Requires -Version 7.0
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$target = 'x86_64-pc-windows-msvc'
Push-Location $root
try {
    # Panic の位置情報などにビルド端末の絶対パスを残さない。
    $roots = @(
        @{ path = $root; replacement = '/talkrecorder' }
        @{ path = $env:CARGO_HOME; replacement = '/cargo' }
        @{ path = $env:RUSTUP_HOME; replacement = '/rustup' }
        @{ path = $env:USERPROFILE; replacement = '/build-user' }
    )
    $options = foreach ($entry in $roots) {
        if ($entry.path) {
            foreach ($path in @($entry.path, $entry.path.Replace('\','/')) | Select-Object -Unique) {
                "--remap-path-prefix=$path=$($entry.replacement)"
            }
        }
    }
    $env:CARGO_ENCODED_RUSTFLAGS = $options -join [char]31
    cargo build --release --locked
    if ($LASTEXITCODE -ne 0) { throw '配布EXEをビルドできません' }
    $metadata = cargo metadata --locked --format-version 1 --filter-platform $target | ConvertFrom-Json -Depth 80
    if ($LASTEXITCODE -ne 0) { throw 'Cargo の依存情報を取得できません' }
    $app = $metadata.packages | Where-Object id -eq $metadata.resolve.root
    $version = $app.version
    $executables = @('TalkRecorder.exe', 'TalkRecorder-cli.exe' | ForEach-Object {
        Join-Path $metadata.target_directory "release/$_"
    })
    foreach ($exe in $executables) {
        $item = Get-Item -LiteralPath $exe
        $resource = $item.VersionInfo
        if ($resource.ProductName -ne 'TalkRecorder' -or $resource.FileVersion -ne $version -or
            $resource.OriginalFilename -ne $item.Name) {
            throw '配布EXEの製品名・ファイル版が一致しません'
        }
        $binary = [IO.File]::ReadAllBytes($exe)
        foreach ($encoding in @([Text.Encoding]::UTF8, [Text.Encoding]::Unicode)) {
            $strings = $encoding.GetString($binary)
            foreach ($entry in $roots) {
                if ($entry.path) {
                    foreach ($path in @($entry.path, $entry.path.Replace('\','/'))) {
                        if ($strings.Contains($path, [StringComparison]::OrdinalIgnoreCase)) {
                            throw '配布EXEに開発PCの絶対パスが含まれています'
                        }
                    }
                }
            }
        }
    }
    $release = Join-Path $root "release/v$version"
    $run = Join-Path $root ("dev/history/v$version/logs/portable-" + [guid]::NewGuid().ToString('N'))
    $stage = Join-Path $run 'TalkRecorder'
    foreach ($dir in @($stage, "$stage/docs", "$stage/scripts", "$stage/licenses", $release)) {
        [IO.Directory]::CreateDirectory($dir) | Out-Null
    }
    Copy-Item -LiteralPath ($executables + @('README.md', 'LICENSE')) -Destination $stage
    Copy-Item -LiteralPath docs/user-guide.md,docs/features.md,docs/denoise-plugins.md,docs/development.md -Destination "$stage/docs"
    Copy-Item -LiteralPath scripts/Prepare-Speech.ps1,scripts/Prepare-Encoder.ps1 -Destination "$stage/scripts"
    $reachable = [Collections.Generic.HashSet[string]]::new()
    $pending = [Collections.Generic.Queue[string]]::new()
    $nodes = @{}
    foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }
    $pending.Enqueue($app.id)
    while ($pending.Count -gt 0) {
        $id = $pending.Dequeue()
        if (!$reachable.Add($id)) { continue }
        foreach ($dep in $nodes[$id].deps) {
            if ($dep.dep_kinds | Where-Object { $null -eq $_.kind }) { $pending.Enqueue($dep.pkg) }
        }
    }
    $notices = foreach ($package in $metadata.packages | Where-Object { $_.id -ne $app.id -and $reachable.Contains($_.id) } | Sort-Object name,version) {
        $source = Split-Path -Parent $package.manifest_path
        $texts = @(Get-ChildItem -LiteralPath $source -File | Where-Object Name -Match '^(LICEN[CS]E|COPYING|COPYRIGHT|NOTICE)([-._].*)?$')
        if ($package.license_file) {
            $texts += Get-Item -LiteralPath (Join-Path $source $package.license_file)
        }
        $out = Join-Path "$stage/licenses" "$($package.name)-$($package.version)"
        foreach ($notice in $texts | Sort-Object FullName -Unique) {
            [IO.Directory]::CreateDirectory($out) | Out-Null
            Copy-Item -LiteralPath $notice.FullName -Destination $out
        }
        "$($package.name) $($package.version) | $($package.license) | https://crates.io/crates/$($package.name)/$($package.version)"
    }
    @('# TalkRecorder の Rust 依存ライセンス', '', '同梱したライセンスと通知は licenses/ を参照。', '', '製品 | ライセンス | 入手先', '--- | --- | ---') + @($notices) |
        Set-Content -LiteralPath (Join-Path $stage 'THIRD-PARTY-NOTICES.md') -Encoding utf8NoBOM
    $zipName = "TalkRecorder-v$version-win-x64.zip"
    $zip = Join-Path $release $zipName
    if (Test-Path -LiteralPath $zip) { throw "既存の配布物を上書きしません: $zip" }
    Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
    $sha = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    "$sha  $zipName" | Set-Content -LiteralPath (Join-Path $release 'SHA256SUMS.txt') -Encoding ascii
    $zip
} finally {
    Pop-Location
}
