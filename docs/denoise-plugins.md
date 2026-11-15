# TalkRecorder ノイズ除去プラグイン

適用版：v0.1.0

RNNoise、DeepFilterNet3、RE-USEを、必要なデバイスだけで使用できます。**初期状態は全デバイスで「ノイズ除去なし」です。** プラグインを配置しただけでは読み込まず、録音開始時に、そのデバイスで選択した方式だけを動作させます。

基本の録音操作は[READMEのユーザー向け説明書](../README.md)、画面ごとの操作は[詳しい操作手順](user-guide.md)を参照してください。

## 利用者向けの使い方

### 1. EXEと同じフォルダー以下へ一式を置く

推奨する配置は次のとおりです。使うプラグインのフォルダーだけを置けば使用できます。

```text
TalkRecorder.exe
plugins/
  rnnoise/
    talkrecorder-plugin.json
    talkrecorder_rnnoise.dll
    ライセンス等
  deepfilternet3/
    talkrecorder-plugin.json
    talkrecorder_deepfilter.dll
    DeepFilterNet3_onnx.tar.gz
    ライセンス等
  reuse/
    talkrecorder-plugin.json
    talkrecorder_reuse.dll
    speech.dll
    NVIDIA-NemotronLabs-RE-USE-10M-F32.gguf
    NOTICE.md
```

**DLLやモデルだけでは候補になりません。`talkrecorder-plugin.json` も必要です。** 定義に書かれたDLL・モデル等がすべて実在する一式だけを候補にします。ファイル名やフォルダー構成を変える場合は、定義内の相対パスも合わせる必要があります。

EXEの親フォルダーより上、PATH、外部のモデル置場は探索しません。たとえばEXEが `release/v0.1.0/TalkRecorder.exe` にある場合、プロジェクト直下の `runtime/models` にRE-USEモデルがあるだけでは候補になりません。必要一式を `release/v0.1.0` 以下へ配置します。

配布済みの一式を使う場合、利用者がRustやPythonを導入する必要はありません。必要なライセンス文書も一緒に保持してください。

### 2. 候補を再取得して選択する

1. 録音・入力確認を停止します。
2. 「デバイス」を開き、「プラグインを再取得」を押します。起動時にも候補を取得します。
3. 使用するデバイス枠のノイズ除去欄で、配置した方式を選びます。
4. 次回にも設定を残す場合は「設定を保存」を押します。
5. 「録音開始」を押します。選択したプラグインが読み込まれ、処理後の音声を保存します。

機器の録音を始める前に、選択した方式で1.5秒の生成無音を処理して妥当性・応答期限を確認します。失敗した場合は機器の取得へ進まず理由を表示します。確認用の音声文脈は破棄してから本録音へ進みます。

1つのデバイス枠では、OFFまたは1つの方式を選びます。複数の方式を重ねる機能はありません。同じ方式を複数のデバイスで選ぶことはできます。

「入力確認」は原音の波形を見る機能で、選択済みでもノイズ除去を行いません。本録音の会話判定も原音のVADを基本とします。除去後の音が小さくなったことを、そのまま会話なしの根拠にはしません。

### 3. 不足・削除・エラーの扱い

| 状況 | 動作・対処 |
| --- | --- |
| 定義・DLL・必要モデルが揃っていない | 再取得した候補に表示しません。「音声処理」内の「ノイズ除去プラグインの確認」で不足理由を確認できます |
| 同じIDの定義が複数ある | どちらも候補から除外します。重複を解消して再取得します |
| 選択済みの方式を削除した | 選択表示が「未配置」等になり、録音開始時に不足エラーを表示します。必要一式を戻すか、「ノイズ除去なし」へ変更します |
| DLLのAPIや依存関係が不適合 | 選択した方式を読み込む時点でエラーになります。候補にあることだけではAPI互換性を保証しません |
| 録音中に処理が失敗した | 該当区間は全トラックを原音・判定不能として保持し、解析の再準備を試みます。失敗を非発話へ置き換えません |
| 開始前の動作確認で応答期限を超える | 録音は始めずエラーを表示します。より軽い方式、またはノイズ除去なしを選択します |
| 方式を無効にしたい | 停止後に「ノイズ除去なし」を選びます。ファイルは残して構いません |

プラグインの追加・移動・削除は停止後に行い、変更後は再取得してください。DLLのコードを実行する機能なので、出所を確認した一式を配置します。

### 選べる方式

| 方式 | 実装と必要ファイル | 注意 |
| --- | --- | --- |
| RNNoise | `nnnoiseless`によるRust実装。モデルをDLLへ内蔵 | GPUランタイムや別モデルファイルは不要。PCごとの負荷・声の残り方は確認が必要 |
| DeepFilterNet3 | Rust DLL、公式のONNXモデルアーカイブ | Python・CUDA・ONNX Runtime DLLを使わない構成。Windows版DLLのVisual C++ランタイム等の依存は別PCで確認 |
| RE-USE | RustアダプターDLL、speech.cppの`speech.dll`、RE-USEモデル | 重なった声では主話者以外を抑える場合があります。小声や相槌も含め保存音声を比較してください |
| RE-USE laughter | RE-USEと同じアダプターDLL・runtime、laughter用モデル | 笑い声を残すモデル。専用モデルがある場合だけ候補になります |

