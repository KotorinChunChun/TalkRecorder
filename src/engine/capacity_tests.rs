use super::*;
use std::collections::VecDeque;

const TEST_LIMIT: u64 = 1_048_576;

struct ScriptedCapacity {
    readings: VecDeque<Result<Option<SpaceReading>>>,
}

impl CapacityChecks for ScriptedCapacity {
    fn poll(&mut self) -> Result<Option<SpaceReading>> {
        self.readings
            .pop_front()
            .expect("停止を要求した後に容量を再照会してはいけません")
    }
}

fn recording_status() -> SharedStatus {
    Arc::new(Mutex::new(Status {
        phase: RecorderPhase::Recording,
        running: true,
        message: "録音中".into(),
        ..Status::default()
    }))
}

fn reading(available: u64, limit: u64) -> Result<Option<SpaceReading>> {
    Ok(Some(SpaceReading {
        available_bytes: available,
        limit_bytes: limit,
    }))
}

#[test]
fn missing_disabled_and_above_limit_readings_do_not_stop_recording() -> Result<()> {
    let status = recording_status();
    let requested = AtomicBool::new(false);
    for sample in [Ok(None), reading(0, 0), reading(TEST_LIMIT + 1, TEST_LIMIT)] {
        assert!(!stop_for_capacity(sample, &status, &requested)?);
        assert!(!requested.load(Ordering::Acquire));
        let current = status.lock().unwrap();
        assert!(current.capacity_stop.is_none());
        assert_eq!(current.phase, RecorderPhase::Recording);
        assert!(current.running);
        assert_eq!(current.message, "録音中");
    }
    Ok(())
}

