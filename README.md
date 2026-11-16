# TalkRecorder

TalkRecorder は、Windows 11 でマイクとスピーカーの音を同時に録音するアプリです。デバイスごとに音声を保存し、後から同じ場面を聞き比べられます。

- マイク最大2台、スピーカー最大2台を選べます。録音せずに入力波形だけを確認できます。
- 全デバイス共通の非発話部分を短くします。切断や解析失敗などで会話の有無を判断できない部分は残します。
- M4A・Opus・FLAC・WAV で保存できます。空き容量が設定値に達すると録音を止め、取得済みの音声を保存します。
- 通知領域で動作し、機器の再接続や、異常終了した録音の復旧に対応します。

## 入手と初回の準備

KotorinToysの一覧でTalkRecorderを選び、「インストール」を押します。単体で導入する場合は、GitHub Releasesの `TalkRecorder-0.1.1-setup.exe` を使います。 [ダウンロード](https://github.com/KotorinChunChun/TalkRecorder/releases/tag/v0.1.1)

最初の録音前に、導入先（既定では `%LOCALAPPDATA%\Programs\TalkRecorder`）で次の準備を行います。PowerShell 7とインターネット接続が必要です。

```powershell
pwsh -NoProfile -File .\scripts\Prepare-Speech.ps1
pwsh -NoProfile -File .\scripts\Prepare-Encoder.ps1
```

準備後に `TalkRecorder.exe` を開きます。音声処理には Vulkan を使える GPU ドライバーが必要です。

**データの扱い**　標準構成では設定を `local/settings.json`、録音を `recordings` に保存します。保存先は設定画面で確認できます。

ZIP版を使っていた場合は、停止してから設定と録音の保存先を確認してください。新しい導入先へ設定を引き継ぐには `local/settings.json` をコピーし、設定画面で録音先を確認します。

## 使い方

1. 音声ランタイムと FFmpeg を準備し、TalkRecorder.exe を開きます。
2. 「デバイス」で使う機器を選びます。「入力確認」で波形を確認できます。
3. 保存先を確認して録音を開始します。録音を止めたら保存が完了するまで待ちます。
4. ウィンドウを閉じると通知領域に移ります。アイコンをダブルクリックすると設定画面に戻ります。

詳しい手順は [利用ガイド](docs/user-guide.md) を参照してください。[機能一覧](docs/features.md) と [ノイズ除去プラグインの説明](docs/denoise-plugins.md) もあります。

TalkRecorder の独自コードと文書は MIT ライセンスです。ソースをビルドする手順は開発ガイドを参照してください。 [ライセンス](LICENSE) · [開発ガイド](docs/development.md)

## 不具合報告と連絡先

配布・利用に関する不具合は、アプリ名を添えて [KotorinToysのIssues](https://github.com/KotorinChunChun/KotorinToys/issues) へお願いします。アプリのバージョン、Windowsのバージョン、再現手順と表示されたエラーを添えてください。ログや画像を添付する前に、ユーザー名、ファイル名、認証情報などが含まれていないか確認してください。

脆弱性は、対象アプリ名を添えて [GitHubの非公開報告](https://github.com/KotorinChunChun/KotorinToys/security/advisories/new) からお知らせください。作者やほかのアプリは [GitHubプロフィール](https://github.com/KotorinChunChun)、一般の連絡や告知は [X](https://x.com/KotorinChunChun) をご覧ください。
