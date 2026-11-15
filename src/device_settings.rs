use anyhow::{Context, Result, ensure};
use std::path::Path;

use crate::config::Config;

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeviceSelection {
    enabled: bool,
    id: String,
    name: String,
}

fn selections(config: &Config) -> Vec<DeviceSelection> {
    config
        .tracks
        .iter()
        .map(|track| DeviceSelection {
            enabled: track.enabled,
            id: track.device_id.clone(),
            name: track.device_name.clone(),
        })
        .collect()
}

/// デバイス選択だけを保存し、画面上の他の未保存設定を確定しない。
pub(crate) struct DeviceSettingsStore {
    saved: Config,
    last_attempt: Vec<DeviceSelection>,
    allow_automatic: bool,
}

impl DeviceSettingsStore {
    pub(crate) fn new(initial: &Config, allow_automatic: bool) -> Self {
        Self {
            saved: initial.clone(),
            last_attempt: selections(initial),
            allow_automatic,
        }
    }

    /// 同じ選択では再書込みしない。失敗した場合も、再描画ごとの再試行を避ける。
    pub(crate) fn remember(&mut self, path: &Path, current: &Config) -> Result<bool> {
        let selected = selections(current);
        if selected == self.last_attempt {
            return Ok(false);
        }
        self.last_attempt = selected;
        ensure!(
            self.allow_automatic,
            "設定の読み込みに失敗しているため、デバイス選択を自動保存できません。設定を確認し、「設定を保存」で保存してください"
        );
        ensure!(
            current.tracks.len() == self.saved.tracks.len(),
            "デバイス枠の数が変わったため、自動保存できません"
        );

        // 有効な外部編集は基準に取り込み、壊れたJSONや読取エラーは上書きしない。
        // 初回のファイル未作成時だけ、読み込み済みの設定を保存の基準にする。
        let exists = path
            .try_exists()
            .with_context(|| format!("設定ファイルの有無を確認できません: {}", path.display()))?;
        let mut updated = if exists {
            Config::load(path).context("デバイス選択の保存前に設定を確認できません")?
        } else {
            self.saved.clone()
        };
        ensure!(
            updated.tracks.len() == current.tracks.len(),
            "保存先のデバイス枠が異なるため、自動保存できません"
        );
        for (saved, current) in updated.tracks.iter_mut().zip(&current.tracks) {
            saved.enabled = current.enabled;
            saved.device_id.clone_from(&current.device_id);
            saved.device_name.clone_from(&current.device_name);
        }
        updated
            .save_atomic(path)
            .context("デバイス選択を自動保存できません")?;
        self.saved = updated;
        Ok(true)
    }

    /// 設定全体の明示保存が成功した場合にだけ呼ぶ。
    pub(crate) fn mark_saved(&mut self, current: &Config) {
        self.saved = current.clone();
        self.last_attempt = selections(current);
        self.allow_automatic = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::OutputFormat, speech::DenoiseMode};
    use std::fs;

    fn initial() -> Config {
        let mut config = Config::default();
        config.tracks[0].device_id = "old-microphone".into();
        config.tracks[0].device_name = "以前のマイク".into();
        config
    }

    fn changed(initial: &Config) -> Config {
        let mut current = initial.clone();
        current.tracks[0].device_id = "new-microphone".into();
        current.tracks[0].device_name = "新しいマイク".into();
        current
    }

    #[test]
    fn selections_survive_restart_without_explicit_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let initial = initial();
        initial.save_atomic(&path).unwrap();
        let mut store = DeviceSettingsStore::new(&initial, true);
        let mut current = changed(&initial);
        current.tracks[0].enabled = false;
        current.tracks[2].enabled = true;
        current.tracks[2].device_id = "speaker-1".into();
        current.tracks[2].device_name = "スピーカー".into();
        assert!(store.remember(&path, &current).unwrap());

