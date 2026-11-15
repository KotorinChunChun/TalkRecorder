//! Bounded alignment of independently clocked WASAPI streams to a 48 kHz axis.

use crate::capture::CaptureEvent;
use anyhow::{Result, bail, ensure};
use std::collections::VecDeque;

const OUTPUT_RATE: f64 = 48_000.0;
const MAX_PROVISIONAL_SPEEDUP: f64 = 0.005;

#[derive(Debug)]
pub struct AlignedBlock {
    /// Interleaved samples per track, using the channel counts passed to new().
    pub tracks: Vec<Vec<f32>>,
    /// False if any output frame in a track is missing or has unreliable timing.
    pub valid: Vec<bool>,
}

#[derive(Debug)]
struct Packet {
    start: f64,
    output_step: f64,
    samples: Vec<f32>,
    valid: bool,
}

impl Packet {
    fn frames(&self, channels: usize) -> usize {
        self.samples.len() / channels
    }

    fn end(&self, channels: usize) -> f64 {
        self.start + self.frames(channels) as f64 * self.output_step
    }
}

#[derive(Debug)]
struct Track {
    channels: usize,
    packets: VecDeque<Packet>,
    estimated_rate: Option<f64>,
    anchor: Option<(u64, u64)>,
    last_device_end: Option<u64>,
    next_hint: f64,
    provisional_clock: bool,
}

/// QPC anchors locate the start of every packet. A smoothed device-frame/QPC
/// slope estimates the sample clock between anchors; long runs never accumulate
/// a nominal-rate offset. All buffers and the retained clock history are bounded.
#[derive(Debug)]
pub struct Aligner {
    origin_qpc_100ns: u64,
    tracks: Vec<Track>,
    cursor: u64,
    max_buffer_frames: f64,
}

impl Aligner {
    pub fn new(origin_qpc_100ns: u64, channels: Vec<u16>, max_buffer_secs: f64) -> Result<Self> {
        ensure!(!channels.is_empty(), "同期対象のトラックがありません");
        ensure!(
            channels.iter().all(|value| (1..=32).contains(value)),
            "同期対象のチャンネル数が範囲外です"
        );
        ensure!(
            max_buffer_secs.is_finite() && (0.01..=3600.0).contains(&max_buffer_secs),
            "同期バッファの時間が範囲外です"
        );
        Ok(Self {
            origin_qpc_100ns,
            tracks: channels
                .into_iter()
                .map(|channels| Track {
                    channels: usize::from(channels),
                    packets: VecDeque::new(),
                    estimated_rate: None,
                    anchor: None,
                    last_device_end: None,
                    next_hint: 0.0,
                    provisional_clock: false,
                })
                .collect(),
            cursor: 0,
            max_buffer_frames: max_buffer_secs * OUTPUT_RATE,
        })
    }

