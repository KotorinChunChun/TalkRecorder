#Requires -Version 7.0
[CmdletBinding()]
param([Parameter(Mandatory)][string] $Exe)

$ErrorActionPreference = 'Stop'
$exePath = (Resolve-Path -LiteralPath $Exe).Path
if (Get-Process TalkRecorder -ErrorAction SilentlyContinue) {
    throw 'TalkRecorder が既に起動しています。検証は別のセッションで行ってください。'
}
$root = Split-Path $PSScriptRoot
$version = (Get-Item -LiteralPath $exePath).VersionInfo.FileVersion
if ($version -notmatch '^\d+\.\d+\.\d+$') { throw '実行ファイルの版を確認できません' }
$run = Join-Path $root ("dev/history/v$version/logs/managed-" + [guid]::NewGuid().ToString('N'))
[IO.Directory]::CreateDirectory($run) | Out-Null
$cfg = Join-Path $run 'settings.json'
$writer = Start-Process -FilePath $exePath -ArgumentList @('--write-default-config','--config',('"' + $cfg + '"')) -WindowStyle Hidden -Wait -PassThru
if ($writer.ExitCode -ne 0 -or !(Test-Path -LiteralPath $cfg)) { throw 'テスト設定を生成できません' }
$settings = Get-Content -LiteralPath $cfg -Raw | ConvertFrom-Json
if ($settings.auto_record_via_host -ne $false) { throw '新規設定で自動録音が有効です' }
$settings.auto_recover = $false
$settings.auto_compact_wav = $false
$settings.output_dir = Join-Path $run 'unused-recordings'
$settings | ConvertTo-Json -Depth 16 | Set-Content -LiteralPath $cfg -Encoding utf8NoBOM

$sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;
public static class TalkRecorderManagedWindow {
    private delegate bool WindowCallback(IntPtr window, IntPtr parameter);
    [DllImport("user32.dll")] private static extern bool EnumWindows(WindowCallback callback, IntPtr parameter);
    [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr window, out uint pid);
    [DllImport("user32.dll")] private static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetWindowText(IntPtr window, StringBuilder text, int capacity);
    [DllImport("kernel32.dll")]
    public static extern bool GetNamedPipeServerProcessId(SafePipeHandle pipe, out uint pid);
    public static bool IsSettingsVisible(uint pid) {
        bool visible = false;
        EnumWindows((window, _) => {
            GetWindowThreadProcessId(window, out uint owner);
            if (owner != pid || !IsWindowVisible(window)) return true;
            var title = new StringBuilder(200);
            GetWindowText(window, title, title.Capacity);
            visible = title.ToString().Contains(" - TalkRecorder - v");
            return !visible;
        }, IntPtr.Zero);
        return visible;
    }
}
'@

function New-ManagementClient {
    [IO.Pipes.NamedPipeClientStream]::new('.', "KotorinToys.$sid.talk-recorder",
        [IO.Pipes.PipeDirection]::InOut,
        ([IO.Pipes.PipeOptions]::Asynchronous -bor [IO.Pipes.PipeOptions]::CurrentUserOnly))
}

