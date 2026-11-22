# DeepFilterNet libDFの取り込み元

`libDF/` は [DeepFilterNetの固定リビジョン](https://github.com/Rikorose/DeepFilterNet/tree/7b14b895fe7fc42cab72b4a4c1e49c982bd6404b/libDF) のRustソースとマニフェスト、配布元のライセンスです。モデルは従来どおり準備時に別途取得します。

`src/tract.rs` と `src/transforms.rs` をtract 0.21.17に合わせています。配列型はtractが公開するndarrayを使い、グラフの記号は `symbols` から取得します。互換差分を更新する際は [上流の修正提案](https://github.com/Rikorose/DeepFilterNet/pull/685) と正規モデルでのDLL試験を確認します。
