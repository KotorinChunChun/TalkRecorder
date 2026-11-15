//! Developer check: existing public fixture only; opens no audio devices.
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{path::PathBuf, process::Command, time::Instant};
use talk_recorder::denoise_plugins::discover;

fn lag(input: &[f32], output: &[f32], start: usize) -> (i32, f64) {
    let count = 24_000;
    let mut best = (0, f64::NEG_INFINITY);
    for shift in -960_i32..=960 {
        let shifted = (start as i64 + i64::from(shift)) as usize;
        if shifted + count > output.len() {
            continue;
        }
        let (mut dot, mut a, mut b) = (0.0, 0.0, 0.0);
        for index in (0..count).step_by(8) {
            let x = f64::from(input[start + index]);
            let y = f64::from(output[shifted + index]);
            dot += x * y;
            a += x * x;
            b += y * y;
        }
        let score = dot / (a * b).sqrt().max(1e-30);
        if score > best.1 {
            best = (shift, score);
        }
    }
    best
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let root = PathBuf::from(args.get(1).context("plugin root is required")?);
    let only = args.get(2);
    let catalog = discover(&root);
    ensure!(
        !catalog.available.is_empty(),
        "no available plugins: {:?}",
        catalog.issues
    );
    let audio = Command::new("runtime/ffmpeg/bin/ffmpeg.exe")
        .args([
            "-nostdin",
            "-v",
            "error",
            "-i",
            "runtime/reference/jfk.wav",
            "-t",
            "6",
            "-ar",
            "48000",
            "-ac",
            "1",
            "-f",
            "f32le",
            "pipe:1",
        ])
        .output()?;
    ensure!(
        audio.status.success(),
        "fixture decode failed: {}",
        String::from_utf8_lossy(&audio.stderr)
    );
    let samples: Vec<f32> = audio
        .stdout
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes(bytes.try_into().unwrap()))
        .collect();
    ensure!(samples.len() == 288_000, "unexpected fixture length");
    let mut results = Vec::new();
    for descriptor in &catalog.available {
        if only.is_some_and(|id| id != &descriptor.id) {
            continue;
        }
        let mut options = serde_json::Map::new();
        if matches!(descriptor.id.as_str(), "reuse" | "reuse-laughter") {
            options.insert("chunk_secs".into(), json!(1.0));
        }
        eprintln!("{}: loading", descriptor.id);
        let mut plugin = catalog.load_with_options(&descriptor.id, &options)?;
        let mut shapes = Vec::new();
        for length in [0, 1, 479, 480, 481, 4095, 4096, 48_001] {
            eprintln!("{}: shape {length}", descriptor.id);
            let output = plugin.process(&samples[..length], 48_000)?;
            ensure!(
                output.len() == length && output.iter().all(|value| value.is_finite()),
                "invalid final fragment: {}",
                descriptor.id
            );
            shapes.push(length);
        }
        let timer = Instant::now();
        let mut output = Vec::with_capacity(samples.len());
        let mut batches = Vec::new();
        for start in (0..samples.len()).step_by(48_000) {
            let end = (start + 48_000).min(samples.len());
            let context_start = start.saturating_sub(24_000);
            let batch_timer = Instant::now();
            eprintln!("{}: batch {start}..{end}", descriptor.id);
            let enhanced = plugin.process(&samples[context_start..end], 48_000)?;
            output.extend_from_slice(&enhanced[start - context_start..]);
            batches.push(batch_timer.elapsed().as_secs_f64());
        }
        let elapsed = timer.elapsed().as_secs_f64();
        let voiced = |from: usize, until: usize| {
            (from..=until - 24_000)
                .step_by(6_000)
                .max_by(|left, right| {
                    let energy = |start: usize| {
                        samples[start..start + 24_000]
                            .iter()
                            .map(|value| f64::from(*value).powi(2))
                            .sum::<f64>()
                    };
                    energy(*left).total_cmp(&energy(*right))
                })
                .unwrap()
        };
        let early_start = voiced(24_000, 144_000);
        let late_start = voiced(144_000, 264_000);
        let early = lag(&samples, &output, early_start);
        let late = lag(&samples, &output, late_start);
        ensure!(
            early.0.abs() < 480 && late.0.abs() < 480 && early.1 > 0.5 && late.1 > 0.5,
            "waveform position check failed for {}: {:?} {:?}",
            descriptor.id,
            early,
            late
        );
        let changed = samples
            .iter()
            .zip(&output)
            .filter(|(a, b)| (*a - *b).abs() > 1e-7)
            .count();
        ensure!(
            changed > 100,
            "plugin did not change fixture: {}",
            descriptor.id
        );
        results.push(json!({"id":descriptor.id,"name":descriptor.name,"samples":output.len(),"shape_cases":shapes,
            "elapsed_secs":elapsed,"early_lag_samples":early.0,"early_ncc":early.1,"late_lag_samples":late.0,"late_ncc":late.1,"changed_samples":changed,
            "early_start_sample":early_start,"late_start_sample":late_start,"options":options,"batch_secs":batches}));
    }
    ensure!(!results.is_empty(), "requested plugin is not available");
    println!(
        "{}",
        serde_json::to_string_pretty(
            &json!({"root":root,"results":results,"issues":catalog.issues,"scope":"public fixture and single caller only; no live capture or four-device quality acceptance"})
        )?
    );
    Ok(())
}
