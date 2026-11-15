# TalkRecorder RNNoiseプラグイン

適用版：v0.1.0

RNNoiseのRust実装 `nnnoiseless 0.5.2` を利用するCPU用雑音抑制プラグインです。標準モデルはDLLへ組み込まれ、別のモデルファイルやGPUランタイムは不要です。DLLと `talkrecorder-plugin.json` をTalkRecorder.exeの同じフォルダー以下へ配置すると候補になり、デバイス枠で選択した場合だけ読み込みます。

```text
TalkRecorder.exe
plugins/
  rnnoise/
    talkrecorder_rnnoise.dll
    talkrecorder-plugin.json
    README.md
    LICENSE.txt
    LICENSE-nnnoiseless.txt
```

ソースからの準備は、Windows x64のRust開発環境とPowerShell 7で `pwsh -NoProfile -File .\scripts\Prepare-RnnoisePlugin.ps1` を実行します。依存はこのプラグイン専用のCargo.lockで固定し、アプリ本体の依存を変更しません。ビルド時の初回依存取得にはネット接続が必要ですが、配置後の実行時はネット接続を使用しません。

## 処理と時間対応

共通ABI v1のmono float32、48kHz入力を受け取ります。入力の任意サンプル数に対応し、出力サンプル数を維持します。RNNoise内部で必要な16bit相当の振幅へ変換し、処理後は元のfloat振幅へ戻します。

各要求は独立した音声窓で、ホストが過去文脈を添えて渡します。モデルをcreate時に保持し、各要求で新しい処理状態を作ります。先頭480 samples（10ms）のSTFT出力遅延を除外し、末尾をゼロ埋めした追加フレームを処理して末尾の実音声を回収します。入力に対する既知の遅延を補償してから、入力長へ切り詰めます。

処理対象は1要求60秒以下です。48kHz以外、非有限値、不正ポインターを検出した場合はエラーを返し、ABI境界でRust panicを捕捉します。任意の第三者ポインターの有効性までは検証できないため、ホストはこのプラグインが返したハンドルだけを使用し、一度だけ破棄してください。

## 一次資料

- https://github.com/jneem/nnnoiseless
- https://docs.rs/nnnoiseless/0.5.2/nnnoiseless/
- https://github.com/jneem/nnnoiseless/blob/main/src/denoise.rs
- https://github.com/jneem/nnnoiseless/blob/main/src/signal.rs

内蔵モデルの品質や4台同時の実時間性能は、利用するPCと録音素材で評価してください。

## 検証

ルートで cargo test --locked --manifest-path native-plugins/rnnoise/Cargo.toml を実行します。任意のサンプル数、末尾の処理、呼び出し間の独立性、ABIのエラー処理と遅延補償を確認できます。過去の実DLLの計測結果は[開発履歴](../../dev/history/v0.1.0/v0.1.0-imp.md)を参照してください。
