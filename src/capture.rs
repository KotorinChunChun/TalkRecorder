//! WASAPI shared-mode capture. Capture threads never wait for the consumer.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{SyncSender, TrySendError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    Microphone,
    Speaker,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug)]
pub enum CaptureEvent {
    Ready {
        track: usize,
        sample_rate: u32,
        channels: u16,
    },
    Packet {
        track: usize,
        sample_rate: u32,
        channels: u16,
        device_frame: u64,
        qpc_100ns: u64,
        samples: Vec<f32>,
        discontinuity: bool,
        timestamp_valid: bool,
    },
    Timing {
        track: usize,
        received_qpc_100ns: u64,
        device_frame: u64,
        packet_frames: u32,
        raw_qpc_100ns: u64,
        estimate: crate::clock_sync::ClockEstimate,
    },
    Error {
        track: usize,
        message: String,
    },
    /// A recoverable endpoint failure affects this track only. Missing samples
    /// stay Unknown on the shared timeline while the same endpoint is retried.
    Connection {
        track: usize,
        connected: bool,
        message: String,
    },
}

/// Only active endpoints are returned. This does not start an audio stream.
pub fn enumerate_devices() -> Result<Vec<DeviceInfo>> {
    platform::enumerate_devices()
}

/// A common monotonic clock, in the same units as WASAPI's packet timestamps.
pub fn qpc_100ns() -> u64 {
    platform::qpc_100ns()
}

