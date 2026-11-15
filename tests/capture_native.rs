//! Explicit one-second API smoke check. Audio stays in memory and is discarded.

#[cfg(windows)]
mod native {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError},
    };
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};
    use talk_recorder::capture::{
        CaptureEvent, DeviceInfo, DeviceKind, enumerate_devices, start_capture_resilient,
    };

    struct CaptureGuard {
        stop: Arc<AtomicBool>,
        handles: Vec<JoinHandle<anyhow::Result<()>>>,
    }

    impl CaptureGuard {
        fn finish(&mut self) -> Vec<String> {
            self.stop.store(true, Ordering::Release);
            self.handles
                .drain(..)
                .filter_map(|handle| match handle.join() {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(_) => Some("録音スレッドがパニックで終了しました".into()),
                })
                .collect()
        }
    }

    impl Drop for CaptureGuard {
        fn drop(&mut self) {
            // Also stop and join if a future change adds a failing assertion.
            let _ = self.finish();
        }
    }

    #[derive(Default)]
    struct Counters {
        ready: usize,
        format: Option<(u32, u16)>,
        packets: usize,
        frames: usize,
        invalid_samples: usize,
        invalid_format: usize,
        flagged_timestamps: usize,
        valid_timestamps: usize,
        invalid_valid_timestamps: usize,
        discontinuities: usize,
        previous_valid_qpc: Option<u64>,
        errors: Vec<String>,
    }

    fn consume(event: CaptureEvent, counters: &mut [Counters]) {
        match event {
            CaptureEvent::Ready {
                track,
                sample_rate,
                channels,
            } => {
                let stats = &mut counters[track];
                stats.ready += 1;
                stats.format = Some((sample_rate, channels));
            }
            CaptureEvent::Packet {
                track,
                sample_rate,
                channels,
                samples,
                qpc_100ns,
                timestamp_valid,
                discontinuity,
                ..
            } => {
                let stats = &mut counters[track];
                stats.packets += 1;
                if channels == 0
                    || !samples.len().is_multiple_of(usize::from(channels))
                    || stats.format != Some((sample_rate, channels))
                {
                    stats.invalid_format += 1;
                } else {
                    stats.frames += samples.len() / usize::from(channels);
                }
                stats.invalid_samples +=
                    samples.iter().filter(|sample| !sample.is_finite()).count();
                if !timestamp_valid {
                    // Native flags or the capture backend's QPC validation
                    // disclaimed this timestamp. Preserve the audio as Unknown.
                    stats.flagged_timestamps += 1;
                } else if qpc_100ns == 0
                    || stats
                        .previous_valid_qpc
                        .is_some_and(|previous| qpc_100ns < previous)
                {
                    // Distinct from an expected timestamp-error flag: a packet
                    // advertised as valid must not contain zero/regressed QPC.
                    stats.invalid_valid_timestamps += 1;
                } else {
                    stats.valid_timestamps += 1;
                    stats.previous_valid_qpc = Some(qpc_100ns);
                }
                stats.discontinuities += usize::from(discontinuity);
                // samples drops here. No waveform, sample value, or audio file.
            }
            CaptureEvent::Error { track, message } => counters[track].errors.push(message),
            CaptureEvent::Connection { .. } | CaptureEvent::Timing { .. } => {}
        }
    }

    #[test]
    #[ignore = "明示実行専用。選択された入力・出力を1秒間メモリ内だけで検査し破棄します"]
    fn wasapi_one_second_memory_only_smoke() {
        let devices = enumerate_devices().expect("WASAPI デバイス列挙を確認できませんでした");
        let selected: Vec<DeviceInfo> = [DeviceKind::Microphone, DeviceKind::Speaker]
            .into_iter()
            .flat_map(|kind| {
                devices
                    .iter()
                    .filter(move |device| device.kind == kind)
                    .take(2)
                    .cloned()
            })
            .collect();
        let inputs = selected
            .iter()
            .filter(|device| device.kind == DeviceKind::Microphone)
            .count();
        let outputs = selected
            .iter()
            .filter(|device| device.kind == DeviceKind::Speaker)
            .count();
        println!("API smoke endpoints: inputs={inputs}, outputs={outputs}; audio is not saved");
        assert!(inputs > 0, "入力デバイスがないため、入力取得は未検証です");
        assert!(
            outputs > 0,
            "出力デバイスがないため、ループバック起動は未検証です"
        );
        let (sender, receiver) = mpsc::sync_channel(128);
        let stop = Arc::new(AtomicBool::new(false));
        let mut capture = CaptureGuard {
            stop: Arc::clone(&stop),
            handles: selected
                .iter()
                .enumerate()
                .map(|(track, device)| {
                    start_capture_resilient(
                        track,
                        device.clone(),
                        sender.clone(),
                        Arc::clone(&stop),
                        Duration::from_millis(50),
                    )
                })
                .collect(),
        };
        drop(sender);
        let mut counters: Vec<Counters> = selected.iter().map(|_| Counters::default()).collect();
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline && !stop.load(Ordering::Acquire) {
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(event) => consume(event, &mut counters),
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        let thread_errors = capture.finish();
        // Capture threads have all joined before any assertion below.
        for event in receiver.try_iter() {
            consume(event, &mut counters);
        }
        let mut input_packets = 0;
        for (track, (device, stats)) in selected.iter().zip(&counters).enumerate() {
            println!(
                "track={track} kind={:?} ready={} packets={} frames={} invalid_samples={} invalid_format={} untrusted_timestamps={} valid_timestamps={} invalid_valid_timestamps={} discontinuities={} errors={}",
                device.kind,
                stats.ready,
                stats.packets,
                stats.frames,
                stats.invalid_samples,
                stats.invalid_format,
                stats.flagged_timestamps,
                stats.valid_timestamps,
                stats.invalid_valid_timestamps,
                stats.discontinuities,
                stats.errors.len()
            );
            let quality = if stats.packets == 0 {
                "パケットなし・時刻品質未検証"
            } else if stats.flagged_timestamps != 0
                || stats.invalid_valid_timestamps != 0
                || stats.discontinuities != 0
            {
                "時刻品質未受入（Unknown 扱いが必要な区間あり）"
            } else {
                "短時間の時刻フラグ検査は正常・長時間同期精度は未受入"
            };
            println!("track={track}: {quality}");
            if device.kind == DeviceKind::Microphone {
                input_packets += stats.packets;
            } else if stats.packets == 0 {
                println!(
                    "track={track}: speaker packet delivery unverified; no playback was requested"
                );
            }
        }
        let event_errors: Vec<_> = counters
            .iter()
            .flat_map(|stats| stats.errors.iter())
            .collect();
        assert!(
            thread_errors.is_empty(),
            "録音スレッドのエラー: {thread_errors:?}"
        );
        assert!(
            event_errors.is_empty(),
            "録音イベントのエラー: {event_errors:?}"
        );
        assert!(
            counters.iter().all(|stats| stats.ready == 1),
            "全端末の Ready を確認できませんでした"
        );
        assert!(
            input_packets > 0,
            "入力パケットがないため、音声取得は未検証です"
        );
        assert!(
            counters
                .iter()
                .all(|stats| stats.invalid_samples == 0 && stats.invalid_format == 0),
            "取得した PCM の有限性またはチャンネル整合性が不正です"
        );
        assert!(
            counters
                .iter()
                .all(|stats| stats.invalid_valid_timestamps == 0),
            "timestamp_valid=true の時刻が不正です。バッファ取得の成否とは別に、時刻フラグの契約を満たしていません"
        );
        assert!(
            capture.handles.is_empty(),
            "録音スレッドの join が完了していません"
        );
        println!(
            "API の入力バッファ取得と停止・join を確認しました。4 物理デバイスの同期受入とは別の検査です。"
        );
    }

    #[test]
    fn timestamp_flags_are_separate_from_valid_timestamp_contract_failures() {
        let mut counters = vec![Counters::default()];
        consume(
            CaptureEvent::Ready {
                track: 0,
                sample_rate: 48_000,
                channels: 1,
            },
            &mut counters,
        );
        for (qpc, timestamp_valid, discontinuity) in [
            (100, true, false),
            (0, false, false),
            (101, true, false),
            (0, true, false),
            (99, true, false),
            (102, true, true),
        ] {
            consume(
                CaptureEvent::Packet {
                    track: 0,
                    sample_rate: 48_000,
                    channels: 1,
                    samples: vec![0.0; 2],
                    device_frame: 0,
                    qpc_100ns: qpc,
                    timestamp_valid,
                    discontinuity,
                },
                &mut counters,
            );
        }
        let stats = &counters[0];
        assert_eq!(stats.packets, 6);
        assert_eq!(stats.flagged_timestamps, 1);
        assert_eq!(stats.valid_timestamps, 3);
        assert_eq!(stats.invalid_valid_timestamps, 2);
        assert_eq!(stats.previous_valid_qpc, Some(102));
        assert_eq!(stats.discontinuities, 1);
        assert_eq!(stats.invalid_samples, 0);
        assert_eq!(stats.invalid_format, 0);
    }
}