function Send-Management([string] $Command, [int] $ExpectedPid = 0) {
    $pipe = New-ManagementClient
    $deadline = [Threading.CancellationTokenSource]::new(2000)
    try {
        $pipe.ConnectAsync($deadline.Token).GetAwaiter().GetResult()
        [uint32]$serverPid = 0
        if (![TalkRecorderManagedWindow]::GetNamedPipeServerProcessId($pipe.SafePipeHandle, [ref]$serverPid)) {
            throw '管理窓口のPIDを取得できません'
        }
        $reader = [IO.StreamReader]::new($pipe, [Text.Encoding]::UTF8, $false, 1024, $true)
        $writer = [IO.StreamWriter]::new($pipe, [Text.UTF8Encoding]::new($false), 1024, $true)
        try {
            $requestId = [guid]::NewGuid().ToString('N')
            $message = if ($Command -eq 'status') { 'status' } else {
                @{command=$Command;expectedPid=$ExpectedPid;requestId=$requestId} | ConvertTo-Json -Compress
            }
            $writer.WriteLine($message)
            $writer.Flush()
            $reply = $reader.ReadLineAsync($deadline.Token).GetAwaiter().GetResult() | ConvertFrom-Json
            if ($reply.pid -ne $serverPid) { throw '管理窓口と応答のPIDが一致しません' }
            if ($Command -ne 'status' -and $reply.requestId -ne $requestId) { throw '要求IDの不一致' }
            $reply
        } finally { $writer.Dispose(); $reader.Dispose() }
    } finally { $pipe.Dispose(); $deadline.Dispose() }
}

$app = Start-Process -FilePath $exePath -ArgumentList @('--startup','--launch-origin','via_kotorintoys','--config',('"' + $cfg + '"')) -WindowStyle Hidden -PassThru
try {
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    $status = $null
    do {
        try { $status = Send-Management status } catch [OperationCanceledException] { }
    } while ($null -eq $status -and [DateTime]::UtcNow -lt $deadline -and !$app.HasExited)
    if ($null -eq $status -or !$status.ready -or $status.protocolVersion -ne 2 -or $status.pid -ne $app.Id -or
        $status.version -ne $version -or $status.launchOrigin -ne 'via_kotorintoys' -or $status.detail -ne '待機中') {
        throw '無表示起動後の状態が一致しません'
    }
    Start-Sleep -Milliseconds 800
    if ([TalkRecorderManagedWindow]::IsSettingsVisible($app.Id)) { throw '自動起動の設定画面が表示されています' }
    # 本体が送信途中で終了する場合を、接続ごとに別ハンドルで再現する。
    $abandoned = New-ManagementClient
    try {
        $abandoned.Connect(2000)
        $prefix = [Text.Encoding]::UTF8.GetBytes('stat')
        $abandoned.Write($prefix, 0, $prefix.Length)
    } finally { $abandoned.Dispose() }
    Start-Sleep -Seconds 2
    foreach ($attempt in 1..12) {
        $status = Send-Management status
        if (!$status.ready -or $status.result -ne 'completed' -or $status.pid -ne $app.Id -or
            $status.protocolVersion -ne 2 -or $status.launchOrigin -ne 'via_kotorintoys' -or $status.detail -ne '待機中') {
            throw "再接続した状態確認（$attempt 回目）が一致しません"
        }
    }
    if ((Send-Management stop ([int]($app.Id + 1))).result -ne 'refused') { throw 'PIDが違う終了が拒否されません' }
    if ((Send-Management show $app.Id).result -ne 'completed') { throw '画面を開けません' }
    $deadline = [DateTime]::UtcNow.AddSeconds(5)
    do {
        Start-Sleep -Milliseconds 100
        $visible = [TalkRecorderManagedWindow]::IsSettingsVisible($app.Id)
    } while (!$visible -and [DateTime]::UtcNow -lt $deadline)
    if (!$visible) { throw '管理操作で設定画面が表示されません' }
    if ((Send-Management stop $app.Id).result -ne 'accepted' -or !$app.WaitForExit(10000) -or $app.ExitCode -ne 0) {
        throw '自動起動したプロセスが正常終了しません'
    }
    if (Test-Path -LiteralPath $settings.output_dir) { throw '自動録音OFFで録音先が作成されました' }
    '無表示・自動録音OFF・中断後に同じPIDへ12回再接続・表示・正常終了を確認'
} finally {
    if (!$app.HasExited) {
        try { $null = Send-Management stop $app.Id } catch { }
        if (!$app.WaitForExit(5000) -and $app.Path -eq $exePath) { $app.Kill(); $app.WaitForExit() }
    }
    $app.Dispose()
}
