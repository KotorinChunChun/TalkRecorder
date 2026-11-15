use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::capture::DeviceKind;
use crate::editor::TimingConfig;
use crate::speech::{DenoiseMode, SpeechSettings};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    Wav,
    Flac,
    Opus,
    #[default]
    M4a,
}

impl OutputFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Opus => "opus",
            Self::M4a => "m4a",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackSettings {
    pub enabled: bool,
    pub device_id: String,
    #[serde(default)]
    pub device_name: String,
    pub kind: DeviceKind,
    pub label: String,
    pub denoise: DenoiseMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub timing: TimingConfig,
    pub tracks: Vec<TrackSettings>,
    pub output_dir: PathBuf,
    pub format: OutputFormat,
    pub auto_compact_wav: bool,
    pub ffmpeg_path: PathBuf,
    pub speech: SpeechSettings,
    pub max_pending_secs: f64,
    pub spool_limit_mb: u64,
    pub stop_free_space_mb: u64,
    pub preview_secs: f64,
    pub checkpoint_secs: f64,
    pub reconnect_secs: f64,
    pub prevent_sleep: bool,
    pub auto_recover: bool,
    pub auto_record_via_host: bool,
    pub analysis_timeout_secs: f64,
}

impl Default for Config {
    fn default() -> Self {
        let tracks = (0..4)
            .map(|index| TrackSettings {
                enabled: index == 0,
                device_id: String::new(),
                device_name: String::new(),
                kind: if index < 2 {
                    DeviceKind::Microphone
                } else {
                    DeviceKind::Speaker
                },
                label: if index < 2 {
                    format!("マイク{}", index + 1)
                } else {
                    format!("スピーカー{}", index - 1)
                },
                denoise: DenoiseMode::default(),
            })
            .collect();
        Self {
            timing: TimingConfig::default(),
            tracks,
            output_dir: PathBuf::from("recordings"),
            format: OutputFormat::default(),
            auto_compact_wav: true,
            ffmpeg_path: PathBuf::from("runtime/ffmpeg/bin/ffmpeg.exe"),
            speech: SpeechSettings::default(),
            max_pending_secs: 30.0,
            spool_limit_mb: 4_096,
            stop_free_space_mb: 1_024,
            preview_secs: 10.0,
            checkpoint_secs: 10.0,
            reconnect_secs: 2.0,
            prevent_sleep: true,
            auto_recover: true,
            auto_record_via_host: false,
            analysis_timeout_secs: 10.0,
        }
    }
}

impl Config {
    /// 録音開始用の検証。編集中の未選択デバイスは保存可能だが開始は不可。
    pub fn validate(&self) -> Result<()> {
        self.validate_structure()?;
        let enabled: Vec<_> = self.tracks.iter().filter(|track| track.enabled).collect();
        ensure!(
            !enabled.is_empty(),
            "録音するデバイスを1つ以上有効にしてください"
        );
        let mut ids = HashSet::new();
        for track in enabled {
            ensure!(
                !track.device_id.trim().is_empty(),
                "{}のデバイスを選択してください",
                track.label
            );
            ensure!(
                ids.insert(&track.device_id),
                "同じデバイスを重複して指定できません"
            );
        }
        Ok(())
    }

    pub fn validate_structure(&self) -> Result<()> {
        self.timing.validate()?;
        self.speech.validate()?;
        ensure!(self.tracks.len() == 4, "デバイス設定は4枠必要です");
        for (index, track) in self.tracks.iter().enumerate() {
            track.denoise.validate()?;
            let kind = if index < 2 {
                DeviceKind::Microphone
            } else {
                DeviceKind::Speaker
            };
            ensure!(track.kind == kind, "デバイス枠の種類が不正です");
            ensure!(
                !track.label.trim().is_empty(),
                "デバイス名を指定してください"
            );
            ensure!(
                track.label.chars().count() <= 80,
                "デバイス名は80文字以内にしてください"
            );
            ensure!(
                !track
                    .label
                    .chars()
                    .any(|ch| ch.is_control() || "<>:\"/\\|?*".contains(ch)),
                "デバイス名にファイル名で使用できない文字があります"
            );
            ensure!(
                !track.label.ends_with([' ', '.']),
                "デバイス名の末尾に空白やピリオドを使用できません"
            );
        }
        ensure!(
            !self.output_dir.as_os_str().is_empty(),
            "保存先を指定してください"
        );
        for (label, value, max) in [
            ("処理待ち時間", self.max_pending_secs, 3_600.0),
            ("波形表示時間", self.preview_secs, 600.0),
            ("内部確定間隔", self.checkpoint_secs, 60.0),
            ("再接続間隔", self.reconnect_secs, 60.0),
            ("音声解析の応答期限", self.analysis_timeout_secs, 300.0),
        ] {
            ensure!(
                value.is_finite() && value > 0.0 && value <= max,
                "{label}は0秒より大きく{max}秒以内にしてください"
            );
        }
        ensure!(
            (16..=1_048_576).contains(&self.spool_limit_mb),
            "一時保存容量は16MB以上、1TB以内にしてください"
        );
        ensure!(
            self.stop_free_space_mb <= 1_048_576,
            "自動停止の空き容量は0MB以上、1TB以内にしてください"
        );
        ensure!(
            self.timing.keep_silence_secs / 2.0 <= self.max_pending_secs,
            "非発話の末尾保持時間が処理待ち上限を超えています"
        );
        ensure!(
            self.checkpoint_secs >= 1.0,
            "内部確定間隔は1秒以上にしてください"
        );
        ensure!(
            self.reconnect_secs >= 0.1,
            "再接続間隔は0.1秒以上にしてください"
        );
        Ok(())
    }

    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        match fs::read(path) {
            Ok(bytes) => {
                let config: Self = serde_json::from_slice(&bytes)
                    .with_context(|| format!("設定ファイルを読み取れません: {}", path.display()))?;
                config.validate_structure()?;
                Ok(config)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => {
                Err(error).with_context(|| format!("設定ファイルを開けません: {}", path.display()))
            }
        }
    }

