use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{
    io::{self, BufRead, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};
use talk_recorder::{
    APP_VERSION,
    config::Config,
    engine::{Recorder, Status},
};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
fn register_interrupt_handler() -> Result<()> {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };

    unsafe extern "system" fn handle(event: u32) -> i32 {
        if matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
            INTERRUPTED.store(true, Ordering::Release);
            1
        } else {
            0
        }
    }

    if unsafe { SetConsoleCtrlHandler(Some(handle), 1) } == 0 {
        return Err(io::Error::last_os_error()).context("コンソールの停止操作を準備できません");
    }
    Ok(())
}

#[cfg(not(windows))]
fn register_interrupt_handler() -> Result<()> {
    Ok(())
}

pub(super) fn run(path: &Path, preview: bool, stop_after: Option<f64>) -> Result<()> {
    ensure!(
        path.is_file(),
        "指定した設定ファイルがありません: {}",
        path.display()
    );
    let config = Config::load(path)?;
    register_interrupt_handler()?;
    let input_stop = Arc::new(AtomicBool::new(false));
    let requested = Arc::clone(&input_stop);
    // 入力待ちがあっても、録音と保存が終わればプロセスは終了する。
    let _input = thread::Builder::new()
        .name("talkrecorder-cli-input".into())
        .spawn(move || read_stop(io::stdin().lock(), &requested, stop_after.is_none()))?;
    let mut recorder = Recorder::start(config, preview)?;
    let mut output = io::stdout().lock();
    while !recorder.is_finished() {
        let status = recorder.status();
        if INTERRUPTED.load(Ordering::Acquire)
            || input_stop.load(Ordering::Acquire)
            || stop_after.is_some_and(|limit| status.elapsed_secs >= limit)
        {
            recorder.stop();
        }
        write_status(&mut output, &status, preview)?;
        thread::sleep(Duration::from_millis(200));
    }
    let result = recorder.join();
    write_status(&mut output, &recorder.status(), preview)?;
    result
}

fn read_stop(input: impl BufRead, requested: &AtomicBool, stop_at_eof: bool) {
    let stop_command = input
        .lines()
        .map_while(Result::ok)
        .any(|line| line.trim().eq_ignore_ascii_case("stop"));
    if stop_command || stop_at_eof {
        requested.store(true, Ordering::Release);
    }
}

fn write_status(output: &mut impl Write, status: &Status, preview: bool) -> Result<()> {
    let tracks = status
        .waves
        .iter()
        .enumerate()
        .map(|(index, wave)| {
            json!({
                "slot": index + 1,
                "connected": status.connected[index],
                "coverage": status.coverage[index],
                "speech": status.speech[index],
                "wave_samples": wave.len(),
                "peak": wave.iter().map(|sample| sample.abs()).fold(0.0, f32::max),
            })
        })
        .collect::<Vec<_>>();
    let value = json!({
        "version": APP_VERSION,
        "mode": if preview { "preview" } else { "record" },
        "phase": status.phase,
        "message": status.message,
        "elapsed_secs": status.elapsed_secs,
        "edited_secs": status.saved_secs,
        "analysis_pending_secs": status.pending_secs,
        "save": status.save_progress(),
        "folder": status.folder,
        "tracks": tracks,
    });
    serde_json::to_writer(&mut *output, &value)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use talk_recorder::engine::RecorderPhase;

    #[test]
    fn explicit_stop_and_untimed_eof_request_finalization() {
        for (input, stop_at_eof, expected) in [
            (b"STOP\r\n".as_slice(), false, true),
            (b"status\n".as_slice(), false, false),
            (b"".as_slice(), true, true),
        ] {
            let requested = AtomicBool::new(false);
            read_stop(input, &requested, stop_at_eof);
            assert_eq!(requested.load(Ordering::Acquire), expected);
        }
    }

    #[test]
    fn preview_status_reports_wave_activity_without_pcm() -> Result<()> {
        let mut status = Status::default();
        status.connected[0] = true;
        status.waves[0] = vec![-0.5, 0.125];
        status.phase = RecorderPhase::Preview;
        let mut encoded = Vec::new();
        write_status(&mut encoded, &status, true)?;
        let value: serde_json::Value = serde_json::from_slice(&encoded)?;
        assert_eq!(value["phase"], "preview");
        assert_eq!(value["tracks"][0]["wave_samples"], 2);
        assert_eq!(value["tracks"][0]["peak"], 0.5);
        assert!(value["folder"].is_null());
        assert!(value["tracks"][0]["coverage"].is_null());
        assert!(value["tracks"][0].get("wave").is_none());
        Ok(())
    }

    #[test]
    fn recording_status_separates_received_unsynchronized_and_missing_frames() -> Result<()> {
        use talk_recorder::sync_audio::InputCoverage;

        let mut status = Status::default();
        status.coverage[0] = Some(InputCoverage {
            synchronized_frames: 47_000,
            unsynchronized_frames: 800,
            missing_frames: 200,
        });
        let mut encoded = Vec::new();
        write_status(&mut encoded, &status, false)?;
        let value: serde_json::Value = serde_json::from_slice(&encoded)?;
        assert_eq!(
            value["tracks"][0]["coverage"]["synchronized_frames"],
            47_000
        );
        assert_eq!(value["tracks"][0]["coverage"]["unsynchronized_frames"], 800);
        assert_eq!(value["tracks"][0]["coverage"]["missing_frames"], 200);
        Ok(())
    }
}