#[test]
fn equal_or_lower_capacity_stops_and_keeps_the_first_reason() -> Result<()> {
    for available in [TEST_LIMIT, TEST_LIMIT - 1] {
        let status = recording_status();
        let requested = AtomicBool::new(false);
        assert!(stop_for_capacity(
            reading(available, TEST_LIMIT),
            &status,
            &requested,
        )?);
        assert!(requested.load(Ordering::Acquire));
        assert!(stop_for_capacity(reading(1, 2), &status, &requested)?);
        let current = status.lock().unwrap();
        let Some(CapacityStopReason::LowSpace {
            available_bytes,
            limit_bytes,
        }) = &current.capacity_stop
        else {
            panic!("空き容量による停止理由が失われました")
        };
        assert_eq!(*available_bytes, available);
        assert_eq!(*limit_bytes, TEST_LIMIT);
        assert_eq!(current.phase, RecorderPhase::Finalizing);
        assert!(!current.running);
        assert!(current.message.contains("停止・保存中"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum StopMode {
    LowSpace,
    ProbeFailure,
}

fn packet(
    origin: u64,
    track: usize,
    channels: u16,
    start: u64,
    frames: u64,
    amplitude: f32,
) -> CaptureEvent {
    CaptureEvent::Packet {
        track,
        sample_rate: RATE as u32,
        channels,
        device_frame: start,
        qpc_100ns: origin + start * 10_000_000 / RATE,
        samples: vec![amplitude; frames as usize * usize::from(channels)],
        discontinuity: false,
        timestamp_valid: true,
    }
}

/// Use actual spool/alignment/editor/journal/finalizer code, with generated
/// samples and capacity readings. No WASAPI or model runs; the optional ignored
/// M4A variant alone uses the separately prepared encoder.
fn stopped_recording_keeps_buffered_audio(mode: StopMode, format: OutputFormat) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let encoder = std::env::var_os("TALKRECORDER_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/ffmpeg/bin/ffmpeg.exe")
        });
    if format == OutputFormat::M4a {
        ensure!(encoder.is_file(), "固定したFFmpegを先に準備してください");
    }
    let config = Config {
        format,
        auto_compact_wav: false,
        ffmpeg_path: encoder.clone(),
        ..Config::default()
    };
    let channels = vec![1, 2];
    let slots = [0, 2];
    let status = recording_status();
    let requested = AtomicBool::new(false);
    let session_stop = AtomicBool::new(false);
    let progress = Progress::default();
    let mut resume = ResumeState::default();
    let mut detector = ResumeDetector::new();
    let mut raw = Spool::create(&directory.path().join("raw"), channels.clone())?;
    let mut processed = Spool::create(&directory.path().join("processed"), channels.clone())?;

    let final_reading = match mode {
        StopMode::LowSpace => reading(TEST_LIMIT, TEST_LIMIT),
        StopMode::ProbeFailure => Err(anyhow!("fixture capacity probe failed")),
    };
    let mut capacity = ScriptedCapacity {
        readings: VecDeque::from([reading(TEST_LIMIT + 1, TEST_LIMIT), final_reading]),
    };
    // A successful initial check precedes the already-acquired prefix. The
    // next check happens in acquire and requests termination before ingestion.
    assert!(!stop_for_capacity(capacity.poll(), &status, &requested,)?);
    let origin = capture::qpc_100ns().saturating_sub(13_000_000);
    let mut aligner = Aligner::new(origin, channels.clone(), 4.0)?;
    for (track, &channel_count) in channels.iter().enumerate() {
        ingest(
            packet(origin, track, channel_count, 0, RATE, [0.25, 0.5][track]),
            &config,
            &slots,
            &mut aligner,
            &status,
        )?;
    }
    append_aligned(&mut aligner, &mut raw, RATE as usize, &progress)?;
    let (sender, receiver) = mpsc::sync_channel(8);
    for (track, &channel_count) in channels.iter().enumerate() {
        sender.send(packet(
            origin,
            track,
            channel_count,
            RATE,
            RATE / 8,
            [0.375, -0.25][track],
        ))?;
    }
    let acquired = acquire(
        &receiver,
        &config,
        &slots,
        origin,
        &mut aligner,
        &mut raw,
        &progress,
        &session_stop,
        &status,
        &requested,
        &mut resume,
        &mut detector,
        &mut capacity,
    );
    match mode {
        StopMode::LowSpace => acquired?,
        StopMode::ProbeFailure => assert!(
            format!("{:#}", acquired.unwrap_err()).contains("fixture capacity probe failed")
        ),
    }
    assert_eq!(raw.frames, RATE);
    assert!(session_stop.load(Ordering::Acquire));
    assert!(!requested.load(Ordering::Acquire));
    assert!(!resume.detected, "容量停止をスリープ復帰として再開しません");
    assert!(capacity.readings.is_empty());

    // A capture can release its final packet after the stop request. This
    // packet and those still in the queue must all pass through flush_capture.
    session_stop.store(true, Ordering::Release);
    for (track, &channel_count) in channels.iter().enumerate() {
        sender.send(packet(
            origin,
            track,
            channel_count,
            RATE + RATE / 8,
            RATE / 8,
            [-0.5, 0.75][track],
        ))?;
    }
    drop(sender);
    let end = resume.end(source_frames(origin), raw.frames);
    assert!(end >= RATE + RATE / 4);
    flush_capture(
        &receiver,
        &config,
        &slots,
        end,
        &mut aligner,
        &mut raw,
        &progress,
        &status,
    )?;
    assert_eq!(raw.frames, end);
    assert_eq!(progress.produced.load(Ordering::Acquire), end);

    // Unknown protects received audio; VAD and noise removal are outside this
    // generated-data test. Actual output editing and publication still run.
    let mut next = 0;
    while next < end {
        let until = (next + RATE).min(end);
        processed.append(&raw.read(next, until)?)?;
        next = until;
    }
    let mut output = Output::new(config, directory.path().into(), channels.clone())?;
    let actions = output.editor.push(0, end, SpeechState::Unknown)?;
    output.apply(actions, &mut processed)?;
    let actions = output.editor.finish();
    output.apply(actions, &mut processed)?;
    output.finalizer.wait()?;
    assert_eq!(output.saved_frames, end);
    assert!(directory.path().join("000001.complete.json").is_file());
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(directory.path().join("000001.complete.json"))?)?;
    assert_eq!(manifest["frames"].as_u64(), Some(end));
    assert_eq!(manifest["format"], serde_json::to_value(format)?);

    for (track, &channel_count) in channels.iter().enumerate() {
        let path =
            directory
                .path()
                .join(format!("000001_track{}.{}", track + 1, format.extension()));
        if format == OutputFormat::M4a {
            assert_m4a_duration(&path, &encoder, end, channel_count)?;
            continue;
        }
        let bytes = fs::read(path)?;
        assert_eq!(u64::from_le_bytes(bytes[36..44].try_into()?), end);
        assert_eq!(bytes.len() as u64, 80 + end * u64::from(channel_count) * 4);
        for (frame, amplitudes) in [
            (RATE / 2, [0.25, 0.5]),
            (RATE + RATE / 16, [0.375, -0.25]),
            (RATE + 3 * RATE / 16, [-0.5, 0.75]),
        ] {
            for channel in 0..u64::from(channel_count) {
                let offset = 80 + (frame * u64::from(channel_count) + channel) as usize * 4;
                let sample = f32::from_le_bytes(bytes[offset..offset + 4].try_into()?);
                assert!((sample - amplitudes[track]).abs() < 1e-6);
            }
        }
    }
    let current = status.lock().unwrap();
    match (&mode, &current.capacity_stop) {
        (
            StopMode::LowSpace,
            Some(CapacityStopReason::LowSpace {
                available_bytes,
                limit_bytes,
            }),
        ) => {
            assert_eq!(*available_bytes, TEST_LIMIT);
            assert_eq!(*limit_bytes, TEST_LIMIT);
        }
        (StopMode::ProbeFailure, Some(CapacityStopReason::ProbeFailed { message })) => {
            assert!(message.contains("fixture capacity probe failed"));
        }
        _ => panic!("停止理由が取得・flush・保存の間に失われました"),
    }
    Ok(())
}

