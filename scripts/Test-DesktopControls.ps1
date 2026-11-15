#Requires -Version 7.0
[CmdletBinding()]
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\release\v0.1.0\TalkRecorder.exe'),
    [string]$ReportDir = (Join-Path $PSScriptRoot '..\dev\history\v0.1.0\logs\desktop-controls'),
    [string]$RememberedDeviceId = '',
    [string]$ExpectedDeviceName = ''
)

# No physical mouse/keyboard input is used. Messages are sent only to windows
# owned by processes created here, after checking their executable and start time.
$ErrorActionPreference = 'Stop'
if (-not $IsWindows) { throw 'この検証はWindows専用です。' }
$checkDeviceMemory = -not [string]::IsNullOrWhiteSpace($RememberedDeviceId)
if ($checkDeviceMemory -ne (-not [string]::IsNullOrWhiteSpace($ExpectedDeviceName))) {
    throw 'デバイス記憶の確認ではRememberedDeviceIdとExpectedDeviceNameを両方指定してください。'
}
$exePath = (Resolve-Path -LiteralPath $Exe).Path
if (-not (Test-Path -LiteralPath $exePath -PathType Leaf)) { throw 'EXEがありません。' }
$reportBase = if ([IO.Path]::IsPathFullyQualified($ReportDir)) { $ReportDir } else { Join-Path $PWD $ReportDir }
$reportRoot = [IO.Path]::GetFullPath($reportBase)
$runDirectory = Join-Path $reportRoot ('desktop-' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $runDirectory -Force | Out-Null
$reportPath = Join-Path $runDirectory 'desktop-controls.json'
$configPath = Join-Path $runDirectory 'settings-primary.json'
$secondConfigPath = Join-Path $runDirectory 'settings-secondary.json'
$cliConfigPath = Join-Path $runDirectory 'settings-cli-while-gui-running.json'
$unusedRecordings = Join-Path $runDirectory 'recordings-unused'
$ownedProcesses = [Collections.Generic.List[object]]::new()
$report = [ordered]@{
    success = $false
    exe = $exePath
    run_directory = $runDirectory
    physical_input = $false
    recorded_audio = $false
    audio_check_basis = '全枠無効の専用設定を使用し、録音開始操作は行わない'
    device_selection_check_requested = $checkDeviceMemory
    device_selection_autosaved = $false
    device_id = $null
    device_name = $null
    expected_device_id = $RememberedDeviceId
    expected_device_name = $ExpectedDeviceName
    device_selection_verification = '指定時だけ、復元済みIDの名前補完と部分自動保存を確認する。デバイス選択UIのクリック試験ではない。'
    duplicate_prevented = $false
    cli_while_gui_running = $false
    speech_worker_while_gui_running = $false
    duplicate_restores_hidden_window = $false
    left_double_click_restores_hidden_window = $false
    tray_notification_diagnostics = @()
    tray_notification_failure_snapshot = $null
    right_menu_created = $false
    right_click_keeps_main_hidden = $false
    menu_items = @()
    exit_route = $null
    normal_exit = $false
    primary_process_id = $null
    secondary_process_id = $null
    ui_automation_note = $null
    recordings_created = $false
    config_files_retained = @($configPath, $secondConfigPath, $cliConfigPath)
    cleanup = @()
    error = $null
    verification_boundary = '対象トレイのネイティブ通知を模擬し、生成された実メニューとアプリの表示・終了を検査する。物理クリック、実録音、保存中の終了の受入ではない。'
}

if (-not ('TalkRecorderDesktopQa.Native' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;

namespace TalkRecorderDesktopQa {
    public sealed class WindowInfo {
        public IntPtr Handle;
        public string ClassName;
        public string Title;
        public bool Visible;
    }
    public sealed class MenuItemInfo {
        public uint Id;
        public string Text;
    }
    public sealed class IconRectInfo {
        public uint Id;
        public int HResult;
        public bool Success;
        public int Left;
        public int Top;
        public int Right;
        public int Bottom;
    }
    public sealed class TrayReadinessInfo {
        public bool CursorAvailable;
        public int CursorError;
        public int CursorX;
        public int CursorY;
        public bool Ready;
        public IconRectInfo[] Icons;
    }
    public static class Native {
        [StructLayout(LayoutKind.Sequential)]
        private struct Point { public int X; public int Y; }
        [StructLayout(LayoutKind.Sequential)]
        private struct Rect { public int Left; public int Top; public int Right; public int Bottom; }
        [StructLayout(LayoutKind.Sequential)]
        private struct NotifyIconIdentifier { public uint Size; public IntPtr Window; public uint Id; public Guid Guid; }
        private delegate bool EnumWindowsProc(IntPtr window, IntPtr parameter);
        [DllImport("user32.dll")] private static extern bool EnumWindows(EnumWindowsProc callback, IntPtr parameter);
        [DllImport("user32.dll")] private static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
        [DllImport("user32.dll", CharSet=CharSet.Unicode)] private static extern int GetClassNameW(IntPtr window, StringBuilder value, int count);
        [DllImport("user32.dll", CharSet=CharSet.Unicode)] private static extern int GetWindowTextW(IntPtr window, StringBuilder value, int count);
        [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr window);
        [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr window);
        [DllImport("user32.dll", SetLastError=true)] public static extern bool PostMessageW(IntPtr window, uint message, UIntPtr wParam, IntPtr lParam);
        [DllImport("user32.dll", SetLastError=true)] private static extern IntPtr SendMessageTimeoutW(IntPtr window, uint message, UIntPtr wParam, IntPtr lParam, uint flags, uint timeout, out UIntPtr result);
        [DllImport("user32.dll")] private static extern int GetMenuItemCount(IntPtr menu);
        [DllImport("user32.dll")] private static extern uint GetMenuItemID(IntPtr menu, int position);
        [DllImport("user32.dll", CharSet=CharSet.Unicode)] private static extern int GetMenuStringW(IntPtr menu, uint item, StringBuilder text, int count, uint flags);
        [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)] private static extern IntPtr OpenMutexW(uint access, bool inheritHandle, string name);
        [DllImport("kernel32.dll")] private static extern bool CloseHandle(IntPtr handle);
        [DllImport("user32.dll", SetLastError=true)] private static extern bool GetCursorPos(out Point point);
        [DllImport("shell32.dll")] private static extern int Shell_NotifyIconGetRect(ref NotifyIconIdentifier identifier, out Rect rect);

        public static uint Owner(IntPtr window) {
            uint result;
            GetWindowThreadProcessId(window, out result);
            return result;
        }
        public static bool GuiMutexExists() {
            IntPtr handle = OpenMutexW(0x00100000, false, @"Local\TalkRecorder.Gui.v1.Instance");
            if (handle != IntPtr.Zero) { CloseHandle(handle); return true; }
            int error = Marshal.GetLastWin32Error();
            if (error == 2) return false;
            throw new InvalidOperationException("既存GUIの起動制限を確認できません。Win32=" + error);
        }
        public static TrayReadinessInfo TrayReadiness(IntPtr window, uint processId) {
            if (!IsWindow(window) || Owner(window) != processId)
                throw new InvalidOperationException("検証外のトレイウィンドウです。");
            Point cursor;
            bool cursorAvailable = GetCursorPos(out cursor);
            int cursorError = cursorAvailable ? 0 : Marshal.GetLastWin32Error();
            var rows = new List<IconRectInfo>();
            bool anyIconAvailable = false;
            // tray-icon 0.24.2 Counter starts at 1. Builder/native IDs share it;
            // probe a small bounded range, only for this owned tray HWND.
            for (uint id = 0; id <= 8; id++) {
                var identifier = new NotifyIconIdentifier {
                    Size = (uint)Marshal.SizeOf(typeof(NotifyIconIdentifier)),
                    Window = window, Id = id, Guid = Guid.Empty
                };
                Rect rect;
                int result = Shell_NotifyIconGetRect(ref identifier, out rect);
                bool success = result == 0;
                anyIconAvailable |= success;
                rows.Add(new IconRectInfo {Id=id, HResult=result, Success=success,
                    Left=rect.Left, Top=rect.Top, Right=rect.Right, Bottom=rect.Bottom});
            }
            return new TrayReadinessInfo {CursorAvailable=cursorAvailable,
                CursorError=cursorError, CursorX=cursor.X, CursorY=cursor.Y,
                Ready=cursorAvailable && anyIconAvailable, Icons=rows.ToArray()};
        }
        public static WindowInfo[] Windows(uint processId) {
            var rows = new List<WindowInfo>();
            EnumWindows((window, parameter) => {
                if (Owner(window) != processId) return true;
                var className = new StringBuilder(256);
                var title = new StringBuilder(512);
                GetClassNameW(window, className, className.Capacity);
                GetWindowTextW(window, title, title.Capacity);
                rows.Add(new WindowInfo {Handle=window, ClassName=className.ToString(), Title=title.ToString(), Visible=IsWindowVisible(window)});
                return true;
            }, IntPtr.Zero);
            return rows.ToArray();
        }
        public static MenuItemInfo[] PopupItems(IntPtr popup, uint processId) {
            if (Owner(popup) != processId) throw new InvalidOperationException("検証外のメニューです。");
            UIntPtr result;
            // MN_GETHMENU: query the actual #32768 popup, with a bounded wait.
            if (SendMessageTimeoutW(popup, 0x01E1, UIntPtr.Zero, IntPtr.Zero, 2, 2000, out result) == IntPtr.Zero)
                throw new InvalidOperationException("メニューハンドルを取得できません。");
            var menu = new IntPtr(unchecked((long)result.ToUInt64()));
            int count = GetMenuItemCount(menu);
            if (count < 1 || count > 32) throw new InvalidOperationException("不正なメニュー項目数です。");
            var rows = new List<MenuItemInfo>();
            for (int index = 0; index < count; index++) {
                var text = new StringBuilder(512);
                GetMenuStringW(menu, (uint)index, text, text.Capacity, 0x0400);
                rows.Add(new MenuItemInfo {Id=GetMenuItemID(menu,index), Text=text.ToString()});
            }
            return rows.ToArray();
        }
    }
}
'@
}

function Assert-OwnedProcess {
    param([int]$OwnedProcessId)
    $record = @($ownedProcesses | Where-Object { $_.Id -eq $OwnedProcessId })
    if ($record.Count -ne 1) { throw "検証で作成していないPIDです: $OwnedProcessId" }
    $current = Get-Process -Id $OwnedProcessId -ErrorAction Stop
    if ($current.Path -ine $exePath -or $current.StartTime.ToUniversalTime().Ticks -ne $record[0].StartTicks) {
        throw "PIDまたはEXEの同一性を確認できません: $OwnedProcessId"
    }
    return $current
}

function Start-OwnedProcess {
    param([string]$Name, [string[]]$Arguments, [switch]$KillDescendantsOnTimeout)
    $child = Start-Process -FilePath $exePath -ArgumentList $Arguments -PassThru -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $runDirectory "$Name.stdout.log") `
        -RedirectStandardError (Join-Path $runDirectory "$Name.stderr.log")
    $ownedProcesses.Add([pscustomobject]@{ Id=$child.Id; StartTicks=$child.StartTime.ToUniversalTime().Ticks; Process=$child; Name=$Name; KillDescendants=$KillDescendantsOnTimeout.IsPresent })
    return $child
}

function Wait-Condition {
    param([scriptblock]$Condition, [string]$Description, [int]$TimeoutSeconds = 8)
    if ($TimeoutSeconds -lt 1 -or $TimeoutSeconds -gt 10) { throw '待機は1〜10秒で指定してください。' }
    $deadline = [datetime]::UtcNow.AddSeconds($TimeoutSeconds)
    do {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 100
    } while ([datetime]::UtcNow -lt $deadline)
    throw "確認が時間切れになりました: $Description"
}

function Send-OwnedMessage {
    param([int]$OwnedProcessId, [IntPtr]$Window, [uint32]$Message, [uint64]$WParam = 0, [int64]$LParam = 0)
    $null = Assert-OwnedProcess $OwnedProcessId
    if (-not [TalkRecorderDesktopQa.Native]::IsWindow($Window) -or [TalkRecorderDesktopQa.Native]::Owner($Window) -ne $OwnedProcessId) {
        throw '送信先ウィンドウの所有を確認できません。'
    }
    if (-not [TalkRecorderDesktopQa.Native]::PostMessageW($Window, $Message, [UIntPtr]$WParam, [IntPtr]$LParam)) {
        throw "対象ウィンドウへ通知を送信できません。Win32=$([Runtime.InteropServices.Marshal]::GetLastWin32Error())"
    }
}

function Hide-OwnedWindow {
    param([int]$OwnedProcessId, [IntPtr]$Window)
    Send-OwnedMessage -OwnedProcessId $OwnedProcessId -Window $Window -Message 0x0010
    Wait-Condition -Description '×による通知領域への退避' -Condition {
        $null = Assert-OwnedProcess $OwnedProcessId
        -not [TalkRecorderDesktopQa.Native]::IsWindowVisible($Window)
    }
}

function Get-TrayNotificationSnapshot {
    param([int]$OwnedProcessId, [IntPtr]$Window)
    $null = Assert-OwnedProcess $OwnedProcessId
    $snapshot = [TalkRecorderDesktopQa.Native]::TrayReadiness($Window, [uint32]$OwnedProcessId)
    return [ordered]@{
        sampled_utc = [datetime]::UtcNow.ToString('o')
        tray_window_handle = $Window.ToInt64()
        cursor_available = $snapshot.CursorAvailable
        cursor_error = $snapshot.CursorError
        cursor_x = $snapshot.CursorX
        cursor_y = $snapshot.CursorY
        ready = $snapshot.Ready
        icon_rects = @($snapshot.Icons | ForEach-Object {
            [ordered]@{id=$_.Id; hresult=$_.HResult; success=$_.Success; left=$_.Left; top=$_.Top; right=$_.Right; bottom=$_.Bottom}
        })
    }
}

function Wait-TrayNotificationReady {
    param([int]$OwnedProcessId, [IntPtr]$Window, [string]$Stage)
    $diagnostic = [ordered]@{stage=$Stage; status='waiting'; attempts=0; first=$null; last=$null}
    $report.tray_notification_diagnostics += $diagnostic
    $deadline = [datetime]::UtcNow.AddSeconds(8)
    do {
        $sample = Get-TrayNotificationSnapshot -OwnedProcessId $OwnedProcessId -Window $Window
        $diagnostic.attempts++
        if ($null -eq $diagnostic.first) { $diagnostic.first = $sample }
        $diagnostic.last = $sample
        if ($sample.ready) {
            $diagnostic.status = 'ready'
            return
        }
        Start-Sleep -Milliseconds 100
    } while ([datetime]::UtcNow -lt $deadline)
    if (-not $diagnostic.last.cursor_available) {
        $diagnostic.status = 'cursor_unavailable_before_notification'
    } else {
        $diagnostic.status = 'shell_icon_rect_unavailable_before_notification'
    }
    throw "通知領域の検証前提を満たしません: $Stage / $($diagnostic.status)。通知は送信しておらず、ダブルクリックやメニュー処理の失敗とは区別します。"
}

$primary = $null
$trayHandle = [IntPtr]::Zero
try {
    # Old versions lack this mutex and are deliberately not operated on.
    if ([TalkRecorderDesktopQa.Native]::GuiMutexExists()) {
        throw '起動制御に対応するGUIが既に動作中です。既存GUIを終了してから検証してください。'
    }
    $processNames = @('TalkRecorder', 'talk-recorder', [IO.Path]::GetFileNameWithoutExtension($exePath)) | Select-Object -Unique
    $existing = @(Get-Process -Name $processNames -ErrorAction SilentlyContinue | Where-Object {
        try { $_.Path -ieq $exePath -or $_.Path -match '[\\/]release[\\/]v0\.1\.4[\\/]TalkRecorder\.exe$' } catch { $false }
    })
    if ($existing.Count -ne 0) { throw 'v0.1.0の既存プロセスがあるため検証を中止します。' }

    $writer = Start-OwnedProcess -Name 'write-default' -Arguments @('--write-default-config', '--config', ('"' + $configPath + '"'))
    Wait-Condition -Description '専用既定設定の生成' -Condition { $writer.HasExited }
    if ($writer.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $configPath -PathType Leaf)) { throw '専用設定を生成できませんでした。' }
    $config = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
    foreach ($track in $config.tracks) { $track.enabled = $false; $track.denoise = 'Off' }
    if ($checkDeviceMemory) {
        $config.tracks[0].device_id = $RememberedDeviceId
        $config.tracks[0] | Add-Member -NotePropertyName device_name -NotePropertyValue '' -Force
    }
    $config.auto_recover = $false
    $config.auto_compact_wav = $false
    $config.prevent_sleep = $false
    $config.output_dir = $unusedRecordings
    $config | ConvertTo-Json -Depth 20 | Set-Content -LiteralPath $configPath -Encoding utf8
    Copy-Item -LiteralPath $configPath -Destination $secondConfigPath

    $primary = Start-OwnedProcess -Name 'primary' -Arguments @('--config', ('"' + $configPath + '"'))
    $report.primary_process_id = $primary.Id
    Wait-Condition -Description '対象GUIとトレイの作成' -Condition {
        $null = Assert-OwnedProcess $primary.Id
        $ownedWindows = @([TalkRecorderDesktopQa.Native]::Windows([uint32]$primary.Id))
        @($ownedWindows | Where-Object { $_.ClassName -eq 'Window Class' -and $_.Visible }).Count -eq 1 -and
        @($ownedWindows | Where-Object { $_.ClassName -eq 'tray_icon_app' }).Count -eq 1
    }
    $mainWindow = @([TalkRecorderDesktopQa.Native]::Windows([uint32]$primary.Id) | Where-Object { $_.ClassName -eq 'Window Class' })
    $trayWindow = @([TalkRecorderDesktopQa.Native]::Windows([uint32]$primary.Id) | Where-Object { $_.ClassName -eq 'tray_icon_app' })
    if ($mainWindow.Count -ne 1 -or $trayWindow.Count -ne 1) { throw '対象ウィンドウを一意に識別できません。' }
    $mainHandle = $mainWindow[0].Handle
    $trayHandle = $trayWindow[0].Handle
    $report.main_window_title = $mainWindow[0].Title
    $report.main_window_handle = $mainHandle.ToInt64()
    $report.tray_window_handle = $trayHandle.ToInt64()

    if ($checkDeviceMemory) {
        Wait-Condition -Description '復元済みデバイスIDの名前補完・部分自動保存' -Condition {
            $null = Assert-OwnedProcess $primary.Id
            $savedSelection = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
            $savedSelection.tracks.Count -eq 4 -and
            $savedSelection.tracks[0].device_id -ceq $RememberedDeviceId -and
            $savedSelection.tracks[0].device_name -ceq $ExpectedDeviceName -and
            @($savedSelection.tracks | Where-Object { $_.enabled -ne $false }).Count -eq 0
        }
        $savedSelection = Get-Content -LiteralPath $configPath -Raw | ConvertFrom-Json
        if ($savedSelection.tracks.Count -ne 4 -or
            $savedSelection.tracks[0].device_id -cne $RememberedDeviceId -or
            $savedSelection.tracks[0].device_name -cne $ExpectedDeviceName -or
            @($savedSelection.tracks | Where-Object { $_.enabled -ne $false }).Count -ne 0) {
            throw 'デバイスの自動保存後にID・名前・全枠無効の維持を確認できませんでした。'
        }
        $report.device_selection_autosaved = $true
        $report.device_id = $savedSelection.tracks[0].device_id
        $report.device_name = $savedSelection.tracks[0].device_name
    }

    if (-not [TalkRecorderDesktopQa.Native]::GuiMutexExists()) { throw '対象GUIが起動制限を保持していません。' }
    $cliWriter = Start-OwnedProcess -Name 'cli-while-gui-running' -Arguments @('--write-default-config', '--config', ('"' + $cliConfigPath + '"'))
    Wait-Condition -Description 'GUI起動中の設定出力CLI' -Condition { $cliWriter.HasExited }
    if ($cliWriter.ExitCode -ne 0 -or -not (Test-Path -LiteralPath $cliConfigPath -PathType Leaf)) {
        throw 'GUI起動中に設定出力CLIが正しく動作しませんでした。'
    }
    $null = Get-Content -LiteralPath $cliConfigPath -Raw | ConvertFrom-Json
    $null = Assert-OwnedProcess $primary.Id
    $report.cli_while_gui_running = $true
    $report.cli_while_gui_running_process_id = $cliWriter.Id

    # --self-check starts the real speech worker but only processes generated
    # silence. If it times out, cleanup kills this owned CLI and its descendant
    # tree together; killing just its parent could leave the worker running.
    $selfCheck = Start-OwnedProcess -Name 'self-check-while-gui-running' -KillDescendantsOnTimeout -Arguments @('--self-check', '--config', ('"' + $configPath + '"'))
    Wait-Condition -Description 'GUI起動中の生成無音・speech worker確認' -TimeoutSeconds 8 -Condition { $selfCheck.HasExited }
    if ($selfCheck.ExitCode -ne 0) { throw "GUI起動中のspeech worker確認が失敗しました: $($selfCheck.ExitCode)" }
    $selfCheckOutput = Get-Content -LiteralPath (Join-Path $runDirectory 'self-check-while-gui-running.stdout.log') -Raw
    if ($selfCheckOutput -notmatch 'speech\.cpp' -or $selfCheckOutput -notmatch '48000 samples') {
        throw '生成無音を処理したspeech workerの結果を確認できませんでした。'
    }
    $null = Assert-OwnedProcess $primary.Id
    if (-not [TalkRecorderDesktopQa.Native]::GuiMutexExists()) { throw 'CLI確認中にGUIの起動制限が失われました。' }
    $report.speech_worker_while_gui_running = $true
    $report.speech_worker_while_gui_running_process_id = $selfCheck.Id

    Hide-OwnedWindow -OwnedProcessId $primary.Id -Window $mainHandle
    $secondary = Start-OwnedProcess -Name 'secondary' -Arguments @('--config', ('"' + $secondConfigPath + '"'))
    $report.secondary_process_id = $secondary.Id
    Wait-Condition -Description '二重起動の正常終了' -Condition { $secondary.HasExited }
    if ($secondary.ExitCode -ne 0) { throw "二重起動が異常終了しました: $($secondary.ExitCode)" }
    $null = Assert-OwnedProcess $primary.Id
    $report.duplicate_prevented = $true
    Wait-Condition -Description '二重起動による既存画面の再表示' -Condition { [TalkRecorderDesktopQa.Native]::IsWindowVisible($mainHandle) }
    $report.duplicate_restores_hidden_window = $true

    Hide-OwnedWindow -OwnedProcessId $primary.Id -Window $mainHandle
    Wait-TrayNotificationReady -OwnedProcessId $primary.Id -Window $trayHandle -Stage 'before_left_double_click'
    # tray-icon 0.24.2 Windows backend: WM_USER_TRAYICON=6002.
    # WM_LBUTTONDBLCLK=0x0203 is sent to the owned tray receiver, not as physical input.
    Send-OwnedMessage -OwnedProcessId $primary.Id -Window $trayHandle -Message 6002 -LParam 0x0203
    Wait-Condition -Description '左ダブルクリック通知による設定表示' -Condition { [TalkRecorderDesktopQa.Native]::IsWindowVisible($mainHandle) }
    $report.left_double_click_restores_hidden_window = $true

    Hide-OwnedWindow -OwnedProcessId $primary.Id -Window $mainHandle
    Wait-TrayNotificationReady -OwnedProcessId $primary.Id -Window $trayHandle -Stage 'before_right_click_menu'
    Send-OwnedMessage -OwnedProcessId $primary.Id -Window $trayHandle -Message 6002 -LParam 0x0205
    Wait-Condition -Description '右クリックの実メニュー生成' -Condition {
        $null = Assert-OwnedProcess $primary.Id
        @([TalkRecorderDesktopQa.Native]::Windows([uint32]$primary.Id) | Where-Object { $_.ClassName -eq '#32768' -and $_.Visible }).Count -eq 1
    }
    if ([TalkRecorderDesktopQa.Native]::IsWindowVisible($mainHandle)) { throw '右クリックだけで設定画面が開きました。' }
    $report.right_click_keeps_main_hidden = $true
    $popup = @([TalkRecorderDesktopQa.Native]::Windows([uint32]$primary.Id) | Where-Object { $_.ClassName -eq '#32768' -and $_.Visible })[0]
    $items = @([TalkRecorderDesktopQa.Native]::PopupItems($popup.Handle, [uint32]$primary.Id))
    $report.menu_items = @($items | ForEach-Object { [ordered]@{id=$_.Id; text=$_.Text} })
    $showItems = @($items | Where-Object { $_.Text.Replace('&', '') -eq '設定を開く' })
    $exitItems = @($items | Where-Object { $_.Text.Replace('&', '') -eq '終了' })
    if ($showItems.Count -ne 1 -or $exitItems.Count -ne 1 -or $exitItems[0].Id -eq [uint32]::MaxValue) { throw '実メニューに設定・終了がありません。' }
    $report.right_menu_created = $true

    $invoked = $false
    try {
        Add-Type -AssemblyName UIAutomationClient
        Add-Type -AssemblyName UIAutomationTypes
        $menuElement = [System.Windows.Automation.AutomationElement]::FromHandle($popup.Handle)
        if ($menuElement.Current.ProcessId -ne $primary.Id) { throw 'UI Automationの所有PIDが一致しません。' }
        $condition = [System.Windows.Automation.PropertyCondition]::new([System.Windows.Automation.AutomationElement]::NameProperty, '終了')
        $exitElement = $menuElement.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $condition)
        $pattern = $null
        if ($null -eq $exitElement -or $exitElement.Current.ProcessId -ne $primary.Id -or
            -not $exitElement.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$pattern)) {
            throw '終了項目のInvokePatternが利用できません。'
        }
        $null = Assert-OwnedProcess $primary.Id
        $pattern.Invoke()
        $invoked = $true
        $report.exit_route = 'UIAutomation.InvokePattern (no physical input)'
    } catch {
        $report.ui_automation_note = $_.Exception.Message
        if ($primary.HasExited -and $primary.ExitCode -eq 0) {
            $invoked = $true
            $report.exit_route = 'UIAutomation.InvokePattern (provider disappeared after normal process exit)'
        }
    }
    if (-not $invoked) {
        # Native fallback is explicit: it validates menu creation and dispatches
        # its actual command ID, rather than claiming a physical menu selection.
        Send-OwnedMessage -OwnedProcessId $primary.Id -Window $trayHandle -Message 0x001F
        Wait-Condition -Description '検証用メニューの解除' -Condition {
            @([TalkRecorderDesktopQa.Native]::Windows([uint32]$primary.Id) | Where-Object { $_.ClassName -eq '#32768' -and $_.Visible }).Count -eq 0
        }
        Send-OwnedMessage -OwnedProcessId $primary.Id -Window $trayHandle -Message 0x0111 -WParam $exitItems[0].Id
        $report.exit_route = 'native WM_COMMAND using observed menu item ID (no physical input)'
    }
    Wait-Condition -Description '通知領域の終了要求による正常終了' -Condition { $primary.HasExited }
    if ($primary.ExitCode -ne 0) { throw "終了コードが正常ではありません: $($primary.ExitCode)" }
    $report.normal_exit = $true
    $report.recordings_created = Test-Path -LiteralPath $unusedRecordings
    if ($report.recordings_created) { throw '未使用の録音先が作成されています。' }
    $report.success = $true
} catch {
    $report.error = $_.Exception.Message
    if ($null -ne $primary -and -not $primary.HasExited -and $trayHandle -ne [IntPtr]::Zero) {
        try {
            $report.tray_notification_failure_snapshot = Get-TrayNotificationSnapshot -OwnedProcessId $primary.Id -Window $trayHandle
        } catch {
            $report.tray_notification_failure_snapshot = [ordered]@{diagnostic_error=$_.Exception.Message}
        }
    }
} finally {
    foreach ($record in $ownedProcesses) {
        $cleanup = [ordered]@{name=$record.Name; process_id=$record.Id; forced_stop=$false; descendant_tree_stopped=$false; status='already_exited'}
        try {
            if (-not $record.Process.HasExited) {
                $current = Assert-OwnedProcess $record.Id
                if ($record.KillDescendants) {
                    $current.Kill($true)
                    $cleanup.descendant_tree_stopped = $true
                } else {
                    $current.Kill()
                }
                if (-not $current.WaitForExit(5000)) { throw '検証用子プロセスの終了待ちが時間切れです。' }
                $cleanup.forced_stop = $true
                $cleanup.status = 'stopped_owned_test_process'
            }
        } catch {
            $cleanup.status = 'cleanup_failed: ' + $_.Exception.Message
            $report.success = $false
        }
        $report.cleanup += $cleanup
    }
    $report.recordings_created = Test-Path -LiteralPath $unusedRecordings
    $report | ConvertTo-Json -Depth 10 | Set-Content -LiteralPath $reportPath -Encoding utf8
    Write-Output "検証記録: $reportPath"
}
if (-not $report.success) { throw "検証に失敗しました。$($report.error) 記録: $reportPath" }
$report | ConvertTo-Json -Depth 10
