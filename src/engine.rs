//! 録音取得をモデル推論・圧縮から切り離す、有限バッファの実行パイプライン。
use std::{
    collections::VecDeque,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{
    SpeechState,
    analysis_recovery::ResilientSpeech,
    audit::{AuditClient, AuditWarning, AuditWorker, Counters, EventKind},
    capture::{self, CaptureEvent, DeviceInfo},
    clock_sync::ClockEstimate,
    config::{Config, OutputFormat},
    disk_space::{SpaceMonitor, SpaceReading},
    editor::{EditAction, Editor},
    finalize::{Finalizer, FinalizerClient, SaveProgress, SharedSaveProgress},
    journal::JournalWriter,
    maintenance,
    power::{ResumeDetector, SleepGuard},
    recovery::SessionLock,
    speech::SpeechClient,
    storage::{Spool, check_encoder},
    sync_audio::{ANALYSIS_WINDOW_FRAMES, Aligner, InputCoverage},
};

const RATE: u64 = 48_000;
const CAPTURE_LAG: u64 = 14_400;
const WAVE_POINTS: usize = 2_000;
const QPC_TICKS_PER_SECOND: u64 = 10_000_000;

#[derive(Debug, Clone, Copy)]
pub struct WavePoint {
    pub qpc_100ns: u64,
    pub amplitude: f32,
}

pub(crate) fn wave_window_ticks(seconds: f64) -> u64 {
    ((seconds * QPC_TICKS_PER_SECOND as f64).round() as u64).clamp(1, 600 * QPC_TICKS_PER_SECOND)
}

#[cfg(test)]
mod capacity_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderPhase {
    Preparing,
    Preview,
    Recording,
    Finalizing,
    Finished,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapacityStopReason {
    LowSpace {
        available_bytes: u64,
        limit_bytes: u64,
    },
    ProbeFailed {
        message: String,
    },
}

impl CapacityStopReason {
    fn description(&self) -> String {
        match self {
            Self::LowSpace {
                available_bytes,
                limit_bytes,
            } => SpaceReading {
                available_bytes: *available_bytes,
                limit_bytes: *limit_bytes,
            }
            .message(),
            Self::ProbeFailed { message } => format!("保存先の空き容量を確認できません: {message}"),
        }
    }
}

trait CapacityChecks {
    fn poll(&mut self) -> Result<Option<SpaceReading>>;
}

impl CapacityChecks for SpaceMonitor {
    fn poll(&mut self) -> Result<Option<SpaceReading>> {
        SpaceMonitor::poll(self)
    }
}

fn stop_for_capacity(
    reading: Result<Option<SpaceReading>>,
    status: &SharedStatus,
    requested: &AtomicBool,
) -> Result<bool> {
    let (reason, failure) = match reading {
        Ok(Some(reading)) if reading.is_low() => (
            CapacityStopReason::LowSpace {
                available_bytes: reading.available_bytes,
                limit_bytes: reading.limit_bytes,
            },
            None,
        ),
        Ok(_) => return Ok(false),
        Err(error) => (
            CapacityStopReason::ProbeFailed {
                message: format!("{error:#}"),
            },
            Some(error),
        ),
    };
    requested.store(true, Ordering::Release);
    update(status, |status| {
        let reason = status.capacity_stop.get_or_insert(reason);
        status.message = format!("{}。停止・保存中", reason.description());
        status.running = false;
        status.phase = RecorderPhase::Finalizing;
    });
    if let Some(error) = failure {
        Err(error).context("保存先の空き容量を確認できないため録音を停止します")
    } else {
        Ok(true)
    }
}

#[derive(Debug, Clone)]
pub struct Status {
    pub phase: RecorderPhase,
    pub capacity_stop: Option<CapacityStopReason>,
    pub running: bool,
    pub message: String,
    pub elapsed_secs: f64,
    pub saved_secs: f64,
    pub pending_secs: f64,
    save_progress: SharedSaveProgress,
    pub folder: Option<PathBuf>,
    pub waves: Vec<VecDeque<WavePoint>>,
    pub speech: Vec<SpeechState>,
    pub connected: Vec<bool>,
    pub timing: Vec<Option<ClockEstimate>>,
    pub coverage: Vec<Option<InputCoverage>>,
    audit_warning: AuditWarning,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            phase: RecorderPhase::Finished,
            capacity_stop: None,
            running: false,
            message: "停止中".into(),
            elapsed_secs: 0.0,
            saved_secs: 0.0,
            pending_secs: 0.0,
            save_progress: SharedSaveProgress::default(),
            folder: None,
            waves: vec![VecDeque::new(); 4],
            speech: vec![SpeechState::Unknown; 4],
            connected: vec![false; 4],
            timing: vec![None; 4],
            coverage: vec![None; 4],
            audit_warning: AuditWarning::default(),
        }
    }
}

impl Status {
    pub fn audit_warning(&self) -> Option<&'static str> {
        *self.audit_warning.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn save_progress(&self) -> SaveProgress {
        *self.save_progress.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn set_finalizing(&mut self, stage: &str) {
        self.phase = RecorderPhase::Finalizing;
        self.running = false;
        self.message = self.capacity_stop.as_ref().map_or_else(
            || stage.into(),
            |reason| format!("{}。{stage}", reason.description()),
        );
    }
}

type SharedStatus = Arc<Mutex<Status>>;

pub struct Recorder {
    stop: Arc<AtomicBool>,
    status: SharedStatus,
    handle: Option<JoinHandle<Result<()>>>,
}

impl Recorder {
    pub fn start(config: Config, preview: bool) -> Result<Self> {
        Self::start_with_policy(config, preview, false)
    }

    pub(crate) fn start_resident(config: Config) -> Result<Self> {
        Self::start_with_policy(config, false, true)
    }

    fn start_with_policy(mut config: Config, preview: bool, resident: bool) -> Result<Self> {
        if config.auto_compact_wav && config.format == OutputFormat::Wav {
            config.format = OutputFormat::M4a;
        }
        if preview {
            ensure!(
                (0.1..=60.0).contains(&config.reconnect_secs)
                    && config.preview_secs > 0.0
                    && config.preview_secs <= 600.0,
                "波形表示時間・再接続間隔を確認してください"
            );
        } else {
            config.validate()?;
            let plugins = crate::denoise_plugins::discover_for_current_exe();
            for track in config.tracks.iter().filter(|track| track.enabled) {
                if let Some(id) = track.denoise.plugin_id() {
                    ensure!(
                        plugins.available.iter().any(|plugin| plugin.id == id),
                        "{}のノイズ除去プラグインが見つかりません: {id}。配置を確認するかノイズ除去なしを選択してください",
                        track.label
                    );
                }
            }
        }
        if !preview {
            ensure!(
                config.max_pending_secs >= 1.0,
                "処理待ち上限は1秒以上にしてください"
            );
        }
        let available = capture::enumerate_devices()?;
        let (slots, devices) = if preview {
            let selection = preview_devices(&config, &available);
            ensure!(!selection.0.is_empty(), "入力を取得できる機器がありません");
            selection
        } else {
            select_devices(&config, &available)?
        };
        if !preview {
            check_encoder(config.format, &config.ffmpeg_path)?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let status = Arc::new(Mutex::new(Status {
            phase: RecorderPhase::Preparing,
            running: false,
            message: "準備中".into(),
            ..Status::default()
        }));
        let thread_stop = Arc::clone(&stop);
        let thread_status = Arc::clone(&status);
        let handle = thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut retries = 0;
                loop {
                    let result = run(
                        &config,
                        preview,
                        resident,
                        &devices,
                        &slots,
                        &thread_stop,
                        &thread_status,
                    );
                    let (probe_failed, elapsed) = {
                        let status = thread_status.lock().unwrap_or_else(|e| e.into_inner());
                        (
                            matches!(
                                status.capacity_stop,
                                Some(CapacityStopReason::ProbeFailed { .. })
                            ),
                            status.elapsed_secs,
                        )
                    };
                    if elapsed >= 30.0 {
                        retries = 0;
                    }
                    if !resident
                        || !probe_failed
                        || result.is_ok()
                        || retries >= 3
                        || thread_stop.load(Ordering::Acquire)
                    {
                        break result;
                    }
                    retries += 1;
                    update(&thread_status, |status| {
                        status.phase = RecorderPhase::Preparing;
                        status.message = "空き容量の再照会を待機中".into();
                    });
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while std::time::Instant::now() < deadline {
                        if thread_stop.load(Ordering::Acquire) {
                            return Ok(());
                        }
                        thread::park_timeout(Duration::from_millis(100));
                    }
                    update(&thread_status, |status| {
                        *status = Status {
                            phase: RecorderPhase::Preparing,
                            ..Status::default()
                        };
                    });
                }
            }))
            .unwrap_or_else(|_| Err(anyhow!("録音管理スレッドが異常終了しました")));
            thread_stop.store(true, Ordering::Release);
            update(&thread_status, |status| {
                status.running = false;
                status.phase = if result.is_ok() {
                    RecorderPhase::Finished
                } else {
                    RecorderPhase::Failed
                };
                let saved = status.folder.as_ref().map(|_| {
                    let progress = status.save_progress();
                    format!(
                        "録音停止・保存完了（保存 {} 区間、除外 {} 区間）",
                        progress.saved, progress.skipped
                    )
                });
                status.message = match (&result, &status.capacity_stop) {
                    (Ok(()), Some(reason)) => format!(
                        "{}。{}",
                        reason.description(),
                        saved.as_deref().unwrap_or("録音を開始しませんでした")
                    ),
                    (Ok(()), None) => saved.unwrap_or_else(|| "停止しました".into()),
                    (Err(error), Some(reason)) => {
                        format!("{}。停止: {error:#}", reason.description())
                    }
                    (Err(error), None) => format!("停止: {error:#}"),
                };
            });
            result
        });
        Ok(Self {
            stop,
            status,
            handle: Some(handle),
        })
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
    /// 波形を複製せず、GUIの描画とは独立して状態だけを監視する。
    pub(crate) fn phase_reader(&self) -> impl Fn() -> RecorderPhase + 'static {
        let status = self.status.clone();
        move || status.lock().unwrap_or_else(|e| e.into_inner()).phase
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        update(&self.status, |status| {
            if !matches!(
                status.phase,
                RecorderPhase::Finalizing | RecorderPhase::Finished | RecorderPhase::Failed
            ) {
                status.set_finalizing("取り込みを停止中");
            }
        });
    }
    pub fn is_finished(&self) -> bool {
        self.handle.as_ref().is_none_or(JoinHandle::is_finished)
    }
    pub fn join(&mut self) -> Result<()> {
        if let Some(handle) = self.handle.take() {
            join_checked(handle, "録音管理")?;
        }
        Ok(())
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop();
        if let Some(handle) = self.handle.take() {
            let _ = join_checked(handle, "録音管理");
        }
    }
}

