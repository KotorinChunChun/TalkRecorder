# DeepFilterNet3 ノイズ除去プラグイン

TalkRecorder ABI v1に対応する独立したRust DLLです。公式DeepFilterNetのlibDF／tractを使用し、Pythonなしで48kHzのmono音声を処理します。モデルはDLLと別ファイルで、プラグインを有効にした時だけ読み込みます。

## 固定した入力

- 上流：<https://github.com/Rikorose/DeepFilterNet>
- リビジョン：`7b14b895fe7fc42cab72b4a4c1e49c982bd6404b`
- 推論依存：上流が対象にしたtract関連crateを`0.21.4`へ厳密固定。後続0.21系のndarray／Graph API変更によるビルド不整合を避けます。
- モデル：`models/DeepFilterNet3_onnx.tar.gz`、7,983,136 bytes
- モデルSHA-256：`c94d91f70911001c946e0fabb4aa9adc37045f45a03b56008cb0c8244cb63616`
- 上流ソース・モデルのライセンス：MITまたはApache-2.0。準備スクリプトが同梱します。

## 準備

基準フォルダーでPowerShell 7から実行します。

```powershell
pwsh -NoProfile -File .\scripts\Prepare-DeepFilterPlugin.ps1
```

既定の配置先は `release/v0.1.0/plugins/deepfilternet3` です。`-Destination` で別のEXE配下を指定できます。ビルドにはRust 1.88以上とMSVCが必要で、配布済みDLLの利用にはRust・Pythonは不要です。初回準備では固定モデル・依存の取得にネット接続を使用します。アプリ自身はダウンロードしません。

Windows版DLLはVisual C++ランタイム（`VCRUNTIME140.dll`）とWindows標準のUCRT・システムDLLに依存します。別PCでの動作確認時はこの依存も確認してください。ONNX Runtime DLLやCUDAは使用しません。

## 処理と同期

各呼び出しを独立した音声窓として扱います。モデルは一度読み込んで最適化し、呼び出し時は初期状態を複製します。強すぎる抑制を抑えるため、attenuation limitは35dBです。

上流の `libDF/src/bin/enhance_wav.rs` と同じ `fft_size - hop_size + lookahead * hop_size` を遅延として補償します。入力末尾に必要なゼロを追加してflushし、遅延分を除いて元の入力と同じサンプル数を返します。単に末尾を切って長さだけ合わせる方式ではありません。

録音・VAD・カット判断はホストの責任です。音声強調モデルのため、小声や同時話者が必ず残る保証はありません。4台同時の処理性能と多様な日本語会話での品質評価は別途必要です。

## 検証

モデル、DLL、公開音声を準備すると、実モデルによる条件付き試験を実行できます。以下はルートから実行します。

```powershell
$env:TALKRECORDER_DEEPFILTER_MODEL = (Join-Path $PWD 'local\plugin-build\deepfilter\DeepFilterNet3_onnx.tar.gz')
$env:TALKRECORDER_DEEPFILTER_DLL = (Join-Path $PWD 'local\plugin-build\deepfilter\target\release\talkrecorder_deepfilter.dll')
$env:TALKRECORDER_SPEECH_FIXTURE = (Join-Path $PWD 'runtime\reference\jfk.wav')
cargo test --locked --release --manifest-path .\native-plugins\deepfilter\Cargo.toml --target-dir .\local\plugin-build\deepfilter\target -- --include-ignored --test-threads=1 --nocapture
```

過去の計測結果は[開発履歴](../../dev/history/v0.1.0/v0.1.0-imp.md)を参照してください。ビルドキャッシュを削除した場合は、先に準備スクリプトで再ビルドしてください。
