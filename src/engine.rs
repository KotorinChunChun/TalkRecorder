//! 録音取得をモデル推論・圧縮から切り離す、有限バッファの実行パイプライン。
use std::{
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
    capture::{self, CaptureEvent, DeviceInfo},
    clock_sync::ClockEstimate,
    config::{Config, OutputFormat},
    disk_space::{SpaceMonitor, SpaceReading},
    editor::{EditAction, Editor},
    finalize::{Finalizer, FinalizerClient},
    journal::JournalWriter,
    maintenance,
    power::{ResumeDetector, SleepGuard},
    recovery::SessionLock,
    speech::SpeechClient,
    storage::{Spool, check_encoder},
    sync_audio::Aligner,
};

const RATE: u64 = 48_000;
const CAPTURE_LAG: u64 = 14_400;

#[cfg(test)]
mod capacity_tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub folder: Option<PathBuf>,
    pub waves: Vec<Vec<f32>>,
    pub speech: Vec<SpeechState>,
    pub connected: Vec<bool>,
    pub timing: Vec<Option<ClockEstimate>>,
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
            folder: None,
            waves: vec![Vec::new(); 4],
            speech: vec![SpeechState::Unknown; 4],
            connected: vec![false; 4],
            timing: vec![None; 4],
        }
    }
}

type SharedStatus = Arc<Mutex<Status>>;

pub struct Recorder {
    stop: Arc<AtomicBool>,
    status: SharedStatus,
    handle: Option<JoinHandle<Result<()>>>,
}

