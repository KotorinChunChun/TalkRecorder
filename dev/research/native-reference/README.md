# 実マイクから実スピーカーへの独立参照経路

これは実機同期検証用の補助プロセス。製品のRust実装や設定を変更しない。WindowsのデフォルトeCapture／eRenderを共有モードで開き、マイクのPCMを同じフォーマットのrender clientへコピーする。音声ファイルを生成せず、PCM内容をCSVやstdoutへ出さない。

## 起動

```powershell
$out = Join-Path $PWD 'dev\history\v0.1.0\logs\native-reference'
pwsh -NoProfile -File .\dev\research\native-reference\build-reference.ps1 -OutputDirectory $out
& (Join-Path $out 'mic-render-reference.exe') --run --out (Join-Path $out 'run-001') --seconds 73 --stop-file (Join-Path $out 'run-001.stop')
```

`--out`と任意の`--stop-file`は絶対パスを要求する。stop-fileが最初から存在する場合、エンドポイントを開かず正常終了する。実行中にそのファイルが作られた場合、取得・再生をStopして正常終了する。時間は1〜80秒、既定73秒。APIが戻らない場合のプロセス全体watchdogは85秒。引数なし・`--help`では取得しない。過去の結果と計測の限界は[開発履歴](../../history/v0.1.0/v0.1.0-imp.md)に記録する。

mix formatはfloat32で全フィールドと拡張領域まで一致する必要がある。不一致時は`error_FormatsMustMatchFloat32`で停止する。リサンプリング・音量変更・OS設定変更・自動feedback補正は行わない。gainは1.0の厳密コピー。

captureのSILENTフラグはゼロに変換する。source ringは1秒上限で、単一packetが1秒を超える場合もoverflowとして明瞭に失敗終了する。上限超過を隠してdropしない。rendererは約50msのpaddingを目標に5ms単位で補充し、sourceがない時もdigital silenceを送る。開始前に同じ50msのsilenceをpreloadする。

## mapping.csv

HRESULTは符号なし32bit十進。Clock2は取得も呼び出しもしない。Clock1の`GetService`／`GetFrequency`／`GetPosition`だけを使用し、診断用SEHで失敗を記録する。positionが0、S_FALSE、失敗、異常なQPCの場合はその回の推定時刻を0・expected_valid=0として扱い、通常のHRESULT失敗は次回再試行する。SEH例外の場合だけ以後のGetPositionを止める。取得／再生経路自体のエラーは`error_*`行とstderrを残しexit 1。watchdogはexit 124。

eventは次のとおり。

- `prefill_silence`：renderer開始前の50ms。written-frameの始点は0。
- `source_packet`：実入力packetを受信した時点のメタデータ。
- `copy`：入力packetからrender bufferへコピーし、ReleaseBufferが成功した範囲。
- `render_silence`：source未供給のためrenderへ挿入したdigital silence。
- `stopped_duration`／`stopped_file`：正常な停止。
- `error_*`：失敗した操作。

主な列：

- source：`source_device_frame`（元packet先頭）、`source_native_qpc_100ns`、`source_received_qpc_100ns`（GetBuffer直後）、`source_packet_frames`、`source_offset_frames`（そのcopyの元packet内先頭）、`source_flags`。
- render：`renderer_written_frame`（preloadとsilenceを含む絶対written-frame index）、`frames_written`、`render_padding`（GetCurrentPadding）、`queued_source_frames`。
- copy時刻：`copy_before_qpc_100ns`／`copy_after_qpc_100ns`。
- render clock：`render_clock_position`、`render_clock_qpc_100ns`、`render_clock_frequency`、各HRESULT、`render_clock_valid`。
- 予測：`source_start_estimated_qpc_100ns`、`render_present_estimated_qpc_100ns`、`expected_delay_100ns`、`expected_valid`。
- `capture_latency_100ns`／`render_latency_100ns`と各HRESULTも参考値として保存。

`copy`行の関係式（sr=sample_rate）：

```text
source_start_estimated = source_received_qpc
                         - (source_packet_frames-source_offset_frames) / sr

render_present_estimated = render_clock_qpc
                          + renderer_written_frame / sr
                          - render_clock_position / render_clock_frequency

expected_delay = render_present_estimated - source_start_estimated
```

式のseconds部分はCSVでは100nsへ換算済み。GetPositionのpositionの単位をaudio frameと決めつけず、必ずGetFrequencyで割って秒へ変換する。初期preloadと挿入silenceもwritten indexへ含める。上式がrender queueの待ちをすでに含むため、padding50msやGetStreamLatencyを無条件にさらに加算すると二重計上になる。

## 実測の限界

source受信時刻からpacket長を引く推定には未知のcapture／ネットワーク遅延が残る。GetPositionも音響的なspeaker出力の完全な正解ではない。したがって期待値と独立NCCの差・時間変動を測る資料であり、物理遅延が厳密ゼロとする証拠ではない。rendererのpaddingが0になった範囲やClock無効の行は別途確認する。

停止要求時はsource ringに残る末尾をそれ以上再生せず止める。そのqueued_at_stopを最終行とstdoutへ明示する。NCCは開始／停止の端を避け、`copy`行が存在する内側の区間を使う。

## ビルド

`pwsh -NoProfile -File .\dev\research\native-reference\build-reference.ps1`。MSVC14.44.35207／Windows SDK10.0.26100.0、C++17、`/W4 /WX /O2 /MT`で成功。EXE／OBJは既定で`dev/history/v0.1.0/logs/native-reference`へ出す。`-OutputDirectory`で変更できる。引数なし・helpでエンドポイントを開かないことを確認済み。