fn update(status: &SharedStatus, change: impl FnOnce(&mut Status)) {
    change(&mut status.lock().unwrap_or_else(|e| e.into_inner()));
}

fn join_checked(handle: JoinHandle<Result<()>>, label: &str) -> Result<()> {
    handle
        .join()
        .map_err(|_| anyhow!("{label}スレッドが異常終了しました"))?
        .with_context(|| format!("{label}スレッドの処理に失敗しました"))
}

fn preview_devices(config: &Config, available: &[DeviceInfo]) -> (Vec<usize>, Vec<DeviceInfo>) {
    config
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| track.enabled)
        .filter_map(|(slot, track)| {
            available
                .iter()
                .find(|device| device.id == track.device_id && device.kind == track.kind)
                .map(|device| (slot, device.clone()))
        })
        .unzip()
}

fn select_devices(
    config: &Config,
    available: &[DeviceInfo],
) -> Result<(Vec<usize>, Vec<DeviceInfo>)> {
    let cached: Vec<DeviceInfo> = match fs::read(config.output_dir.join("device-formats.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("デバイス形式の保存記録が不正です")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => vec![],
        Err(error) => return Err(error.into()),
    };
    let mut slots = Vec::new();
    let mut devices = Vec::new();
    for (slot, track) in config.tracks.iter().enumerate().filter(|(_, t)| t.enabled) {
        let device = available
            .iter()
            .find(|d| d.id == track.device_id && d.kind == track.kind)
            .or_else(|| {
                cached.iter().find(|d| {
                    d.id == track.device_id
                        && d.kind == track.kind
                        && (1..=8).contains(&d.channels)
                        && d.sample_rate > 0
                })
            })
            .with_context(|| {
                format!(
                    "{}は未接続で取得形式の記録もありません。一度接続した状態で開始してください",
                    track.label
                )
            })?;
        slots.push(slot);
        devices.push(device.clone());
    }
    Ok((slots, devices))
}

fn remember_devices(output: &Path, devices: &[DeviceInfo]) -> Result<()> {
    fs::create_dir_all(output)?;
    let path = output.join("device-formats.json");
    let mut all: Vec<DeviceInfo> = fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    for device in devices {
        all.retain(|old| old.id != device.id);
        all.insert(0, device.clone());
    }
    all.truncate(128);
    let temporary = output.join(format!(
        "device-formats-{}-{}.partial",
        std::process::id(),
        capture::qpc_100ns()
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, &all)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, path)?;
    Ok(())
}

struct CaptureGroup {
    stop: Arc<AtomicBool>,
    handles: Vec<JoinHandle<Result<()>>>,
}

impl CaptureGroup {
    fn join(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        let mut error = None;
        for handle in self.handles.drain(..) {
            if let Err(failure) = join_checked(handle, "音声取得") {
                error.get_or_insert(failure);
            }
        }
        error.map_or(Ok(()), Err)
    }
}

impl Drop for CaptureGroup {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

#[derive(Default)]
struct Progress {
    produced: AtomicU64,
    analyzed: AtomicU64,
    settled: AtomicU64,
    committed: AtomicU64,
    done: AtomicBool,
}

impl Progress {
    fn is_drained(&self, next: u64) -> bool {
        // done の Release より前に最終 produced が公開される。done を観測して
        // から位置を再読込し、先に読んだ古い produced で終端を決めない。
        self.done.load(Ordering::Acquire) && next >= self.produced.load(Ordering::Acquire)
    }
}

#[derive(Serialize, Deserialize)]
struct BlockMetadata {
    frames: u64,
    valid: Vec<bool>,
    coverage: Vec<InputCoverage>,
    valid_windows: Vec<Vec<bool>>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct SpeechInterval {
    frames: u64,
    state: SpeechState,
}

impl BlockMetadata {
    fn speech_intervals(
        &self,
        states: &[SpeechState],
        track_windows: &[Vec<SpeechState>],
    ) -> Vec<SpeechInterval> {
        let mut intervals: Vec<SpeechInterval> = Vec::new();
        let mut remaining = self.frames;
        for window in 0..(self.frames as usize).div_ceil(ANALYSIS_WINDOW_FRAMES) {
            let mut window_states = [SpeechState::Unknown; 4];
            for (index, &state) in states.iter().enumerate() {
                if self.valid_windows[index][window] {
                    window_states[index] =
                        track_windows[index].get(window).copied().unwrap_or(state);
                }
            }
            let state = aggregate_states(&window_states[..states.len()]);
            let frames = remaining.min(ANALYSIS_WINDOW_FRAMES as u64);
            remaining -= frames;
            if let Some(last) = intervals.last_mut().filter(|last| last.state == state) {
                last.frames += frames;
            } else {
                intervals.push(SpeechInterval { frames, state });
            }
        }
        intervals
    }
}

fn run(
    config: &Config,
    preview: bool,
    resident: bool,
    devices: &[DeviceInfo],
    slots: &[usize],
    stop: &Arc<AtomicBool>,
    status: &SharedStatus,
) -> Result<()> {
    let limit = if preview {
        0
    } else {
        config.stop_free_space_mb
    };
    let mut capacity = if resident {
        SpaceMonitor::start_resident(&config.output_dir, limit)
    } else {
        SpaceMonitor::start(&config.output_dir, limit)
    }?;
    if !preview {
        update(status, |s| {
            s.message = "録音の復旧・容量削減を確認中".into()
        });
        let report = maintenance::run(config, true)?;
        update(status, |s| s.message = report.message());
    }
    let _sleep_guard = SleepGuard::new(config.prevent_sleep && !preview)?;
    let audit = if preview {
        None
    } else {
        let warning = status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .audit_warning
            .clone();
        match AuditWorker::start(&config.output_dir, warning.clone()) {
            Ok(worker) => Some(worker),
            Err(_) => {
                *warning.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some("録音統計の処理を開始できませんでした");
                None
            }
        }
    };
    let finalizer = Finalizer::with_audit(audit.as_ref().map(AuditWorker::client));
    update(status, |s| s.save_progress = finalizer.progress());
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Acquire) {
            let attempt = run_session(
                config.clone(),
                preview,
                devices.to_vec(),
                slots.to_vec(),
                stop,
                status,
                finalizer.client(),
                &mut capacity,
            );
            let resume = attempt?;
            if !resume || stop.load(Ordering::Acquire) {
                break;
            }
            update(status, |s| {
                s.phase = RecorderPhase::Preparing;
                s.message = "スリープ復帰後の録音を再開中".into()
            });
        }
        Ok(())
    })();
    update(status, |s| {
        if s.folder.is_some() {
            s.set_finalizing("取り込み停止・音声ファイルを保存中");
        }
    });
    let finalized = finalizer.finish();
    if result.is_err()
        && let Some(audit) = &audit
    {
        audit.client().record(
            EventKind::RecordingFailure,
            Counters {
                recording_failures: 1,
                ..Counters::default()
            },
        );
    }
    drop(audit);
    // 現在の録音の確定を優先し、容量停止後に過去WAVの追加変換を始めない。
    let capacity_stopped = status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .capacity_stop
        .is_some();
    let allow_maintenance = !preview
        && !capacity_stopped
        && config.auto_compact_wav
        && capacity
            .check_now()
            .is_ok_and(|reading| reading.is_none_or(|reading| !reading.is_low()));
    let maintenance = if allow_maintenance {
        update(status, |s| s.set_finalizing("過去のWAVを確認・圧縮中"));
        let report = maintenance::run(config, false)?;
        let errors: Vec<_> = report.errors().cloned().collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!(
                "録音は保持しました。容量削減保留: {}",
                errors.join("\n")
            ))
        }
    } else {
        Ok(())
    };
    result.and(finalized).and(maintenance)
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    config: Config,
    preview: bool,
    mut devices: Vec<DeviceInfo>,
    slots: Vec<usize>,
    requested: &Arc<AtomicBool>,
    status: &SharedStatus,
    finalizer: FinalizerClient,
    capacity: &mut SpaceMonitor,
) -> Result<bool> {
    let session_stop = Arc::new(AtomicBool::new(false));
    let stop = &session_stop;
    // DLL・モデルの読み込みは取得開始前に完了する。プレビューでは不要。
    let speech = if preview {
        None
    } else {
        update(status, |status| {
            status.message = "音声モデルを準備中".into()
        });
        let mut speech = match SpeechClient::spawn_cancellable(&config.speech, requested) {
            Ok(client) => client,
            Err(_) if requested.load(Ordering::Acquire) => return Ok(false),
            Err(error) => return Err(error),
        };
        let modes: Vec<_> = slots
            .iter()
            .map(|&slot| config.tracks[slot].denoise.clone())
            .collect();
        if let Err(error) = speech.prepare_denoise_cancellable(&modes, requested) {
            if requested.load(Ordering::Acquire) {
                return Ok(false);
            }
            return Err(error);
        }
        if modes.iter().any(|mode| mode.plugin_id().is_some()) {
            update(status, |status| {
                status.message = "ノイズ除去プラグインを確認中".into()
            });
            let budget = Duration::from_secs_f64(
                config
                    .analysis_timeout_secs
                    .min(config.max_pending_secs / (devices.len() + 1) as f64)
                    .max(0.001),
            );
            speech.check_denoise_cancellable(&modes, budget, requested)?;
        }
        Some(speech)
    };
    if requested.load(Ordering::Acquire) {
        return Ok(false);
    }
    if !preview && stop_for_capacity(capacity.check_now(), status, stop)? {
        return Ok(false);
    }
    let (sender, receiver) = mpsc::sync_channel(1_024);
    let mut captures = CaptureGroup {
        stop: Arc::clone(stop),
        handles: Vec::new(),
    };
    for (track, device) in devices.iter().cloned().enumerate() {
        captures.handles.push(capture::start_capture_resilient(
            track,
            device,
            sender.clone(),
            Arc::clone(stop),
            Duration::from_secs_f64(config.reconnect_secs),
        ));
    }
    drop(sender);
    if preview {
        update(status, |status| {
            status.phase = RecorderPhase::Preview;
            status.message = "入力確認中".into();
        });
        let result = run_preview(&receiver, &config, &slots, requested, status);
        return result.and(captures.join()).map(|()| false);
    }
    let ready = match await_ready(
        &receiver,
        &mut devices,
        requested,
        Duration::from_secs_f64(config.reconnect_secs.clamp(0.1, 10.0)),
    ) {
        Ok(ready) => ready,
        Err(error) => {
            let _ = captures.join();
            return Err(error);
        }
    };
    update(status, |s| {
        for (index, &slot) in slots.iter().enumerate() {
            s.connected[slot] = ready[index];
            s.timing[slot] = None;
            s.coverage[slot] = None;
        }
    });
    if requested.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
        captures.join()?;
        return Ok(false);
    }
    let origin = capture::qpc_100ns();
    let mut detector = ResumeDetector::new();
    let channels: Vec<_> = devices.iter().map(|device| device.channels).collect();
    let folder = create_session_folder(&config.output_dir)?;
    let session_lock = Arc::new(SessionLock::acquire(&folder)?);
    let finalizer = finalizer.with_session_lock(session_lock);
    remember_devices(&config.output_dir, &devices)?;
    fs::write(
        folder.join("session.json"),
        serde_json::to_vec_pretty(&json!({
            "app":"TalkRecorder", "version":crate::APP_VERSION, "started":chrono::Local::now().to_rfc3339(),
            "origin_qpc_100ns":origin, "sample_rate":RATE, "config":config, "slots":slots, "devices":devices,
        }))?,
    )?;
    update(status, |status| {
        status.folder = Some(folder.clone());
        status.running = true;
        status.phase = RecorderPhase::Recording;
        status.message = "録音中".into();
    });
    let mut spool = Spool::create(&folder.join("raw"), channels.clone())?;
    Spool::create(&folder.join("processed"), channels.clone())?;
    let mut aligner = Aligner::new(origin, channels.clone(), config.max_pending_secs.max(2.0))?;
    let progress = Arc::new(Progress::default());
    let processing = {
        let config = config.clone();
        let folder = folder.clone();
        let slots = slots.clone();
        let status = Arc::clone(status);
        let stop = Arc::clone(stop);
        let progress = Arc::clone(&progress);
        let finalize = finalizer.clone();
        thread::spawn(move || {
            let result = analyze(
                config,
                folder,
                channels,
                slots,
                speech.expect("録音モードでは音声処理を準備済み"),
                &progress,
                &status,
                finalize,
            );
            if result.is_err() {
                stop.store(true, Ordering::Release);
            }
            result
        })
    };
    let mut resume = ResumeState::default();
    let acquired = acquire(
        &receiver,
        &config,
        &slots,
        origin,
        &mut aligner,
        &mut spool,
        &progress,
        stop,
        status,
        requested,
        &mut resume,
        &mut detector,
        capacity,
    );
    // 終了時刻を先に固定してから取得スレッドを全て join する。
    // ReleaseBuffer 後にキューへ到着する最後のパケットも回収する。
    let end = resume.end(source_frames(origin), spool.frames);
    update(status, |status| {
        status.set_finalizing("取り込みを停止中");
        status.elapsed_secs = end as f64 / RATE as f64;
    });
    let joined = captures.join();
    let flushed = flush_capture(
        &receiver,
        &config,
        &slots,
        end,
        &mut aligner,
        &mut spool,
        &progress,
        status,
    );
    // 捕捉スレッドの終了を待ち、生成済みの prefix を最後まで解析する。
    update(status, |s| {
        s.set_finalizing("取り込み停止・残りの音声を解析中")
    });
    progress.done.store(true, Ordering::Release);
    let analyzed = join_checked(processing, "音声解析・保存");
    let result = acquired.and(joined).and(flushed).and(analyzed);
    let mut timeline = OpenOptions::new()
        .append(true)
        .create(true)
        .open(folder.join("timeline.jsonl"))?;
    log(
        &mut timeline,
        json!({"event":"done","source_frames":progress.produced.load(Ordering::Acquire),"success":result.is_ok(),"resumed":resume.detected,"public_finalization":"background","capacity_stop":status.lock().unwrap_or_else(|e| e.into_inner()).capacity_stop.clone(),"error":result.as_ref().err().map(|e|format!("{e:#}"))}),
    )?;
    timeline.sync_all()?;
    result.map(|()| resume.detected)
}