    pub fn ingest(&mut self, event: CaptureEvent) -> Result<()> {
        if let CaptureEvent::Connection {
            track, connected, ..
        } = &event
        {
            let stream = self
                .tracks
                .get_mut(*track)
                .ok_or_else(|| anyhow::anyhow!("再接続トラック番号が範囲外です: {track}"))?;
            if !connected {
                // Keep the already received tail instead of replacing real
                // audio with zeros while the other tracks continue. Its timing
                // is not trusted across the disconnect, so it stays Unknown.
                for packet in &mut stream.packets {
                    packet.valid = false;
                }
                stream.anchor = None;
                stream.last_device_end = None;
                stream.estimated_rate = None;
                stream.next_hint = stream.next_hint.max(self.cursor as f64).max(
                    stream
                        .packets
                        .back()
                        .map_or(self.cursor as f64, |packet| packet.end(stream.channels)),
                );
                stream.provisional_clock = true;
            }
            return Ok(());
        }
        let CaptureEvent::Packet {
            track,
            sample_rate,
            channels,
            device_frame,
            qpc_100ns,
            samples,
            discontinuity,
            timestamp_valid,
        } = event
        else {
            return match event {
                CaptureEvent::Error { track, message } => {
                    bail!("録音トラック {track} の取得に失敗しました: {message}")
                }
                CaptureEvent::Ready { .. } | CaptureEvent::Timing { .. } => Ok(()),
                _ => unreachable!(),
            };
        };
        let stream = self
            .tracks
            .get_mut(track)
            .ok_or_else(|| anyhow::anyhow!("同期トラック番号が範囲外です: {track}"))?;
        ensure!(
            stream.channels == usize::from(channels),
            "取得中にトラックのチャンネル数が変化しました"
        );
        ensure!(
            (8_000..=384_000).contains(&sample_rate),
            "入力サンプルレートが範囲外です"
        );
        ensure!(
            samples.len().is_multiple_of(stream.channels),
            "入力音声が完全なフレームになっていません"
        );
        ensure!(
            samples.iter().all(|sample| sample.is_finite()),
            "入力音声に非有限サンプルがあります"
        );
        if samples.is_empty() {
            return Ok(());
        }
        let original_frames = samples.len() / stream.channels;
        let original_end = device_frame
            .checked_add(original_frames as u64)
            .ok_or_else(|| anyhow::anyhow!("デバイスのサンプル位置が上限を超えました"))?;
        if discontinuity {
            stream.anchor = None;
            stream.last_device_end = None;
            stream.estimated_rate = None;
        }
        // Repeated and overlapping packets must not replay already ingested
        // audio. Only a discontinuity permits the device position to restart.
        let trim = if let Some(previous_end) = stream.last_device_end {
            if original_end <= previous_end {
                return Ok(());
            }
            previous_end.saturating_sub(device_frame) as usize
        } else {
            0
        };
        let nominal_rate = f64::from(sample_rate);
        let rate = stream.estimated_rate.get_or_insert(nominal_rate);
        let provisional_start = stream.next_hint.max(self.cursor as f64).max(
            stream
                .packets
                .back()
                .map_or(self.cursor as f64, |packet| packet.end(stream.channels)),
        );
        let elapsed = i128::from(qpc_100ns) - i128::from(self.origin_qpc_100ns);
        let clock_start = elapsed as f64 * OUTPUT_RATE / 10_000_000.0;
        // A recovered input clock can be trusted for rate estimation before its
        // phase has caught up with the already retained provisional tail. Keep
        // these two kinds of validity separate; otherwise a slightly faster
        // recovered clock can remain Unknown forever at nominal output speed.
        if timestamp_valid {
            if let Some((previous_frame, previous_qpc)) = stream.anchor
                && device_frame > previous_frame
                && qpc_100ns > previous_qpc
            {
                let observed = (device_frame - previous_frame) as f64 * 10_000_000.0
                    / (qpc_100ns - previous_qpc) as f64;
                // Ignore impossible slopes instead of allowing timestamp jitter
                // or a corrupted anchor to produce an extreme resampling ratio.
                if (nominal_rate * 0.95..=nominal_rate * 1.05).contains(&observed) {
                    *rate += 0.05 * (observed - *rate);
                }
            }
            stream.anchor = Some((device_frame, qpc_100ns));
        }
        let mut output_step = OUTPUT_RATE / *rate;
        let timed_start = clock_start + trim as f64 * output_step;
        let overlap = stream.provisional_clock && timed_start + 1e-7 < provisional_start;
        let behind_cursor = timed_start + 1e-7 < self.cursor as f64;
        let placement_valid = timestamp_valid && !overlap && !behind_cursor;
        let start = if placement_valid {
            // Subtract as integers first: absolute QPC may have been running for
            // months, but nearby packet positions retain sub-sample precision.
            timed_start
        } else {
            // An invalid timestamp is only placed provisionally. Its samples
            // remain Unknown even when interpolation is otherwise possible.
            provisional_start
        };
        if timestamp_valid && !placement_valid {
            // Append, never move or overwrite the retained tail. Resample only
            // this new provisional packet up to 0.5% faster, and never shorten
            // it by more than the remaining phase difference. This bounded
            // servo lets a recovered clock catch the tail without dropping a
            // packet or labelling the transition as valid silence.
            let count = original_frames - trim;
            let phase = (provisional_start - timed_start).max(0.0);
            output_step -= (phase / count as f64).min(output_step * MAX_PROVISIONAL_SPEEDUP);
        }
        stream.provisional_clock = !placement_valid;
        let samples = if trim == 0 {
            samples
        } else {
            samples[trim * stream.channels..].to_vec()
        };
        let packet = Packet {
            start,
            output_step,
            samples,
            valid: placement_valid && !discontinuity,
        };
        let end = packet.end(stream.channels);
        ensure!(
            end - self.cursor as f64 <= self.max_buffer_frames + 2.0,
            "同期バッファの将来音声が上限を超えました"
        );
        // Never allow timestamp regression to reorder a stream silently.
        if let Some(previous) = stream.packets.back() {
            ensure!(
                packet.start >= previous.start,
                "デバイスの共通時刻が逆戻りしました"
            );
        }
        stream.next_hint = end;
        stream.last_device_end = Some(original_end);
        if end <= self.cursor as f64 {
            return Ok(());
        }
        while stream.packets.len() > 1 && stream.packets[1].start <= self.cursor as f64 {
            stream.packets.pop_front();
        }
        let buffered = stream
            .packets
            .iter()
            .map(|packet| packet.frames(stream.channels) as f64 * packet.output_step)
            .sum::<f64>()
            + packet.frames(stream.channels) as f64 * packet.output_step;
        ensure!(
            buffered <= self.max_buffer_frames + 4.0,
            "同期バッファの音声量が上限を超えました"
        );
        stream.packets.push_back(packet);
        Ok(())
    }

