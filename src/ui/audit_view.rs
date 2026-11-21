use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, TryRecvError},
    thread,
};

use chrono::{DateTime, Local};
use eframe::egui::{self, Label, RichText};

use crate::audit::{self, Counters, Detail, Report};

#[derive(Default)]
pub(super) struct AuditView {
    directory: Option<PathBuf>,
    report: Option<anyhow::Result<Report>>,
    loading: Option<Receiver<anyhow::Result<Report>>>,
    show_details: bool,
    day: String,
}

impl AuditView {
    pub(super) fn show(&mut self, ui: &mut egui::Ui, directory: &Path) {
        let directory_changed = self.directory.as_deref() != Some(directory);
        if directory_changed {
            self.directory = Some(directory.into());
            self.report = None;
            self.loading = None;
            self.day.clear();
        }
        if let Some(loading) = &self.loading {
            match loading.try_recv() {
                Ok(result) => {
                    self.report = Some(result);
                    self.loading = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.report = Some(Err(anyhow::anyhow!("ログの読み込みが中断されました")));
                    self.loading = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        let refresh = ui
            .horizontal(|ui| {
                ui.selectable_value(&mut self.show_details, false, "日別集計");
                ui.selectable_value(&mut self.show_details, true, "最近の明細");
                let refresh = ui
                    .add_enabled(self.loading.is_none(), egui::Button::new("再読込"))
                    .clicked();
                if self.loading.is_some() {
                    ui.spinner();
                }
                refresh
            })
            .inner;
        if directory_changed || refresh {
            let (sender, receiver) = mpsc::sync_channel(1);
            let directory = directory.to_owned();
            let ctx = ui.ctx().clone();
            match thread::Builder::new()
                .name("talkrecorder-audit-view".into())
                .spawn(move || {
                    let _ = sender.send(audit::read_report(&directory, true));
                    ctx.request_repaint();
                }) {
                Ok(_) => self.loading = Some(receiver),
                Err(error) => self.report = Some(Err(error.into())),
            }
        }
        let Some(report) = &self.report else { return };
        let report = match report {
            Ok(report) => report,
            Err(error) => {
                ui.label(format!("録音ログを読み込めません。{error:#}"));
                return;
            }
        };
        if self.show_details {
            if !report.recent.iter().any(|row| row.day == self.day) {
                self.day = report
                    .recent
                    .last()
                    .map_or_else(String::new, |row| row.day.clone());
            }
            if self.day.is_empty() {
                ui.label("最近の明細がありません");
                return;
            }
            egui::ComboBox::from_id_salt("audit-day")
                .selected_text(&self.day)
                .show_ui(ui, |ui| {
                    for day in report.recent.iter().rev() {
                        ui.selectable_value(&mut self.day, day.day.clone(), &day.day);
                    }
                });
            let rows: Vec<_> = report
                .details
                .iter()
                .flatten()
                .rev()
                .filter(|detail| detail.day == self.day)
                .collect();
            table(ui, "時刻", "イベント", rows.len(), |ui, index| {
                let detail = rows[index];
                let time = DateTime::from_timestamp_millis(detail.utc_ms).map_or_else(
                    || "—".into(),
                    |date| date.with_timezone(&Local).format("%H:%M:%S").to_string(),
                );
                entry(
                    ui,
                    &time,
                    event_name(detail),
                    detail.counters,
                    report.sample_rate,
                );
            });
        } else {
            let rows: Vec<_> = report
                .recent
                .iter()
                .rev()
                .map(|row| (row, "明細あり"))
                .chain(report.archived.iter().rev().map(|row| (row, "集約済み")))
                .collect();
            if rows.is_empty() {
                ui.label("録音ログがありません");
                return;
            }
            table(ui, "日付", "区分", rows.len(), |ui, index| {
                let (day, kind) = rows[index];
                entry(ui, &day.day, kind, day.counters, report.sample_rate);
            });
        }
    }
}

const WIDTHS: [f32; 9] = [100., 130., 76., 76., 96., 76., 76., 94., 50.];

fn table(
    ui: &mut egui::Ui,
    first: &str,
    second: &str,
    count: usize,
    mut render: impl FnMut(&mut egui::Ui, usize),
) {
    egui::ScrollArea::both()
        .auto_shrink([false, false])
        .show_rows(ui, 22., count + 1, |ui, indices| {
            for index in indices {
                if index == 0 {
                    ui.horizontal(|ui| {
                        for (width, label) in WIDTHS.into_iter().zip([
                            first,
                            second,
                            "録音",
                            "保存",
                            "無音カット",
                            "除外",
                            "未判定",
                            "回収",
                            "失敗",
                        ]) {
                            ui.add_sized(
                                [width, 22.],
                                Label::new(RichText::new(label).strong()).truncate(),
                            );
                        }
                    });
                } else {
                    render(ui, index - 1);
                }
            }
        });
}

fn entry(ui: &mut egui::Ui, time: &str, kind: &str, data: Counters, rate: u32) {
    let values = [
        time.to_owned(),
        kind.to_owned(),
        duration(data.recorded_frames, rate),
        duration(data.saved_frames, rate),
        duration(data.silence_cut_frames, rate),
        duration(data.excluded_frames, rate),
        duration(data.unknown_frames, rate),
        if data.reclaimed_bytes == 0 {
            "—".into()
        } else {
            format!("{:.2} MiB", data.reclaimed_bytes as f64 / 1_048_576.0)
        },
        (data.save_failures + data.recording_failures).to_string(),
    ];
    ui.horizontal(|ui| {
        for (index, (width, value)) in WIDTHS.into_iter().zip(values).enumerate() {
            let cell = ui.add_sized([width, 22.], Label::new(value).truncate());
            if index == 1 {
                cell.on_hover_text(format!(
                    "保存 {} 区間　除外 {} 区間\n入力途絶 {}　同期未確定 {}（トラック合計）\n保存失敗 {} 件　録音エラー {} 件",
                    data.saved_segments, data.excluded_segments, duration(data.missing_track_frames, rate),
                    duration(data.unsynchronized_track_frames, rate), data.save_failures, data.recording_failures
                ));
            }
        }
    });
}

fn duration(frames: u64, rate: u32) -> String {
    if frames == 0 {
        return "—".into();
    }
    let seconds = frames as f64 / f64::from(rate);
    if seconds < 60.0 {
        return format!("{seconds:.1} 秒");
    }
    let whole = seconds as u64;
    format!("{}:{:02}:{:02}", whole / 3600, whole / 60 % 60, whole % 60)
}

fn event_name(detail: &Detail) -> &str {
    match detail.kind.as_str() {
        "segment" if detail.counters.save_failures > 0 => "保存失敗",
        "segment" if detail.counters.excluded_segments > 0 => "無音区間を除外",
        "segment" if detail.counters.saved_segments > 0 => "保存完了",
        "segment" => "保存対象なし",
        "capture" => "取得・回収",
        "recording_failure" => "録音エラー",
        _ => "記録",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::DaySummary;

    #[test]
    fn audit_view_draws_archive_and_recent_events() {
        let ctx = egui::Context::default();
        let directory = Path::new("fixture");
        let day = "2026-11-21".to_owned();
        let counters = Counters {
            recorded_frames: 48_000,
            excluded_frames: 48_000,
            excluded_segments: 1,
            ..Counters::default()
        };
        let mut view = AuditView {
            directory: Some(directory.into()),
            report: Some(Ok(Report {
                sample_rate: 48_000,
                archived: vec![DaySummary {
                    day: "2026-11-10".into(),
                    counters,
                }],
                recent: vec![DaySummary {
                    day: day.clone(),
                    counters,
                }],
                details: Some(vec![Detail {
                    day,
                    utc_ms: 0,
                    kind: "segment".into(),
                    counters,
                }]),
            })),
            ..AuditView::default()
        };
        for (details, expected) in [(false, "集約済み"), (true, "無音区間を除外")] {
            view.show_details = details;
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| view.show(ui, directory));
            });
            assert!(output.shapes.iter().any(|shape| {
                matches!(&shape.shape, egui::Shape::Text(text) if text.galley.text() == expected)
            }), "{expected} が描画されていません");
        }
    }
}