fn await_ready(
    receiver: &Receiver<CaptureEvent>,
    devices: &mut [DeviceInfo],
    stop: &AtomicBool,
    timeout: Duration,
) -> Result<Vec<bool>> {
    let deadline = Instant::now() + timeout;
    let mut ready = vec![false; devices.len()];
    while ready.iter().any(|&value| !value) && !stop.load(Ordering::Acquire) {
        if Instant::now() >= deadline {
            ensure!(
                devices
                    .iter()
                    .all(|d| (1..=8).contains(&d.channels) && d.sample_rate > 0),
                "取得形式が不明なデバイスがあります。一度接続した状態で開始してください"
            );
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(CaptureEvent::Ready {
                track,
                sample_rate,
                channels,
            }) => {
                let device = devices
                    .get_mut(track)
                    .context("準備するトラック番号が不正です")?;
                device.sample_rate = sample_rate;
                device.channels = channels;
                ensure!(
                    (1..=8).contains(&channels),
                    "保存対象は1～8チャンネルのデバイスにしてください"
                );
                ready[track] = true;
            }
            Ok(CaptureEvent::Error { track, message }) => {
                bail!("デバイス{}の準備に失敗しました: {message}", track + 1)
            }
            Ok(CaptureEvent::Connection {
                track,
                connected: false,
                ..
            }) => {
                if let Some(value) = ready.get_mut(track) {
                    *value = false;
                }
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("録音デバイスが準備中に終了しました"),
        }
    }
    Ok(ready)
}

fn run_preview(
    receiver: &Receiver<CaptureEvent>,
    config: &Config,
    slots: &[usize],
    stop: &AtomicBool,
    status: &SharedStatus,
) -> Result<()> {
    let started = Instant::now();
    let mut next_update = started;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= next_update {
            update(status, |status| {
                status.elapsed_secs = (now - started).as_secs_f64();
            });
            next_update = now + Duration::from_millis(100);
        }
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(CaptureEvent::Connection {
                track, connected, ..
            }) => {
                if let Some(&slot) = slots.get(track) {
                    update(status, |status| {
                        status.connected[slot] = connected;
                        if !connected {
                            status.waves[slot].clear();
                        }
                    });
                }
            }
            Ok(CaptureEvent::Error { track, message }) => bail!("デバイス{}: {message}", track + 1),
            Ok(event) => update_wave(
                &event,
                config.preview_secs,
                slots,
                status,
                capture::qpc_100ns(),
            ),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("入力確認が予期せず終了しました"),
        }
    }
    update(status, |status| {
        status.elapsed_secs = started.elapsed().as_secs_f64()
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[derive(Default)]
struct ResumeState {
    detected: bool,
    last_awake_frames: u64,
}

impl ResumeState {
    fn observe(&mut self, now: u64, resumed: bool) -> bool {
        if resumed {
            self.detected = true;
            true
        } else {
            self.last_awake_frames = now;
            false
        }
    }
    fn end(&self, now: u64, persisted: u64) -> u64 {
        if self.detected {
            self.last_awake_frames
        } else {
            now
        }
        .max(persisted)
    }
}

#[allow(clippy::too_many_arguments)]
fn acquire(
    receiver: &Receiver<CaptureEvent>,
    config: &Config,
    slots: &[usize],
    origin: u64,
    aligner: &mut Aligner,
    spool: &mut Spool,
    progress: &Progress,
    stop: &AtomicBool,
    status: &SharedStatus,
    requested: &AtomicBool,
    resume: &mut ResumeState,
    detector: &mut ResumeDetector,
    capacity: &mut dyn CapacityChecks,
) -> Result<()> {
    let mut failure = None;
    while !stop.load(Ordering::Acquire) {
        if requested.load(Ordering::Acquire) {
            break;
        }
        if resume.observe(source_frames(origin), detector.poll()) {
            break;
        }
        match stop_for_capacity(capacity.poll(), status, stop) {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
        match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(event) => {
                if let Err(error) = ingest(event, config, slots, aligner, status) {
                    failure = Some(error);
                    stop.store(true, Ordering::Release);
                }
                for _ in 0..1_024 {
                    let Ok(event) = receiver.try_recv() else {
                        break;
                    };
                    if let Err(error) = ingest(event, config, slots, aligner, status) {
                        failure.get_or_insert(error);
                        stop.store(true, Ordering::Release);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                failure = Some(anyhow!("全録音デバイスが予期せず終了しました"));
                stop.store(true, Ordering::Release);
            }
        }
        if resume.observe(source_frames(origin), detector.poll()) {
            break;
        }
        let available = source_frames(origin).saturating_sub(CAPTURE_LAG);
        while spool.frames + RATE <= available {
            append_aligned(aligner, spool, RATE as usize, progress, slots, status)?;
            if let Err(error) = check_backlog(config, progress, &spool.channels) {
                failure.get_or_insert(error);
                stop.store(true, Ordering::Release);
                break;
            }
        }
        update(status, |status| {
            status.elapsed_secs = source_frames(origin) as f64 / RATE as f64;
            status.pending_secs = progress
                .produced
                .load(Ordering::Acquire)
                .saturating_sub(progress.analyzed.load(Ordering::Acquire))
                as f64
                / RATE as f64;
        });
    }
    failure.map_or(Ok(()), Err)
}

#[allow(clippy::too_many_arguments)]
fn flush_capture(
    receiver: &Receiver<CaptureEvent>,
    config: &Config,
    slots: &[usize],
    end: u64,
    aligner: &mut Aligner,
    spool: &mut Spool,
    progress: &Progress,
    status: &SharedStatus,
) -> Result<()> {
    let mut failure = None;
    while let Ok(event) = receiver.try_recv() {
        if let Err(error) = ingest(event, config, slots, aligner, status) {
            failure.get_or_insert(error);
        }
    }
    while spool.frames < end {
        let frames = (end - spool.frames).min(RATE) as usize;
        append_aligned(aligner, spool, frames, progress, slots, status)?;
    }
    failure.map_or(Ok(()), Err)
}

fn source_frames(origin: u64) -> u64 {
    let elapsed = capture::qpc_100ns().saturating_sub(origin);
    ((u128::from(elapsed) * u128::from(RATE)) / 10_000_000) as u64
}

fn ingest(
    event: CaptureEvent,
    config: &Config,
    slots: &[usize],
    aligner: &mut Aligner,
    status: &SharedStatus,
) -> Result<()> {
    if let CaptureEvent::Timing {
        track,
        received_qpc_100ns,
        device_frame,
        packet_frames,
        raw_qpc_100ns,
        estimate,
    } = &event
    {
        let slot = *slots
            .get(*track)
            .context("時刻診断のトラック番号が不正です")?;
        update(status, |s| s.timing[slot] = Some(*estimate));
        log_capture(
            status,
            json!({"event":"clock_sync","slot":slot,"received_qpc_100ns":received_qpc_100ns,"device_frame":device_frame,"packet_frames":packet_frames,"raw_qpc_100ns":raw_qpc_100ns,"estimate":estimate}),
        )?;
        return Ok(());
    }
    if let CaptureEvent::Connection {
        track,
        connected,
        message,
    } = &event
        && let Some(&slot) = slots.get(*track)
    {
        update(status, |s| {
            s.connected[slot] = *connected;
            if !connected {
                s.speech[slot] = SpeechState::Unknown;
                s.timing[slot] = None;
            }
            s.message = if slots.iter().any(|&slot| !s.connected[slot]) {
                format!("録音継続・再接続待ち: {message}")
            } else {
                "録音中".into()
            };
        });
        log_capture(
            status,
            json!({"event":"connection","slot":slot,"connected":connected,"observed_qpc_100ns":capture::qpc_100ns(),"observed_at":chrono::Local::now().to_rfc3339(),"message":message}),
        )?;
    }
    update_wave(
        &event,
        config.preview_secs,
        slots,
        status,
        capture::qpc_100ns(),
    );
    aligner.ingest(event)
}

fn log_capture(status: &SharedStatus, event: serde_json::Value) -> Result<()> {
    let folder = status
        .lock()
        .map_err(|_| anyhow!("状態のロックに失敗しました"))?
        .folder
        .clone();
    if let Some(folder) = folder {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(folder.join("capture-timing.jsonl"))?;
        log(&mut file, event)?;
    }
    Ok(())
}

fn append_aligned(
    aligner: &mut Aligner,
    spool: &mut Spool,
    frames: usize,
    progress: &Progress,
    slots: &[usize],
    status: &SharedStatus,
) -> Result<()> {
    let start = spool.frames;
    let block = aligner.take(start, frames)?;
    spool.append(&block.tracks)?;
    let metadata = BlockMetadata {
        frames: frames as u64,
        valid: block.valid,
        coverage: block.coverage,
        valid_windows: block.valid_windows,
    };
    fs::write(
        spool.dir.join(format!("{start:020}_valid.json")),
        serde_json::to_vec(&metadata)?,
    )?;
    update(status, |s| {
        for (&slot, coverage) in slots.iter().zip(metadata.coverage) {
            s.coverage[slot] = Some(coverage);
        }
    });
    progress.produced.store(spool.frames, Ordering::Release);
    Ok(())
}

fn check_backlog(config: &Config, progress: &Progress, channels: &[u16]) -> Result<()> {
    let produced = progress.produced.load(Ordering::Acquire);
    let analyzed = progress.analyzed.load(Ordering::Acquire);
    let settled = progress.settled.load(Ordering::Acquire);
    let committed = progress.committed.load(Ordering::Acquire);
    ensure!(
        (produced.saturating_sub(analyzed) as f64 / RATE as f64) <= config.max_pending_secs,
        "音声解析の処理待ちが上限（{}秒）を超えたため停止します",
        config.max_pending_secs
    );
    let bytes_per_frame = channels.iter().map(|&ch| u64::from(ch)).sum::<u64>() * 4;
    // 原音は公開用ファイル一式の確定まで保持し、処理済み音声だけを
    // コーデックへの書込み位置で解放する。各1秒ファイルの丸め分も確保する。
    let raw_frames = produced.saturating_sub(committed).saturating_add(RATE);
    let processed_frames = analyzed.saturating_sub(settled).saturating_add(RATE);
    let spool_bytes = raw_frames
        .saturating_add(processed_frames)
        .saturating_mul(bytes_per_frame);
    ensure!(
        spool_bytes <= config.spool_limit_mb.saturating_mul(1_024 * 1_024),
        "一時音声の容量が上限を超えたため停止します"
    );
    Ok(())
}

fn update_wave(
    event: &CaptureEvent,
    preview_secs: f64,
    slots: &[usize],
    status: &SharedStatus,
    now: u64,
) {
    let CaptureEvent::Packet {
        track,
        sample_rate,
        channels,
        samples,
        qpc_100ns,
        timestamp_valid,
        ..
    } = event
    else {
        return;
    };
    let Some(&slot) = slots.get(*track) else {
        return;
    };
    if *channels == 0 || *sample_rate == 0 || samples.is_empty() {
        return;
    }
    let rate = u64::from(*sample_rate);
    let channels = usize::from(*channels);
    let duration = samples.len() as u64 / channels as u64 * QPC_TICKS_PER_SECOND / rate;
    let latest_start = now.saturating_sub(duration);
    let packet_start = if *timestamp_valid {
        (*qpc_100ns).min(latest_start)
    } else {
        latest_start
    };
    let window = wave_window_ticks(preview_secs);
    let bucket = window.div_ceil(WAVE_POINTS as u64);
    let stride = (bucket * rate).div_ceil(QPC_TICKS_PER_SECOND) as usize;
    update(status, |status| {
        let wave = &mut status.waves[slot];
        for (index, frame) in samples.chunks_exact(channels).enumerate().step_by(stride) {
            let time = packet_start + index as u64 * QPC_TICKS_PER_SECOND / rate;
            if wave
                .back()
                .is_some_and(|last| last.qpc_100ns / bucket >= time / bucket)
            {
                continue;
            }
            let amplitude = frame
                .iter()
                .copied()
                .max_by(|a, b| a.abs().total_cmp(&b.abs()))
                .unwrap_or(0.0);
            wave.push_back(WavePoint {
                qpc_100ns: time,
                amplitude,
            });
        }
        let cutoff = (packet_start + duration).saturating_sub(window);
        while wave.front().is_some_and(|point| point.qpc_100ns < cutoff) || wave.len() > WAVE_POINTS
        {
            wave.pop_front();
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn analyze(
    config: Config,
    folder: PathBuf,
    channels: Vec<u16>,
    slots: Vec<usize>,
    speech: SpeechClient,
    progress: &Progress,
    status: &SharedStatus,
    finalizer: FinalizerClient,
) -> Result<()> {
    let mut raw = Spool::open(&folder.join("raw"), channels.clone())?;
    let mut processed = Spool::create(&folder.join("processed"), channels.clone())?;
    let mut capture_audit = CaptureAudit {
        client: finalizer.audit.clone(),
        counters: Counters::default(),
        frames: 0,
    };
    let mut output =
        Output::with_finalizer(config.clone(), folder.clone(), channels.clone(), finalizer)?;
    let mut next = 0;
    let modes = slots
        .iter()
        .map(|&slot| config.tracks[slot].denoise.clone())
        .collect();
    let timeout = Duration::from_secs_f64(
        config
            .analysis_timeout_secs
            .min(config.max_pending_secs / (channels.len() + 1) as f64)
            .max(0.001),
    );
    let mut speech = ResilientSpeech::new(
        speech,
        config.speech.clone(),
        modes,
        Duration::from_secs_f64(config.reconnect_secs),
    )
    .with_timeout(timeout);
    loop {
        let produced = progress.produced.load(Ordering::Acquire);
        if next >= produced {
            if progress.is_drained(next) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        let frames = (produced - next).min(RATE);
        let metadata: BlockMetadata =
            serde_json::from_slice(&fs::read(raw.dir.join(format!("{next:020}_valid.json")))?)?;
        let window_count = (frames as usize).div_ceil(ANALYSIS_WINDOW_FRAMES);
        ensure!(
            metadata.frames == frames
                && metadata.valid.len() == channels.len()
                && metadata.coverage.len() == channels.len()
                && metadata.valid_windows.len() == channels.len()
                && metadata
                    .valid_windows
                    .iter()
                    .all(|windows| windows.len() == window_count),
            "一時音声の確定情報が不正です"
        );
        let mut tracks = raw.read(next, next + frames)?;
        let mut states = vec![SpeechState::Unknown; channels.len()];
        let mut track_windows = vec![Vec::new(); channels.len()];
        let split_state = metadata.valid.contains(&false);
        let overloaded = (produced - next) as f64 > config.max_pending_secs * RATE as f64 / 2.0;
        let mut analysis_wait = overloaded;
        if !overloaded {
            for index in 0..channels.len() {
                let windows = &metadata.valid_windows[index];
                if !windows.contains(&true) {
                    continue;
                }
                let mode = config.tracks[slots[index]].denoise.clone();
                match speech.process(
                    index,
                    RATE as u32,
                    channels[index],
                    &tracks[index],
                    mode,
                    split_state,
                ) {
                    Ok(Some(analysis)) => {
                        states[index] = analysis.state;
                        track_windows[index] = analysis.window_states;
                        if metadata.valid[index] {
                            tracks[index] = analysis.samples;
                        } else {
                            // 解析要求は従来どおり毎秒1回。同期未確定の音声には加工を適用しない。
                            let window_samples =
                                ANALYSIS_WINDOW_FRAMES * usize::from(channels[index]);
                            for ((original, enhanced), &valid) in tracks[index]
                                .chunks_mut(window_samples)
                                .zip(analysis.samples.chunks(window_samples))
                                .zip(windows)
                            {
                                if valid {
                                    original.copy_from_slice(enhanced);
                                }
                            }
                        }
                        if analysis.transcript.is_some() {
                            log(
                                &mut output.timeline,
                                json!({"event":"transcript","track":slots[index],"source_start":next,"text":analysis.transcript,"segments":analysis.segments}),
                            )?;
                        }
                    }
                    Ok(None) => {
                        analysis_wait = true;
                        states.fill(SpeechState::Unknown);
                        track_windows.iter_mut().for_each(Vec::clear);
                        tracks = raw.read(next, next + frames)?;
                        break;
                    }
                    Err(error) => {
                        let message = format!(
                            "音声解析を再準備します。該当区間は原音・判定不能で保持します: {error:#}"
                        );
                        log(
                            &mut output.timeline,
                            json!({"event":"analysis_error","source_start":next,"message":message}),
                        )?;
                        analysis_wait = true;
                        states.fill(SpeechState::Unknown);
                        track_windows.iter_mut().for_each(Vec::clear);
                        // 一部だけ除去処理済みにしない。原音一式で当該区間を残す。
                        tracks = raw.read(next, next + frames)?;
                        break;
                    }
                }
            }
        }
        processed.append(&tracks)?;
        let intervals = metadata.speech_intervals(&states, &track_windows);
        // 表示上の「非発話」は、対象トラックの全区間を判定できた場合に限る。
        for (state, &valid) in states.iter_mut().zip(&metadata.valid) {
            if *state == SpeechState::Silence && !valid {
                *state = SpeechState::Unknown;
            }
        }
        let mut event = json!({"event":"speech_state","source_start":next,"frames":frames,"slots":&slots,"timestamp_coverage_valid":&metadata.valid,"states":&states,"aggregate":aggregate_states(&states)});
        if split_state {
            event["coverage"] = json!(metadata.coverage);
            event["valid_windows"] = json!(metadata.valid_windows);
            if intervals.len() > 1 {
                event["intervals"] = json!(intervals);
            } else {
                event["aggregate"] = json!(intervals[0].state);
            }
        }
        if analysis_wait {
            event["analysis_deferred"] = json!(if overloaded { "backlog" } else { "worker" });
        }
        log(&mut output.timeline, event)?;
        let mut start = next;
        for interval in intervals {
            let actions = output.editor.push(start, interval.frames, interval.state)?;
            output.apply(actions, &mut processed)?;
            if interval.state == SpeechState::Unknown {
                capture_audit.counters.unknown_frames += interval.frames;
            }
            start += interval.frames;
        }
        for track in metadata.coverage {
            capture_audit.counters.missing_track_frames += track.missing_frames as u64;
            capture_audit.counters.unsynchronized_track_frames +=
                track.unsynchronized_frames as u64;
        }
        capture_audit.counters.reclaimed_bytes += raw.discard_before(output.committed_source)?;
        capture_audit.counters.reclaimed_bytes += processed.discard_before(output.settled)?;
        capture_audit.frames += frames;
        if capture_audit.frames >= 60 * RATE {
            capture_audit.flush();
        }
        next += frames;
        progress.settled.store(output.settled, Ordering::Release);
        progress
            .committed
            .store(output.committed_source, Ordering::Release);
        progress.analyzed.store(next, Ordering::Release);
        update(status, |status| {
            if status.phase == RecorderPhase::Recording && analysis_wait {
                status.message = if overloaded {
                    "録音継続・解析待ちを原音で保持".into()
                } else {
                    "録音継続・音声モデル再準備中（原音保持）".into()
                };
            } else if status.phase == RecorderPhase::Recording
                && slots.iter().all(|&slot| status.connected[slot])
            {
                status.message = "録音中".into();
            }
            for (index, &slot) in slots.iter().enumerate() {
                status.speech[slot] = states[index];
            }
            status.saved_secs = output.saved_frames as f64 / RATE as f64;
            status.pending_secs = progress
                .produced
                .load(Ordering::Acquire)
                .saturating_sub(next) as f64
                / RATE as f64;
        });
    }
    let actions = output.editor.finish();
    output.apply(actions, &mut processed)?;
    let released = if output.committed_source >= next {
        output.committed_source.div_ceil(RATE).saturating_mul(RATE)
    } else {
        output.committed_source
    };
    capture_audit.counters.reclaimed_bytes += raw.discard_before(released)?;
    capture_audit.counters.reclaimed_bytes +=
        processed.discard_before(if output.settled >= next {
            output.settled.div_ceil(RATE).saturating_mul(RATE)
        } else {
            output.settled
        })?;
    output.timeline.sync_all()?;
    progress
        .committed
        .store(output.committed_source, Ordering::Release);
    update(status, |status| {
        status.saved_secs = output.saved_frames as f64 / RATE as f64;
        status.pending_secs = 0.0;
    });
    Ok(())
}

struct CaptureAudit {
    client: Option<AuditClient>,
    counters: Counters,
    frames: u64,
}

impl CaptureAudit {
    fn flush(&mut self) {
        if let Some(client) = &self.client
            && (self.frames > 0 || self.counters.reclaimed_bytes > 0)
        {
            client.record(EventKind::Capture, std::mem::take(&mut self.counters));
            self.frames = 0;
        }
    }
}

impl Drop for CaptureAudit {
    fn drop(&mut self) {
        self.flush();
    }
}

fn aggregate_states(states: &[SpeechState]) -> SpeechState {
    if states.contains(&SpeechState::Speech) {
        SpeechState::Speech
    } else if !states.is_empty() && states.iter().all(|state| *state == SpeechState::Silence) {
        SpeechState::Silence
    } else {
        SpeechState::Unknown
    }
}

struct Output {
    editor: Editor,
    config: Config,
    folder: PathBuf,
    channels: Vec<u16>,
    timeline: File,
    writer: Option<JournalWriter>,
    finalizer: FinalizerClient,
    #[cfg(test)]
    local_finalizer: Option<Finalizer>,
    number: u64,
    segment_start: u64,
    segment_frames: u64,
    saved_frames: u64,
    settled: u64,
    committed_source: u64,
}

impl Output {
    #[cfg(test)]
    fn new(config: Config, folder: PathBuf, channels: Vec<u16>) -> Result<Self> {
        let service = Finalizer::new();
        let mut output = Self::with_finalizer(config, folder, channels, service.client())?;
        output.local_finalizer = Some(service);
        Ok(output)
    }
    fn with_finalizer(
        config: Config,
        folder: PathBuf,
        channels: Vec<u16>,
        finalizer: FinalizerClient,
    ) -> Result<Self> {
        Ok(Self {
            editor: Editor::new(config.timing.clone(), RATE as u32)?,
            timeline: OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(folder.join("timeline.jsonl"))?,
            config,
            folder,
            channels,
            writer: None,
            finalizer,
            #[cfg(test)]
            local_finalizer: None,
            number: 1,
            segment_start: 0,
            segment_frames: 0,
            saved_frames: 0,
            settled: 0,
            committed_source: 0,
        })
    }

    fn apply(&mut self, actions: Vec<EditAction>, spool: &mut Spool) -> Result<()> {
        self.finalizer.check()?;
        for action in actions {
            match action {
                EditAction::Keep { start, end } => {
                    if self.writer.is_none() {
                        self.writer = Some(JournalWriter::new(
                            &self.folder,
                            self.number,
                            self.channels.clone(),
                            self.config.format,
                            &self.config.ffmpeg_path,
                        )?);
                    }
                    let mut cursor = start;
                    while cursor < end {
                        let part_end = end.min(cursor + RATE);
                        let samples = spool.read(cursor, part_end)?;
                        self.writer
                            .as_mut()
                            .context("保存ファイルがありません")?
                            .write(&samples)?;
                        cursor = part_end;
                    }
                    self.segment_frames += end - start;
                    self.saved_frames += end - start;
                    self.settled = end;
                }
                EditAction::Cut { end, .. } => {
                    self.settled = end;
                }
                EditAction::Split { at, discard } => {
                    if self.writer.is_none() {
                        self.writer = Some(JournalWriter::new(
                            &self.folder,
                            self.number,
                            self.channels.clone(),
                            self.config.format,
                            &self.config.ffmpeg_path,
                        )?);
                    }
                    self.timeline.sync_data()?;
                    self.writer
                        .as_mut()
                        .expect("内部保存")
                        .prepare_finish(at, discard)?;
                    if let Some(writer) = self.writer.take() {
                        let recorded_frames = at - self.segment_start;
                        self.finalizer.submit_segment(
                            writer,
                            at,
                            discard,
                            if discard {
                                Counters {
                                    recorded_frames,
                                    excluded_frames: recorded_frames,
                                    ..Counters::default()
                                }
                            } else {
                                Counters {
                                    recorded_frames,
                                    saved_frames: self.segment_frames,
                                    silence_cut_frames: recorded_frames - self.segment_frames,
                                    ..Counters::default()
                                }
                            },
                        )?;
                        self.segment_start = at;
                    }
                    if discard {
                        self.saved_frames -= self.segment_frames;
                    }
                    self.committed_source = at;
                }
            }
            log(
                &mut self.timeline,
                json!({"event":"edit","segment":self.number,"action":action,"segment_frames":self.segment_frames,"saved_frames":self.saved_frames}),
            )?;
            if matches!(action, EditAction::Split { .. }) {
                self.number += 1;
                self.segment_frames = 0;
                self.timeline.sync_data()?;
            }
        }
        let checkpoint_frames = (self.config.checkpoint_secs * RATE as f64).round() as u64;
        if self.settled.saturating_sub(self.committed_source) >= checkpoint_frames {
            if self.writer.is_none() {
                self.writer = Some(JournalWriter::new(
                    &self.folder,
                    self.number,
                    self.channels.clone(),
                    self.config.format,
                    &self.config.ffmpeg_path,
                )?);
            }
            self.timeline.sync_data()?;
            let writer = self.writer.as_mut().expect("内部保存");
            writer.checkpoint(self.settled)?;
            self.committed_source = writer.durable_source();
        }
        Ok(())
    }
}

fn log(file: &mut File, value: serde_json::Value) -> Result<()> {
    serde_json::to_writer(&mut *file, &value)?;
    file.write_all(b"\n")?;
    Ok(())
}

fn create_session_folder(base: &Path) -> Result<PathBuf> {
    fs::create_dir_all(base)?;
    let stem = format!(
        "{}_{}",
        chrono::Local::now().format("%Y%m%d-%H%M%S-%3f"),
        std::process::id()
    );
    for suffix in 0..1_000 {
        let path = base.join(format!("{stem}_{suffix:03}"));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("録音フォルダーを作成できません"),
        }
    }
    bail!("重複しない録音フォルダーを作成できません");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputFormat;

    #[test]
    fn a_gap_only_protects_its_window_across_all_tracks() -> Result<()> {
        let block = BlockMetadata {
            frames: RATE,
            valid: vec![false, true],
            coverage: vec![],
            valid_windows: vec![vec![true, false, true, true], vec![true; 4]],
        };
        let intervals = block.speech_intervals(
            &[SpeechState::Speech, SpeechState::Silence],
            &[
                vec![
                    SpeechState::Silence,
                    SpeechState::Speech,
                    SpeechState::Silence,
                    SpeechState::Silence,
                ],
                vec![],
            ],
        );
        assert_eq!(
            intervals,
            vec![
                SpeechInterval {
                    frames: 12_000,
                    state: SpeechState::Silence
                },
                SpeechInterval {
                    frames: 12_000,
                    state: SpeechState::Unknown
                },
                SpeechInterval {
                    frames: 24_000,
                    state: SpeechState::Silence
                },
            ]
        );
        let timing = crate::editor::TimingConfig {
            keep_silence_secs: 0.0,
            ..Default::default()
        };
        let mut editor = Editor::new(timing, RATE as u32)?;
        let mut at = 0;
        let mut edits = Vec::new();
        for interval in intervals {
            edits.extend(editor.push(at, interval.frames, interval.state)?);
            at += interval.frames;
        }
        edits.extend(editor.finish());
        assert_eq!(
            edits[0],
            EditAction::Cut {
                start: 0,
                end: 12_000
            }
        );
        assert_eq!(
            edits[1],
            EditAction::Keep {
                start: 12_000,
                end: 24_000
            }
        );
        assert_eq!(
            edits[2],
            EditAction::Cut {
                start: 24_000,
                end: RATE
            }
        );
        Ok(())
    }

    #[test]
    fn phase_reader_tracks_changes_and_stop_does_not_overwrite_saving_or_terminal_state() {
        let status = Arc::new(Mutex::new(Status {
            phase: RecorderPhase::Preparing,
            ..Status::default()
        }));
        let recorder = Recorder {
            stop: Arc::new(AtomicBool::new(false)),
            status: status.clone(),
            handle: None,
        };
        let read = recorder.phase_reader();
        assert_eq!(read(), RecorderPhase::Preparing);
        update(&status, |status| status.phase = RecorderPhase::Preview);
        assert_eq!(read(), RecorderPhase::Preview);
        recorder.stop();
        assert_eq!(read(), RecorderPhase::Finalizing);
        update(&status, |status| {
            status.set_finalizing("音声ファイルを保存中")
        });
        recorder.stop();
        assert_eq!(recorder.status().message, "音声ファイルを保存中");
        for phase in [RecorderPhase::Finished, RecorderPhase::Failed] {
            update(&status, |status| {
                status.phase = phase;
                status.message = "最終結果".into();
            });
            recorder.stop();
            assert_eq!(read(), phase);
            assert_eq!(recorder.status().message, "最終結果");
        }
    }

    #[test]
    fn one_speech_track_or_unknown_never_becomes_common_silence() {
        assert_eq!(
            aggregate_states(&[
                SpeechState::Silence,
                SpeechState::Speech,
                SpeechState::Unknown,
                SpeechState::Silence
            ]),
            SpeechState::Speech
        );
        assert_eq!(
            aggregate_states(&[
                SpeechState::Silence,
                SpeechState::Silence,
                SpeechState::Unknown,
                SpeechState::Silence
            ]),
            SpeechState::Unknown
        );
        assert_eq!(
            aggregate_states(&[SpeechState::Silence; 4]),
            SpeechState::Silence
        );
        assert_eq!(aggregate_states(&[]), SpeechState::Unknown);
    }

    fn synthetic(
        states: &[SpeechState],
        timing: crate::editor::TimingConfig,
    ) -> Result<(tempfile::TempDir, u64)> {
        let directory = tempfile::tempdir()?;
        let channels = vec![1, 2, 1, 2];
        let config = Config {
            format: OutputFormat::Wav,
            timing,
            ..Config::default()
        };
        let mut spool = Spool::create(&directory.path().join("processed"), channels.clone())?;
        let mut output = Output::new(config, directory.path().into(), channels.clone())?;
        for (index, &state) in states.iter().enumerate() {
            let tracks = channels
                .iter()
                .enumerate()
                .map(|(track, &channels)| {
                    vec![0.1 * (track + 1) as f32; RATE as usize * usize::from(channels)]
                })
                .collect::<Vec<_>>();
            spool.append(&tracks)?;
            let actions = output.editor.push(index as u64 * RATE, RATE, state)?;
            output.apply(actions, &mut spool)?;
            spool.discard_before(output.settled)?;
        }
        let actions = output.editor.finish();
        output.apply(actions, &mut spool)?;
        Ok((directory, output.saved_frames))
    }

    #[test]
    fn four_tracks_keep_same_frames_through_cross_block_silence_trim() -> Result<()> {
        let mut states = vec![SpeechState::Speech];
        states.extend(vec![SpeechState::Silence; 20]);
        states.push(SpeechState::Unknown);
        let (directory, frames) = synthetic(&states, crate::editor::TimingConfig::default())?;
        assert_eq!(frames, 5 * RATE);
        for (track, channels) in [1, 2, 1, 2].into_iter().enumerate() {
            let bytes = fs::read(
                directory
                    .path()
                    .join(format!("000001_track{}.wav", track + 1)),
            )?;
            assert_eq!(u64::from_le_bytes(bytes[36..44].try_into()?), frames);
            assert_eq!(bytes.len() as u64, 80 + frames * channels * 4);
        }
        assert!(directory.path().join("000001.complete.json").is_file());
        Ok(())
    }

    fn verify_periodic_output(format: OutputFormat, ffmpeg_path: PathBuf) -> Result<()> {
        let directory = tempfile::tempdir()?;
        let channels = vec![1, 2];
        let config = Config {
            format,
            ffmpeg_path,
            checkpoint_secs: 1.0,
            timing: crate::editor::TimingConfig {
                segment_secs: 2.0,
                split_silence_secs: 0.5,
                ..Default::default()
            },
            ..Config::default()
        };
        let mut spool = Spool::create(&directory.path().join("processed"), channels.clone())?;
        let mut output = Output::new(config, directory.path().into(), channels)?;
        let tracks = [vec![0.25; RATE as usize], vec![0.5; 2 * RATE as usize]];
        for second in 0..6 {
            spool.append(&tracks)?;
            let state = if second < 3 {
                SpeechState::Unknown
            } else {
                SpeechState::Speech
            };
            let actions = output.editor.push(second * RATE, RATE, state)?;
            output.apply(actions, &mut spool)?;
            spool.discard_before(output.settled)?;
        }
        let verify_segment = |number: u64, frames: u64| -> Result<()> {
            let manifest: serde_json::Value = serde_json::from_slice(&fs::read(
                directory.path().join(format!("{number:06}.complete.json")),
            )?)?;
            assert_eq!(manifest["frames"], frames);
            assert_eq!(manifest["channels"], json!([1, 2]));
            Ok(())
        };
        output.finalizer.wait()?;
        assert_eq!(output.finalizer.snapshot().saved, 2);
        verify_segment(1, 5 * RATE / 2)?;
        verify_segment(2, 5 * RATE / 2)?;
        assert!(!directory.path().join("000003.complete.json").exists());
        let actions = output.editor.finish();
        output.apply(actions, &mut spool)?;
        output.finalizer.wait()?;
        assert_eq!(output.finalizer.snapshot().saved, 3);
        verify_segment(3, RATE)?;
        assert_eq!(output.saved_frames, 6 * RATE);
        Ok(())
    }

    #[test]
    fn periodic_wav_segments_are_saved_before_stop() -> Result<()> {
        verify_periodic_output(OutputFormat::Wav, PathBuf::from("missing"))
    }

    #[test]
    #[ignore = "FFmpeg が必要。録音中の M4A 分割・確定を検証"]
    fn periodic_m4a_segments_are_saved_before_stop() -> Result<()> {
        let ffmpeg = std::env::var_os("TALKRECORDER_FFMPEG")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("runtime/ffmpeg/bin/ffmpeg.exe"));
        verify_periodic_output(OutputFormat::M4a, ffmpeg)
    }

    #[test]
    fn all_silent_segment_is_removed_as_one_group() -> Result<()> {
        let timing = crate::editor::TimingConfig {
            segment_secs: 6.,
            split_silence_secs: 1.,
            keep_silence_secs: 0.3,
            discard_silence_secs: 6.,
        };
        let (directory, frames) = synthetic(&[SpeechState::Silence; 6], timing)?;
        assert_eq!(frames, 0);
        assert!(
            fs::read_dir(directory.path())?.all(|entry| entry.is_ok_and(|entry| entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "wav")))
        );
        assert!(!directory.path().join("000001.complete.json").exists());
        Ok(())
    }

    #[test]
    fn backlog_limit_and_thread_failures_are_observable() {
        let config = Config::default();
        let progress = Progress::default();
        progress.produced.store(31 * RATE, Ordering::Release);
        assert!(check_backlog(&config, &progress, &[1, 2, 1, 2]).is_err());
        let handle = thread::spawn(|| -> Result<()> { bail!("synthetic capture failure") });
        assert!(join_checked(handle, "音声取得").is_err());
        let handle = thread::spawn(|| -> Result<()> { panic!("synthetic panic") });
        assert!(join_checked(handle, "音声取得").is_err());
    }

    #[test]
    fn final_publication_is_reloaded_after_done_is_observed() {
        use std::sync::Barrier;
        let progress = Arc::new(Progress::default());
        let start = Arc::new(Barrier::new(2));
        let done = Arc::new(Barrier::new(2));
        let producer = {
            let progress = Arc::clone(&progress);
            let start = Arc::clone(&start);
            let done = Arc::clone(&done);
            thread::spawn(move || {
                start.wait();
                progress.produced.store(RATE, Ordering::Release);
                progress.done.store(true, Ordering::Release);
                done.wait();
            })
        };
        // 実際の解析ループと同じく、完了フラグより先に位置を読んだ状況。
        let stale_position = progress.produced.load(Ordering::Acquire);
        assert_eq!(stale_position, 0);
        start.wait();
        done.wait();
        assert!(!progress.is_drained(stale_position));
        assert!(progress.is_drained(RATE));
        producer.join().unwrap();
    }

    fn uncommitted_source(directory: &Path) -> Result<(Spool, Spool, Output)> {
        let channels = vec![1, 2];
        let mut raw = Spool::create(&directory.join("raw"), channels.clone())?;
        let mut processed = Spool::create(&directory.join("processed"), channels.clone())?;
        for _ in 0..3 {
            let tracks = vec![vec![0.25; RATE as usize], vec![0.5; RATE as usize * 2]];
            raw.append(&tracks)?;
            processed.append(&tracks)?;
        }
        let config = Config {
            format: OutputFormat::Wav,
            ..Config::default()
        };
        let mut output = Output::new(config, directory.into(), channels)?;
        let actions = output.editor.push(0, 3 * RATE, SpeechState::Speech)?;
        output.apply(actions, &mut processed)?;
        Ok((raw, processed, output))
    }

    #[test]
    fn raw_source_is_retained_until_the_segment_group_is_committed() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (raw, mut processed, mut output) = uncommitted_source(directory.path())?;
        assert_eq!(output.settled, 3 * RATE);
        assert_eq!(output.committed_source, 0);
        raw.discard_before(output.committed_source)?;
        processed.discard_before(output.settled)?;
        assert_eq!(fs::read_dir(&raw.dir)?.count(), 6);
        assert_eq!(fs::read_dir(&processed.dir)?.count(), 0);
        let actions = output.editor.finish();
        output.apply(actions, &mut processed)?;
        assert_eq!(output.committed_source, 3 * RATE);
        output.finalizer.wait()?;
        assert!(directory.path().join("000001.complete.json").exists());
        raw.discard_before(output.committed_source)?;
        assert_eq!(fs::read_dir(&raw.dir)?.count(), 0);
        Ok(())
    }

    #[test]
    fn failed_public_finalization_keeps_its_durable_internal_audio() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let (raw, mut processed, mut output) = uncommitted_source(directory.path())?;
        // 確定名にディレクトリを置いて、書込み後の最終リネームを失敗させる。
        fs::create_dir(directory.path().join("000001_track1.wav"))?;
        let actions = output.editor.finish();
        output.apply(actions, &mut processed)?;
        assert!(output.finalizer.wait().is_err());
        assert_eq!(output.committed_source, 3 * RATE);
        let checkpoint = crate::journal::load_state(directory.path(), 1)?.expect("内部確定済み");
        assert_eq!(checkpoint.frames, 3 * RATE);
        assert!(!checkpoint.published);
        raw.discard_before(output.committed_source)?;
        assert_eq!(fs::read_dir(&raw.dir)?.count(), 0);
        assert!(!directory.path().join("000001.complete.json").exists());
        Ok(())
    }

    #[test]
    fn spool_limit_counts_uncommitted_raw_even_when_analysis_has_caught_up() {
        let config = Config {
            spool_limit_mb: 16,
            ..Config::default()
        };
        let progress = Progress::default();
        progress.produced.store(20 * RATE, Ordering::Release);
        progress.analyzed.store(20 * RATE, Ordering::Release);
        progress.settled.store(20 * RATE, Ordering::Release);
        assert!(check_backlog(&config, &progress, &[1, 2, 1, 2]).is_err());
        progress.committed.store(20 * RATE, Ordering::Release);
        assert!(check_backlog(&config, &progress, &[1, 2, 1, 2]).is_ok());
    }

    #[test]
    fn continuous_speech_releases_raw_at_checkpoints_without_a_public_split() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let channels = vec![1, 1, 2, 2];
        let mut raw = Spool::create(&dir.path().join("raw"), channels.clone())?;
        let mut processed = Spool::create(&dir.path().join("processed"), channels.clone())?;
        let mut output = Output::new(
            Config {
                format: OutputFormat::Wav,
                checkpoint_secs: 5.0,
                ..Config::default()
            },
            dir.path().into(),
            channels.clone(),
        )?;
        let tracks: Vec<_> = channels
            .iter()
            .map(|&ch| vec![0.25; RATE as usize * usize::from(ch)])
            .collect();
        for second in 0..16 {
            raw.append(&tracks)?;
            processed.append(&tracks)?;
            let actions = output
                .editor
                .push(second * RATE, RATE, SpeechState::Speech)?;
            output.apply(actions, &mut processed)?;
            raw.discard_before(output.committed_source)?;
            processed.discard_before(output.settled)?;
            assert!(fs::read_dir(&raw.dir)?.count() <= 4 * 4);
            assert_eq!(output.number, 1, "利用者向けの分割は発生していない");
        }
        assert_eq!(output.committed_source, 15 * RATE);
        assert!(!dir.path().join("000001.complete.json").exists());
        let state = crate::journal::load_state(dir.path(), 1)?.expect("内部確定");
        assert_eq!(state.source_until, 15 * RATE);
        assert_eq!(state.parts, 3);
        let actions = output.editor.finish();
        output.apply(actions, &mut processed)?;
        output.finalizer.wait()?;
        Ok(())
    }

    #[test]
    fn cumulative_audio_over_four_gib_does_not_count_as_pending_spool() {
        let config = Config {
            spool_limit_mb: 16,
            ..Config::default()
        };
        let progress = Progress::default();
        let historic_frames = 5 * 1024_u64.pow(3) / (6 * 4);
        for seconds in 0..20 {
            let produced = historic_frames + seconds * RATE;
            progress.produced.store(produced, Ordering::Release);
            progress.analyzed.store(produced, Ordering::Release);
            progress.settled.store(produced, Ordering::Release);
            progress
                .committed
                .store(produced - 10 * RATE, Ordering::Release);
            assert!(check_backlog(&config, &progress, &[1, 1, 2, 2]).is_ok());
        }
    }

    fn fake_device(id: &str, kind: crate::capture::DeviceKind, ch: u16) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: id.into(),
            kind,
            sample_rate: 48000,
            channels: ch,
        }
    }

    #[test]
    fn preview_keeps_available_tracks_with_other_tracks_unconfigured() {
        use crate::capture::DeviceKind::{Microphone, Speaker};
        let mut config = Config::default();
        config.tracks[1].enabled = true;
        config.tracks[1].device_id = "offline".into();
        config.tracks[2].enabled = true;
        config.tracks[2].device_id = "render".into();
        let available = [
            fake_device("render", Speaker, 2),
            fake_device("unused", Microphone, 1),
        ];
        assert!(config.validate().is_err());
        let (slots, devices) = preview_devices(&config, &available);
        assert_eq!(slots, [2]);
        assert_eq!(devices[0].id, "render");
    }

    #[test]
    fn preview_clears_only_the_disconnected_wave_and_accepts_new_samples() -> Result<()> {
        let status = Arc::new(Mutex::new(Status::default()));
        let config = Config::default();
        let packet = |track, sample| CaptureEvent::Packet {
            track,
            sample_rate: 48_000,
            channels: 1,
            device_frame: 0,
            qpc_100ns: 0,
            samples: vec![sample; 480],
            discontinuity: false,
            timestamp_valid: true,
        };
        let process = |events: Vec<CaptureEvent>| -> Result<()> {
            let (send, receive) = mpsc::sync_channel(4);
            for event in events {
                send.send(event)?;
            }
            drop(send);
            let error = run_preview(&receive, &config, &[0, 2], &AtomicBool::new(false), &status)
                .unwrap_err();
            assert!(error.to_string().contains("入力確認が予期せず終了"));
            Ok(())
        };
        process(vec![
            packet(0, 0.1),
            packet(1, 0.2),
            CaptureEvent::Connection {
                track: 0,
                connected: false,
                message: String::new(),
            },
        ])?;
        assert!(status.lock().unwrap().waves[0].is_empty());
        assert_eq!(
            status.lock().unwrap().waves[2]
                .iter()
                .map(|p| p.amplitude)
                .collect::<Vec<_>>(),
            [0.2, 0.2]
        );
        process(vec![
            CaptureEvent::Connection {
                track: 0,
                connected: true,
                message: String::new(),
            },
            packet(0, 0.3),
        ])?;
        let status = status.lock().unwrap();
        assert!(status.connected[0]);
        assert_eq!(
            status.waves[0]
                .iter()
                .map(|p| p.amplitude)
                .collect::<Vec<_>>(),
            [0.3, 0.3]
        );
        Ok(())
    }

    #[test]
    fn waveform_sampling_uses_the_time_window_across_short_packets() {
        let status = Arc::new(Mutex::new(Status::default()));
        let interval = QPC_TICKS_PER_SECOND / 100;
        for index in 0..100 {
            let time = index * interval;
            update_wave(
                &CaptureEvent::Packet {
                    track: 0,
                    sample_rate: 48_000,
                    channels: 1,
                    device_frame: index * 480,
                    qpc_100ns: time,
                    samples: vec![0.25; 480],
                    discontinuity: false,
                    timestamp_valid: true,
                },
                600.0,
                &[0],
                &status,
                time + interval,
            );
        }
        let view = status.lock().unwrap();
        let times: Vec<_> = view.waves[0].iter().map(|point| point.qpc_100ns).collect();
        assert_eq!(times, [0, 3_000_000, 6_000_000, 9_000_000]);
    }

    #[test]
    fn cached_device_format_allows_starting_while_unplugged() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let mut config = Config {
            output_dir: dir.path().into(),
            ..Config::default()
        };
        config.tracks[0].device_id = "mic".into();
        let device = fake_device("mic", crate::capture::DeviceKind::Microphone, 1);
        assert!(select_devices(&config, &[]).is_err());
        remember_devices(dir.path(), std::slice::from_ref(&device))?;
        remember_devices(dir.path(), std::slice::from_ref(&device))?;
        let (slots, selected) = select_devices(&config, &[])?;
        assert_eq!(slots, vec![0]);
        assert_eq!(selected[0].channels, 1);
        assert!(
            select_devices(
                &config,
                &[fake_device("other", crate::capture::DeviceKind::Speaker, 2)]
            )
            .is_ok()
        );
        Ok(())
    }

    #[test]
    fn startup_keeps_a_known_missing_track_and_the_other_ready_track() -> Result<()> {
        let (sender, receiver) = mpsc::sync_channel(4);
        sender.send(CaptureEvent::Connection {
            track: 0,
            connected: false,
            message: "unplugged".into(),
        })?;
        sender.send(CaptureEvent::Ready {
            track: 1,
            sample_rate: 44100,
            channels: 2,
        })?;
        let mut devices = vec![
            fake_device("mic", crate::capture::DeviceKind::Microphone, 1),
            fake_device("speaker", crate::capture::DeviceKind::Speaker, 2),
        ];
        let ready = await_ready(
            &receiver,
            &mut devices,
            &AtomicBool::new(false),
            Duration::from_millis(5),
        )?;
        assert_eq!(ready, vec![false, true]);
        assert_eq!(devices[1].sample_rate, 44100);
        Ok(())
    }

    #[test]
    fn suspend_gap_never_becomes_hours_of_zero_audio() {
        let mut state = ResumeState::default();
        assert!(!state.observe(12 * RATE, false));
        assert!(state.observe(3600 * RATE, true));
        assert_eq!(state.end(3601 * RATE, 11 * RATE), 12 * RATE);
        assert_eq!(state.end(3601 * RATE, 13 * RATE), 13 * RATE);
    }

    #[test]
    fn device_disconnect_is_a_track_status_change_instead_of_global_stop() -> Result<()> {
        let status = Arc::new(Mutex::new(Status {
            connected: vec![true, true, false, false],
            ..Status::default()
        }));
        let mut aligner = Aligner::new(1000, vec![1, 1], 2.0)?;
        ingest(
            CaptureEvent::Connection {
                track: 0,
                connected: false,
                message: "test".into(),
            },
            &Config::default(),
            &[0, 1],
            &mut aligner,
            &status,
        )?;
        assert_eq!(
            status.lock().unwrap().connected,
            vec![false, true, false, false]
        );
        assert_eq!(aligner.take(0, 10)?.valid, vec![false, false]);
        Ok(())
    }
}
