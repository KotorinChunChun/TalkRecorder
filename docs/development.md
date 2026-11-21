# TalkRecorder 開発ガイド

Windows 11、PowerShell 7、Rust 1.89 以上の MSVC ツールチェーン、Visual Studio の C++ ビルドツールと Windows SDK を使用します。以下のコマンドは TalkRecorder のルートで実行します。

## 起動と配布用ビルド

```powershell
pwsh -NoProfile -File .\scripts\Prepare-Speech.ps1
pwsh -NoProfile -File .\scripts\Prepare-Encoder.ps1
cargo run --locked
```

音声ランタイム・モデル・FFmpeg は `runtime/` に取得します。準備スクリプトは固定版と SHA-256 を検証します。音声処理には GPU ドライバーが提供する Vulkan loader が必要です。Haas やノイズ除去の準備は[プラグインと音声処理の説明](denoise-plugins.md)を参照してください。

配布用 EXE は次の手順で `release/v0.1.0/` に置きます。

```powershell
cargo build --locked --release
New-Item -ItemType Directory -Force .\release\v0.1.0 | Out-Null
Copy-Item -LiteralPath .\target\release\TalkRecorder.exe -Destination .\release\v0.1.0\TalkRecorder.exe
```

プロジェクト外へ移す場合は EXE と同じフォルダーに `runtime/` を配置します。ノイズ除去を使う場合は `plugins/` も一緒に配置します。別の環境で既存設定を使う場合は、保存先やモデルの絶対パス、デバイスの選択を実際の環境に合わせて確認します。

## 検証

```powershell
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

録音や既存の設定を使わずに、準備済みランタイムと保存条件を確認できます。

```powershell
$check = Join-Path $PWD 'dev\history\v0.1.0\logs\cli\settings.json'
New-Item -ItemType Directory -Force (Split-Path $check) | Out-Null
cargo run --locked -- --write-default-config --config $check
cargo run --locked -- --self-check --config $check
cargo run --locked -- --check-storage --config $check
```

Windows の通知領域や多重起動の検証には [Test-DesktopControls.ps1](../scripts/Test-DesktopControls.ps1) を使用します。既存の TalkRecorder を終了した隔離環境で実行してください。結果は既定で `dev/history/v0.1.0/logs/desktop-controls/` に保存されます。

## データと文書

- `src/`、`tests/`、`examples/` は本体と検証コード、`native-plugins/` は任意のノイズ除去プラグイン、`scripts/` は準備・検証手順です。
- `local/`、`recordings/`、`runtime/`、`release/` は各環境のデータです。実機の設定、録音、診断ログ、ローカル再配布用のアーカイブをコミットしないでください。
- 以前の設計判断や試験条件は [開発履歴](../dev/history/v0.1.0/v0.1.0-imp.md) を参照してください。
