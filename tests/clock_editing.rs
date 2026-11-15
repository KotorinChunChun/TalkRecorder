//! Synthetic clock/alignment/editor integration, deliberately not a VAD test.
//! The input is known speech-labelled constant signal or exact digital zero.
//! No native device, model, encoder, or user audio is accessed by this test.
use talk_recorder::{
    SpeechState,
    capture::CaptureEvent,
    clock_sync::{ClockMode, ClockObservation, ClockRecovery},
    editor::{EditAction, Editor, TimingConfig},
    sync_audio::{AlignedBlock, Aligner},
};

const RATE: u64 = 48_000;
const NATIVE_RATE: u32 = 44_100;
const PACKET_FRAMES: u32 = 441;
const EPOCH: u64 = 21_000_000_000_000;
const TOTAL_SECONDS: u64 = 80;

struct Input {
    track: usize,
    clock: ClockObservation,
    samples: Vec<f32>,
}

#[derive(Clone, Copy, Default)]
struct SignalSpan {
    start: Option<u64>,
    end: Option<u64>,
}

struct ObservationLog {
    common_states: Vec<SpeechState>,
    track_valid: [Vec<bool>; 2],
    signals: [[SignalSpan; 2]; 2],
    actions: Vec<EditAction>,
}

fn labelled_speech(seconds: f64) -> bool {
    (10.0..21.0).contains(&seconds) || (40.0..51.0).contains(&seconds)
}

fn inputs(remote_ppm: f64) -> Vec<Input> {
    let mut inputs = Vec::with_capacity(16_000);
    for track in 0..2 {
        let actual_rate = f64::from(NATIVE_RATE)
            * (1.0
                + if track == 0 {
                    remote_ppm / 1_000_000.0
                } else {
                    0.0
                });
        for packet in 0..TOTAL_SECONDS * 100 {
            let frame = packet * u64::from(PACKET_FRAMES);
            let start_ticks = (frame as f64 / actual_rate * 10_000_000.0).round() as u64;
            let end_ticks = ((frame + u64::from(PACKET_FRAMES)) as f64 / actual_rate * 10_000_000.0)
                .round() as u64;
            let jitter = (packet % 7) * 5_000; // 0..3 ms, always monotonic arrivals.
            let transport = if track == 0 { 200_000 } else { 0 }; // 20 ms unknown fixed delay.
            inputs.push(Input {
                track,
                clock: ClockObservation {
                    device_frame: frame,
                    frames: PACKET_FRAMES,
                    sample_rate: NATIVE_RATE,
                    raw_qpc_100ns: if track == 0 {
                        100_000_000 + start_ticks // Different epoch, never a local QPC.
                    } else {
                        EPOCH + start_ticks
                    },
                    received_qpc_100ns: EPOCH + end_ticks + transport + jitter,
                    native_timestamp_valid: track == 1,
                    discontinuity: false,
                },
                samples: (0..PACKET_FRAMES)
                    .map(|index| {
                        if labelled_speech((frame + u64::from(index)) as f64 / actual_rate) {
                            0.5
                        } else {
                            0.0
                        }
                    })
                    .collect(),
            });
        }
    }
    inputs.sort_by_key(|input| input.clock.received_qpc_100ns);
    inputs
}

fn consume(
    start: u64,
    block: AlignedBlock,
    editor: &mut Editor,
    log: &mut ObservationLog,
) -> anyhow::Result<()> {
    let mut states = [SpeechState::Unknown; 2];
    for (track, samples) in block.tracks.iter().enumerate() {
        log.track_valid[track].push(block.valid[track]);
        let mut signal = false;
        for (offset, &sample) in samples.iter().enumerate() {
            assert!(sample.is_finite());
            if sample > 0.25 {
                signal = true;
                let frame = start + offset as u64;
                let burst = if (8 * RATE..25 * RATE).contains(&frame) {
                    0
                } else if (38 * RATE..55 * RATE).contains(&frame) {
                    1
                } else {
                    panic!("signal was placed outside its expected marker window: {frame}");
                };
                let span = &mut log.signals[track][burst];
                span.start.get_or_insert(frame);
                span.end = Some(frame + 1);
            }
        }
        if block.valid[track] {
            states[track] = if signal {
                SpeechState::Speech
            } else {
                SpeechState::Silence
            };
        }
    }
    // Same common-state rule as the production engine. Signal/zero is known
    // synthetic ground truth here; no model speech result is fabricated.
    let common = if states.contains(&SpeechState::Speech) {
        SpeechState::Speech
    } else if states.iter().all(|state| *state == SpeechState::Silence) {
        SpeechState::Silence
    } else {
        SpeechState::Unknown
    };
    log.common_states.push(common);
    log.actions.extend(editor.push(start, RATE, common)?);
    Ok(())
}

fn overlap(start: u64, end: u64, other_start: u64, other_end: u64) -> u64 {
    end.min(other_end).saturating_sub(start.max(other_start))
}

