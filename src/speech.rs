//! speech.cpp 4.1 ABI と隔離プロセスの接続。VAD の欠落を無音として扱わない。
use crate::SpeechState;
use crate::denoise_plugins::{LoadedPlugin, PluginCatalog, discover_for_current_exe};
use anyhow::{Context, Result, bail, ensure};
use libloading::Library;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::{CStr, CString, c_char, c_int, c_void},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpeechSettings {
    pub dll_path: PathBuf,
    pub vad_model: PathBuf,
    pub haas_model: PathBuf,
    pub reuse_model: PathBuf,
    pub reuse_laughter_model: PathBuf,
    pub haas_enabled: bool,
    pub vad_threshold: f64,
    pub pad_before_secs: f64,
    pub pad_after_secs: f64,
    pub min_speech_secs: f64,
    pub chunk_secs: f64,
}

impl Default for SpeechSettings {
    fn default() -> Self {
        Self {
            dll_path: "runtime/speech/speech.dll".into(),
            vad_model: "runtime/models/silero-vad-309K-v6.2-F32.gguf".into(),
            haas_model: "runtime/models/Haas-616M-ja-v1.0-Q8_0.gguf".into(),
            reuse_model: "runtime/models/NVIDIA-NemotronLabs-RE-USE-10M-F32.gguf".into(),
            reuse_laughter_model: "runtime/models/NVIDIA-NemotronLabs-RE-USE-10M-laughter-F32.gguf"
                .into(),
            haas_enabled: false,
            vad_threshold: 0.5,
            pad_before_secs: 0.3,
            pad_after_secs: 0.5,
            min_speech_secs: 0.1,
            chunk_secs: 5.0,
        }
    }
}