    pub fn take(&mut self, start: u64, frames: usize) -> Result<AlignedBlock> {
        ensure!(start >= self.cursor, "確定済みの音声位置へ逆戻りできません");
        ensure!(
            frames as f64 <= self.max_buffer_frames,
            "取り出す音声が同期バッファ上限を超えています"
        );
        let end = start
            .checked_add(frames as u64)
            .ok_or_else(|| anyhow::anyhow!("音声のサンプル位置が上限を超えました"))?;
        let mut tracks = Vec::with_capacity(self.tracks.len());
        let mut valid = Vec::with_capacity(self.tracks.len());
        for stream in &mut self.tracks {
            let sample_count = frames
                .checked_mul(stream.channels)
                .ok_or_else(|| anyhow::anyhow!("音声バッファが大きすぎます"))?;
            let mut output = vec![0.0; sample_count];
            let mut complete = true;
            for offset in 0..frames {
                let position = start as f64 + offset as f64;
                // Keep only the packet that can cover this position and future
                // packets. No history grows with the duration of a recording.
                while stream.packets.len() > 1 && stream.packets[1].start <= position {
                    stream.packets.pop_front();
                }
                let frame_output =
                    &mut output[offset * stream.channels..(offset + 1) * stream.channels];
                if !interpolate(stream, position, frame_output) {
                    complete = false;
                }
            }
            while stream
                .packets
                .front()
                .is_some_and(|packet| packet.end(stream.channels) <= end as f64)
            {
                stream.packets.pop_front();
            }
            tracks.push(output);
            valid.push(complete);
        }
        self.cursor = end;
        Ok(AlignedBlock { tracks, valid })
    }
}