fn check_pipeline(ppm: f64) -> anyhow::Result<()> {
    let mut clocks = [ClockRecovery::new(), ClockRecovery::new()];
    let mut modes = [None, None];
    let mut aligner = Aligner::new(EPOCH, vec![1, 1], 30.0)?;
    let mut editor = Editor::new(
        TimingConfig {
            segment_secs: 20.0,
            split_silence_secs: 10.0,
            keep_silence_secs: 3.0,
            discard_silence_secs: 600.0,
        },
        RATE as u32,
    )?;
    let mut log = ObservationLog {
        common_states: Vec::new(),
        track_valid: [Vec::new(), Vec::new()],
        signals: [[SignalSpan::default(); 2]; 2],
        actions: Vec::new(),
    };
    let mut next = 0;
    let mut remote_recovered = false;
    for input in inputs(ppm) {
        let estimate = clocks[input.track].observe(input.clock);
        if input.track == 0 && estimate.valid {
            assert_eq!(estimate.mode, ClockMode::Recovered);
            remote_recovered = true;
        }
        if modes[input.track] != Some(estimate.mode) {
            aligner.ingest(CaptureEvent::Timing {
                track: input.track,
                received_qpc_100ns: input.clock.received_qpc_100ns,
                device_frame: input.clock.device_frame,
                packet_frames: input.clock.frames,
                raw_qpc_100ns: input.clock.raw_qpc_100ns,
                estimate,
            })?;
            modes[input.track] = Some(estimate.mode);
        }
        aligner.ingest(CaptureEvent::Packet {
            track: input.track,
            sample_rate: input.clock.sample_rate,
            channels: 1,
            device_frame: input.clock.device_frame,
            qpc_100ns: estimate.qpc_100ns,
            samples: input.samples,
            discontinuity: input.clock.discontinuity,
            timestamp_valid: estimate.valid,
        })?;
        let elapsed = (input.clock.received_qpc_100ns - EPOCH) as f64 / 10_000_000.0;
        // Match the production 300 ms capture lag and one-second analysis blocks.
        while elapsed >= next as f64 / RATE as f64 + 1.3 {
            consume(
                next,
                aligner.take(next, RATE as usize)?,
                &mut editor,
                &mut log,
            )?;
            next += RATE;
        }
    }
    while next < TOTAL_SECONDS * RATE {
        consume(
            next,
            aligner.take(next, RATE as usize)?,
            &mut editor,
            &mut log,
        )?;
        next += RATE;
    }
    log.actions.extend(editor.finish());
    assert!(remote_recovered);
    for (track, validity) in log.track_valid.iter().enumerate() {
        assert_eq!(validity.len(), TOTAL_SECONDS as usize);
        assert!(
            validity[10..].iter().all(|valid| *valid),
            "{ppm} ppm: steady track {track} has invalid seconds {:?}",
            validity
                .iter()
                .enumerate()
                .filter_map(|(i, v)| (!v).then_some(i))
                .collect::<Vec<_>>()
        );
    }
    // We intentionally retain the fixed receiver-side 20 ms delay in this
    // synthetic case; it must not accumulate as clock drift or be claimed zero.
    let mut marker_lags = Vec::new();
    for burst in 0..2 {
        let mic = log.signals[0][burst];
        let speaker = log.signals[1][burst];
        let lag = (mic.start.unwrap() as i64 - speaker.start.unwrap() as i64) as f64 / RATE as f64;
        assert!((lag - 0.020).abs() < 0.010, "{ppm} ppm: marker lag {lag}");
        assert!((mic.end.unwrap() as i64 - speaker.end.unwrap() as i64).abs() < (RATE / 20) as i64);
        marker_lags.push(lag);
    }
    assert!(
        (marker_lags[1] - marker_lags[0]).abs() < 0.001,
        "{ppm} ppm: synthetic marker lag drifted: {marker_lags:?}"
    );

    let mut silence_runs = Vec::new();
    let mut silence_start = None;
    for second in 0..=log.common_states.len() {
        if log.common_states.get(second) == Some(&SpeechState::Silence) {
            silence_start.get_or_insert(second as u64 * RATE);
        } else if let Some(start) = silence_start.take() {
            silence_runs.push((start, second as u64 * RATE));
        }
    }
    assert!(
        silence_runs
            .iter()
            .filter(|(a, b)| b - a >= 10 * RATE)
            .count()
            >= 2
    );
    let mut cursor = 0;
    let mut total_cut = 0;
    let mut split_count = 0;
    for action in &log.actions {
        match *action {
            EditAction::Keep { start, end } => {
                assert_eq!(start, cursor);
                cursor = end;
            }
            EditAction::Cut { start, end } => {
                assert_eq!(start, cursor);
                cursor = end;
                total_cut += end - start;
                assert!(
                    silence_runs.iter().any(|&(a, b)| a <= start && end <= b),
                    "a cut included unknown or synthetic signal"
                );
            }
            EditAction::Split { at, discard } => {
                assert_eq!(at, cursor);
                assert!(!discard);
                split_count += 1;
            }
        }
    }
    assert_eq!(cursor, TOTAL_SECONDS * RATE);
    assert!(total_cut > 30 * RATE);
    assert!(split_count >= 3);
    for (start, end) in silence_runs {
        let kept: u64 = log
            .actions
            .iter()
            .map(|action| match *action {
                EditAction::Keep { start: a, end: b } => overlap(start, end, a, b),
                _ => 0,
            })
            .sum();
        assert_eq!(
            kept,
            (end - start).min(3 * RATE),
            "{ppm} ppm: silence {start}..{end} did not retain one shared three-second budget"
        );
    }
    println!(
        "{ppm:+} ppm: all steady blocks valid, marker lags {marker_lags:?}, common cut={}s, splits={split_count}",
        total_cut as f64 / RATE as f64
    );
    Ok(())
}

#[test]
fn recovered_44100_mic_and_native_speaker_cut_known_common_silence_after_positive_drift()
-> anyhow::Result<()> {
    check_pipeline(100.0)
}

#[test]
fn recovered_44100_mic_and_native_speaker_cut_known_common_silence_after_negative_drift()
-> anyhow::Result<()> {
    check_pipeline(-100.0)
}
