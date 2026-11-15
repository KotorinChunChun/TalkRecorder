//! 共通の元サンプル位置だけを編集する、音声を所有しないストリーミング編集器。
use std::collections::VecDeque;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::SpeechState;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TimingConfig {
    pub segment_secs: f64,
    pub split_silence_secs: f64,
    pub keep_silence_secs: f64,
    pub discard_silence_secs: f64,
}

impl Default for TimingConfig {
    fn default() -> Self {
        Self {
            segment_secs: 600.0,
            split_silence_secs: 10.0,
            keep_silence_secs: 3.0,
            discard_silence_secs: 600.0,
        }
    }
}

impl TimingConfig {
    pub fn validate(&self) -> Result<()> {
        for (name, value, allow_zero) in [
            ("分割基準時間", self.segment_secs, false),
            ("分割用の非発話時間", self.split_silence_secs, false),
            ("非発話を残す長さ", self.keep_silence_secs, true),
            ("会話なし除外時間", self.discard_silence_secs, false),
        ] {
            ensure!(value.is_finite(), "{name}は有限の秒数を指定してください");
            ensure!(
                value >= 0.0 && (allow_zero || value > 0.0),
                "{name}の秒数が不正です"
            );
            ensure!(value <= 604_800.0, "{name}は7日以内にしてください");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EditAction {
    Keep { start: u64, end: u64 },
    Cut { start: u64, end: u64 },
    Split { at: u64, discard: bool },
}

#[derive(Debug, Clone, Copy)]
struct Boundary {
    at: u64,
    discard: bool,
}

/// 全有効デバイスを合成した状態を、連続する半開区間で投入する。
///
/// 非発話の末尾を確定するため、最大 `keep_silence_secs / 2` 秒だけ
/// Keep/Cut と Split の通知が遅れる。音声自体は呼び出し元が保存する。
/// Split は直前の Keep/Cut が確定した後に通知されるため、通知順のまま
/// 保存ファイルを切り替えられる。元の Split.at は遅延しない。
#[derive(Debug)]
pub struct Editor {
    segment_frames: u64,
    split_silence_frames: u64,
    discard_frames: u64,
    head_frames: u64,
    tail_frames: u64,
    input_until: u64,
    settled_until: u64,
    segment_start: u64,
    segment_has_signal: bool,
    silence_start: Option<u64>,
    boundaries: VecDeque<Boundary>,
    finished: bool,
}

impl Editor {
    pub fn new(timing: TimingConfig, sample_rate: u32) -> Result<Self> {
        timing.validate()?;
        ensure!(
            (1..=384_000).contains(&sample_rate),
            "サンプルレートが不正です"
        );
        let frames = |seconds: f64, nonzero: bool| -> Result<u64> {
            let count = (seconds * f64::from(sample_rate)).round() as u64;
            ensure!(
                !nonzero || count > 0,
                "時間設定は1サンプル以上にしてください"
            );
            Ok(count)
        };
        let keep = frames(timing.keep_silence_secs, false)?;
        Ok(Self {
            segment_frames: frames(timing.segment_secs, true)?,
            split_silence_frames: frames(timing.split_silence_secs, true)?,
            discard_frames: frames(timing.discard_silence_secs, true)?,
            head_frames: keep / 2,
            tail_frames: keep - keep / 2,
            input_until: 0,
            settled_until: 0,
            segment_start: 0,
            segment_has_signal: false,
            silence_start: None,
            boundaries: VecDeque::new(),
            finished: false,
        })
    }

    pub fn push(&mut self, start: u64, frames: u64, state: SpeechState) -> Result<Vec<EditAction>> {
        ensure!(!self.finished, "編集処理は終了しています");
        ensure!(
            start == self.input_until,
            "入力区間が不連続です: 期待位置 {}, 入力 {start}",
            self.input_until
        );
        let Some(end) = start.checked_add(frames) else {
            bail!("入力サンプル位置が上限を超えました");
        };
        if frames == 0 {
            return Ok(Vec::new());
        }
        let mut actions = Vec::new();
        if state == SpeechState::Silence {
            let silence_start = *self.silence_start.get_or_insert(start);
            self.schedule_boundaries(silence_start, end);
            let head_end = silence_start.saturating_add(self.head_frames).min(end);
            if self.settled_until < head_end {
                self.emit_range(head_end, true, &mut actions);
            }
            let cut_until = end.saturating_sub(self.tail_frames).max(head_end);
            if self.settled_until < cut_until {
                self.emit_range(cut_until, false, &mut actions);
            }
        } else {
            // Unknown は保持し、当該セグメントを自動削除できなくする。
            self.flush_silence(start, &mut actions);
            self.segment_has_signal = true;
            self.emit_range(end, true, &mut actions);
        }
        self.input_until = end;
        Ok(actions)
    }

    pub fn finish(&mut self) -> Vec<EditAction> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut actions = Vec::new();
        self.flush_silence(self.input_until, &mut actions);
        if self.segment_start < self.input_until {
            actions.push(EditAction::Split {
                at: self.input_until,
                discard: !self.segment_has_signal
                    && self.input_until - self.segment_start >= self.discard_frames,
            });
            self.segment_start = self.input_until;
        }
        actions
    }

    fn schedule_boundaries(&mut self, silence_start: u64, end: u64) {
        loop {
            // 会話のない短いファイルを先に公開して、後から削除し直すことを
            // 避ける。全非発話の場合だけ除外判定時間まで確定を待つ。
            let min_segment = if self.segment_has_signal {
                self.segment_frames
            } else {
                self.segment_frames.max(self.discard_frames)
            };
            let at = self
                .segment_start
                .saturating_add(min_segment)
                .max(silence_start.saturating_add(self.split_silence_frames));
            if at > end || at <= self.segment_start {
                break;
            }
            self.boundaries.push_back(Boundary {
                at,
                discard: !self.segment_has_signal && at - self.segment_start >= self.discard_frames,
            });
            self.segment_start = at;
            self.segment_has_signal = false;
        }
    }

    fn flush_silence(&mut self, end: u64, actions: &mut Vec<EditAction>) {
        if self.silence_start.take().is_some() && self.settled_until < end {
            self.emit_range(end, true, actions);
        }
        self.emit_boundaries(actions);
    }

    fn emit_range(&mut self, end: u64, keep: bool, actions: &mut Vec<EditAction>) {
        self.emit_boundaries(actions);
        while self.settled_until < end {
            let part_end = self.boundaries.front().map_or(end, |b| end.min(b.at));
            if self.settled_until < part_end {
                let start = self.settled_until;
                if keep {
                    actions.push(EditAction::Keep {
                        start,
                        end: part_end,
                    });
                } else {
                    actions.push(EditAction::Cut {
                        start,
                        end: part_end,
                    });
                }
                self.settled_until = part_end;
            }
            self.emit_boundaries(actions);
        }
    }

    fn emit_boundaries(&mut self, actions: &mut Vec<EditAction>) {
        while self
            .boundaries
            .front()
            .is_some_and(|b| b.at <= self.settled_until)
        {
            if let Some(boundary) = self.boundaries.pop_front() {
                actions.push(EditAction::Split {
                    at: boundary.at,
                    discard: boundary.discard,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timing() -> TimingConfig {
        TimingConfig::default()
    }

    fn ranges(actions: &[EditAction]) -> (u64, u64) {
        let mut cursor = 0;
        let mut kept = 0;
        let mut cut = 0;
        for action in actions {
            match *action {
                EditAction::Keep { start, end } => {
                    assert_eq!(start, cursor, "重複または欠落");
                    assert!(end > start);
                    cursor = end;
                    kept += end - start;
                }
                EditAction::Cut { start, end } => {
                    assert_eq!(start, cursor, "重複または欠落");
                    assert!(end > start);
                    cursor = end;
                    cut += end - start;
                }
                EditAction::Split { at, .. } => {
                    assert_eq!(at, cursor, "分割位置より前の編集が未確定")
                }
            }
        }
        (kept, cut)
    }

    #[test]
    fn twenty_seconds_of_silence_removes_seventeen() {
        let mut editor = Editor::new(timing(), 10).unwrap();
        let mut actions = editor.push(0, 200, SpeechState::Silence).unwrap();
        actions.extend(editor.finish());
        assert_eq!(ranges(&actions), (30, 170));
    }

    #[test]
    fn short_silence_is_unchanged() {
        for frames in 1..=30 {
            let mut editor = Editor::new(timing(), 10).unwrap();
            let mut actions = editor.push(0, frames, SpeechState::Silence).unwrap();
            actions.extend(editor.finish());
            assert_eq!(ranges(&actions), (frames, 0));
        }
    }

    #[test]
    fn split_waits_for_ten_seconds_after_speech_and_uses_real_time() {
        let mut editor = Editor::new(timing(), 10).unwrap();
        let mut actions = editor.push(0, 6_200, SpeechState::Speech).unwrap();
        actions.extend(editor.push(6_200, 300, SpeechState::Silence).unwrap());
        actions.extend(editor.finish());
        assert!(actions.contains(&EditAction::Split {
            at: 6_300,
            discard: false
        }));
        assert_eq!(ranges(&actions), (6_230, 270));
    }

    #[test]
    fn existing_long_pause_splits_at_six_hundred() {
        let mut editor = Editor::new(timing(), 10).unwrap();
        let mut actions = editor.push(0, 5_800, SpeechState::Speech).unwrap();
        actions.extend(editor.push(5_800, 400, SpeechState::Silence).unwrap());
        actions.extend(editor.finish());
        assert!(actions.contains(&EditAction::Split {
            at: 6_000,
            discard: false
        }));
        assert_eq!(ranges(&actions), (5_830, 370));
    }

    #[test]
    fn silence_budget_is_not_repeated_across_segment_boundaries() {
        let mut editor = Editor::new(timing(), 10).unwrap();
        let mut actions = editor.push(0, 100, SpeechState::Speech).unwrap();
        actions.extend(editor.push(100, 18_000, SpeechState::Silence).unwrap());
        actions.extend(editor.push(18_100, 100, SpeechState::Speech).unwrap());
        actions.extend(editor.finish());
        assert_eq!(ranges(&actions), (230, 17_970));
        assert!(actions.contains(&EditAction::Split {
            at: 12_000,
            discard: true
        }));
        assert!(actions.contains(&EditAction::Split {
            at: 6_000,
            discard: false
        }));
    }

    #[test]
    fn unknown_and_a_single_speech_sample_prevent_discard() {
        for state in [SpeechState::Speech, SpeechState::Unknown] {
            let mut editor = Editor::new(timing(), 10).unwrap();
            let mut actions = editor.push(0, 1, state).unwrap();
            actions.extend(editor.push(1, 6_099, SpeechState::Silence).unwrap());
            actions.extend(editor.finish());
            assert!(actions.contains(&EditAction::Split {
                at: 6_000,
                discard: false
            }));
            assert_eq!(ranges(&actions), (31, 6_069));
        }
    }

    #[test]
    fn unknown_is_not_trimmed() {
        let mut editor = Editor::new(timing(), 10).unwrap();
        let mut actions = editor.push(0, 12_000, SpeechState::Unknown).unwrap();
        actions.extend(editor.finish());
        assert_eq!(ranges(&actions), (12_000, 0));
        assert!(
            actions
                .iter()
                .all(|a| !matches!(a, EditAction::Split { discard: true, .. }))
        );
    }

    #[test]
    fn all_silent_full_segments_are_discarded_and_no_duplicate_final_split() {
        let mut editor = Editor::new(timing(), 10).unwrap();
        let mut actions = editor.push(0, 12_000, SpeechState::Silence).unwrap();
        actions.extend(editor.finish());
        let splits: Vec<_> = actions
            .iter()
            .filter_map(|a| match a {
                EditAction::Split { at, discard } => Some((*at, *discard)),
                _ => None,
            })
            .collect();
        assert_eq!(splits, vec![(6_000, true), (12_000, true)]);
        assert_eq!(ranges(&actions), (30, 11_970));
        assert!(editor.finish().is_empty());
    }

    #[test]
    fn whole_or_small_pushes_have_identical_keep_samples_and_boundaries() {
        fn execute(chunk: u64) -> (Vec<bool>, Vec<(u64, bool)>) {
            let mut editor = Editor::new(timing(), 10).unwrap();
            let mut actions = Vec::new();
            for (start, end, state) in [
                (0, 31, SpeechState::Speech),
                (31, 12_345, SpeechState::Silence),
                (12_345, 12_401, SpeechState::Unknown),
            ] {
                let mut cursor = start;
                while cursor < end {
                    let count = chunk.min(end - cursor);
                    actions.extend(editor.push(cursor, count, state).unwrap());
                    cursor += count;
                }
            }
            actions.extend(editor.finish());
            ranges(&actions);
            let mut kept = vec![false; 12_401];
            let mut splits = Vec::new();
            for action in actions {
                match action {
                    EditAction::Keep { start, end } => {
                        kept[start as usize..end as usize].fill(true)
                    }
                    EditAction::Split { at, discard } => splits.push((at, discard)),
                    _ => {}
                }
            }
            (kept, splits)
        }
        assert_eq!(execute(100_000), execute(1));
        assert_eq!(execute(100_000), execute(73));
    }

    #[test]
    fn repeated_silence_does_not_accumulate_history() {
        let mut editor = Editor::new(timing(), 10).unwrap();
        for start in (0..864_000).step_by(10) {
            editor.push(start, 10, SpeechState::Silence).unwrap();
            assert!(editor.boundaries.len() <= 1);
            assert!(editor.input_until - editor.settled_until <= 15);
        }
    }

    #[test]
    fn non_contiguous_input_and_bad_times_are_errors() {
        let mut editor = Editor::new(timing(), 10).unwrap();
        assert!(editor.push(1, 1, SpeechState::Silence).is_err());
        for invalid in [f64::NAN, f64::INFINITY, -1.0, 0.0] {
            let mut config = timing();
            config.segment_secs = invalid;
            assert!(Editor::new(config, 48_000).is_err());
        }
        assert!(Editor::new(timing(), 0).is_err());
    }

    #[test]
    fn discard_interval_can_be_longer_than_split_interval() {
        let mut config = timing();
        config.segment_secs = 100.0;
        let mut editor = Editor::new(config, 10).unwrap();
        let mut actions = editor.push(0, 6_000, SpeechState::Silence).unwrap();
        actions.extend(editor.finish());
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, EditAction::Split { .. }))
                .count(),
            1
        );
        assert!(actions.contains(&EditAction::Split {
            at: 6_000,
            discard: true
        }));
        ranges(&actions);
    }
}
