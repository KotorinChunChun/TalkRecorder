//! 起動・録音前後の復旧と、停止済み WAV の容量削減。
use anyhow::Result;
use serde::Serialize;

use crate::{
    compact::{CompactReport, compact_recordings},
    config::Config,
    recovery::{RecoveryReport, recover_pending},
};

#[derive(Debug, Default, Serialize)]
pub struct MaintenanceReport {
    pub recovery: RecoveryReport,
    pub compaction: CompactReport,
}

impl MaintenanceReport {
    pub fn errors(&self) -> impl Iterator<Item = &String> {
        self.recovery.errors.iter().chain(&self.compaction.errors)
    }

    pub fn message(&self) -> String {
        let saved = self
            .compaction
            .bytes_before
            .saturating_sub(self.compaction.bytes_after);
        let mut message = format!(
            "復旧 {} 件・M4A圧縮 {} 件（{:.1} MiB削減）・稼働中のため保留 {} 件",
            self.recovery.recovered,
            self.compaction.converted_groups,
            saved as f64 / 1_048_576.0,
            self.recovery
                .skipped_active
                .max(self.compaction.skipped_active),
        );
        for error in self.errors() {
            message.push('\n');
            message.push_str(error);
        }
        message
    }
}

pub fn run(config: &Config, recover: bool) -> Result<MaintenanceReport> {
    let mut report = MaintenanceReport::default();
    if recover && config.auto_recover {
        match recover_pending(&config.output_dir, &config.ffmpeg_path) {
            Ok(result) => report.recovery = result,
            Err(error) => report
                .recovery
                .errors
                .push(format!("復旧保留（元データ保持）: {error:#}")),
        }
    }
    if config.auto_compact_wav {
        match compact_recordings(&config.output_dir, &config.ffmpeg_path) {
            Ok(result) => report.compaction = result,
            Err(error) => report
                .compaction
                .errors
                .push(format!("圧縮保留（WAV保持）: {error:#}")),
        }
    }
    Ok(report)
}