        let restored = Config::load(&path).unwrap();
        assert_eq!(selections(&restored), selections(&current));
        let mut restarted = DeviceSettingsStore::new(&restored, true);
        assert!(!restarted.remember(&path, &restored).unwrap());
    }

    #[test]
    fn unsaved_other_settings_are_not_committed_and_external_edits_are_preserved() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let initial = initial();
        initial.save_atomic(&path).unwrap();
        let mut store = DeviceSettingsStore::new(&initial, true);
        let mut current = changed(&initial);
        current.format = OutputFormat::Wav;
        current.output_dir = "unsaved-output".into();
        current.timing.segment_secs = 900.0;
        current.tracks[0].denoise = DenoiseMode::Plugin("rnnoise".into());
        current.tracks[0].label = "未保存の枠名".into();

        let mut external = Config::load(&path).unwrap();
        external.format = OutputFormat::Flac;
        external.output_dir = "external-output".into();
        external.timing.segment_secs = 1200.0;
        external.tracks[0].denoise = DenoiseMode::Plugin("deepfilternet3".into());
        external.save_atomic(&path).unwrap();
        assert!(store.remember(&path, &current).unwrap());

        let restored = Config::load(&path).unwrap();
        assert_eq!(restored.format, OutputFormat::Flac);
        assert_eq!(
            restored.output_dir,
            std::path::PathBuf::from("external-output")
        );
        assert_eq!(restored.timing.segment_secs, 1200.0);
        assert_eq!(
            restored.tracks[0].denoise.plugin_id(),
            Some("deepfilternet3")
        );
        assert_eq!(restored.tracks[0].label, "マイク1");
        assert_eq!(selections(&restored), selections(&current));
    }

    #[test]
    fn broken_json_is_preserved_and_same_selection_is_not_retried() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let initial = initial();
        initial.save_atomic(&path).unwrap();
        let mut store = DeviceSettingsStore::new(&initial, true);
        let mut current = changed(&initial);
        fs::write(&path, b"{broken").unwrap();
        assert!(store.remember(&path, &current).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{broken");

        initial.save_atomic(&path).unwrap();
        assert!(!store.remember(&path, &current).unwrap());
        assert_eq!(
            Config::load(&path).unwrap().tracks[0].device_id,
            "old-microphone"
        );
        current.tracks[0].device_id = "next-microphone".into();
        assert!(store.remember(&path, &current).unwrap());
        assert_eq!(
            Config::load(&path).unwrap().tracks[0].device_id,
            "next-microphone"
        );
    }

    #[test]
    fn failed_initial_load_requires_successful_explicit_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, b"{broken").unwrap();
        let fallback = initial();
        let mut store = DeviceSettingsStore::new(&fallback, false);
        let mut current = changed(&fallback);
        assert!(store.remember(&path, &current).is_err());
        assert!(!store.remember(&path, &current).unwrap());
        assert_eq!(fs::read(&path).unwrap(), b"{broken");

        current.save_atomic(&path).unwrap();
        store.mark_saved(&current);
        current.tracks[0].device_name = "明示保存後のマイク名".into();
        assert!(store.remember(&path, &current).unwrap());
        assert_eq!(
            Config::load(&path).unwrap().tracks[0].device_name,
            "明示保存後のマイク名"
        );
    }

    #[test]
    fn write_failure_does_not_retry_every_frame_or_remove_existing_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let temporary = directory
            .path()
            .join(format!("settings.json.{}.partial", std::process::id()));
        fs::write(&temporary, b"another-save").unwrap();
        let initial = initial();
        let mut store = DeviceSettingsStore::new(&initial, true);
        let mut current = changed(&initial);
        assert!(store.remember(&path, &current).is_err());
        assert!(!path.exists());
        assert_eq!(fs::read(&temporary).unwrap(), b"another-save");

        fs::remove_file(&temporary).unwrap();
        assert!(!store.remember(&path, &current).unwrap());
        assert!(!path.exists());
        current.tracks[0].device_name = "再選択したマイク".into();
        assert!(store.remember(&path, &current).unwrap());
        assert!(path.is_file());
    }

    #[test]
    fn custom_config_stores_do_not_touch_each_other() {
        let directory = tempfile::tempdir().unwrap();
        let first_path = directory.path().join("settings.json");
        let second_path = directory.path().join("custom-settings.json");
        let first = initial();
        let mut second = initial();
        second.tracks[0].device_id = "custom-microphone".into();
        first.save_atomic(&first_path).unwrap();
        second.save_atomic(&second_path).unwrap();
        let mut first_store = DeviceSettingsStore::new(&first, true);
        let mut second_store = DeviceSettingsStore::new(&second, true);

        let first_current = changed(&first);
        assert!(first_store.remember(&first_path, &first_current).unwrap());
        assert_eq!(
            Config::load(&second_path).unwrap().tracks[0].device_id,
            "custom-microphone"
        );
        let mut second_current = changed(&second);
        second_current.tracks[0].device_id = "custom-next".into();
        assert!(
            second_store
                .remember(&second_path, &second_current)
                .unwrap()
        );
        assert_eq!(
            Config::load(&first_path).unwrap().tracks[0].device_id,
            "new-microphone"
        );
        assert_eq!(
            Config::load(&second_path).unwrap().tracks[0].device_id,
            "custom-next"
        );
    }

    #[test]
    fn new_file_uses_initial_saved_values_for_unrelated_settings() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let mut initial = initial();
        initial.format = OutputFormat::Flac;
        initial.timing.segment_secs = 900.0;
        let mut store = DeviceSettingsStore::new(&initial, true);
        let mut current = changed(&initial);
        current.format = OutputFormat::Wav;
        current.timing.segment_secs = 1200.0;
        assert!(store.remember(&path, &current).unwrap());
        let restored = Config::load(&path).unwrap();
        assert_eq!(restored.format, OutputFormat::Flac);
        assert_eq!(restored.timing.segment_secs, 900.0);
    }

    #[test]
    fn unchanged_devices_do_not_create_a_file_for_unrelated_edits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let initial = initial();
        let mut store = DeviceSettingsStore::new(&initial, true);
        let mut current = initial.clone();
        current.format = OutputFormat::Flac;
        current.timing.segment_secs = 1500.0;
        assert!(!store.remember(&path, &current).unwrap());
        assert!(!path.exists());
    }
}
