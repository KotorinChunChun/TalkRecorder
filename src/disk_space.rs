//! 保存先の空き容量を、音声取得スレッドを待たせずに監視する。

use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MB: u64 = 1_048_576;
const INTERVAL: Duration = Duration::from_secs(1);
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SpaceReading {
    pub available_bytes: u64,
    pub limit_bytes: u64,
}

impl SpaceReading {
    pub fn is_low(self) -> bool {
        self.limit_bytes != 0 && self.available_bytes <= self.limit_bytes
    }

    pub fn message(self) -> String {
        if self.limit_bytes == 0 {
            return "空き容量による停止は無効です".into();
        }
        format!(
            "保存先の空き容量{}（空き {:.1} MB／停止設定 {:.1} MB）",
            if self.is_low() {
                "が停止設定以下です"
            } else {
                "は停止設定を上回っています"
            },
            self.available_bytes as f64 / MB as f64,
            self.limit_bytes as f64 / MB as f64,
        )
    }
}

/// 未作成の保存先でも、その場所を含む実ボリュームを調べる。
/// `available_bytes` は総空き容量ではなく、実行ユーザーが利用できる量。
pub fn available_bytes(path: &Path) -> Result<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{Win32::Storage::FileSystem::GetDiskFreeSpaceExW, core::PCWSTR};

        let directory = existing_directory(path)?;
        let mut encoded: Vec<u16> = directory.as_os_str().encode_wide().collect();
        // UNC共有のルートを含め、ディレクトリは区切り文字で終える。
        if !matches!(encoded.last(), Some(47 | 92)) {
            encoded.push(92);
        }
        encoded.push(0);
        let mut available = 0;
        unsafe { GetDiskFreeSpaceExW(PCWSTR(encoded.as_ptr()), Some(&mut available), None, None) }
            .with_context(|| {
                format!("保存先の空き容量を取得できません: {}", directory.display())
            })?;
        Ok(available)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        anyhow::bail!("空き容量の取得はWindowsのみ対応しています")
    }
}

#[cfg(any(windows, test))]
fn existing_directory(path: &Path) -> Result<PathBuf> {
    ensure!(!path.as_os_str().is_empty(), "保存先が空です");
    let mut candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("保存先の基準フォルダーを取得できません")?
            .join(path)
    };
    loop {
        match fs::metadata(&candidate) {
            Ok(metadata) => {
                ensure!(
                    metadata.is_dir(),
                    "保存先またはその親がフォルダーではありません: {}",
                    candidate.display()
                );
                return candidate.canonicalize().with_context(|| {
                    format!(
                        "保存先の実際のフォルダーを解決できません: {}",
                        candidate.display()
                    )
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    candidate.pop(),
                    "保存先を含む既存フォルダーが見つかりません: {}",
                    path.display()
                );
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("保存先を確認できません: {}", candidate.display()));
            }
        }
    }
}

#[derive(Clone)]
struct Measurement {
    sequence: u64,
    completed: Instant,
    value: std::result::Result<u64, String>,
}

#[derive(Default)]
struct Shared {
    latest: Mutex<Option<Measurement>>,
    changed: Condvar,
}

/// 0指定ではworkerもOS照会も作らない。`poll` は共有結果を読むだけ。
/// 遅いOS照会はworkerに残るため、終了処理は無期限にjoinしない。
pub struct SpaceMonitor {
    shared: Option<Arc<Shared>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    seen: u64,
    limit_bytes: u64,
    timeout: Duration,
}

impl SpaceMonitor {
    pub fn start(path: &Path, limit_mb: u64) -> Result<Self> {
        Self::start_with_probe(path, limit_mb, INTERVAL, TIMEOUT, available_bytes)
    }