impl SpeechSettings {
    /// 設定編集中にはモデルの存在を要求しない。実際の読み込みは起動ハンドシェイクで行う。
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (0.01..=0.99).contains(&self.vad_threshold),
            "発話しきい値は 0.01～0.99 で指定してください"
        );
        for (name, value) in [
            ("発話前の余裕", self.pad_before_secs),
            ("発話後の余裕", self.pad_after_secs),
            ("最小発話長", self.min_speech_secs),
        ] {
            ensure!(
                value.is_finite() && (0.0..=5.0).contains(&value),
                "{name}は 0～5 秒で指定してください"
            );
        }
        ensure!(
            self.chunk_secs.is_finite() && (0.05..=30.0).contains(&self.chunk_secs),
            "ノイズ除去の処理単位は 0.05～30 秒で指定してください"
        );
        ensure!(
            !self.dll_path.as_os_str().is_empty() && !self.vad_model.as_os_str().is_empty(),
            "音声ランタイムと VAD モデルのパスが必要です"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DenoiseMode {
    #[default]
    Off,
    Reuse,
    ReuseLaughter,
    Plugin(String),
}

impl DenoiseMode {
    pub fn plugin_id(&self) -> Option<&str> {
        match self {
            Self::Off => None,
            Self::Reuse => Some("reuse"),
            Self::ReuseLaughter => Some("reuse-laughter"),
            Self::Plugin(id) => Some(id),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(id) = self.plugin_id() {
            ensure!(
                !id.is_empty()
                    && id.len() <= 80
                    && id.bytes().all(|c| c.is_ascii_lowercase()
                        || c.is_ascii_digit()
                        || b"-_.".contains(&c)),
                "ノイズ除去プラグインIDが不正です"
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start_secs: f64,
    pub end_secs: f64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Analysis {
    pub state: SpeechState,
    pub samples: Vec<f32>,
    pub transcript: Option<String>,
    /// 当該バッチの先頭からの相対時刻。過去文脈の時刻は除外・クランプする。
    pub segments: Vec<TranscriptSegment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub window_states: Vec<SpeechState>,
}

#[derive(Serialize, Deserialize)]
enum Request {
    Init(SpeechSettings),
    Prepare(Vec<DenoiseMode>),
    ResetContext,
    Process {
        track: usize,
        sample_rate: u32,
        channels: u16,
        samples: Vec<f32>,
        denoise: DenoiseMode,
        #[serde(default)]
        window_states: bool,
    },
}
#[derive(Serialize, Deserialize)]
enum Response {
    Ready { version: String },
    Analyzed(Analysis),
    Error(String),
}

const MAX_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

fn read_message(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut message = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            ensure!(message.is_empty(), "音声プロセスの応答が途中で終了しました");
            return Ok(None);
        }
        let newline = available.iter().position(|&c| c == b'\n');
        let length = newline.map_or(available.len(), |n| n + 1);
        ensure!(
            message.len() + length <= MAX_MESSAGE_BYTES,
            "音声プロセスのメッセージが上限を超えました"
        );
        message.extend_from_slice(&available[..length]);
        reader.consume(length);
        if newline.is_some() {
            return Ok(Some(message));
        }
    }
}

pub struct SpeechClient {
    child: Child,
    input: Option<SyncSender<Vec<u8>>>,
    responses: Option<Receiver<Result<Response>>>,
    io_threads: Vec<JoinHandle<()>>,
    alive: bool,
    pub version: String,
}

impl SpeechClient {
    pub fn spawn(settings: &SpeechSettings) -> Result<Self> {
        Self::spawn_cancellable(settings, &AtomicBool::new(false))
    }

    /// Model initialization stays in the child. Cancellation interrupts the
    /// parent wait and reaps the child, including its stdio helper threads.
    pub fn spawn_cancellable(settings: &SpeechSettings, cancel: &AtomicBool) -> Result<Self> {
        settings.validate()?;
        ensure!(
            !cancel.load(Ordering::Acquire),
            "音声モデルの準備を中止しました"
        );
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("--speech-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        Self::spawn_command(settings, cancel, command)
    }

    fn spawn_command(
        settings: &SpeechSettings,
        cancel: &AtomicBool,
        mut command: Command,
    ) -> Result<Self> {
        let mut child = command
            .spawn()
            .context("音声解析プロセスを開始できません")?;
        let mut child_input = child
            .stdin
            .take()
            .context("音声プロセスの入力がありません")?;
        let output = child
            .stdout
            .take()
            .context("音声プロセスの出力がありません")?;
        let (sender, responses) = mpsc::sync_channel(2);
        let (input, writes) = mpsc::sync_channel::<Vec<u8>>(1);
        let write_errors = sender.clone();
        // pipe が詰まっても親の exchange() は応答期限で子を停止できる。
        let writer = std::thread::spawn(move || {
            for line in writes {
                if let Err(error) = child_input
                    .write_all(&line)
                    .and_then(|()| child_input.flush())
                {
                    let _ =
                        write_errors.send(Err(error).context("音声プロセスへ要求を送信できません"));
                    break;
                }
            }
        });
        let reader = std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let response = match read_message(&mut reader) {
                    Ok(Some(line)) => {
                        serde_json::from_slice(&line).context("音声プロセスの応答形式が不正です")
                    }
                    Ok(None) => {
                        let _ =
                            sender.send(Err(anyhow::anyhow!("音声プロセスが応答せず終了しました")));
                        break;
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                };
                if sender.send(response).is_err() {
                    break;
                }
            }
        });
        let mut client = Self {
            child,
            input: Some(input),
            responses: Some(responses),
            io_threads: vec![writer, reader],
            alive: true,
            version: String::new(),
        };
        match client.exchange_cancellable(
            &Request::Init(settings.clone()),
            Duration::from_secs(90),
            Some(cancel),
        )? {
            Response::Ready { version } => client.version = version,
            Response::Error(message) => bail!(message),
            _ => bail!("音声ランタイムの開始応答が不正です"),
        }
        Ok(client)
    }

    fn exchange(&mut self, request: &Request, timeout: Duration) -> Result<Response> {
        self.exchange_cancellable(request, timeout, None)
    }

    fn exchange_cancellable(
        &mut self,
        request: &Request,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
    ) -> Result<Response> {
        ensure!(
            self.alive,
            "音声解析プロセスは停止済みです。録音を再開して初期化してください"
        );
        let response = self.exchange_inner(request, timeout, cancel);
        if response.is_err() {
            self.alive = false;
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        response
    }

    fn exchange_inner(
        &mut self,
        request: &Request,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
    ) -> Result<Response> {
        ensure!(
            !cancel.is_some_and(|flag| flag.load(Ordering::Acquire)),
            "音声モデルの準備を中止しました"
        );
        let mut line = serde_json::to_vec(request)?;
        ensure!(
            line.len() < MAX_MESSAGE_BYTES,
            "音声プロセスへの要求が上限を超えました"
        );
        line.push(b'\n');
        self.input
            .as_ref()
            .context("音声プロセスの送信口が閉じています")?
            .try_send(line)
            .context("音声プロセスの送信待ちが上限を超えたか、送信が停止しました")?;
        let deadline = Instant::now() + timeout;
        loop {
            ensure!(
                !cancel.is_some_and(|flag| flag.load(Ordering::Acquire)),
                "音声モデルの準備を中止しました"
            );
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "音声解析が停止または時間切れになりました"
            );
            match self
                .responses
                .as_ref()
                .context("音声プロセスの受信口が閉じています")?
                .recv_timeout(remaining.min(Duration::from_millis(50)))
            {
                Ok(response) => return response,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => bail!("音声解析プロセスが終了しました"),
            }
        }
    }

    /// 録音開始前に、選択した任意モデルを実際にロードして検証する。
    pub fn prepare_denoise(&mut self, modes: &[DenoiseMode]) -> Result<()> {
        self.prepare_denoise_cancellable(modes, &AtomicBool::new(false))
    }

    pub fn prepare_denoise_cancellable(
        &mut self,
        modes: &[DenoiseMode],
        cancel: &AtomicBool,
    ) -> Result<()> {
        match self.exchange_cancellable(
            &Request::Prepare(modes.to_vec()),
            Duration::from_secs(90),
            Some(cancel),
        )? {
            Response::Ready { .. } => Ok(()),
            Response::Error(message) => bail!(message),
            _ => bail!("ノイズ除去モデルの準備応答が不正です"),
        }
    }

    pub fn process(
        &mut self,
        track: usize,
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
        denoise: DenoiseMode,
    ) -> Result<Analysis> {
        self.process_with_timeout(
            track,
            sample_rate,
            channels,
            samples,
            denoise,
            Duration::from_secs(60),
        )
    }

    /// Exercise only explicitly selected plugins before audio-device capture starts.
    pub fn check_denoise(&mut self, modes: &[DenoiseMode], timeout: Duration) -> Result<()> {
        self.check_denoise_cancellable(modes, timeout, &AtomicBool::new(false))
    }

    pub fn check_denoise_cancellable(
        &mut self,
        modes: &[DenoiseMode],
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut checked = std::collections::HashSet::new();
        for mode in modes {
            if let Some(id) = mode.plugin_id()
                && checked.insert(id.to_owned())
            {
                let response = self.exchange_cancellable(&Request::Process {
                    track:0, sample_rate:48_000, channels:1, samples:vec![0.0;72_000], denoise:mode.clone(), window_states:false,
                }, timeout, Some(cancel))
                    .with_context(|| format!("ノイズ除去プラグインの動作確認に失敗しました: {id}。処理負荷・配置を確認するか、別の方式を選択してください"))?;
                match response {
                    Response::Analyzed(analysis) => ensure!(
                        analysis.samples.len() == 72_000
                            && analysis.samples.iter().all(|sample| sample.is_finite()),
                        "プラグインの確認出力が不正です: {id}"
                    ),
                    Response::Error(message) => {
                        bail!("ノイズ除去プラグインの動作確認に失敗しました: {id}: {message}")
                    }
                    _ => bail!("ノイズ除去プラグインの確認応答が不正です: {id}"),
                }
            }
        }
        if !checked.is_empty() {
            match self.exchange_cancellable(
                &Request::ResetContext,
                Duration::from_secs(10),
                Some(cancel),
            )? {
                Response::Ready { .. } => {}
                Response::Error(message) => bail!(message),
                _ => bail!("ノイズ除去の動作確認後の応答が不正です"),
            }
        }
        Ok(())
    }

    pub fn process_with_timeout(
        &mut self,
        track: usize,
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
        denoise: DenoiseMode,
        timeout: Duration,
    ) -> Result<Analysis> {
        self.process_block(
            track,
            sample_rate,
            channels,
            samples,
            denoise,
            timeout,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn process_block(
        &mut self,
        track: usize,
        sample_rate: u32,
        channels: u16,
        samples: &[f32],
        denoise: DenoiseMode,
        timeout: Duration,
        window_states: bool,
    ) -> Result<Analysis> {
        ensure!(
            !timeout.is_zero(),
            "音声解析の応答期限は0秒より大きくしてください"
        );
        validate_audio(sample_rate, channels, samples)?;
        let response = self.exchange(
            &Request::Process {
                track,
                sample_rate,
                channels,
                samples: samples.to_vec(),
                denoise,
                window_states,
            },
            timeout,
        )?;
        match response {
            Response::Analyzed(analysis) => {
                ensure!(
                    analysis.samples.len() == samples.len()
                        && analysis.samples.iter().all(|v| v.is_finite())
                        && (!window_states
                            || analysis.window_states.len()
                                == (samples.len() / usize::from(channels))
                                    .div_ceil(crate::sync_audio::ANALYSIS_WINDOW_FRAMES)),
                    "音声処理後のサンプル数または値が不正です"
                );
                Ok(analysis)
            }
            Response::Error(message) => bail!(message),
            _ => bail!("音声解析の応答が不正です"),
        }
    }
}

impl Drop for SpeechClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.input.take();
        self.responses.take();
        for thread in self.io_threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// 本体の `--speech-worker` 経路からのみ呼ぶ。標準出力はプロトコル専用。
pub fn run_worker() -> Result<()> {
    let mut reader = BufReader::new(std::io::stdin().lock());
    let mut output = std::io::stdout().lock();
    let mut engine: Option<Engine> = None;
    while let Some(line) = read_message(&mut reader)? {
        let response = match serde_json::from_slice::<Request>(&line) {
            Ok(Request::Init(settings)) => match Engine::load(settings) {
                Ok(loaded) => {
                    let version = loaded.native.version();
                    engine = Some(loaded);
                    Response::Ready { version }
                }
                Err(error) => Response::Error(format!("{error:#}")),
            },
            Ok(Request::Prepare(modes)) => {
                match engine
                    .as_mut()
                    .context("音声ランタイムが初期化されていません")
                    .and_then(|engine| {
                        engine
                            .prepare_denoise(&modes)
                            .map(|()| engine.native.version())
                    }) {
                    Ok(version) => Response::Ready { version },
                    Err(error) => Response::Error(format!("{error:#}")),
                }
            }
            Ok(Request::ResetContext) => match engine.as_mut() {
                Some(engine) => {
                    engine.tracks.clear();
                    Response::Ready {
                        version: engine.native.version(),
                    }
                }
                None => Response::Error("音声ランタイムが初期化されていません".into()),
            },
            Ok(Request::Process {
                track,
                sample_rate,
                channels,
                samples,
                denoise,
                window_states,
            }) => {
                match engine
                    .as_mut()
                    .context("音声ランタイムが初期化されていません")
                    .and_then(|engine| {
                        engine.process(
                            track,
                            sample_rate,
                            channels,
                            &samples,
                            denoise,
                            window_states,
                        )
                    }) {
                    Ok(analysis) => Response::Analyzed(analysis),
                    Err(error) => Response::Error(format!("{error:#}")),
                }
            }
            Err(error) => Response::Error(format!("音声解析への要求形式が不正です: {error}")),
        };
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

fn validate_audio(rate: u32, channels: u16, samples: &[f32]) -> Result<()> {
    ensure!(
        (8000..=48000).contains(&rate),
        "解析音声のサンプルレートは 8～48 kHz が必要です"
    );
    ensure!(
        (1..=2).contains(&channels),
        "解析音声はモノラルまたはステレオが必要です"
    );
    ensure!(
        !samples.is_empty() && samples.len().is_multiple_of(channels as usize),
        "解析音声が空またはチャンネル数と不一致です"
    );
    ensure!(
        samples.len() <= rate as usize * channels as usize * 30,
        "解析区間は 30 秒以下にしてください"
    );
    ensure!(
        samples.iter().all(|sample| sample.is_finite()),
        "解析音声に非数または無限値があります"
    );
    Ok(())
}

type Ptr = *mut c_void;
type Free = unsafe extern "C" fn(Ptr);
type StatusFn = unsafe extern "C" fn(Ptr) -> c_int;
struct Native {
    model_load: unsafe extern "C" fn(*const c_char, Ptr, *mut Ptr) -> c_int,
    model_free: Free,
    params_new: unsafe extern "C" fn(*mut Ptr) -> c_int,
    params_free: Free,
    params_device: unsafe extern "C" fn(Ptr, *const c_char) -> c_int,
    params_threads: unsafe extern "C" fn(Ptr, c_int) -> c_int,
    request_new: unsafe extern "C" fn(Ptr, *mut Ptr) -> c_int,
    request_free: Free,
    set_audio: unsafe extern "C" fn(Ptr, *const f32, usize, c_int) -> c_int,
    set_float: unsafe extern "C" fn(Ptr, c_int, f64) -> c_int,
    set_int: unsafe extern "C" fn(Ptr, c_int, i64) -> c_int,
    set_bool: unsafe extern "C" fn(Ptr, c_int, c_int) -> c_int,
    detect: StatusFn,
    transcribe: StatusFn,
    result: unsafe extern "C" fn(Ptr) -> Ptr,
    segment_count: unsafe extern "C" fn(Ptr) -> usize,
    segment: unsafe extern "C" fn(Ptr, usize, *mut f64, *mut f64, *mut *const c_char) -> c_int,
    text: unsafe extern "C" fn(Ptr) -> *const c_char,
    last_error: unsafe extern "C" fn() -> *const c_char,
    version_fn: unsafe extern "C" fn() -> *const c_char,
    _library: Library,
}

struct Owned {
    ptr: Ptr,
    free: Free,
}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { (self.free)(self.ptr) };
    }
}

impl Native {
    fn load(path: &Path) -> Result<Self> {
        ensure!(
            path.is_file(),
            "speech.cpp DLL がありません: {}。scripts/Prepare-Speech.ps1 で準備してください",
            path.display()
        );
        // ABI を確認してから他の関数を呼ぶ。モデル・要求は DLL より先に解放する。
        unsafe {
            let library = Library::new(path)
                .with_context(|| format!("音声 DLL を読み込めません: {}", path.display()))?;
            let major =
                library.get::<unsafe extern "C" fn() -> c_int>(b"speech_api_version_major\0")?;
            let minor =
                library.get::<unsafe extern "C" fn() -> c_int>(b"speech_api_version_minor\0")?;
            ensure!(
                major() == 4 && minor() >= 1,
                "speech.cpp C API 4.1 以降（major 4）が必要です"
            );
            macro_rules! symbol {
                ($name:literal) => {
                    *library.get(concat!($name, "\0").as_bytes())?
                };
            }
            Ok(Self {
                model_load: symbol!("speech_model_load"),
                model_free: symbol!("speech_model_free"),
                params_new: symbol!("speech_load_params_new"),
                params_free: symbol!("speech_load_params_free"),
                params_device: symbol!("speech_load_params_set_device"),
                params_threads: symbol!("speech_load_params_set_threads"),
                request_new: symbol!("speech_request_new"),
                request_free: symbol!("speech_request_free"),
                set_audio: symbol!("speech_request_set_audio"),
                set_float: symbol!("speech_request_set_float"),
                set_int: symbol!("speech_request_set_int"),
                set_bool: symbol!("speech_request_set_bool"),
                detect: symbol!("speech_detect"),
                transcribe: symbol!("speech_transcribe"),
                result: symbol!("speech_request_result"),
                segment_count: symbol!("speech_result_segment_count"),
                segment: symbol!("speech_result_segment"),
                text: symbol!("speech_result_text"),
                last_error: symbol!("speech_last_error"),
                version_fn: symbol!("speech_version"),
                _library: library,
            })
        }
    }
    fn check(&self, status: c_int) -> Result<()> {
        if status != 0 {
            bail!(
                "speech.cpp エラー {status}: {}",
                c_string(unsafe { (self.last_error)() })
            );
        }
        Ok(())
    }
    fn version(&self) -> String {
        c_string(unsafe { (self.version_fn)() })
    }
    fn model(&self, path: &Path, cpu: bool) -> Result<Owned> {
        ensure!(path.is_file(), "音声モデルがありません: {}", path.display());
        let path = CString::new(
            path.to_str()
                .context("モデルパスは有効な Unicode が必要です")?,
        )?;
        let mut params = std::ptr::null_mut();
        self.check(unsafe { (self.params_new)(&mut params) })?;
        let params = Owned {
            ptr: params,
            free: self.params_free,
        };
        if cpu {
            self.check(unsafe { (self.params_device)(params.ptr, c"cpu".as_ptr()) })?;
            self.check(unsafe { (self.params_threads)(params.ptr, 1) })?;
        }
        let mut model = std::ptr::null_mut();
        self.check(unsafe { (self.model_load)(path.as_ptr(), params.ptr, &mut model) })?;
        ensure!(!model.is_null(), "モデルの取得結果が空です");
        Ok(Owned {
            ptr: model,
            free: self.model_free,
        })
    }
    fn request(&self, model: &Owned, samples: &[f32], rate: u32) -> Result<Owned> {
        let mut request = std::ptr::null_mut();
        self.check(unsafe { (self.request_new)(model.ptr, &mut request) })?;
        ensure!(!request.is_null(), "解析要求の取得結果が空です");
        let request = Owned {
            ptr: request,
            free: self.request_free,
        };
        self.check(unsafe {
            (self.set_audio)(request.ptr, samples.as_ptr(), samples.len(), rate as c_int)
        })?;
        Ok(request)
    }
    fn regions(&self, request: &Owned) -> Result<Vec<TranscriptSegment>> {
        let result = unsafe { (self.result)(request.ptr) };
        ensure!(!result.is_null(), "音声解析の結果がありません");
        let count = unsafe { (self.segment_count)(result) };
        ensure!(count <= 100_000, "音声解析の区間数が上限を超えました");
        (0..count)
            .map(|index| {
                let (mut start, mut end) = (0.0, 0.0);
                let mut text = std::ptr::null();
                self.check(unsafe {
                    (self.segment)(result, index, &mut start, &mut end, &mut text)
                })?;
                ensure!(
                    start.is_finite() && end.is_finite() && end >= start,
                    "音声解析の区間時刻が不正です"
                );
                Ok(TranscriptSegment {
                    start_secs: start,
                    end_secs: end,
                    text: c_string(text),
                })
            })
            .collect()
    }
}

fn c_string(pointer: *const c_char) -> String {
    if pointer.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    }
}

#[derive(Default)]
struct TrackContext {
    rate: u32,
    channels: u16,
    tail: Vec<Vec<f32>>,
}

struct Engine {
    // Rust はフィールド宣言順に drop する。DLL を最後まで保持する。
    vad: Owned,
    haas: Option<Owned>,
    plugin_catalog: PluginCatalog,
    plugins: HashMap<String, LoadedPlugin>,
    settings: SpeechSettings,
    tracks: HashMap<usize, TrackContext>,
    native: Native,
}

impl Engine {
    fn load(settings: SpeechSettings) -> Result<Self> {
        settings.validate()?;
        let native = Native::load(&settings.dll_path)?;
        let vad = native.model(&settings.vad_model, true)?;
        let haas = if settings.haas_enabled {
            Some(native.model(&settings.haas_model, false)?)
        } else {
            None
        };
        Ok(Self {
            vad,
            haas,
            plugin_catalog: discover_for_current_exe(),
            plugins: HashMap::new(),
            settings,
            tracks: HashMap::new(),
            native,
        })
    }

    fn prepare_denoise(&mut self, modes: &[DenoiseMode]) -> Result<()> {
        for mode in modes {
            mode.validate()?;
            if let Some(id) = mode.plugin_id()
                && !self.plugins.contains_key(id)
            {
                let plugin = if matches!(id, "reuse" | "reuse-laughter") {
                    let mut options = serde_json::Map::new();
                    options.insert(
                        "chunk_secs".into(),
                        serde_json::json!(self.settings.chunk_secs),
                    );
                    self.plugin_catalog.load_with_options(id, &options)?
                } else {
                    self.plugin_catalog.load(id)?
                };
                self.plugins.insert(id.to_owned(), plugin);
            }
        }
        Ok(())
    }

    fn detect(&self, samples: &[f32], rate: u32, min_secs: f64) -> Result<Vec<TranscriptSegment>> {
        let request = self.native.request(&self.vad, samples, rate)?;
        self.native.check(unsafe {
            (self.native.set_float)(request.ptr, 39, self.settings.vad_threshold)
        })?;
        self.native.check(unsafe {
            (self.native.set_int)(request.ptr, 40, (min_secs * 1000.0).round() as i64)
        })?;
        let pad = self
            .settings
            .pad_before_secs
            .max(self.settings.pad_after_secs);
        self.native.check(unsafe {
            (self.native.set_int)(request.ptr, 42, (pad * 1000.0).round() as i64)
        })?;
        self.native
            .check(unsafe { (self.native.detect)(request.ptr) })?;
        self.native.regions(&request)
    }

    fn process(
        &mut self,
        track: usize,
        rate: u32,
        channels: u16,
        input: &[f32],
        denoise: DenoiseMode,
        split_state: bool,
    ) -> Result<Analysis> {
        validate_audio(rate, channels, input)?;
        ensure!(track < 4, "解析トラック番号が範囲外です");
        let frames = input.len() / channels as usize;
        let window_frames = crate::sync_audio::ANALYSIS_WINDOW_FRAMES;
        let mut window_states = Vec::new();
        if split_state {
            ensure!(
                rate == 48_000 && frames <= 48_000,
                "区間判定は48kHzの1秒以内です"
            );
            window_states.extend((0..frames).step_by(window_frames).map(|start| {
                if frames - start >= window_frames {
                    SpeechState::Silence
                } else {
                    SpeechState::Unknown
                }
            }));
        }
        let mut context = self.tracks.remove(&track).unwrap_or_default();
        if context.rate != rate || context.channels != channels {
            context = TrackContext {
                rate,
                channels,
                tail: vec![Vec::new(); channels as usize],
            };
        }
        let mut windows = Vec::with_capacity(channels as usize);
        // 切断・停止時の短い末尾や初回窓は、VAD の文脈不足を非発話と誤認しない。
        let mut state = if frames as f64 / (rate as f64) < 0.25 {
            SpeechState::Unknown
        } else {
            SpeechState::Silence
        };
        let mut transcript = None;
        let mut segments = Vec::new();
        for channel in 0..channels as usize {
            let mut window = context.tail[channel].clone();
            let offset = window.len();
            window.extend(
                input
                    .iter()
                    .skip(channel)
                    .step_by(channels as usize)
                    .copied(),
            );
            let offset_secs = offset as f64 / rate as f64;
            let overlap = |region: &TranscriptSegment| {
                region.end_secs > offset_secs
                    && region.start_secs < window.len() as f64 / rate as f64
            };
            let regions = self.detect(&window, rate, self.settings.min_speech_secs)?;
            let confirmed = regions.iter().any(overlap);
            let candidates = if !confirmed || split_state {
                self.detect(&window, rate, 0.0)?
            } else {
                Vec::new()
            };
            let candidate = confirmed || candidates.iter().any(overlap);
            for (index, state) in window_states.iter_mut().enumerate() {
                let start = offset_secs + (index * window_frames) as f64 / f64::from(rate);
                let end = offset_secs
                    + ((index + 1) * window_frames).min(frames) as f64 / f64::from(rate);
                let overlaps_window =
                    |region: &TranscriptSegment| region.end_secs > start && region.start_secs < end;
                if regions.iter().any(overlaps_window) {
                    *state = SpeechState::Speech;
                } else if *state == SpeechState::Silence && candidates.iter().any(overlaps_window) {
                    *state = SpeechState::Unknown;
                }
            }
            if confirmed {
                state = SpeechState::Speech;
            } else if state != SpeechState::Speech && candidate {
                state = SpeechState::Unknown;
            }
            if let Some(model) = self.haas.as_ref().filter(|_| candidate) {
                let request = self.native.request(model, &window, rate)?;
                self.native
                    .check(unsafe { (self.native.set_bool)(request.ptr, 8, 1) })?;
                self.native
                    .check(unsafe { (self.native.transcribe)(request.ptr) })?;
                let result = unsafe { (self.native.result)(request.ptr) };
                ensure!(!result.is_null(), "STT の結果がありません");
                let text = c_string(unsafe { (self.native.text)(result) });
                if !text.trim().is_empty() {
                    state = SpeechState::Speech;
                    // Haas が発話を補足した場合は保守的にこの入力全体を残す。
                    window_states.fill(SpeechState::Speech);
                    transcript = Some(text);
                    for region in self.native.regions(&request)?.into_iter().filter(overlap) {
                        segments.push(TranscriptSegment {
                            start_secs: (region.start_secs - offset_secs).max(0.0),
                            end_secs: (region.end_secs - offset_secs)
                                .min(frames as f64 / rate as f64),
                            text: region.text,
                        });
                    }
                }
            }
            windows.push((window, offset));
        }
        let mut output = input.to_vec();
        if let Some(id) = denoise.plugin_id() {
            if !self.plugins.contains_key(id) {
                self.prepare_denoise(std::slice::from_ref(&denoise))?;
            }
            let plugin = self
                .plugins
                .get_mut(id)
                .context("ノイズ除去プラグインが準備されていません")?;
            for (channel, (window, offset)) in windows.iter().enumerate() {
                let enhanced = plugin.process(window, rate)?;
                ensure!(
                    enhanced.len() == window.len() && enhanced.iter().all(|v| v.is_finite()),
                    "ノイズ除去後のサンプル数・値が不正です"
                );
                for frame in 0..frames {
                    output[frame * channels as usize + channel] = enhanced[offset + frame];
                }
            }
        }
        for (channel, (window, _)) in windows.into_iter().enumerate() {
            let keep = (rate as f64
                * self
                    .settings
                    .pad_before_secs
                    .max(self.settings.pad_after_secs)
                    .max(0.5))
            .round() as usize;
            context.tail[channel] = window[window.len().saturating_sub(keep)..].to_vec();
        }
        self.tracks.insert(track, context);
        Ok(Analysis {
            state,
            samples: output,
            transcript,
            segments,
            window_states,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    fn delayed_test_worker(ready: bool) -> Command {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new("pwsh");
        let script = if ready {
            r#"Write-Output '{"Ready":{"version":"test"}}'; Start-Sleep -Seconds 2"#
        } else {
            "Start-Sleep -Seconds 2"
        };
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(0x08000000);
        command
    }

    #[cfg(windows)]
    #[test]
    fn initialization_cancel_reaps_a_child_that_never_answers() {
        use std::sync::Arc;
        let cancel = Arc::new(AtomicBool::new(false));
        let thread_cancel = cancel.clone();
        let signal = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            thread_cancel.store(true, Ordering::Release);
        });
        let start = Instant::now();
        let result = SpeechClient::spawn_command(
            &SpeechSettings::default(),
            &cancel,
            delayed_test_worker(false),
        );
        let error = match result {
            Ok(_) => panic!("worker unexpectedly initialized"),
            Err(error) => error,
        };
        signal.join().unwrap();
        assert!(error.to_string().contains("中止"), "{error:#}");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "cancel must not wait for the 2-second child"
        );
    }

    #[cfg(windows)]
    #[test]
    fn process_deadline_covers_blocked_pipe_and_drop_joins_io_threads() {
        let mut client = SpeechClient::spawn_command(
            &SpeechSettings::default(),
            &AtomicBool::new(false),
            delayed_test_worker(true),
        )
        .unwrap();
        let samples = vec![0.25; 48_000 * 2];
        let start = Instant::now();
        let result = client.process_with_timeout(
            0,
            48_000,
            2,
            &samples,
            DenoiseMode::Off,
            Duration::from_millis(30),
        );
        assert!(result.is_err());
        assert!(
            client.child.try_wait().unwrap().is_some(),
            "timed-out child must be reaped"
        );
        assert!(!client.alive);
        drop(client);
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "I/O shutdown must not wait for the 2-second child"
        );
    }

    #[cfg(windows)]
    #[test]
    fn plugin_preflight_honors_deadline_and_cancellation() {
        for cancelled in [false, true] {
            let mut client = SpeechClient::spawn_command(
                &SpeechSettings::default(),
                &AtomicBool::new(false),
                delayed_test_worker(true),
            )
            .unwrap();
            let start = Instant::now();
            let result = client.check_denoise_cancellable(
                &[DenoiseMode::Plugin("fixture".into())],
                Duration::from_millis(30),
                &AtomicBool::new(cancelled),
            );
            assert!(result.is_err());
            assert!(!client.alive);
            assert!(client.child.try_wait().unwrap().is_some());
            drop(client);
            assert!(start.elapsed() < Duration::from_secs(1));
        }
    }

    #[cfg(windows)]
    #[test]
    fn off_preflight_does_not_contact_the_worker() {
        let mut client = SpeechClient::spawn_command(
            &SpeechSettings::default(),
            &AtomicBool::new(false),
            delayed_test_worker(true),
        )
        .unwrap();
        client
            .check_denoise(&[DenoiseMode::Off], Duration::from_millis(1))
            .unwrap();
        assert!(client.alive);
        assert!(client.child.try_wait().unwrap().is_none());
    }

    #[test]
    fn invalid_audio_never_becomes_silence() {
        assert!(validate_audio(48_000, 1, &[]).is_err());
        assert!(validate_audio(48_000, 2, &[0.0]).is_err());
        assert!(validate_audio(48_000, 1, &[f32::NAN]).is_err());
        assert!(validate_audio(192_000, 1, &[0.0]).is_err());
        assert!(validate_audio(48_000, 1, &[0.0; 480]).is_ok());
    }
    #[test]
    fn malformed_or_truncated_protocol_is_rejected() {
        assert!(serde_json::from_slice::<Request>(b"bad").is_err());
        assert!(read_message(&mut std::io::Cursor::new(b"{\"unfinished\":")).is_err());
        assert!(
            read_message(&mut std::io::Cursor::new(b""))
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn protocol_rejects_unterminated_oversized_messages() {
        let endless = std::io::Read::take(std::io::repeat(b'x'), MAX_MESSAGE_BYTES as u64 + 1);
        assert!(read_message(&mut BufReader::new(endless)).is_err());
    }
    #[test]
    fn settings_validation_does_not_require_download() {
        assert!(SpeechSettings::default().validate().is_ok());
        let settings = SpeechSettings {
            vad_threshold: f64::NAN,
            ..Default::default()
        };
        assert!(settings.validate().is_err());
        let settings = SpeechSettings {
            pad_after_secs: -0.1,
            ..Default::default()
        };
        assert!(settings.validate().is_err());
    }
    #[test]
    fn missing_dll_is_a_failure_not_a_silence_result() {
        assert!(Native::load(Path::new("nonexistent-speech.dll")).is_err());
    }
    #[test]
    fn incompatible_dll_is_rejected_before_model_symbols_are_used() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("wrong_api.rs");
        let dll = temp.path().join(if cfg!(windows) {
            "wrong_api.dll"
        } else {
            "wrong_api.so"
        });
        std::fs::write(&source, "#[unsafe(no_mangle)] pub extern \"C\" fn speech_api_version_major() -> i32 { 3 }\n#[unsafe(no_mangle)] pub extern \"C\" fn speech_api_version_minor() -> i32 { 1 }\n").unwrap();
        let result = Command::new("rustc")
            .args(["--edition", "2024", "--crate-type", "cdylib"])
            .arg(&source)
            .arg("-o")
            .arg(&dll)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let error = Native::load(&dll)
            .err()
            .expect("非互換 DLL を受理してはいけません");
        assert!(
            error.to_string().contains("C API 4.1"),
            "関数探索より先に ABI を拒否する必要があります: {error:#}"
        );
    }
    fn reference_audio() -> (u32, Vec<f32>) {
        let wav = std::fs::read("runtime/reference/jfk.wav").unwrap();
        assert_eq!(&wav[..4], b"RIFF");
        let mut position = 12;
        let mut audio = Vec::new();
        let mut rate = 0;
        while position + 8 <= wav.len() {
            let length =
                u32::from_le_bytes(wav[position + 4..position + 8].try_into().unwrap()) as usize;
            let chunk = &wav[position + 8..position + 8 + length];
            match &wav[position..position + 4] {
                b"fmt " => {
                    assert_eq!(u16::from_le_bytes(chunk[0..2].try_into().unwrap()), 1);
                    assert_eq!(u16::from_le_bytes(chunk[2..4].try_into().unwrap()), 1);
                    assert_eq!(u16::from_le_bytes(chunk[14..16].try_into().unwrap()), 16);
                    rate = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
                }
                b"data" => {
                    audio = chunk
                        .chunks_exact(2)
                        .map(|sample| {
                            i16::from_le_bytes(sample.try_into().unwrap()) as f32 / 32768.0
                        })
                        .collect()
                }
                _ => {}
            }
            position += 8 + length + length % 2;
        }
        (rate, audio)
    }
    #[test]
    #[ignore = "固定 DLL・モデルと runtime/reference/jfk.wav（whisper.cpp 公開試験音声）が必要"]
    fn native_vad_detects_public_speech_sample() {
        let (rate, audio) = reference_audio();
        let mut engine = Engine::load(SpeechSettings::default()).unwrap();
        let mut speech_batches = 0;
        let batches = audio.chunks(rate as usize).count();
        for batch in audio.chunks(rate as usize) {
            let analysis = engine
                .process(0, rate, 1, batch, DenoiseMode::Off, false)
                .unwrap();
            assert_eq!(analysis.samples.len(), batch.len());
            if analysis.state == SpeechState::Speech {
                speech_batches += 1;
            }
        }
        assert!(
            speech_batches > batches / 2,
            "発話バッチが少なすぎます: {speech_batches}/{batches}"
        );
    }
    #[test]
    #[ignore = "固定 DLL・モデルと runtime/reference/jfk.wav が必要"]
    fn native_vad_short_speech_is_not_confirmed_as_silence() {
        let (rate, audio) = reference_audio();
        for milliseconds in [1, 10, 20, 31, 32, 50, 100] {
            let mut engine = Engine::load(SpeechSettings::default()).unwrap();
            let start = rate as usize * 2;
            let end = start + rate as usize * milliseconds / 1000;
            let analysis = engine
                .process(0, rate, 1, &audio[start..end], DenoiseMode::Off, false)
                .unwrap();
            assert_ne!(
                analysis.state,
                SpeechState::Silence,
                "{milliseconds}ms を非発話と確定してはいけません"
            );
        }
    }
    #[test]
    #[ignore = "scripts/Prepare-Speech.ps1 による固定 DLL・モデルの導入が必要"]
    fn native_vad_silence_stereo_and_bounded_context() {
        let mut engine = Engine::load(SpeechSettings::default()).unwrap();
        assert_eq!(engine.native.version(), "0.10.1");
        let input = vec![0.0; 48_000 * 2];
        for _ in 0..10 {
            let analysis = engine
                .process(0, 48_000, 2, &input, DenoiseMode::Off, true)
                .unwrap();
            assert_eq!(analysis.state, SpeechState::Silence);
            assert_eq!(analysis.window_states, vec![SpeechState::Silence; 4]);
            assert_eq!(analysis.samples, input);
        }
        assert!(
            engine.tracks[&0]
                .tail
                .iter()
                .all(|tail| tail.len() <= 24_000)
        );
    }
    #[test]
    #[ignore = "Prepare-ReusePlugin.ps1 でrelease/v0.1.0へ配置したpluginとruntime/reference/jfk.wavが必要"]
    fn native_reuse_preserves_stereo_frames_across_batches() {
        let (rate, audio) = reference_audio();
        let mut engine = Engine::load(SpeechSettings::default()).unwrap();
        engine.plugin_catalog = crate::denoise_plugins::discover(Path::new("release/v0.1.0"));
        engine.prepare_denoise(&[DenoiseMode::Reuse]).unwrap();
        let mut speech = false;
        let mut nonzero = false;
        for mono in audio.chunks(rate as usize).take(2) {
            // 逆相の左右を混ぜると消えるため、左右別 VAD の回帰確認も兼ねる。
            let stereo: Vec<f32> = mono.iter().flat_map(|sample| [*sample, -*sample]).collect();
            let result = engine
                .process(0, rate, 2, &stereo, DenoiseMode::Reuse, false)
                .unwrap();
            assert_eq!(result.samples.len(), stereo.len());
            assert!(result.samples.iter().all(|sample| sample.is_finite()));
            speech |= result.state == SpeechState::Speech;
            nonzero |= result.samples.iter().any(|sample| sample.abs() > 0.001);
        }
        assert!(speech && nonzero);
    }
    #[test]
    #[ignore = "Prepare-Speech.ps1 -IncludeHaas と runtime/reference/jfk.wav が必要"]
    fn native_haas_optional_model_and_timestamps() {
        let (rate, audio) = reference_audio();
        let started = std::time::Instant::now();
        let settings = SpeechSettings {
            haas_enabled: true,
            ..Default::default()
        };
        let mut engine = Engine::load(settings).unwrap();
        let input = &audio[rate as usize..rate as usize * 4];
        let result = engine
            .process(0, rate, 1, input, DenoiseMode::Off, false)
            .unwrap();
        assert_eq!(result.samples, input);
        assert_eq!(result.state, SpeechState::Speech);
        for segment in &result.segments {
            assert!(segment.start_secs.is_finite() && segment.end_secs.is_finite());
            assert!(segment.start_secs >= 0.0 && segment.end_secs <= 3.0);
            assert!(segment.end_secs >= segment.start_secs);
        }
        if result
            .transcript
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty())
        {
            assert!(
                !result.segments.is_empty(),
                "非空の STT 結果には時刻付き区間が必要です"
            );
        }
        assert!(
            engine
                .process(0, rate, 1, &[f32::NAN], DenoiseMode::Off, false)
                .is_err()
        );
        eprintln!(
            "Haas API経路: elapsed={:?}, text_chars={}, segments={}",
            started.elapsed(),
            result.transcript.as_deref().unwrap_or("").chars().count(),
            result.segments.len()
        );
        // 英語試料なので、日本語モデルによる文字内容の正確さは評価しない。
    }
}
