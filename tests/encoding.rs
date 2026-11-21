//! Optional integration checks against the pinned, separately prepared encoder.

use std::path::{Path, PathBuf};
use std::process::Command;
use talk_recorder::config::OutputFormat;
use talk_recorder::storage::{SegmentWriter, hide_console};

fn encoder_path() -> PathBuf {
    std::env::var_os("TALKRECORDER_FFMPEG")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime/ffmpeg/bin/ffmpeg.exe")
        })
}

fn decode(path: &Path, encoder: &Path) -> Vec<u8> {
    let mut command = Command::new(encoder);
    hide_console(&mut command);
    let output = command
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
        .output()
        .expect("FFmpeg decoder could not start");
    assert!(
        output.status.success(),
        "decoder failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn round_trip(format: OutputFormat, extension: &str) {
    round_trip_duration(format, extension, 59_259);
}

fn round_trip_duration(format: OutputFormat, extension: &str, frames: usize) {
    let encoder = encoder_path();
    assert!(
        encoder.is_file(),
        "Run pwsh -File scripts/Prepare-Encoder.ps1 first"
    );
    let destination = tempfile::tempdir().unwrap();
    let channels = vec![1_u16, 1, 2, 2];
    // Deliberately not a whole Opus packet: the granule/pre-skip handling must
    // reproduce this exact duration instead of rounding to a codec frame size.
    let source: Vec<Vec<f32>> = channels
        .iter()
        .enumerate()
        .map(|(track, &count)| {
            (0..frames)
                .flat_map(move |frame| {
                    (0..count).map(move |channel| {
                        let frequency = 220.0 + track as f32 * 37.0 + f32::from(channel) * 11.0;
                        (std::f32::consts::TAU * frequency * frame as f32 / 48_000.0).sin() * 0.2
                    })
                })
                .collect()
        })
        .collect();
    let mut writer =
        SegmentWriter::new(destination.path(), 1, channels.clone(), format, &encoder).unwrap();
    // Split input at a non-packet-aligned boundary as well.
    let first_end = 17_003.min(frames);
    for (start, end) in [(0, first_end), (first_end, frames)] {
        let block: Vec<Vec<f32>> = source
            .iter()
            .zip(&channels)
            .map(|(samples, &channels)| {
                samples[start * usize::from(channels)..end * usize::from(channels)].to_vec()
            })
            .collect();
        writer.write(&block).unwrap();
    }
    writer.finish(false).unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(destination.path().join("000001.complete.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["frames"].as_u64(), Some(frames as u64));
    assert_eq!(manifest["format"], serde_json::to_value(format).unwrap());
    for (track, &count) in channels.iter().enumerate() {
        assert!(
            !destination
                .path()
                .join(format!("000001_track{}.encoder.log", track + 1))
                .exists()
        );
        let path = destination
            .path()
            .join(format!("000001_track{}.{extension}", track + 1));
        assert!(path.is_file());
        if format == OutputFormat::M4a {
            check_m4a_metadata(&path, &encoder, frames, count);
        }
        let decoded = decode(&path, &encoder);
        assert_eq!(
            decoded.len(),
            frames * usize::from(count) * 4,
            "track {} decoded duration differs",
            track + 1
        );
        let decoded: Vec<_> = decoded
            .chunks_exact(4)
            .map(|sample| f32::from_le_bytes(sample.try_into().unwrap()))
            .collect();
        assert!(decoded.iter().all(|sample| sample.is_finite()));
        let error = decoded
            .iter()
            .zip(&source[track])
            .map(|(a, b)| f64::from(a - b).powi(2))
            .sum::<f64>()
            / decoded.len() as f64;
        assert!(
            error.sqrt() < 0.03,
            "track {} audio is misaligned or damaged",
            track + 1
        );
    }
    assert!(std::fs::read_dir(destination.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".partial")
    }));
}

fn check_m4a_metadata(path: &Path, encoder: &Path, frames: usize, channels: u16) {
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
            "stream=codec_name,profile,channels,sample_rate,start_pts,time_base,duration_ts",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .expect("FFprobe could not start");
    assert!(output.status.success());
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let stream = &metadata["streams"][0];
    assert_eq!(stream["codec_name"], "aac");
    assert_eq!(stream["profile"], "LC");
    assert_eq!(stream["channels"], channels);
    assert_eq!(stream["sample_rate"], "48000");
    assert_eq!(stream["start_pts"], 0);
    assert_eq!(stream["time_base"], "1/48000");
    // Check the container's presentation duration independently of decoding.
    // This catches edit-list millisecond rounding even if a decoder compensates.
    assert_eq!(stream["duration_ts"].as_u64(), Some(frames as u64));
}

#[test]
#[ignore = "requires the separately prepared FFmpeg encoder"]
fn opus_four_tracks_preserve_exact_decoded_duration() {
    round_trip(OutputFormat::Opus, "opus");
}

#[test]
#[ignore = "requires the separately prepared FFmpeg encoder"]
fn flac_four_tracks_preserve_exact_decoded_duration() {
    round_trip(OutputFormat::Flac, "flac");
}

#[test]
#[ignore = "requires the separately prepared FFmpeg decoder"]
fn rf64_four_tracks_preserve_exact_decoded_duration() {
    round_trip(OutputFormat::Wav, "wav");
}

#[test]
#[ignore = "requires the separately prepared FFmpeg encoder and FFprobe"]
fn m4a_four_tracks_preserve_exact_decoded_duration_and_presentation_clock() {
    for frames in [1, 17, 1_024, 48_000, 59_259, 480_000] {
        round_trip_duration(OutputFormat::M4a, "m4a", frames);
    }
}