laughter用モデルは今回未配置で、候補に表示されません。

現行実装は各要求を独立した音声窓として処理します。RE-USEの連続入力APIや、VADと除去処理の別キュー化は現行実装の対象ではありません。4台同時で重い方式を使用する実時間性能、多様な日本語会話・同時話者での品質は未受入です。処理後のサンプル数が同じことと、声が十分残ることは別に確認します。

## ソースから準備する場合

以下は準備済みDLLを使う利用者には不要な、ビルド・配置手順です。PowerShell 7でリポジトリの基準フォルダーから実行します。Windows x64向けのRust・MSVC開発環境と、初回依存取得のためのネット接続が必要です。アプリ通常起動時の自動ダウンロードは行いません。

```powershell
pwsh -NoProfile -File .\scripts\Prepare-RnnoisePlugin.ps1
pwsh -NoProfile -File .\scripts\Prepare-DeepFilterPlugin.ps1
```

- [RNNoise準備スクリプト](../scripts/Prepare-RnnoisePlugin.ps1)は固定したCargo.lockでDLLをビルドし、内蔵モデル・定義・ライセンスを配置します。別モデルのダウンロードはありません。初回のRust依存取得にはネット接続を使う場合があります。
- [DeepFilterNet3準備スクリプト](../scripts/Prepare-DeepFilterPlugin.ps1)は固定リビジョンのモデル・ライセンスを取得し、SHA-256検証後、DLL・定義と一緒に配置します。初回のRust依存取得も必要です。`-SkipBuild`は既にビルドしたDLLを使うための指定です。

RE-USEは、既に準備したspeech.cppのruntimeとモデルを使って配置します。

```powershell
pwsh -NoProfile -File .\scripts\Prepare-ReusePlugin.ps1
```

[RE-USE準備スクリプト](../scripts/Prepare-ReusePlugin.ps1)自身はモデルをダウンロードしません。既定の `runtime/speech/speech.dll` と `runtime/models` のモデルを確認・コピーします。標準モデルがない場合は、先に次を実行します。

```powershell
pwsh -NoProfile -File .\scripts\Prepare-Speech.ps1 -IncludeDenoise
```

RE-USEのビルドは`--offline`で行います。Rust依存が未取得の場合は、事前に `cargo fetch --locked --manifest-path .\native-plugins\reuse\Cargo.toml` で取得してください。

笑い声対応版も用意する場合は、必要モデルを準備してから両方を配置します。

```powershell
pwsh -NoProfile -File .\scripts\Prepare-Speech.ps1 -IncludeDenoise -IncludeLaughter
pwsh -NoProfile -File .\scripts\Prepare-ReusePlugin.ps1 -IncludeLaughter
```

既定の配置先は `release/v0.1.0/plugins` 以下です。別の場所のEXEで使う場合、`-Destination`の指定単位が方式により異なります。

| スクリプト | `-Destination`に渡す場所の例 |
| --- | --- |
| RNNoise | `C:\TalkRecorder\plugins\rnnoise` |
| DeepFilterNet3 | `C:\TalkRecorder\plugins\deepfilternet3` |
| RE-USE | `C:\TalkRecorder\plugins`。この下に`reuse`、任意で`reuse-laughter`を作成 |

ビルドやモデル取得が失敗した場合は、表示された理由を解消してから再実行します。準備スクリプトはアプリのデバイス選択や有効・無効設定を変更しません。

実装・個別の検証範囲は、[RNNoise](../native-plugins/rnnoise/README.md)、[DeepFilterNet3](../native-plugins/deepfilter/README.md)、[RE-USE](../native-plugins/reuse/NOTICE.md)を参照してください。ソースは各フォルダーの [RNNoise実装](../native-plugins/rnnoise/src/lib.rs)、[DeepFilterNet3実装](../native-plugins/deepfilter/src/lib.rs)、[RE-USE実装](../native-plugins/reuse/src/lib.rs)にあります。

## 開発者向けの定義と実行契約

### 録音しない動作確認

```powershell
release/v0.1.0/TalkRecorder.exe --check-denoise rnnoise
release/v0.1.0/TalkRecorder.exe --check-denoise deepfilternet3
```

生成無音を使って標準VADと選択DLLの接続を検査します。音声デバイスを開きません。公開音声による開発用の波形・末尾確認は `cargo run --locked --offline --example check_denoise_plugins -- release/v0.1.0 rnnoise` 等で実行できます。処理負荷は別PCの速度を保証しません。応答期限を超える場合は、軽い方式またはノイズ除去なしを選択してください。

### 定義ファイル

ファイル名は `talkrecorder-plugin.json` です。ASCIIの大小文字差は許容します。RNNoiseの例は次のとおりです。

```json
{
  "schema_version": 1,
  "id": "rnnoise",
  "name": "RNNoise",
  "library": "talkrecorder_rnnoise.dll",
  "resources": {}
}
```

外部モデル等が必要なら `resources`へ記載します。RE-USEの例です。