    pub fn save_atomic(&self, path: impl AsRef<Path>) -> Result<()> {
        self.validate_structure()?;
        let path = path.as_ref();
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("設定の保存先を作成できません: {}", parent.display()))?;
        let filename = path.file_name().context("設定ファイル名がありません")?;
        let temporary = parent.join(format!(
            "{}.{}.partial",
            filename.to_string_lossy(),
            std::process::id()
        ));
        let mut temporary_created = false;
        let result = (|| -> Result<()> {
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .with_context(|| {
                    format!(
                        "設定の一時ファイルを作成できません: {}",
                        temporary.display()
                    )
                })?;
            temporary_created = true;
            serde_json::to_writer_pretty(&mut output, self)?;
            output.write_all(b"\n")?;
            output.sync_all()?;
            drop(output);
            fs::rename(&temporary, path)
                .with_context(|| format!("設定ファイルを置換できません: {}", path.display()))?;
            // Unix ではディレクトリエントリの更新も確定する。Windows のディレクトリ
            // File::open は失敗するため、ファイルの sync と rename の結果を採用する。
            if let Ok(directory) = File::open(parent) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if result.is_err() && temporary_created {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selected() -> Config {
        let mut config = Config::default();
        config.tracks[0].device_id = "input-1".into();
        config
    }

    #[test]
    fn low_space_limit_defaults_for_old_settings_and_accepts_disabled_and_bounds() {
        let mut config = selected();
        assert_eq!(config.stop_free_space_mb, 1024);
        let mut old = serde_json::to_value(&config).unwrap();
        old.as_object_mut().unwrap().remove("stop_free_space_mb");
        let loaded: Config = serde_json::from_value(old).unwrap();
        assert_eq!(loaded.stop_free_space_mb, 1024);
        for value in [0, 1, 1024, 1_048_576] {
            config.stop_free_space_mb = value;
            config.validate_structure().unwrap();
        }
        config.stop_free_space_mb = 1_048_577;
        assert!(config.validate_structure().is_err());
    }

    #[test]
    fn new_settings_require_device_selection_before_recording() {
        let config = Config::default();
        assert!(config.validate_structure().is_ok());
        assert!(config.validate().is_err());
        assert!(selected().validate().is_ok());
    }

    #[test]
    fn compression_defaults_preserve_explicit_legacy_format() {
        assert_eq!(Config::default().format, OutputFormat::M4a);
        assert!(Config::default().auto_compact_wav);
        let mut legacy = serde_json::to_value(Config::default()).unwrap();
        legacy["format"] = serde_json::to_value(OutputFormat::Wav).unwrap();
        legacy.as_object_mut().unwrap().remove("auto_compact_wav");
        let loaded: Config = serde_json::from_value(legacy).unwrap();
        assert_eq!(loaded.format, OutputFormat::Wav);
        assert!(loaded.auto_compact_wav);
    }

    #[test]
    fn duplicate_enabled_devices_are_rejected() {
        let mut config = selected();
        config.tracks[1].enabled = true;
        config.tracks[1].device_id = "input-1".into();
        assert!(config.validate().is_err());
        config.tracks[1].device_id = "input-2".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn legacy_settings_without_device_names_remain_readable() {
        let mut value = serde_json::to_value(selected()).unwrap();
        for track in value["tracks"].as_array_mut().unwrap() {
            track.as_object_mut().unwrap().remove("device_name");
        }
        let loaded: Config = serde_json::from_value(value).unwrap();
        loaded.validate().unwrap();
        assert_eq!(loaded.tracks[0].device_id, "input-1");
        assert!(
            loaded
                .tracks
                .iter()
                .all(|track| track.device_name.is_empty())
        );
    }

    #[test]
    fn denoise_plugins_are_opt_in_and_legacy_settings_still_load() {
        let config = Config::default();
        assert!(
            config
                .tracks
                .iter()
                .all(|track| track.denoise == DenoiseMode::Off)
        );
        let mut value = serde_json::to_value(config).unwrap();
        value["tracks"][0]["denoise"] = serde_json::json!({"Plugin":"rnnoise"});
        let selected: Config = serde_json::from_value(value.clone()).unwrap();
        selected.validate_structure().unwrap();
        assert_eq!(selected.tracks[0].denoise.plugin_id(), Some("rnnoise"));
        value["tracks"][0]["denoise"] = serde_json::json!("Reuse");
        let legacy: Config = serde_json::from_value(value).unwrap();
        assert_eq!(legacy.tracks[0].denoise.plugin_id(), Some("reuse"));
        let mut invalid = legacy;
        invalid.tracks[0].denoise = DenoiseMode::Plugin("../outside".into());
        assert!(invalid.validate_structure().is_err());
    }

    #[test]
    fn invalid_limits_and_non_finite_numbers_are_rejected() {
        for invalid in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            let mut config = selected();
            config.max_pending_secs = invalid;
            assert!(config.validate().is_err());
        }
        let mut config = selected();
        config.spool_limit_mb = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn labels_cannot_escape_recording_directory() {
        let mut config = selected();
        config.tracks[0].label = "../important".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn settings_replace_atomically_and_invalid_json_is_not_silently_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        assert!(Config::load(&path).is_ok());
        let mut config = selected();
        config.save_atomic(&path).unwrap();
        config.timing.segment_secs = 900.0;
        config.save_atomic(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().timing.segment_secs, 900.0);
        fs::write(&path, b"{broken").unwrap();
        assert!(Config::load(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{broken");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