pub fn start_capture(
    track: usize,
    device: DeviceInfo,
    sender: SyncSender<CaptureEvent>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<Result<()>> {
    thread::spawn(move || {
        let result = platform::capture(track, &device, &sender, &stop);
        if let Err(error) = &result {
            // An error event cannot be inserted into an already full queue. The
            // shared stop flag and the thread's returned error remain observable.
            stop.store(true, Ordering::Release);
            let _ = sender.try_send(CaptureEvent::Error {
                track,
                message: format!("{}: {error:#}", device.name),
            });
        }
        result
    })
}

/// Retains the endpoint ID and retries disconnects without stopping other
/// tracks. The first successful channel count remains fixed for this session;
/// if Windows exposes another channel layout, wait for the original layout.
/// Queue overflow and receiver shutdown are fatal and stop the shared session.
pub fn start_capture_resilient(
    track: usize,
    device: DeviceInfo,
    sender: SyncSender<CaptureEvent>,
    stop: Arc<AtomicBool>,
    reconnect_interval: Duration,
) -> JoinHandle<Result<()>> {
    thread::spawn(move || {
        let result = retry_capture(track, &device, &sender, &stop, reconnect_interval, |sink| {
            platform::capture_with_sink(track, &device, &stop, sink)
        });
        if let Err(error) = &result {
            stop.store(true, Ordering::Release);
            let _ = sender.try_send(CaptureEvent::Error {
                track,
                message: format!("{}: {error:#}", device.name),
            });
        }
        result
    })
}

#[derive(Debug)]
struct DeliveryFailure(&'static str);

impl std::fmt::Display for DeliveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for DeliveryFailure {}

fn send_event(sender: &SyncSender<CaptureEvent>, event: CaptureEvent) -> Result<()> {
    match sender.try_send(event) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => {
            Err(DeliveryFailure("録音キューの上限に達したため録音を停止します").into())
        }
        Err(TrySendError::Disconnected(_)) => {
            Err(DeliveryFailure("録音データの受信側が終了しました").into())
        }
    }
}

struct ReconnectState {
    expected_channels: Option<u16>,
    first_packet: bool,
    previous_trusted_qpc: Option<u64>,
}

impl ReconnectState {
    fn new(channels: u16) -> Self {
        Self {
            expected_channels: (channels > 0).then_some(channels),
            first_packet: true,
            previous_trusted_qpc: None,
        }
    }

    fn begin_attempt(&mut self) {
        self.first_packet = true;
    }

    fn forward(&mut self, event: CaptureEvent, sender: &SyncSender<CaptureEvent>) -> Result<()> {
        match event {
            CaptureEvent::Ready {
                track,
                sample_rate,
                channels,
            } => {
                let expected = self.expected_channels.get_or_insert(channels);
                if channels != *expected {
                    bail!(
                        "チャンネル数が変更されました（必要 {}、現在 {}）。元の形式への復帰を待機します",
                        expected,
                        channels
                    );
                }
                send_event(
                    sender,
                    CaptureEvent::Connection {
                        track,
                        connected: true,
                        message: "接続しました".into(),
                    },
                )?;
                send_event(
                    sender,
                    CaptureEvent::Ready {
                        track,
                        sample_rate,
                        channels,
                    },
                )
            }
            CaptureEvent::Packet {
                track,
                sample_rate,
                channels,
                device_frame,
                qpc_100ns,
                samples,
                discontinuity,
                timestamp_valid,
            } => {
                if self.expected_channels != Some(channels) {
                    bail!("再接続後のチャンネル数が確定済みの形式と一致しません");
                }
                let first = std::mem::replace(&mut self.first_packet, false);
                // Device frame positions restart on reconnect, but QPC is a
                // shared clock. Keep its trusted anchor across attempts.
                let timestamp_valid = trusted_packet_timestamp(
                    qpc_100ns,
                    timestamp_valid,
                    &mut self.previous_trusted_qpc,
                );
                send_event(
                    sender,
                    CaptureEvent::Packet {
                        track,
                        sample_rate,
                        channels,
                        device_frame,
                        qpc_100ns,
                        samples,
                        discontinuity: discontinuity || first,
                        timestamp_valid,
                    },
                )
            }
            event => send_event(sender, event),
        }
    }
}

fn retry_capture(
    track: usize,
    device: &DeviceInfo,
    sender: &SyncSender<CaptureEvent>,
    stop: &AtomicBool,
    reconnect_interval: Duration,
    mut attempt: impl FnMut(&mut dyn FnMut(CaptureEvent) -> Result<()>) -> Result<()>,
) -> Result<()> {
    if reconnect_interval.is_zero() || reconnect_interval > Duration::from_secs(3600) {
        bail!("再接続間隔は0秒より大きく3600秒以内にしてください");
    }
    let mut state = ReconnectState::new(device.channels);
    while !stop.load(Ordering::Acquire) {
        state.begin_attempt();
        let result = attempt(&mut |event| state.forward(event, sender));
        if let Err(error) = &result
            && error.downcast_ref::<DeliveryFailure>().is_some()
        {
            return result;
        }
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let message = result.err().map_or_else(
            || "入力が終了したため再接続します".into(),
            |error| format!("再接続待ち: {error:#}"),
        );
        send_event(
            sender,
            CaptureEvent::Connection {
                track,
                connected: false,
                message,
            },
        )?;
        let deadline = Instant::now() + reconnect_interval;
        while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(20)),
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum SampleEncoding {
    Pcm(u16),
    Float32,
}

/// Decodes container samples. WAVEFORMATEXTENSIBLE PCM valid bits are left
/// aligned, so normalization by container width also handles e.g. 24-in-32 PCM.
fn decode_samples(data: &[u8], encoding: SampleEncoding) -> Result<Vec<f32>> {
    let width = match encoding {
        SampleEncoding::Pcm(8) => 1,
        SampleEncoding::Pcm(16) => 2,
        SampleEncoding::Pcm(24) => 3,
        SampleEncoding::Pcm(32) | SampleEncoding::Float32 => 4,
        SampleEncoding::Pcm(bits) => bail!("未対応の PCM サンプル幅です: {bits} bit"),
    };
    if !data.len().is_multiple_of(width) {
        bail!("PCM バッファの長さがサンプル幅に一致しません");
    }
    data.chunks_exact(width)
        .map(|sample| {
            let value = match encoding {
                SampleEncoding::Pcm(8) => (sample[0] as f32 - 128.0) / 128.0,
                SampleEncoding::Pcm(16) => {
                    i16::from_le_bytes([sample[0], sample[1]]) as f32 / 32768.0
                }
                SampleEncoding::Pcm(24) => {
                    // Sign extension through an arithmetic right shift.
                    let value = i32::from_le_bytes([0, sample[0], sample[1], sample[2]]) >> 8;
                    value as f32 / 8_388_608.0
                }
                SampleEncoding::Pcm(32) => {
                    i32::from_le_bytes(sample.try_into().unwrap()) as f32 / 2_147_483_648.0
                }
                SampleEncoding::Float32 => f32::from_le_bytes(sample.try_into().unwrap()),
                SampleEncoding::Pcm(_) => unreachable!(),
            };
            if !value.is_finite() {
                bail!("取得した音声に非有限サンプルが含まれています");
            }
            // Floating-point WASAPI samples may exceed ±1. Do not clip them here.
            Ok(value)
        })
        .collect()
}

fn packet_samples(
    data: Option<&[u8]>,
    sample_count: usize,
    encoding: SampleEncoding,
) -> Result<Vec<f32>> {
    match data {
        None => Ok(vec![0.0; sample_count]),
        Some(data) => {
            let samples = decode_samples(data, encoding)?;
            if samples.len() != sample_count {
                bail!("取得した音声のサンプル数が一致しません");
            }
            Ok(samples)
        }
    }
}

fn ticks_to_100ns(ticks: u64, frequency: u64) -> u64 {
    if frequency == 0 {
        return 0;
    }
    ((u128::from(ticks) * 10_000_000) / u128::from(frequency)).min(u128::from(u64::MAX)) as u64
}

/// Some endpoint drivers return zero or regressing QPC even without the native
/// TIMESTAMP_ERROR flag. Preserve the last trusted anchor across such packets
/// and across discontinuities; an unknown clock must never be labelled valid.
fn trusted_packet_timestamp(
    qpc_100ns: u64,
    native_valid: bool,
    previous_trusted: &mut Option<u64>,
) -> bool {
    if !native_valid
        || qpc_100ns == 0
        || previous_trusted.is_some_and(|previous| qpc_100ns < previous)
    {
        return false;
    }
    *previous_trusted = Some(qpc_100ns);
    true
}

#[cfg(test)]
fn trusted_packet_timestamp_at(
    packet_qpc_100ns: u64,
    native_valid: bool,
    received_qpc_100ns: u64,
    previous_trusted: &mut Option<u64>,
) -> bool {
    // The shared-mode endpoint buffer is 100 ms and polling is normally 5 ms.
    // Allow ten seconds for scheduling stalls, while rejecting drivers that
    // advertise another epoch (observed on Remote Audio) as a valid QPC. Check
    // this before updating the monotonic anchor: a far-future value must not
    // make all subsequent normal timestamps appear to regress.
    const MAX_TIMESTAMP_SKEW_100NS: u64 = 10 * 10_000_000;
    if received_qpc_100ns == 0
        || packet_qpc_100ns.abs_diff(received_qpc_100ns) > MAX_TIMESTAMP_SKEW_100NS
    {
        return false;
    }
    trusted_packet_timestamp(packet_qpc_100ns, native_valid, previous_trusted)
}

#[cfg(windows)]
mod platform {
    use super::*;
    use anyhow::{Context, ensure};
    use std::ffi::c_void;
    use std::sync::OnceLock;
    use std::time::Duration;
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::Media::Audio::{
        AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
        AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient,
        IMMDevice, IMMDeviceEnumerator, IMMEndpoint, MMDeviceEnumerator, WAVEFORMATEX,
        WAVEFORMATEXTENSIBLE, eCapture, eRender,
    };
    use windows::Win32::Media::KernelStreaming::{
        KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE,
    };
    use windows::Win32::Media::Multimedia::{
        KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT,
    };
    use windows::Win32::System::Com::StructuredStorage::{
        PROPVARIANT, PropVariantClear, PropVariantToStringAlloc,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize, STGM_READ,
    };
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    use windows::core::{Interface, PCWSTR, PWSTR};

    struct ComGuard(bool);

    impl ComGuard {
        fn new() -> Result<Self> {
            // Enumeration can also be called on a UI thread initialized as STA.
            let status = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if status == RPC_E_CHANGED_MODE {
                return Ok(Self(false));
            }
            status.ok().context("COM の初期化に失敗しました")?;
            Ok(Self(true))
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            if self.0 {
                unsafe { CoUninitialize() };
            }
        }
    }

    struct TaskMem<T>(*mut T);

    impl<T> Drop for TaskMem<T> {
        fn drop(&mut self) {
            unsafe { CoTaskMemFree(Some(self.0.cast::<c_void>())) };
        }
    }

    struct PropertyValue(PROPVARIANT);

    impl Drop for PropertyValue {
        fn drop(&mut self) {
            let _ = unsafe { PropVariantClear(&mut self.0) };
        }
    }

    struct RunningClient(IAudioClient);

    impl Drop for RunningClient {
        fn drop(&mut self) {
            let _ = unsafe { self.0.Stop() };
        }
    }

    #[derive(Clone, Copy)]
    struct Format {
        sample_rate: u32,
        channels: u16,
        block_align: u16,
        encoding: SampleEncoding,
    }

    unsafe fn describe_format(format: *const WAVEFORMATEX) -> Result<Format> {
        ensure!(!format.is_null(), "音声形式が取得できませんでした");
        // Windows returns at least WAVEFORMATEX; extensible data is read only
        // after checking both the tag and the extension length.
        let base = unsafe { format.read_unaligned() };
        let bits = base.wBitsPerSample;
        let tag = base.wFormatTag;
        let encoding = if tag == WAVE_FORMAT_EXTENSIBLE as u16 {
            let required_extension =
                std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>();
            ensure!(
                usize::from(base.cbSize) >= required_extension,
                "拡張音声形式が短すぎます"
            );
            let extended = unsafe { format.cast::<WAVEFORMATEXTENSIBLE>().read_unaligned() };
            let subtype = extended.SubFormat;
            if subtype == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT && bits == 32 {
                SampleEncoding::Float32
            } else if subtype == KSDATAFORMAT_SUBTYPE_PCM {
                SampleEncoding::Pcm(bits)
            } else {
                bail!("未対応の拡張音声形式です: {subtype:?}");
            }
        } else if tag == WAVE_FORMAT_IEEE_FLOAT as u16 && bits == 32 {
            SampleEncoding::Float32
        } else if tag == 1 {
            SampleEncoding::Pcm(bits)
        } else {
            bail!("未対応の音声形式です: tag={tag}, bits={bits}");
        };
        ensure!(
            matches!(bits, 8 | 16 | 24 | 32),
            "未対応のサンプル幅です: {bits} bit"
        );
        let channels = base.nChannels;
        let sample_rate = base.nSamplesPerSec;
        let block_align = base.nBlockAlign;
        ensure!((1..=32).contains(&channels), "チャンネル数が範囲外です");
        ensure!(
            (8_000..=384_000).contains(&sample_rate),
            "サンプルレートが範囲外です"
        );
        ensure!(
            u32::from(block_align) == u32::from(channels) * u32::from(bits / 8),
            "音声フレームの幅がサンプル形式に一致しません"
        );
        Ok(Format {
            sample_rate,
            channels,
            block_align,
            encoding,
        })
    }

    fn endpoint_id(device: &IMMDevice) -> Result<String> {
        let pointer = TaskMem(unsafe { device.GetId()? }.0);
        unsafe { PWSTR(pointer.0).to_string() }.context("デバイス ID が無効です")
    }

    fn endpoint_name(device: &IMMDevice) -> Result<String> {
        let store = unsafe { device.OpenPropertyStore(STGM_READ)? };
        let value = PropertyValue(unsafe { store.GetValue(&PKEY_Device_FriendlyName)? });
        let pointer = TaskMem(unsafe { PropVariantToStringAlloc(&value.0)? }.0);
        unsafe { PWSTR(pointer.0).to_string() }.context("デバイス名が無効です")
    }

    pub(super) fn enumerate_devices() -> Result<Vec<DeviceInfo>> {
        let _com = ComGuard::new()?;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let mut devices = Vec::new();
        for (kind, flow) in [
            (DeviceKind::Microphone, eCapture),
            (DeviceKind::Speaker, eRender),
        ] {
            let endpoints = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)? };
            for index in 0..unsafe { endpoints.GetCount()? } {
                let endpoint = unsafe { endpoints.Item(index)? };
                let id = endpoint_id(&endpoint)?;
                let name = endpoint_name(&endpoint).unwrap_or_else(|_| id.clone());
                // Keep an endpoint selectable even if its driver temporarily
                // refuses format inspection; opening it reports the actual error.
                let inspected = (|| -> Result<Format> {
                    let client: IAudioClient = unsafe { endpoint.Activate(CLSCTX_ALL, None)? };
                    let format = TaskMem(unsafe { client.GetMixFormat()? });
                    unsafe { describe_format(format.0) }
                })();
                let (sample_rate, channels) = inspected
                    .map(|format| (format.sample_rate, format.channels))
                    .unwrap_or((0, 0));
                devices.push(DeviceInfo {
                    id,
                    name,
                    kind,
                    sample_rate,
                    channels,
                });
            }
        }
        Ok(devices)
    }

    pub(super) fn qpc_100ns() -> u64 {
        static FREQUENCY: OnceLock<u64> = OnceLock::new();
        let frequency = *FREQUENCY.get_or_init(|| {
            let mut frequency = 0;
            if unsafe { QueryPerformanceFrequency(&mut frequency) }.is_ok() && frequency > 0 {
                frequency as u64
            } else {
                0
            }
        });
        let mut counter = 0;
        if unsafe { QueryPerformanceCounter(&mut counter) }.is_err() || counter < 0 {
            return 0;
        }
        ticks_to_100ns(counter as u64, frequency)
    }

    pub(super) fn capture(
        track: usize,
        device: &DeviceInfo,
        sender: &SyncSender<CaptureEvent>,
        stop: &AtomicBool,
    ) -> Result<()> {
        capture_with_sink(track, device, stop, &mut |event| send_event(sender, event))
    }

    pub(super) fn capture_with_sink(
        track: usize,
        device: &DeviceInfo,
        stop: &AtomicBool,
        sink: &mut dyn FnMut(CaptureEvent) -> Result<()>,
    ) -> Result<()> {
        let _com = ComGuard::new()?;
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
        let identifier: Vec<u16> = device.id.encode_utf16().chain(Some(0)).collect();
        ensure!(
            !device.id.contains('\0'),
            "デバイス ID に NUL が含まれています"
        );
        let endpoint = unsafe { enumerator.GetDevice(PCWSTR(identifier.as_ptr())) }
            .context("選択したデバイスを開けませんでした")?;
        let endpoint_type: IMMEndpoint = endpoint.cast()?;
        let actual_flow = unsafe { endpoint_type.GetDataFlow()? };
        let expected_flow = if device.kind == DeviceKind::Speaker {
            eRender
        } else {
            eCapture
        };
        ensure!(
            actual_flow == expected_flow,
            "デバイスの入力・出力種別が設定と一致しません"
        );
        let client: IAudioClient = unsafe { endpoint.Activate(CLSCTX_ALL, None)? };
        let native_format = TaskMem(unsafe { client.GetMixFormat()? });
        let format = unsafe { describe_format(native_format.0)? };
        let flags = if device.kind == DeviceKind::Speaker {
            AUDCLNT_STREAMFLAGS_LOOPBACK
        } else {
            0
        };
        unsafe {
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                1_000_000, // 100 ms endpoint buffer; polling is independent.
                0,
                native_format.0,
                None,
            )
        }
        .context("WASAPI 共有モードの初期化に失敗しました")?;
        let capture: IAudioCaptureClient = unsafe { client.GetService()? };
        if stop.load(Ordering::Acquire) {
            return Ok(());
        }
        unsafe { client.Start()? };
        let _running = RunningClient(client);
        sink(CaptureEvent::Ready {
            track,
            sample_rate: format.sample_rate,
            channels: format.channels,
        })?;
        let mut clock_recovery = crate::clock_sync::ClockRecovery::new();
        let mut previous_clock_mode = None;
        let mut last_clock_report = 0u64;
        let mut previous_packet_start = None;
        while !stop.load(Ordering::Acquire) {
            // A zero packet count is absence of data, not proof of silence.
            while unsafe { capture.GetNextPacketSize()? } > 0 {
                let mut data = std::ptr::null_mut();
                let mut frames = 0;
                let mut packet_flags = 0;
                let mut device_frame = 0;
                let mut packet_qpc = 0;
                unsafe {
                    capture.GetBuffer(
                        &mut data,
                        &mut frames,
                        &mut packet_flags,
                        Some(&mut device_frame),
                        Some(&mut packet_qpc),
                    )?
                };
                let received_qpc_100ns = qpc_100ns();
                let samples = (|| -> Result<Vec<f32>> {
                    ensure!(
                        frames <= format.sample_rate,
                        "取得した音声パケットが上限の 1 秒を超えています"
                    );
                    let count = frames as usize * usize::from(format.channels);
                    if packet_flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || frames == 0 {
                        return packet_samples(None, count, format.encoding);
                    }
                    ensure!(!data.is_null(), "音声バッファが NULL です");
                    let bytes = frames as usize * usize::from(format.block_align);
                    // The buffer is owned by WASAPI until ReleaseBuffer below.
                    let data = unsafe { std::slice::from_raw_parts(data, bytes) };
                    packet_samples(Some(data), count, format.encoding)
                })();
                // Release even when conversion has failed, before propagating it.
                unsafe { capture.ReleaseBuffer(frames)? };
                let samples = samples?;
                if frames == 0 {
                    break;
                }
                let discontinuity = packet_flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32
                    != 0
                    || previous_packet_start.is_some_and(|previous| device_frame < previous);
                previous_packet_start = Some(device_frame);
                let estimate = clock_recovery.observe(crate::clock_sync::ClockObservation {
                    device_frame,
                    frames,
                    sample_rate: format.sample_rate,
                    raw_qpc_100ns: packet_qpc,
                    received_qpc_100ns,
                    native_timestamp_valid: packet_flags
                        & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32
                        == 0,
                    discontinuity,
                });
                if previous_clock_mode != Some(estimate.mode)
                    || received_qpc_100ns.saturating_sub(last_clock_report) >= 50_000_000
                {
                    sink(CaptureEvent::Timing {
                        track,
                        received_qpc_100ns,
                        device_frame,
                        packet_frames: frames,
                        raw_qpc_100ns: packet_qpc,
                        estimate,
                    })?;
                    previous_clock_mode = Some(estimate.mode);
                    last_clock_report = received_qpc_100ns;
                }
                sink(CaptureEvent::Packet {
                    track,
                    sample_rate: format.sample_rate,
                    channels: format.channels,
                    device_frame,
                    qpc_100ns: estimate.qpc_100ns,
                    samples,
                    discontinuity,
                    timestamp_valid: estimate.valid,
                })?;
                if stop.load(Ordering::Acquire) {
                    break;
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub(super) fn enumerate_devices() -> Result<Vec<DeviceInfo>> {
        bail!("WASAPI 録音は Windows 11 で利用してください")
    }

    pub(super) fn qpc_100ns() -> u64 {
        0
    }

    pub(super) fn capture(
        _track: usize,
        _device: &DeviceInfo,
        _sender: &SyncSender<CaptureEvent>,
        _stop: &AtomicBool,
    ) -> Result<()> {
        bail!("WASAPI 録音は Windows 11 で利用してください")
    }

    pub(super) fn capture_with_sink(
        _track: usize,
        _device: &DeviceInfo,
        _stop: &AtomicBool,
        _sink: &mut dyn FnMut(CaptureEvent) -> Result<()>,
    ) -> Result<()> {
        bail!("WASAPI 録音は Windows 11 で利用してください")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_device() -> DeviceInfo {
        DeviceInfo {
            id: "fixed-endpoint".into(),
            name: "test".into(),
            kind: DeviceKind::Microphone,
            sample_rate: 48_000,
            channels: 1,
        }
    }

    fn ready(channels: u16) -> CaptureEvent {
        CaptureEvent::Ready {
            track: 0,
            sample_rate: 48_000,
            channels,
        }
    }

    fn test_packet(device_frame: u64) -> CaptureEvent {
        CaptureEvent::Packet {
            track: 0,
            sample_rate: 48_000,
            channels: 1,
            device_frame,
            qpc_100ns: 1_000_000 + device_frame,
            samples: vec![0.5; 4],
            discontinuity: false,
            timestamp_valid: true,
        }
    }

    #[test]
    fn recoverable_endpoint_failure_retries_without_stopping_shared_session() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(32);
        let stop = AtomicBool::new(false);
        let mut attempts = 0;
        retry_capture(
            0,
            &test_device(),
            &sender,
            &stop,
            Duration::from_millis(1),
            |sink| {
                attempts += 1;
                if attempts == 1 {
                    bail!("一時切断");
                }
                sink(ready(1))?;
                sink(test_packet(0))?;
                assert!(!stop.load(Ordering::Acquire));
                stop.store(true, Ordering::Release);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(attempts, 2);
        let events: Vec<_> = receiver.try_iter().collect();
        assert!(matches!(
            &events[0],
            CaptureEvent::Connection {
                connected: false,
                ..
            }
        ));
        assert!(events.iter().any(|e| matches!(
            e,
            CaptureEvent::Connection {
                connected: true,
                ..
            }
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            CaptureEvent::Packet {
                discontinuity: true,
                device_frame: 0,
                ..
            }
        )));
    }

    #[test]
    fn reconnect_marks_only_first_packet_discontinuous_and_keeps_channel_contract() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(32);
        let mut state = ReconnectState::new(1);
        state.forward(ready(1), &sender).unwrap();
        state.forward(test_packet(4), &sender).unwrap();
        state.forward(test_packet(8), &sender).unwrap();
        state.begin_attempt();
        assert!(state.forward(ready(2), &sender).is_err());
        state.forward(ready(1), &sender).unwrap();
        state.forward(test_packet(0), &sender).unwrap();
        let packets: Vec<_> = receiver
            .try_iter()
            .filter_map(|e| match e {
                CaptureEvent::Packet {
                    device_frame,
                    discontinuity,
                    ..
                } => Some((device_frame, discontinuity)),
                _ => None,
            })
            .collect();
        assert_eq!(packets, vec![(4, true), (8, false), (0, true)]);
    }

    #[test]
    fn reconnect_never_resets_the_shared_qpc_trust_and_preserves_unknown_samples() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(32);
        let mut state = ReconnectState::new(1);
        state.forward(ready(1), &sender).unwrap();
        for (index, qpc) in [1000, 900, 0, 1100].into_iter().enumerate() {
            if index == 1 {
                state.begin_attempt();
                state.forward(ready(1), &sender).unwrap();
            }
            let mut event = test_packet(if index == 0 { 100 } else { 0 });
            if let CaptureEvent::Packet { qpc_100ns, .. } = &mut event {
                *qpc_100ns = qpc;
            }
            state.forward(event, &sender).unwrap();
        }
        let packets: Vec<_> = receiver
            .try_iter()
            .filter_map(|event| match event {
                CaptureEvent::Packet {
                    qpc_100ns,
                    timestamp_valid,
                    samples,
                    ..
                } => {
                    assert_eq!(
                        samples,
                        vec![0.5; 4],
                        "untrusted timestamps must not discard audio"
                    );
                    Some((qpc_100ns, timestamp_valid))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            packets,
            vec![(1000, true), (900, false), (0, false), (1100, true)]
        );
        assert_eq!(state.previous_trusted_qpc, Some(1100));
    }

    #[test]
    fn full_capture_queue_is_fatal_instead_of_becoming_a_retry_loop() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(1);
        let stop = AtomicBool::new(false);
        let mut attempts = 0;
        let error = retry_capture(
            0,
            &test_device(),
            &sender,
            &stop,
            Duration::from_millis(1),
            |sink| {
                attempts += 1;
                sink(ready(1))?;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert!(error.downcast_ref::<DeliveryFailure>().is_some());
    }

    #[test]
    fn permanent_endpoint_failure_can_be_stopped_during_long_backoff() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(32);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (failed, observed) = std::sync::mpsc::channel();
        let started = Instant::now();
        let worker = thread::spawn(move || {
            retry_capture(
                0,
                &test_device(),
                &sender,
                &worker_stop,
                Duration::from_secs(3600),
                |_| {
                    failed.send(()).unwrap();
                    bail!("未接続");
                },
            )
        });
        observed.recv_timeout(Duration::from_secs(1)).unwrap();
        stop.store(true, Ordering::Release);
        worker.join().unwrap().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn pcm_8_and_16_are_signed_correctly() {
        assert_eq!(
            decode_samples(&[0, 128, 255], SampleEncoding::Pcm(8)).unwrap(),
            vec![-1.0, 0.0, 127.0 / 128.0]
        );
        assert_eq!(
            decode_samples(&[0, 128, 0, 0, 255, 127], SampleEncoding::Pcm(16)).unwrap(),
            vec![-1.0, 0.0, 32767.0 / 32768.0]
        );
    }

    #[test]
    fn pcm_24_sign_extension_preserves_stereo_sample_order() {
        assert_eq!(
            decode_samples(
                &[0, 0, 128, 255, 255, 127, 0, 0, 0, 255, 255, 255],
                SampleEncoding::Pcm(24)
            )
            .unwrap(),
            vec![-1.0, 8388607.0 / 8388608.0, 0.0, -1.0 / 8388608.0]
        );
    }

    #[test]
    fn pcm_32_and_24_in_32_normalize_the_container() {
        let bytes: Vec<_> = [i32::MIN, 0, 0x40000000, -256]
            .into_iter()
            .flat_map(i32::to_le_bytes)
            .collect();
        assert_eq!(
            decode_samples(&bytes, SampleEncoding::Pcm(32)).unwrap(),
            vec![-1.0, 0.0, 0.5, -1.0 / 8388608.0]
        );
    }

    #[test]
    fn float32_is_not_clipped_and_nan_is_rejected() {
        let bytes: Vec<_> = [-1.25_f32, 0.0, 0.5, 1.25]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        assert_eq!(
            decode_samples(&bytes, SampleEncoding::Float32).unwrap(),
            vec![-1.25, 0.0, 0.5, 1.25]
        );
        assert!(decode_samples(&f32::NAN.to_le_bytes(), SampleEncoding::Float32).is_err());
    }

    #[test]
    fn silent_buffers_need_no_data_pointer() {
        assert_eq!(
            packet_samples(None, 4, SampleEncoding::Float32).unwrap(),
            vec![0.0; 4]
        );
        assert!(packet_samples(Some(&[0, 0]), 2, SampleEncoding::Pcm(16)).is_err());
        assert!(decode_samples(&[0, 0, 0], SampleEncoding::Pcm(16)).is_err());
    }

    #[test]
    fn clock_conversion_uses_100_nanoseconds_and_avoids_multiply_overflow() {
        assert_eq!(ticks_to_100ns(3_000_000, 3_000_000), 10_000_000);
        assert_eq!(ticks_to_100ns(1_500_000, 3_000_000), 5_000_000);
        assert_eq!(ticks_to_100ns(u64::MAX, 10_000_000), u64::MAX);
        assert_eq!(ticks_to_100ns(1, 0), 0);
    }

    #[test]
    fn timestamp_error_flags_and_invalid_qpc_never_replace_a_trusted_anchor() {
        let mut trusted = None;
        assert!(!trusted_packet_timestamp(0, true, &mut trusted));
        assert_eq!(trusted, None);
        assert!(!trusted_packet_timestamp(100, false, &mut trusted));
        assert_eq!(trusted, None);
        assert!(trusted_packet_timestamp(100, true, &mut trusted));
        assert_eq!(trusted, Some(100));
        assert!(!trusted_packet_timestamp(0, true, &mut trusted));
        assert!(!trusted_packet_timestamp(99, true, &mut trusted));
        assert!(!trusted_packet_timestamp(200, false, &mut trusted));
        assert_eq!(trusted, Some(100));
        // A native error with a spuriously high timestamp must not poison the
        // anchor: later valid packets continue from the last trusted value.
        assert!(trusted_packet_timestamp(101, true, &mut trusted));
        assert_eq!(trusted, Some(101));
        // The same helper/anchor is kept through a discontinuity; device-frame
        // resets do not authorize a reversal of the common monotonic QPC clock.
        assert!(!trusted_packet_timestamp(50, true, &mut trusted));
        assert_eq!(trusted, Some(101));
        assert!(trusted_packet_timestamp(101, true, &mut trusted));
        assert!(trusted_packet_timestamp(102, true, &mut trusted));
    }

    #[test]
    fn far_packet_clocks_are_rejected_before_they_poison_the_anchor() {
        let received = 20_300_000_000_000;
        let mut trusted = None;
        assert!(!trusted_packet_timestamp_at(
            8_000_000_000,
            true,
            received,
            &mut trusted
        ));
        assert_eq!(trusted, None);
        assert!(!trusted_packet_timestamp_at(
            received + 200_000_000,
            true,
            received,
            &mut trusted
        ));
        assert_eq!(trusted, None);
        assert!(trusted_packet_timestamp_at(
            received - 1_000_000,
            true,
            received,
            &mut trusted
        ));
        assert_eq!(trusted, Some(received - 1_000_000));
    }

    #[test]
    fn initially_stale_native_valid_audio_is_preserved_as_unknown() {
        let received = 20_300_000_000_000;
        let stale = 8_000_000_000;
        let mut trusted = None;
        let valid = trusted_packet_timestamp_at(stale, true, received, &mut trusted);
        let mut aligner = crate::sync_audio::Aligner::new(received, vec![1], 2.0).unwrap();
        aligner
            .ingest(CaptureEvent::Packet {
                track: 0,
                sample_rate: 48_000,
                channels: 1,
                device_frame: 0,
                qpc_100ns: stale,
                samples: vec![0.25; 480],
                timestamp_valid: valid,
                discontinuity: false,
            })
            .unwrap();
        let output = aligner.take(0, 480).unwrap();
        assert_eq!(output.valid, vec![false]);
        assert!(output.tracks[0].iter().all(|sample| *sample == 0.25));
    }
}