```json
{
  "schema_version": 1,
  "id": "reuse",
  "name": "RE-USE",
  "library": "talkrecorder_reuse.dll",
  "resources": {
    "runtime": "speech.dll",
    "model": "NVIDIA-NemotronLabs-RE-USE-10M-F32.gguf"
  }
}
```

- `schema_version`は1。未定義の項目や未対応版は拒否します。
- `id`は小文字英数字と`-_.`の1〜80文字。`name`は制御文字を含まない、空でない80文字以内の表示名です。
- `library`と`resources`の値は定義ファイルの場所を基準にした相対パスです。必要ファイルはすべてEXE配下に置きます。
- 絶対パス、UNC指定、`..`、コロンを含むパス、symlink、Windows reparse経由の参照は拒否します。EXE配下へ戻る`..`も受け入れません。
- `resources`のキーは英数字と`-_`の1〜80文字、最大32件です。DLL・モデル・runtime等、読み込みに必要な外部ファイルを列挙します。

探索の境界は実行中EXEの親フォルダーです。親へ遡らず、その下を深さ12・項目16,384件・定義128件まで探索し、定義1件は64KiB以下とします。深さ上限より下は省略し、件数上限を超えた場合は候補を採用せず診断を残します。リンク先のファイル・フォルダーは探索しません。

探索では定義とファイルの実在だけを確認し、DLLをロードしません。選択した方式を使うときに定義と全ファイルを再検証し、DLLのABIを確認します。Windowsの依存DLLの探索先は選択DLLのフォルダーとSystem32です。必要な独自DLLはプラグインのフォルダーへ置いてください。

### C ABI v1

Windows x64のDLLから、以下のC ABIシンボルを公開します。

```c
#include <stddef.h>
#include <stdint.h>

uint32_t tr_denoise_api_version(void);
void *tr_denoise_create(const char *config_json,
                       char *error, size_t error_capacity);
int tr_denoise_process(void *state, const float *input,
                      size_t samples, uint32_t sample_rate,
                      float *output, char *error, size_t error_capacity);
void tr_denoise_destroy(void *state);
```

`tr_denoise_api_version`は1を返します。createは成功時にstate、失敗時にNULLを返します。processは成功時0、失敗時は0以外を返します。エラー欄は有効なポインターかつ容量がある場合に、容量内のUTF-8文字列としてNUL終端してください。標準出力はホストのプロセス間通信に使うため、出力しません。

createのJSONは、検証済みresourceのキーと絶対パスを並べたオブジェクトです。例：`{"model":"C:/TalkRecorder/plugins/example/model.bin"}`。ホストが追加設定を渡す場合も同じオブジェクトへ含めます。resourceキーの上書きは拒否します。現在のRE-USE用IDでは、設定画面の処理単位を `chunk_secs`として渡します。

### 音声と時刻の契約

- 入力はmonoの正規化float32です。現行アプリは48,000Hzで呼び出し、`samples`はバイト数でなくfloatの個数です。
- 出力バッファには入力と**同じ個数・同じ時刻位置**の音声を書き込みます。音声の速度・長さを変更しません。
- 各要求は独立した音声窓です。ホストが過去文脈を付け、ステレオも左右別に渡します。モデルの重みは保持できますが、同じhandleへ別デバイス・別チャンネルの要求も来るため、前回の音声状態を無条件に持ち越しません。
- ネイティブのフレーム長に満たない末尾を含め、任意の入力長に対応します。必要な末尾ゼロを補ってflushし、STFTやモデル先読みの既知の遅延を補償してから元の範囲を返します。長さを合わせるだけで、音の位置をずらしたままにはしません。
- ホストは出力をNaNで初期化し、全サンプルが有限値で書かれたかを確認します。遅延補償や会話の保存品質は、実際の音声マーカー・小声・同時発話等で別途検証します。
- createで返したstateだけを処理し、destroyで一度だけ解放します。モデル・状態を解放してからDLLをアンロードできる順序にします。Rust実装ではABI境界を越えてpanicを伝播させないようにします。

RNNoiseは既知のSTFT遅延、DeepFilterNet3は上流のSTFTとlookahead遅延を補償します。RE-USEは区間処理の同数・同位置出力を使用し、短い末尾のために追加したゼロ部分を戻り値から除きます。詳細は各実装を参照してください。

### 新しいプラグインを追加する

1. 共通ABI v1と音声の契約を満たす独立DLLを作成します。
2. 一意なIDと必要ファイルを記載した定義を用意します。
3. DLL・モデル・定義・必要な依存DLLとライセンスを、EXE配下の1フォルダーへ配置します。
4. ホストの再取得で候補になること、選択した場合だけ実行されることを確認します。
5. 無音・実音声・任意長末尾・連続する独立窓・エラーを試し、サンプル数と音の位置、状態の解放、原音保持への復帰を確認します。

互換ABIの新方式は、方式ごとの候補をアプリ本体へ追加せずに読み込めます。ホスト側の探索・ロードは [denoise_plugins.rs](../src/denoise_plugins.rs)、会話判定と除去の接続は [speech.rs](../src/speech.rs)を参照してください。