    fn start_with_probe(
        path: &Path,
        limit_mb: u64,
        interval: Duration,
        timeout: Duration,
        mut probe: impl FnMut(&Path) -> Result<u64> + Send + 'static,
    ) -> Result<Self> {
        let limit_bytes = limit_mb.checked_mul(MB).context("停止容量が大きすぎます")?;
        let mut monitor = Self {
            shared: None,
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
            seen: 0,
            limit_bytes,
            timeout,
        };
        if limit_bytes == 0 {
            return Ok(monitor);
        }
        ensure!(
            !interval.is_zero() && !timeout.is_zero(),
            "空き容量の監視間隔が不正です"
        );
        let shared = Arc::new(Shared::default());
        let results = shared.clone();
        let stopped = monitor.stop.clone();
        let path = path.to_path_buf();
        monitor.worker = Some(
            thread::Builder::new()
                .name("disk-space".into())
                .spawn(move || {
                    let mut sequence: u64 = 0;
                    while !stopped.load(Ordering::Acquire) {
                        let value =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| probe(&path)))
                                .unwrap_or_else(|_| {
                                    Err(anyhow::anyhow!("空き容量の照会処理が異常終了しました"))
                                })
                                .map_err(|error| format!("{error:#}"));
                        {
                            let mut latest =
                                results.latest.lock().unwrap_or_else(|e| e.into_inner());
                            // Dropと同じlockで順序を付け、停止後の結果を公開しない。
                            if stopped.load(Ordering::Acquire) {
                                break;
                            }
                            sequence = sequence.wrapping_add(1);
                            *latest = Some(Measurement {
                                sequence,
                                completed: Instant::now(),
                                value,
                            });
                            results.changed.notify_all();
                        }
                        thread::park_timeout(interval);
                    }
                })
                .context("空き容量の監視を開始できません")?,
        );
        monitor.shared = Some(shared);
        monitor.wait_initial()?;
        let first = monitor.poll()?.context("最初の空き容量を取得できません")?;
        ensure!(!first.is_low(), "{}。録音を開始できません", first.message());
        Ok(monitor)
    }

    fn wait_initial(&self) -> Result<()> {
        let shared = self.shared.as_ref().expect("enabled monitor");
        let deadline = Instant::now() + self.timeout;
        let mut latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
        while latest.is_none() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "最初の空き容量の照会が時間切れになりました"
            );
            let (value, _) = shared
                .changed
                .wait_timeout(latest, remaining)
                .unwrap_or_else(|e| e.into_inner());
            latest = value;
        }
        Ok(())
    }

    pub fn poll(&mut self) -> Result<Option<SpaceReading>> {
        let Some(shared) = &self.shared else {
            return Ok(None);
        };
        let latest = shared
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("空き容量の測定結果がありません")?;
        let reading = self.reading(&latest)?;
        if self.seen == latest.sequence {
            return Ok(None);
        }
        self.seen = latest.sequence;
        Ok(Some(reading))
    }

    /// 取得開始直前など、現在のgenerationより新しい測定を待つ場合に使う。
    /// 音声取得ループでは、待たない`poll`を使う。
    pub fn check_now(&mut self) -> Result<Option<SpaceReading>> {
        let Some(shared) = &self.shared else {
            return Ok(None);
        };
        let latest = {
            let mut latest = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
            let previous = latest.as_ref().map(|value| value.sequence);
            self.worker
                .as_ref()
                .expect("enabled monitor worker")
                .thread()
                .unpark();
            let deadline = Instant::now() + self.timeout;
            loop {
                if let Some(value) = latest.as_ref()
                    && Some(value.sequence) != previous
                {
                    break value.clone();
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                ensure!(
                    !remaining.is_zero(),
                    "新しい空き容量の照会が時間切れになりました"
                );
                let (value, _) = shared
                    .changed
                    .wait_timeout(latest, remaining)
                    .unwrap_or_else(|e| e.into_inner());
                latest = value;
            }
        };
        let reading = self.reading(&latest)?;
        self.seen = latest.sequence;
        Ok(Some(reading))
    }

    fn reading(&self, latest: &Measurement) -> Result<SpaceReading> {
        ensure!(
            latest.completed.elapsed() < self.timeout,
            "空き容量の照会が{:.1}秒以上更新されていないため停止します",
            self.timeout.as_secs_f64()
        );
        // 同じ失敗も無視しない。呼出側はエラーを停止の根拠にできる。
        let available_bytes = *latest
            .value
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.clone()))?;
        Ok(SpaceReading {
            available_bytes,
            limit_bytes: self.limit_bytes,
        })
    }
}

impl Drop for SpaceMonitor {
    fn drop(&mut self) {
        if let Some(shared) = &self.shared {
            let _gate = shared.latest.lock().unwrap_or_else(|e| e.into_inner());
            self.stop.store(true, Ordering::Release);
        } else {
            self.stop.store(true, Ordering::Release);
        }
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let deadline = Instant::now() + Duration::from_millis(50);
            while !worker.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            if worker.is_finished() {
                let _ = worker.join();
            }
            // OS照会が詰まった場合だけdetachする。結果の公開はstopで禁止済み。
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;
    use std::sync::{atomic::AtomicUsize, mpsc};

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !condition() {
            assert!(Instant::now() < deadline, "fixtureの応答期限超過");
            thread::sleep(Duration::from_millis(1));
        }
    }