impl Recorder {
    pub fn start(mut config: Config, preview: bool) -> Result<Self> {
        if config.auto_compact_wav && config.format == OutputFormat::Wav {
            config.format = OutputFormat::M4a;
        }
        config.validate()?;
        if !preview {
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
        ensure!(
            config.max_pending_secs >= 1.0,
            "処理待ち上限は1秒以上にしてください"
        );
        let available = capture::enumerate_devices()?;
        let (slots, devices) = select_devices(&config, &available)?;
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
                run(
                    config,
                    preview,
                    devices,
                    slots,
                    &thread_stop,
                    &thread_status,
                )
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
                status.message = match (&result, &status.capacity_stop) {
                    (Ok(()), Some(reason)) => format!(
                        "{}。{}",
                        reason.description(),
                        if status.folder.is_some() {
                            "録音を停止しました（保存処理完了）"
                        } else {
                            "録音を開始しませんでした"
                        }
                    ),
                    (Ok(()), None) => "停止しました".into(),
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
                RecorderPhase::Finished | RecorderPhase::Failed
            ) {
                status.phase = RecorderPhase::Finalizing;
                status.running = false;
                status.message = status.capacity_stop.as_ref().map_or_else(
                    || "停止処理中".into(),
                    |reason| format!("{}。停止・保存中", reason.description()),
                );
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
}

fn run(
    config: Config,
    preview: bool,
    devices: Vec<DeviceInfo>,
    slots: Vec<usize>,
    stop: &Arc<AtomicBool>,
    status: &SharedStatus,
) -> Result<()> {
    let mut capacity = SpaceMonitor::start(
        &config.output_dir,
        if preview {
            0
        } else {
            config.stop_free_space_mb
        },
    )?;
    if !preview {
        update(status, |s| {
            s.message = "録音の復旧・容量削減を確認中".into()
        });
        let report = maintenance::run(&config, true)?;
        update(status, |s| s.message = report.message());
    }
    let _sleep_guard = SleepGuard::new(config.prevent_sleep && !preview)?;
    let finalizer = Finalizer::new();
    let result = (|| -> Result<()> {
        while !stop.load(Ordering::Acquire) {
            let attempt = run_session(
                config.clone(),
                preview,
                devices.clone(),
                slots.clone(),
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
    let finalized = finalizer.finish();
    // 現在の録音の確定を優先し、容量停止後に過去WAVの追加変換を始めない。
    let capacity_stopped = status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .capacity_stop
        .is_some();
    let allow_maintenance = !preview
        && !capacity_stopped
        && capacity
            .check_now()
            .is_ok_and(|reading| reading.is_none_or(|reading| !reading.is_low()));
    let maintenance = if allow_maintenance {
        let report = maintenance::run(&config, false)?;
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
    if !preview && stop_for_capacity(capacity.check_now(), status, requested)? {
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
        }
    });
    if requested.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
        captures.join()?;
        return Ok(false);
    }
    let origin = capture::qpc_100ns();
    let mut detector = ResumeDetector::new();
    if preview {
        update(status, |status| {
            status.phase = RecorderPhase::Preview;
            status.message = "入力確認中".into();
        });
        let result = run_preview(&receiver, &config, &slots, origin, requested, status);
        let joined = captures.join();
        return result.and(joined).map(|()| false);
    }
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
        status.running = false;
        status.phase = RecorderPhase::Finalizing;
        status.elapsed_secs = end as f64 / RATE as f64;
        status.message = status.capacity_stop.as_ref().map_or_else(
            || "取得済み音声を確定中".into(),
            |reason| format!("{}。停止・保存中", reason.description()),
        );
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
    origin: u64,
    stop: &AtomicBool,
    status: &SharedStatus,
) -> Result<()> {
    while !stop.load(Ordering::Acquire) {
        match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => {
                update_wave(&event, config.preview_secs, slots, status);
                if let CaptureEvent::Error { track, message } = event {
                    bail!("デバイス{}: {message}", track + 1);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => bail!("入力確認が予期せず終了しました"),
        }
        update(status, |status| {
            status.elapsed_secs = source_frames(origin) as f64 / RATE as f64
        });
    }
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
        match stop_for_capacity(capacity.poll(), status, requested) {
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
            append_aligned(aligner, spool, RATE as usize, progress)?;
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
        append_aligned(aligner, spool, frames, progress)?;
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
            log(
                &mut file,
                json!({"event":"clock_sync","slot":slot,"received_qpc_100ns":received_qpc_100ns,"device_frame":device_frame,"packet_frames":packet_frames,"raw_qpc_100ns":raw_qpc_100ns,"estimate":estimate}),
            )?;
        }
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
            }
            s.message = if slots.iter().any(|&slot| !s.connected[slot]) {
                format!("録音継続・再接続待ち: {message}")
            } else {
                "録音中".into()
            };
        });
    }
    update_wave(&event, config.preview_secs, slots, status);
    aligner.ingest(event)
}

fn append_aligned(
    aligner: &mut Aligner,
    spool: &mut Spool,
    frames: usize,
    progress: &Progress,
) -> Result<()> {
    let start = spool.frames;
    let block = aligner.take(start, frames)?;
    spool.append(&block.tracks)?;
    let metadata = BlockMetadata {
        frames: frames as u64,
        valid: block.valid,
    };
    fs::write(
        spool.dir.join(format!("{start:020}_valid.json")),
        serde_json::to_vec(&metadata)?,
    )?;
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

fn update_wave(event: &CaptureEvent, preview_secs: f64, slots: &[usize], status: &SharedStatus) {
    let CaptureEvent::Packet {
        track,
        sample_rate,
        channels,
        samples,
        ..
    } = event
    else {
        return;
    };
    let Some(&slot) = slots.get(*track) else {
        return;
    };
    if *channels == 0 {
        return;
    }
    let stride = ((f64::from(*sample_rate) * preview_secs / 2_000.0).ceil() as usize).max(1);
    let values: Vec<_> = samples
        .chunks_exact(usize::from(*channels))
        .step_by(stride)
        .map(|frame| {
            frame
                .iter()
                .copied()
                .max_by(|a, b| a.abs().total_cmp(&b.abs()))
                .unwrap_or(0.0)
        })
        .collect();
    update(status, |status| {
        let wave = &mut status.waves[slot];
        wave.extend(values);
        if wave.len() > 2_000 {
            wave.drain(..wave.len() - 2_000);
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
        ensure!(
            metadata.frames == frames && metadata.valid.len() == channels.len(),
            "一時音声の確定情報が不正です"
        );
        let mut tracks = raw.read(next, next + frames)?;
        let mut states = vec![SpeechState::Unknown; channels.len()];
        let overloaded = (produced - next) as f64 > config.max_pending_secs * RATE as f64 / 2.0;
        let mut analysis_wait = overloaded;
        if !overloaded {
            for index in 0..channels.len() {
                if !metadata.valid[index] {
                    continue;
                }
                let mode = config.tracks[slots[index]].denoise.clone();
                match speech.process(index, RATE as u32, channels[index], &tracks[index], mode) {
                    Ok(Some(analysis)) => {
                        states[index] = analysis.state;
                        tracks[index] = analysis.samples;
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
                        // 一部だけ除去処理済みにしない。原音一式で当該区間を残す。
                        tracks = raw.read(next, next + frames)?;
                        break;
                    }
                }
            }
        }
        processed.append(&tracks)?;
        let state = aggregate_states(&states);
        log(
            &mut output.timeline,
            json!({"event":"speech_state","source_start":next,"frames":frames,"slots":&slots,"timestamp_coverage_valid":&metadata.valid,"states":&states,"aggregate":state}),
        )?;
        if metadata.valid.iter().any(|valid| !valid) || states.contains(&SpeechState::Unknown) {
            log(
                &mut output.timeline,
                json!({"event":"uncertain_audio","source_start":next,"frames":frames,"slots":&slots,"timestamp_coverage_valid":&metadata.valid,"states":&states}),
            )?;
        }
        let actions = output.editor.push(next, frames, state)?;
        output.apply(actions, &mut processed)?;
        raw.discard_before(output.committed_source)?;
        processed.discard_before(output.settled)?;
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
    raw.discard_before(released)?;
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
                        self.finalizer.submit(writer, at, discard)?;
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
    fn phase_reader_tracks_changes_and_stop_does_not_overwrite_terminal_state() {
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