fn assert_m4a_duration(path: &Path, encoder: &Path, frames: u64, channels: u16) -> Result<()> {
    use crate::storage::hide_console;
    use std::process::Command;

    let probe = encoder.with_file_name(if cfg!(windows) {
        "ffprobe.exe"
    } else {
        "ffprobe"
    });
    let mut command = Command::new(probe);
    hide_console(&mut command);
    let output = command
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=codec_name,channels,sample_rate,start_pts,time_base,duration_ts",
            "-of",
            "json",
        ])
        .arg(path)
        .output()?;
    ensure!(output.status.success(), "FFprobeの確認に失敗しました");
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let stream = &metadata["streams"][0];
    assert_eq!(stream["codec_name"], "aac");
    assert_eq!(stream["channels"], channels);
    assert_eq!(stream["sample_rate"], "48000");
    assert_eq!(stream["start_pts"], 0);
    assert_eq!(stream["time_base"], "1/48000");
    assert_eq!(stream["duration_ts"].as_u64(), Some(frames));

    let mut command = Command::new(encoder);
    hide_console(&mut command);
    let decoded = command
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "pipe:1",
        ])
        .output()?;
    ensure!(decoded.status.success(), "M4Aの実復号に失敗しました");
    assert_eq!(
        decoded.stdout.len() as u64,
        frames * u64::from(channels) * 4
    );
    let samples: Vec<_> = decoded
        .stdout
        .chunks_exact(4)
        .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
        .collect();
    assert!(samples.iter().all(|sample| sample.is_finite()));
    assert!(samples.iter().any(|sample| sample.abs() > 0.01));
    Ok(())
}

#[test]
fn low_capacity_flushes_queued_and_late_audio_to_synchronized_wav_files() -> Result<()> {
    stopped_recording_keeps_buffered_audio(StopMode::LowSpace, OutputFormat::Wav)
}

#[test]
fn capacity_probe_failure_still_flushes_and_publishes_received_audio() -> Result<()> {
    stopped_recording_keeps_buffered_audio(StopMode::ProbeFailure, OutputFormat::Wav)
}

#[test]
#[ignore = "requires the separately prepared FFmpeg encoder and FFprobe"]
fn low_capacity_flushes_received_audio_to_m4a_with_equal_decoded_frames() -> Result<()> {
    stopped_recording_keeps_buffered_audio(StopMode::LowSpace, OutputFormat::M4a)
}