    struct Release(Option<mpsc::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[test]
    fn threshold_includes_equality_and_zero_disables_it() {
        for (available_bytes, expected) in [(MB - 1, true), (MB, true), (MB + 1, false)] {
            let reading = SpaceReading {
                available_bytes,
                limit_bytes: MB,
            };
            assert_eq!(reading.is_low(), expected);
            assert!(reading.message().contains("MB"));
        }
        assert!(
            !SpaceReading {
                available_bytes: 0,
                limit_bytes: 0
            }
            .is_low()
        );
    }

    #[test]
    fn disabled_monitor_never_calls_probe_or_touches_invalid_path() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut monitor =
            SpaceMonitor::start_with_probe(Path::new(""), 0, INTERVAL, TIMEOUT, move |_| {
                count.fetch_add(1, Ordering::Relaxed);
                bail!("must not query")
            })
            .unwrap();
        assert!(monitor.worker.is_none());
        assert!(monitor.poll().unwrap().is_none());
        assert!(monitor.check_now().unwrap().is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn missing_destination_uses_real_existing_parent_and_file_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let absent = directory.path().join("new").join("nested");
        assert_eq!(
            existing_directory(&absent).unwrap(),
            directory.path().canonicalize().unwrap()
        );
        let file = directory.path().join("not-a-folder");
        fs::write(&file, "fixture").unwrap();
        assert!(existing_directory(&file).is_err());
        assert!(existing_directory(&file.join("child")).is_err());
        assert!(existing_directory(Path::new("")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn actual_local_volume_and_relative_destination_can_be_queried() {
        assert!(available_bytes(Path::new(".")).is_ok());
        let directory = tempfile::tempdir().unwrap();
        assert!(available_bytes(&directory.path().join("not-created")).is_ok());
    }

    #[test]
    fn initial_low_space_and_probe_error_reject_start() {
        let result =
            SpaceMonitor::start_with_probe(Path::new("unused"), 1, INTERVAL, TIMEOUT, |_| Ok(MB));
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("録音を開始できません")
        );
        let result =
            SpaceMonitor::start_with_probe(Path::new("unused"), 1, INTERVAL, TIMEOUT, |_| {
                bail!("fixture query failure")
            });
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("fixture query failure")
        );
        assert!(SpaceMonitor::start(Path::new("unused"), u64::MAX).is_err());
    }

    #[test]
    fn initial_timeout_does_not_wait_indefinitely_for_native_probe() {
        let (release, gate) = mpsc::channel();
        let _release = Release(Some(release));
        let (finished, done) = mpsc::channel();
        let started = Instant::now();
        let result = SpaceMonitor::start_with_probe(
            Path::new("unused"),
            1,
            INTERVAL,
            Duration::from_millis(20),
            move |_| {
                let _ = gate.recv_timeout(Duration::from_secs(2));
                let _ = finished.send(());
                Ok(2 * MB)
            },
        );
        assert!(result.err().unwrap().to_string().contains("時間切れ"));
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(_release);
        done.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn poll_returns_latest_sample_without_draining_an_old_queue() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut monitor = SpaceMonitor::start_with_probe(
            Path::new("unused"),
            1,
            Duration::from_millis(2),
            TIMEOUT,
            move |_| Ok((count.fetch_add(1, Ordering::Relaxed) as u64 + 2) * MB),
        )
        .unwrap();
        wait_until(|| {
            monitor
                .shared
                .as_ref()
                .unwrap()
                .latest
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|value| value.value.as_ref().is_ok_and(|value| *value >= 5 * MB))
        });
        let reading = monitor.poll().unwrap().unwrap();
        assert!(reading.available_bytes >= 5 * MB);
        // A second immediate poll can only emit a newer measurement, never the previous one.
        if let Some(newer) = monitor.poll().unwrap() {
            assert!(newer.available_bytes > reading.available_bytes);
        }
    }

    #[test]
    fn later_low_space_or_query_failure_is_observable() {
        let later = Arc::new(AtomicBool::new(false));
        let source = later.clone();
        let mut monitor = SpaceMonitor::start_with_probe(
            Path::new("unused"),
            1,
            Duration::from_millis(5),
            TIMEOUT,
            move |_| {
                Ok(if source.load(Ordering::Acquire) {
                    MB - 1
                } else {
                    2 * MB
                })
            },
        )
        .unwrap();
        later.store(true, Ordering::Release);
        let mut low = None;
        wait_until(|| {
            low = monitor.poll().unwrap();
            low.is_some()
        });
        assert!(low.unwrap().is_low());

        let later = Arc::new(AtomicBool::new(false));
        let source = later.clone();
        let mut monitor = SpaceMonitor::start_with_probe(
            Path::new("unused"),
            1,
            Duration::from_millis(5),
            TIMEOUT,
            move |_| {
                if !source.load(Ordering::Acquire) {
                    Ok(2 * MB)
                } else {
                    bail!("later failure")
                }
            },
        )
        .unwrap();
        later.store(true, Ordering::Release);
        wait_until(|| monitor.poll().is_err());
        assert!(
            monitor
                .poll()
                .unwrap_err()
                .to_string()
                .contains("later failure")
        );
    }

    #[test]
    fn check_now_wakes_parked_worker_and_consumes_a_new_generation() {
        let mut calls = 0;
        let mut monitor = SpaceMonitor::start_with_probe(
            Path::new("unused"),
            1,
            Duration::from_secs(60),
            TIMEOUT,
            move |_| {
                calls += 1;
                Ok((calls + 1) * MB)
            },
        )
        .unwrap();
        let before = Instant::now();
        let reading = monitor.check_now().unwrap().unwrap();
        assert_eq!(reading.available_bytes, 3 * MB);
        assert!(before.elapsed() < Duration::from_secs(1));
        assert!(monitor.poll().unwrap().is_none());
    }

    #[test]
    fn check_now_ignores_notification_without_a_new_measurement() {
        let (release, gate) = mpsc::channel();
        let release = Release(Some(release));
        let (entered, entry) = mpsc::channel();
        let mut calls = 0;
        let mut monitor = SpaceMonitor::start_with_probe(
            Path::new("unused"),
            1,
            Duration::from_secs(60),
            TIMEOUT,
            move |_| {
                calls += 1;
                if calls == 2 {
                    let _ = entered.send(());
                    let _ = gate.recv_timeout(Duration::from_secs(2));
                }
                Ok((calls + 1) * MB)
            },
        )
        .unwrap();
        let shared = monitor.shared.as_ref().unwrap().clone();
        let notifier = thread::spawn(move || {
            entry.recv_timeout(Duration::from_secs(1)).unwrap();
            shared.changed.notify_all();
            thread::sleep(Duration::from_millis(20));
            drop(release);
        });
        let reading = monitor.check_now().unwrap().unwrap();
        assert_eq!(reading.available_bytes, 3 * MB);
        assert_eq!(monitor.seen, 2);
        notifier.join().unwrap();
    }

    #[test]
    fn blocked_later_probe_does_not_block_poll_and_drop_cancels_publication() {
        let (release, gate) = mpsc::channel();
        let release = Release(Some(release));
        let (entered, entry) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        let mut calls = 0;
        let mut monitor = SpaceMonitor::start_with_probe(
            Path::new("unused"),
            1,
            Duration::from_millis(1),
            Duration::from_millis(100),
            move |_| {
                calls += 1;
                if calls > 1 {
                    let _ = entered.send(());
                    let _ = gate.recv_timeout(Duration::from_secs(2));
                    let _ = finished.send(());
                }
                Ok(2 * MB)
            },
        )
        .unwrap();
        entry.recv_timeout(Duration::from_secs(1)).unwrap();
        let results = monitor.shared.as_ref().unwrap().clone();
        let before = Instant::now();
        assert!(monitor.poll().unwrap().is_none());
        assert!(before.elapsed() < Duration::from_millis(100));
        thread::sleep(Duration::from_millis(105));
        assert!(
            monitor
                .poll()
                .unwrap_err()
                .to_string()
                .contains("更新されていない")
        );
        let before = Instant::now();
        drop(monitor);
        assert!(before.elapsed() < Duration::from_millis(500));
        drop(release);
        done.recv_timeout(Duration::from_secs(1)).unwrap();
        thread::sleep(Duration::from_millis(5));
        assert_eq!(results.latest.lock().unwrap().as_ref().unwrap().sequence, 1);
    }

    #[test]
    fn normal_drop_stops_parked_worker_without_more_queries() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let monitor = SpaceMonitor::start_with_probe(
            Path::new("unused"),
            1,
            Duration::from_secs(60),
            TIMEOUT,
            move |_| {
                count.fetch_add(1, Ordering::Relaxed);
                Ok(2 * MB)
            },
        )
        .unwrap();
        drop(monitor);
        thread::sleep(Duration::from_millis(5));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}
