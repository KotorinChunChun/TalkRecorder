# RE-USEプラグイン

TalkRecorder用の任意ノイズ除去アダプター。モデルは一度ロードし、原音VADとは独立に、入力区間を同じサンプル数・位置で処理します。短い末尾には末尾ゼロを補い、出力を元の長さへ戻します。

- speech.cpp v0.10.1 / C API 4.1: https://github.com/nyosegawa/speech.cpp （MIT）
- NVIDIA NemotronLabs RE-USEモデル: https://huggingface.co/nvidia/NVIDIA-NemotronLabs-RE-USE （OpenMDW-1.1）
- laughterモデル: https://huggingface.co/nvidia/NVIDIA-NemotronLabs-RE-USE-laughter
- アダプター依存: anyhow（MIT/Apache-2.0）、libloading（ISC）、serde_json（MIT/Apache-2.0）。版はCargo.lockを参照。

重なった会話では主話者以外を抑える場合があります。実際の会話内容が残るかを比較してください。このプラグインの既定値はOFFです。配置しただけではロードしません。
