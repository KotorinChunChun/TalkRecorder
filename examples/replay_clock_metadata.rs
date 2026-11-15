//! Replay metadata-only CSV from the native diagnostic; never opens audio devices.
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{collections::BTreeMap, env, fs};
use talk_recorder::clock_sync::{ClockMode, ClockObservation, ClockRecovery};

fn main() -> Result<()> {
    let paths: Vec<_> = env::args().skip(1).collect();
    ensure!(
        !paths.is_empty(),
        "Specify one or more native clock CSV paths"
    );
    for path in paths {
        let input = fs::read_to_string(&path)?;
        let mut clock = ClockRecovery::new();
        let mut counts = BTreeMap::<String, usize>::new();
        let mut valid = 0usize;
        let mut packets = 0usize;
        let mut first_received = None;
        let mut transitions = Vec::new();
        let mut previous_mode = None;
        let mut last = None;
        for line in input.lines().skip(1) {
            let fields: Vec<_> = line.split(',').collect();
            if fields.first() != Some(&"packet") {
                continue;
            }
            ensure!(fields.len() >= 11, "Incomplete metadata row");
            let number = |index: usize| -> Result<u64> {
                fields[index]
                    .parse()
                    .with_context(|| format!("Invalid field {index}"))
            };
            let received = number(6)?;
            let flags = number(8)?;
            let estimate = clock.observe(ClockObservation {
                device_frame: number(9)?,
                frames: u32::try_from(number(7)?)?,
                sample_rate: u32::try_from(number(3)?)?,
                raw_qpc_100ns: number(10)?,
                received_qpc_100ns: received,
                native_timestamp_valid: flags & 4 == 0,
                discontinuity: flags & 1 != 0,
            });
            let start = *first_received.get_or_insert(received);
            if previous_mode != Some(estimate.mode) {
                transitions
                    .push(json!({"seconds":(received-start) as f64/1e7,"estimate":estimate}));
                previous_mode = Some(estimate.mode);
            }
            let mode = match estimate.mode {
                ClockMode::Native => "native",
                ClockMode::Learning => "learning",
                ClockMode::Recovered => "recovered",
                ClockMode::Unstable => "unstable",
            };
            *counts.entry(mode.to_owned()).or_default() += 1;
            valid += usize::from(estimate.valid);
            packets += 1;
            last = Some(estimate);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"file":path,"packets":packets,"valid_packets":valid,"modes":counts,"transitions":transitions,"last":last})
            )?
        );
    }
    Ok(())
}
