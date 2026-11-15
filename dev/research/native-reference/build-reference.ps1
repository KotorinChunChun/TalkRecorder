param([string]$OutputDirectory = (Join-Path $PSScriptRoot '..\..\history\v0.1.0\logs\native-reference'))
$ErrorActionPreference = 'Stop'
$taskRoot = [IO.Path]::GetFullPath($OutputDirectory)
New-Item -ItemType Directory -Path $taskRoot -Force | Out-Null
$vcRoot = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Tools\MSVC\14.44.35207'
$sdkRoot = 'C:\Program Files (x86)\Windows Kits\10'
$sdkVersion = '10.0.26100.0'
$compiler = Join-Path $vcRoot 'bin\Hostx64\x64\cl.exe'
if (-not (Test-Path -LiteralPath $compiler)) { throw 'Pinned MSVC compiler not found.' }
$env:INCLUDE = @(
    (Join-Path $vcRoot 'include'),
    (Join-Path $sdkRoot "Include\$sdkVersion\ucrt"),
    (Join-Path $sdkRoot "Include\$sdkVersion\shared"),
    (Join-Path $sdkRoot "Include\$sdkVersion\um"),
    (Join-Path $sdkRoot "Include\$sdkVersion\winrt")
) -join ';'
$env:LIB = @(
    (Join-Path $vcRoot 'lib\x64'),
    (Join-Path $sdkRoot "Lib\$sdkVersion\ucrt\x64"),
    (Join-Path $sdkRoot "Lib\$sdkVersion\um\x64")
) -join ';'
$source = Join-Path $PSScriptRoot 'mic-render-reference.cpp'
$object = Join-Path $taskRoot 'mic-render-reference.obj'
$executable = Join-Path $taskRoot 'mic-render-reference.exe'
& $compiler /nologo /std:c++17 /EHsc /W4 /WX /O2 /MT $source "/Fo$object" "/Fe$executable" /link ole32.lib uuid.lib
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& $executable --help
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
& $executable
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
Get-FileHash -LiteralPath $executable -Algorithm SHA256