fn interpolate(stream: &Track, position: f64, output: &mut [f32]) -> bool {
    let Some(packet) = stream.packets.front() else {
        return false;
    };
    let input_position = (position - packet.start) / packet.output_step;
    if input_position < -1e-7 {
        return false;
    }
    let input_position = input_position.max(0.0);
    let count = packet.frames(stream.channels);
    let index = input_position.floor() as usize;
    if index + 1 < count {
        let mix = input_position.fract() as f32;
        for (channel, output_sample) in output.iter_mut().enumerate() {
            let a = packet.samples[index * stream.channels + channel];
            let b = packet.samples[(index + 1) * stream.channels + channel];
            *output_sample = a + (b - a) * mix;
        }
        return packet.valid;
    }
    let last_position = packet.start + (count - 1) as f64 * packet.output_step;
    if let Some(next) = stream.packets.get(1) {
        let distance = next.start - last_position;
        if distance > 0.0
            && distance <= packet.output_step * 2.5
            && position >= last_position
            && position < next.start
        {
            let mix = ((position - last_position) / distance) as f32;
            for (channel, output_sample) in output.iter_mut().enumerate() {
                let a = packet.samples[(count - 1) * stream.channels + channel];
                let b = next.samples[channel];
                *output_sample = a + (b - a) * mix;
            }
            return packet.valid && next.valid;
        }
    }
    if input_position < count as f64 {
        // At a final packet's sub-sample edge there is no future sample yet.
        // Hold that last sample; never synthesize a whole missing frame interval.
        output.copy_from_slice(&packet.samples[(count - 1) * stream.channels..]);
        return packet.valid;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_track_preserves_its_received_tail_as_unknown_and_accepts_reset_position() {
        let mut aligner = Aligner::new(0, vec![1, 1], 2.0).unwrap();
        aligner
            .ingest(packet(0, 48_000, 1000, 0, vec![0.25; 960]))
            .unwrap();
        aligner
            .ingest(packet(1, 48_000, 0, 0, vec![0.75; 960]))
            .unwrap();
        aligner
            .ingest(CaptureEvent::Connection {
                track: 0,
                connected: false,
                message: "切断".into(),
            })
            .unwrap();
        let gap = aligner.take(0, 480).unwrap();
        assert_eq!(gap.valid, vec![false, true]);
        assert!(gap.tracks[0].iter().all(|&s| s == 0.25));
        assert!(gap.tracks[1].iter().all(|&s| s == 0.75));
        aligner
            .ingest(CaptureEvent::Connection {
                track: 0,
                connected: true,
                message: "復帰".into(),
            })
            .unwrap();
        let mut restarted = packet(0, 44_100, 0, 100_000, vec![0.5; 441]);
        if let CaptureEvent::Packet { discontinuity, .. } = &mut restarted {
            *discontinuity = true;
        }
        aligner.ingest(restarted).unwrap();
        let first = aligner.take(480, 480).unwrap();
        assert_eq!(first.valid, vec![false, true]);
        assert!(first.tracks[0].iter().all(|&s| s == 0.25));
        aligner
            .ingest(packet(0, 44_100, 441, 200_000, vec![0.6; 441]))
            .unwrap();
        let retained = aligner.take(960, 480).unwrap();
        assert!(!retained.valid[0]);
        assert!(retained.tracks[0][..477].iter().all(|&s| s == 0.5));
        // Only the new recovery packet is shortened by the bounded servo. Its
        // final fractional sample interpolates into the following retained one.
        assert!((0.5..0.6).contains(&retained.tracks[0][477]));
        assert!(retained.tracks[0][478..].iter().all(|&s| s == 0.6));
        let provisional = aligner.take(1440, 480).unwrap();
        assert!(!provisional.valid[0]);
        assert!(provisional.tracks[0][..476].iter().all(|&s| s == 0.6));
        assert!(provisional.tracks[0][476..].iter().all(|&s| s == 0.0));
        aligner
            .ingest(packet(0, 44_100, 882, 400_000, vec![0.7; 441]))
            .unwrap();
        let resumed = aligner.take(1920, 480).unwrap();
        assert!(resumed.valid[0]);
        assert!(resumed.tracks[0].iter().all(|&s| s == 0.7));
    }

    fn packet(
        track: usize,
        sample_rate: u32,
        frame: u64,
        qpc: u64,
        samples: Vec<f32>,
    ) -> CaptureEvent {
        CaptureEvent::Packet {
            track,
            sample_rate,
            channels: 1,
            device_frame: frame,
            qpc_100ns: qpc,
            samples,
            discontinuity: false,
            timestamp_valid: true,
        }
    }

    #[test]
    fn offset_and_44100_resampling_share_the_48000_axis() {
        let mut aligner = Aligner::new(100_000_000, vec![1, 1], 2.0).unwrap();
        aligner
            .ingest(packet(0, 48_000, 0, 100_000_000, vec![0.25; 48_000]))
            .unwrap();
        aligner
            .ingest(packet(1, 44_100, 0, 105_000_000, vec![0.75; 44_100]))
            .unwrap();
        let first = aligner.take(0, 24_000).unwrap();
        assert_eq!(first.valid, vec![true, false]);
        assert!(first.tracks[0].iter().all(|sample| *sample == 0.25));
        assert!(first.tracks[1].iter().all(|sample| *sample == 0.0));
        let second = aligner.take(24_000, 24_000).unwrap();
        assert_eq!(second.valid, vec![true, true]);
        assert!(second.tracks[1].iter().all(|sample| *sample == 0.75));
        assert_eq!(second.tracks[0].len(), second.tracks[1].len());
    }

    #[test]
    fn linear_interpolation_preserves_channels() {
        let mut aligner = Aligner::new(0, vec![2], 1.0).unwrap();
        aligner
            .ingest(CaptureEvent::Packet {
                track: 0,
                sample_rate: 24_000,
                channels: 2,
                device_frame: 0,
                qpc_100ns: 0,
                samples: vec![0.0, 1.0, 1.0, 0.0, 0.0, -1.0],
                discontinuity: false,
                timestamp_valid: true,
            })
            .unwrap();
        let block = aligner.take(0, 4).unwrap();
        assert_eq!(block.valid, vec![true]);
        assert_eq!(
            block.tracks[0],
            vec![0.0, 1.0, 0.5, 0.5, 1.0, 0.0, 0.5, -0.5]
        );
    }

    #[test]
    fn missing_and_bad_timestamps_are_never_valid_silence() {
        let mut aligner = Aligner::new(0, vec![1], 1.0).unwrap();
        let mut event = packet(0, 48_000, 0, 0, vec![0.5; 480]);
        if let CaptureEvent::Packet {
            timestamp_valid, ..
        } = &mut event
        {
            *timestamp_valid = false;
        }
        aligner.ingest(event).unwrap();
        assert!(!aligner.take(0, 480).unwrap().valid[0]);
        assert!(!aligner.take(480, 480).unwrap().valid[0]);
        let mut event = packet(0, 48_000, 960, 200_000, vec![0.25; 480]);
        if let CaptureEvent::Packet { discontinuity, .. } = &mut event {
            *discontinuity = true;
        }
        aligner.ingest(event).unwrap();
        assert!(!aligner.take(960, 480).unwrap().valid[0]);
    }

    #[test]
    fn late_untrusted_packet_is_not_discarded_behind_the_cursor() {
        let mut aligner = Aligner::new(0, vec![1], 2.0).unwrap();
        aligner.take(0, 48_000).unwrap();
        let mut event = packet(0, 48_000, 0, 0, vec![0.5; 480]);
        if let CaptureEvent::Packet {
            timestamp_valid, ..
        } = &mut event
        {
            *timestamp_valid = false;
        }
        aligner.ingest(event).unwrap();
        let output = aligner.take(48_000, 480).unwrap();
        assert_eq!(output.valid, vec![false]);
        assert!(output.tracks[0].iter().all(|sample| *sample == 0.5));
    }

    #[test]
    fn recovering_clock_never_overwrites_provisional_audio() {
        let mut aligner = Aligner::new(0, vec![1], 2.0).unwrap();
        aligner.take(0, 480).unwrap();
        let mut event = packet(0, 48_000, 0, 0, vec![0.25; 480]);
        if let CaptureEvent::Packet {
            timestamp_valid, ..
        } = &mut event
        {
            *timestamp_valid = false;
        }
        aligner.ingest(event).unwrap();
        aligner
            .ingest(packet(0, 48_000, 480, 100_000, vec![0.75; 480]))
            .unwrap();
        let output = aligner.take(480, 960).unwrap();
        assert_eq!(output.valid, vec![false]);
        assert!(output.tracks[0][..480].iter().all(|sample| *sample == 0.25));
        assert!(
            output.tracks[0][480..]
                .iter()
                .filter(|&&sample| sample == 0.75)
                .count()
                >= 477
        );
        assert!(
            output.tracks[0][480..]
                .iter()
                .all(|sample| *sample == 0.75 || *sample == 0.0)
        );
        aligner
            .ingest(packet(0, 48_000, 960, 300_000, vec![0.9; 480]))
            .unwrap();
        let restored = aligner.take(1440, 480).unwrap();
        assert_eq!(restored.valid, vec![true]);
        assert!(restored.tracks[0].iter().all(|sample| *sample == 0.9));
    }

    #[test]
    fn trusted_packet_behind_the_committed_cursor_is_retained_as_unknown() {
        let mut aligner = Aligner::new(0, vec![1], 2.0).unwrap();
        aligner.take(0, 48_000).unwrap();
        aligner
            .ingest(packet(0, 48_000, 0, 0, vec![0.5; 480]))
            .unwrap();
        let packet = aligner.tracks[0].packets.back().unwrap();
        assert!(!packet.valid);
        assert_eq!(packet.start, 48_000.0);
        assert!(packet.output_step >= 1.0 - MAX_PROVISIONAL_SPEEDUP);
        let output = aligner.take(48_000, 477).unwrap();
        assert_eq!(output.valid, vec![false]);
        assert!(output.tracks[0].iter().all(|sample| *sample == 0.5));
    }

    #[test]
    fn recovery_servo_catches_a_persistent_overlap_without_moving_old_audio() {
        let mut aligner = Aligner::new(0, vec![1], 4.0).unwrap();
        aligner.take(0, 480).unwrap();
        let mut untrusted = packet(0, 48_000, 0, 0, vec![0.25; 480]);
        if let CaptureEvent::Packet {
            timestamp_valid, ..
        } = &mut untrusted
        {
            *timestamp_valid = false;
        }
        aligner.ingest(untrusted).unwrap();
        let mut previous_end = 960.0;
        let mut valid = false;
        for index in 1..=250 {
            aligner
                .ingest(packet(
                    0,
                    48_000,
                    index * 480,
                    index * 100_000,
                    vec![0.75; 480],
                ))
                .unwrap();
            let packet = aligner.tracks[0].packets.back().unwrap();
            assert!(packet.output_step >= 1.0 - MAX_PROVISIONAL_SPEEDUP);
            if !packet.valid {
                assert!((packet.start - previous_end).abs() < 1e-6);
            }
            previous_end = packet.end(1);
            valid |= packet.valid;
        }
        assert!(
            valid,
            "a persistent phase difference must eventually recover"
        );
        let retained = aligner.take(480, 480).unwrap();
        assert!(!retained.valid[0]);
        assert!(retained.tracks[0].iter().all(|sample| *sample == 0.25));
    }

    #[test]
    fn gap_does_not_get_interpolated_into_a_valid_recording() {
        let mut aligner = Aligner::new(0, vec![1], 1.0).unwrap();
        aligner
            .ingest(packet(0, 48_000, 0, 0, vec![0.5; 480]))
            .unwrap();
        aligner
            .ingest(packet(0, 48_000, 960, 200_000, vec![0.75; 480]))
            .unwrap();
        let block = aligner.take(0, 1440).unwrap();
        assert!(!block.valid[0]);
        assert!(
            block.tracks[0][480..960]
                .iter()
                .all(|sample| *sample == 0.0)
        );
    }

    #[test]
    fn repeated_and_overlapping_packets_do_not_replay_samples() {
        let mut aligner = Aligner::new(0, vec![1], 1.0).unwrap();
        aligner
            .ingest(packet(0, 48_000, 0, 0, vec![0.25; 480]))
            .unwrap();
        aligner
            .ingest(packet(0, 48_000, 0, 0, vec![0.75; 480]))
            .unwrap();
        aligner
            .ingest(packet(0, 48_000, 240, 50_000, vec![0.5; 480]))
            .unwrap();
        let output = aligner.take(0, 720).unwrap();
        assert!(output.valid[0]);
        assert!(output.tracks[0][..480].iter().all(|sample| *sample == 0.25));
        assert!(output.tracks[0][480..].iter().all(|sample| *sample == 0.5));
    }

    #[test]
    fn future_audio_has_a_hard_limit_and_read_position_is_monotonic() {
        let mut aligner = Aligner::new(0, vec![1], 0.1).unwrap();
        assert!(
            aligner
                .ingest(packet(0, 48_000, 0, 0, vec![0.0; 4_801 + 2]))
                .is_err()
        );
        aligner.take(0, 480).unwrap();
        assert!(aligner.take(0, 480).is_err());
        assert!(
            aligner
                .ingest(packet(0, 48_000, 0, 2_000_000, vec![0.0; 480]))
                .is_err()
        );
    }

    #[test]
    fn one_hour_100ppm_drift_keeps_timing_and_bounded_history() {
        let actual_rate = 48_000.0 * 1.0001;
        let mut aligner = Aligner::new(0, vec![1], 1.0).unwrap();
        // Use short local windows one hour apart: correlation retains the clock
        // estimate, while take() advances past the gap without storing the hour.
        for block in 0..40_u64 {
            let device_frame = block * 480;
            let qpc = (device_frame as f64 / actual_rate * 10_000_000.0).round() as u64;
            aligner
                .ingest(packet(0, 48_000, device_frame, qpc, vec![0.25; 480]))
                .unwrap();
            if block > 0 {
                let start = ((block - 1) as f64 * 480.0 / 1.0001).floor() as u64;
                let end = (block as f64 * 480.0 / 1.0001).floor() as u64;
                aligner.take(start, (end - start) as usize).unwrap();
            }
        }
        let hour_frame = (3600.0 * actual_rate) as u64;
        let hour_qpc = (hour_frame as f64 / actual_rate * 10_000_000.0).round() as u64;
        // No audio need be retained across an explicitly skipped gap.
        aligner.take(172_800_000 - 480, 480).unwrap();
        aligner
            .ingest(packet(0, 48_000, hour_frame, hour_qpc, vec![0.75; 960]))
            .unwrap();
        let aligned_start = (hour_qpc as f64 * OUTPUT_RATE / 10_000_000.0).ceil() as u64;
        let output = aligner.take(aligned_start, 480).unwrap();
        assert!(output.valid[0]);
        assert!(output.tracks[0].iter().all(|sample| *sample == 0.75));
        let rate = aligner.tracks[0].estimated_rate.unwrap();
        assert!((rate - actual_rate).abs() < 1.0);
        assert!(aligner.tracks[0].packets.len() <= 2);
        // A nominal-clock-only implementation would drift by 17,280 frames.
        assert!(hour_frame - aligned_start > 17_000);
    }
}
