use crate::{
    APP_VERSION, SpeechState,
    capture::{self, DeviceInfo},
    clock_sync::ClockMode,
    config::{Config, OutputFormat, TrackSettings},
    denoise_plugins::{PluginCatalog, discover_for_current_exe},
    desktop_controls::{DesktopAction, DesktopRequests, menu_action, tray_action},
    device_settings::DeviceSettingsStore,
    engine::{Recorder, RecorderPhase, WavePoint, wave_window_ticks},
    maintenance::{self, MaintenanceReport},
    single_instance::{SingleInstance, acquire_or_notify},
    speech::DenoiseMode,
    tray_status::{TrayMonitor, TrayState},
};
use anyhow::Result;
use eframe::egui::{self, Color32, RichText, Stroke};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
        mpsc::{self, Receiver, TryRecvError},
    },
    time::{Duration, Instant},
};
use tray_icon::{
    TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem},
};

mod audit_view;

fn auto_record_on_startup(startup: bool, origin: &str) -> bool {
    startup && matches!(origin, "via_kotorintoys" | "direct_task")
}

pub fn run(path: PathBuf, startup: bool, origin: &'static str) -> Result<()> {
    let Some(instance) = acquire_or_notify(!startup)? else {
        return Ok(());
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(format!("設定 - TalkRecorder - v{APP_VERSION}"))
            .with_visible(!startup)
            .with_inner_size([960., 760.])
            .with_min_inner_size([800., 600.]),
        ..Default::default()
    };
    eframe::run_native(
        "TalkRecorder",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, path, instance, startup, origin)?))),
    )
    .map_err(|e| anyhow::anyhow!("画面を起動できません: {e}"))
}

struct InputPreview {
    recorder: Recorder,
    selection: Vec<(usize, String)>,
    stopping: bool,
}

struct App {
    config: Config,
    device_settings: DeviceSettingsStore,
    device_save_error: Option<String>,
    path: PathBuf,
    devices: Vec<DeviceInfo>,
    plugins: PluginCatalog,
    recorder: Option<Recorder>,
    preview: Option<InputPreview>,
    preview_retry_at: Instant,
    recording_requested: bool,
    message: String,
    page: usize,
    audit_view: audit_view::AuditView,
    tray: Option<TrayIcon>,
    tray_monitor: Option<TrayMonitor>,
    hidden: bool,
    exit: bool,
    folder_picker: Option<PathBuf>,
    desktop_requests: Arc<DesktopRequests>,
    instance: SingleInstance,
    smoke_dir: Option<PathBuf>,
    smoke_frames: u32,
    recovery: Option<Receiver<Result<MaintenanceReport>>>,
    stop_after_secs: Option<f64>,
    mic_reference_test: bool,
    #[cfg(windows)]
    recording_status: Arc<crate::management::RecordingStatus>,
    #[cfg(windows)]
    _management: Option<crate::management::Management>,
}
impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        path: PathBuf,
        mut instance: SingleInstance,
        startup: bool,
        origin: &'static str,
    ) -> Result<Self> {
        let mut fonts = egui::FontDefinitions::default();
        for font in [
            "C:/Windows/Fonts/YuGothM.ttc",
            "C:/Windows/Fonts/meiryo.ttc",
        ] {
            if let Ok(data) = std::fs::read(font) {
                fonts
                    .font_data
                    .insert("japanese".into(), egui::FontData::from_owned(data).into());
                fonts
                    .families
                    .entry(egui::FontFamily::Proportional)
                    .or_default()
                    .insert(0, "japanese".into());
                break;
            }
        }
        cc.egui_ctx.set_fonts(fonts);
        let mut style = (*cc.egui_ctx.style()).clone();
        for size in style.text_styles.values_mut() {
            size.size = size.size.max(16.);
        }
        style.spacing.item_spacing = egui::vec2(6., 5.);
        cc.egui_ctx.set_style(style);
        let (config, mut message, allow_automatic) = match Config::load(&path) {
            Ok(c) => (c, String::new(), true),
            Err(e) => (
                Config::default(),
                format!("設定の読み込みに失敗: {e:#}"),
                false,
            ),
        };
        let device_settings = DeviceSettingsStore::new(&config, allow_automatic);
        let devices = match capture::enumerate_devices() {
            Ok(d) => d,
            Err(e) => {
                message = format!("デバイスを取得できません: {e:#}");
                vec![]
            }
        };
        let desktop_requests = Arc::new(DesktopRequests::new(cc, startup)?);
        let ctx = cc.egui_ctx.clone();
        let requests = desktop_requests.clone();
        instance.set_waker(move || requests.send(&ctx, DesktopAction::Show))?;
        let menu = Menu::new();
        let show_item = MenuItem::with_id("talkrecorder-show", "設定を開く", true, None);
        let exit_item = MenuItem::with_id("talkrecorder-exit", "終了", true, None);
        let tray_result = (|| -> Result<TrayIcon> {
            menu.append_items(&[&show_item, &exit_item])?;
            let icon = TrayState::Idle.icon()?;
            Ok(TrayIconBuilder::new()
                .with_icon(icon)
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .with_menu_on_right_click(true)
                .with_tooltip(TrayState::Idle.tooltip())
                .build()?)
        })();
        let mut tray = match tray_result {
            Ok(tray) => Some(tray),
            Err(error) => {
                message = format!("通知領域を作成できません: {error:#}");
                None
            }
        };
        let tray_monitor = match tray
            .as_ref()
            .map(|tray| TrayMonitor::new(tray.clone()))
            .transpose()
        {
            Ok(monitor) => monitor,
            Err(error) => {
                tray.take();
                if !message.is_empty() {
                    message.push('\n');
                }
                message.push_str(&format!("通知領域の状態監視を開始できません: {error:#}"));
                None
            }
        };
        let ctx = cc.egui_ctx.clone();
        let requests = desktop_requests.clone();
        let tray_id = tray.as_ref().map(|tray| tray.id().clone());
        TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
            if tray_id.as_ref() == Some(event.id())
                && let Some(action) = tray_action(&event)
            {
                requests.send(&ctx, action);
            }
        }));
        let ctx = cc.egui_ctx.clone();
        let requests = desktop_requests.clone();
        let show_id = show_item.id().clone();
        let exit_id = exit_item.id().clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            if let Some(action) = menu_action(&event, &show_id, &exit_id) {
                requests.send(&ctx, action);
            }
        }));
        let smoke_dir = std::env::args()
            .collect::<Vec<_>>()
            .windows(2)
            .find(|p| p[0] == "--ui-smoke")
            .map(|p| PathBuf::from(&p[1]));
        if startup && tray.is_none() {
            anyhow::bail!("無表示起動には通知領域の準備が必要です: {message}");
        }
        let auto_record =
            auto_record_on_startup(startup, origin) && allow_automatic && smoke_dir.is_none();
        let recovery = if !auto_record
            && (config.auto_recover || config.auto_compact_wav)
            && smoke_dir.is_none()
        {
            let (sender, receiver) = mpsc::sync_channel(1);
            let maintenance_config = config.clone();
            let ctx = cc.egui_ctx.clone();
            let completion = Arc::new(AtomicU8::new(0));
            if let Some(monitor) = &tray_monitor {
                let state = completion.clone();
                monitor.watch(move || match state.load(Ordering::Acquire) {
                    0 => TrayState::Recovering,
                    1 => TrayState::Idle,
                    _ => TrayState::Failed,
                });
            }
            std::thread::spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    maintenance::run(&maintenance_config, true)
                }))
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "復旧・圧縮スレッドが異常終了しました。元データを保持しています"
                    ))
                });
                let successful = result
                    .as_ref()
                    .is_ok_and(|report| report.errors().next().is_none());
                completion.store(if successful { 1 } else { 2 }, Ordering::Release);
                let _ = sender.send(result);
                ctx.request_repaint();
            });
            Some(receiver)
        } else {
            None
        };
        #[cfg(windows)]
        let recording_status = Arc::new(crate::management::RecordingStatus::default());
        #[cfg(windows)]
        let management = {
            let show = desktop_requests.clone();
            let show_ctx = cc.egui_ctx.clone();
            let stop = desktop_requests.clone();
            let stop_ctx = cc.egui_ctx.clone();
            match crate::management::Management::start(
                origin,
                recording_status.clone(),
                move || show.send(&show_ctx, DesktopAction::Show),
                move || stop.send(&stop_ctx, DesktopAction::Exit),
            ) {
                Ok(management) => Some(management),
                Err(error) if startup => return Err(error),
                Err(error) => {
                    message.push_str(&format!("\n管理窓口を開けません: {error:#}"));
                    None
                }
            }
        };
        let mut app = Self {
            config,
            device_settings,
            device_save_error: None,
            path,
            devices,
            plugins: discover_for_current_exe(),
            recorder: None,
            preview: None,
            preview_retry_at: Instant::now(),
            recording_requested: false,
            message,
            page: 0,
            audit_view: audit_view::AuditView::default(),
            tray,
            tray_monitor,
            hidden: startup,
            exit: false,
            folder_picker: None,
            desktop_requests,
            instance,
            smoke_dir,
            smoke_frames: 0,
            recovery,
            stop_after_secs: std::env::args()
                .collect::<Vec<_>>()
                .windows(2)
                .find(|pair| pair[0] == "--stop-after-secs")
                .and_then(|pair| pair[1].parse::<f64>().ok()),
            mic_reference_test: std::env::args().any(|arg| arg == "--mic-reference-test"),
            #[cfg(windows)]
            recording_status,
            #[cfg(windows)]
            _management: management,
        };
        if auto_record {
            // Recorder の準備処理が録音開始前に既存データの復旧を行う。
            app.start_recording(true);
        }
        Ok(app)
    }
    fn start_recording(&mut self, resident: bool) {
        if self.exit || self.recorder.is_some() || self.recovery.is_some() {
            return;
        }
        self.remember_devices();
        let recording = if resident {
            Recorder::start_resident(self.config.clone())
        } else {
            Recorder::start(self.config.clone(), false)
        };
        match recording {
            Ok(r) => {
                #[cfg(windows)]
                self.recording_status.watch(r.phase_reader());
                if let Some(monitor) = &self.tray_monitor {
                    let phase = r.phase_reader();
                    monitor.watch(move || tray_state(phase()));
                }
                self.recorder = Some(r);
                self.message.clear();
            }
            Err(e) => {
                #[cfg(windows)]
                self.recording_status.watch(|| RecorderPhase::Failed);
                if let Some(monitor) = &self.tray_monitor {
                    monitor.set_state(TrayState::Failed);
                }
                self.message = format!("{e:#}");
            }
        }
    }

    fn update_input_preview(&mut self, minimized: bool) {
        if self
            .preview
            .as_ref()
            .is_some_and(|p| p.recorder.is_finished())
        {
            let mut finished = self.preview.take().unwrap();
            if finished.recorder.join().is_err() && !finished.stopping {
                self.preview_retry_at = Instant::now() + Duration::from_secs(2);
            }
        }
        if self.recording_requested && self.preview.is_none() {
            self.recording_requested = false;
            self.start_recording(false);
        }
        let selection = if !self.hidden && !minimized && self.page == 0 && self.can_edit_settings()
        {
            preview_selection(&self.config, &self.devices)
        } else {
            Vec::new()
        };
        if let Some(preview) = &mut self.preview {
            if !preview.stopping && preview.selection != selection {
                preview.stopping = true;
                preview.recorder.stop();
            }
        } else if !selection.is_empty() && Instant::now() >= self.preview_retry_at {
            match Recorder::start(self.config.clone(), true) {
                Ok(recorder) => {
                    self.preview = Some(InputPreview {
                        recorder,
                        selection,
                        stopping: false,
                    })
                }
                Err(_) => self.preview_retry_at = Instant::now() + Duration::from_secs(2),
            }
        }
    }
    fn can_edit_settings(&self) -> bool {
        !self.exit
            && !self.recording_requested
            && self.recorder.is_none()
            && self.recovery.is_none()
    }

    fn remember_devices(&mut self) {
        refresh_device_names(&mut self.config, &self.devices);
        match self.device_settings.remember(&self.path, &self.config) {
            Ok(true) => self.device_save_error = None,
            Ok(false) => {}
            Err(error) => {
                self.device_save_error =
                    Some(format!("デバイス設定を自動保存できません: {error:#}"))
            }
        }
    }
    fn request_exit(&mut self) {
        if !self.exit {
            self.exit = true;
            self.recording_requested = false;
            self.hidden = false;
            if let Some(recorder) = &self.recorder {
                recorder.stop();
            }
            if self.recovery.is_some() {
                self.message = "終了準備中：復旧・圧縮の完了を待っています".into();
            }
        }
    }
    fn devices(&mut self, ui: &mut egui::Ui) {
        let recording = self.recorder.as_ref().map(Recorder::status);
        let is_recording = recording
            .as_ref()
            .is_some_and(|s| s.phase == RecorderPhase::Recording);
        let preview = self.preview.as_ref().filter(|p| !p.stopping);
        let preview_status = preview.map(|p| p.recorder.status());
        let now = capture::qpc_100ns();
        let wave_window = wave_window_ticks(self.config.preview_secs);
        let can_edit = self.can_edit_settings();
        for (index, track) in self.config.tracks.iter_mut().enumerate() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                let mut status = recording.as_ref();
                ui.horizontal(|ui| {
                    ui.add_enabled(
                        can_edit,
                        egui::Checkbox::new(&mut track.enabled, &track.label),
                    );
                    let name = device_display_name(track, &self.devices);
                    ui.add_enabled_ui(can_edit, |ui| {
                        egui::ComboBox::from_id_salt(("device", index))
                            .selected_text(name)
                            .width(380.)
                            .show_ui(ui, |ui| {
                                for device in self.devices.iter().filter(|d| d.kind == track.kind) {
                                    ui.selectable_value(
                                        &mut track.device_id,
                                        device.id.clone(),
                                        &device.name,
                                    );
                                }
                            });
                        egui::ComboBox::from_id_salt(("denoise", index))
                            .selected_text(denoise_name(&track.denoise, &self.plugins))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut track.denoise, DenoiseMode::Off, "ノイズ除去なし");
                                for plugin in &self.plugins.available {
                                    ui.selectable_value(
                                        &mut track.denoise,
                                        DenoiseMode::Plugin(plugin.id.clone()),
                                        &plugin.name,
                                    );
                                }
                            });
                    });
                    if track.enabled && preview.is_some_and(|p| {
                        p.selection.iter().any(|(slot, id)| *slot == index && *id == track.device_id)
                    }) {
                        status = status.or(preview_status.as_ref());
                    }
                    if recording.is_some() {
                        let state = status
                            .and_then(|s| s.speech.get(index))
                            .copied()
                            .unwrap_or(SpeechState::Unknown);
                        ui.label(match state {
                            SpeechState::Speech => "発話",
                            SpeechState::Silence => "非発話",
                            SpeechState::Unknown => "未判定",
                        });
                        if track.enabled && let Some(s) = status {
                            if !s.connected[index] && s.running {
                                ui.label("デバイス再接続待ち");
                            } else if let Some(coverage) = s.coverage[index] {
                                let total = coverage.synchronized_frames
                                    + coverage.unsynchronized_frames
                                    + coverage.missing_frames;
                                if coverage.missing_frames == total {
                                    ui.label("入力途絶");
                                } else if coverage.missing_frames > 0 || coverage.unsynchronized_frames > 0 {
                                    ui.label(if coverage.unsynchronized_frames > 0 {
                                        "入力あり・同期未確定"
                                    } else {
                                        "入力に欠損あり"
                                    }).on_hover_text(format!(
                                        "直近の {} フレーム中、未取得 {}・同期未確定 {}",
                                        total, coverage.missing_frames, coverage.unsynchronized_frames
                                    ));
                                }
                            }
                        }
                    } else if track.enabled && !track.device_id.is_empty() {
                        let has_input = status.is_some_and(|s| {
                            s.waves.get(index).is_some_and(|wave| !wave.is_empty())
                        });
                        ui.label(if has_input { "入力確認中" } else { "入力待ち" });
                    }
                    if let Some(timing) = status.as_ref().and_then(|s| s.timing.get(index)).copied().flatten() {
                        match timing.mode {
                            ClockMode::Native => {},
                            ClockMode::Learning => { ui.label("時計を補正中"); },
                            ClockMode::Recovered => { ui.label("時計補正済み").on_hover_text(format!("推定のばらつき {:.0}ms。機器間の固定遅延は共通音声で別に測定します。", timing.uncertainty_ms)); },
                            ClockMode::Unstable => { ui.label("時刻不安定・音声保持"); },
                        }
                    }
                });
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 75.),
                    egui::Sense::hover(),
                );
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 3., if is_recording {
                    Color32::from_rgb(25, 34, 46)
                } else {
                    Color32::from_rgb(17, 23, 30)
                });
                painter.line_segment(
                    [
                        egui::pos2(rect.left(), rect.center().y),
                        egui::pos2(rect.right(), rect.center().y),
                    ],
                    Stroke::new(1.0_f32, Color32::DARK_GRAY),
                );
                if let Some(wave) = status.and_then(|s| s.waves.get(index)) {
                    let points = waveform_points(rect, wave.iter(), now, wave_window);
                    let stroke = Stroke::new(if is_recording { 1.6_f32 } else { 1.3_f32 }, if is_recording {
                            Color32::from_rgb(94, 238, 185)
                        } else {
                            Color32::from_rgb(104, 150, 144)
                        });
                    match points.as_slice() {
                        [] => {}
                        [point] => { painter.circle_filled(*point, stroke.width / 2.0, stroke.color); }
                        _ => { painter.add(egui::Shape::line(points, stroke)); }
                    }
                }
            });
        }
        if ui
            .add_enabled(can_edit, egui::Button::new("デバイスを再取得"))
            .clicked()
        {
            match capture::enumerate_devices() {
                Ok(d) => self.devices = d,
                Err(e) => self.message = format!("{e:#}"),
            }
        }
        if ui
            .add_enabled(can_edit, egui::Button::new("プラグインを再取得"))
            .clicked()
        {
            self.plugins = discover_for_current_exe();
            self.message = format!(
                "利用できるノイズ除去プラグイン: {}件",
                self.plugins.available.len()
            );
        }
    }
    fn settings(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("storage-settings")
            .num_columns(2)
            .spacing([16., 10.])
            .show(ui, |ui| {
                ui.label("保存先");
                ui.horizontal(|ui| {
                    path_edit(ui, &mut self.config.output_dir);
                    if ui.button("選択").clicked() {
                        self.folder_picker =
                            Some(std::env::current_dir().unwrap_or_else(|_| PathBuf::from("C:/")));
                    }
                });
                ui.end_row();
                ui.label("保存形式");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.config.format, OutputFormat::M4a, "M4A / AAC");
                    ui.selectable_value(&mut self.config.format, OutputFormat::Opus, "Opus");
                    ui.selectable_value(&mut self.config.format, OutputFormat::Flac, "FLAC");
                    ui.selectable_value(&mut self.config.format, OutputFormat::Wav, "WAV");
                });
                ui.end_row();
                ui.label("WAV自動圧縮");
                ui.checkbox(
                    &mut self.config.auto_compact_wav,
                    "新規はM4A保存・既存は変換後削除",
                );
                ui.end_row();
                ui.label("FFmpeg");
                path_edit(ui, &mut self.config.ffmpeg_path);
                ui.end_row();
                ui.label("停止する空き容量");
                ui.add(
                    egui::DragValue::new(&mut self.config.stop_free_space_mb)
                        .suffix(" MB")
                        .range(0..=1_048_576),
                )
                .on_hover_text(
                    "保存先の空き容量がこの値以下になると録音を停止します。0は無効です。",
                );
                ui.end_row();
                seconds(ui, "分割間隔", &mut self.config.timing.segment_secs);
                seconds(
                    ui,
                    "分割用の共通非発話",
                    &mut self.config.timing.split_silence_secs,
                );
                seconds(
                    ui,
                    "非発話を残す長さ",
                    &mut self.config.timing.keep_silence_secs,
                );
                seconds(
                    ui,
                    "会話なし除外の判定",
                    &mut self.config.timing.discard_silence_secs,
                );
                seconds(ui, "処理待ちの上限", &mut self.config.max_pending_secs);
                seconds(ui, "内部確定間隔", &mut self.config.checkpoint_secs);
                seconds(ui, "再接続間隔", &mut self.config.reconnect_secs);
                seconds(
                    ui,
                    "音声解析の応答期限",
                    &mut self.config.analysis_timeout_secs,
                );
                seconds(ui, "波形表示の時間", &mut self.config.preview_secs);
                ui.label("未処理音声の上限");
                ui.add(
                    egui::DragValue::new(&mut self.config.spool_limit_mb)
                        .suffix(" MB")
                        .range(16..=1_048_576),
                );
                ui.end_row();
                ui.label("自動スリープ抑止");
                ui.checkbox(&mut self.config.prevent_sleep, "録音中に有効");
                ui.end_row();
                ui.label("起動時自動復旧");
                ui.checkbox(&mut self.config.auto_recover, "有効");
                ui.end_row();
            });
    }
    fn speech_settings(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.config.speech;
        egui::Grid::new("speech-settings")
            .num_columns(2)
            .spacing([16., 10.])
            .show(ui, |ui| {
                ui.label("speech.cpp DLL");
                path_edit(ui, &mut s.dll_path);
                ui.end_row();
                ui.label("Silero VAD");
                path_edit(ui, &mut s.vad_model);
                ui.end_row();
                ui.label("発話しきい値");
                ui.add(
                    egui::DragValue::new(&mut s.vad_threshold)
                        .speed(0.01)
                        .range(0.01..=0.99),
                );
                ui.end_row();
                seconds(ui, "発話前の余裕", &mut s.pad_before_secs);
                seconds(ui, "発話後の余裕", &mut s.pad_after_secs);
                seconds(ui, "最小発話長", &mut s.min_speech_secs);
                ui.label("Haas 補助認識");
                ui.checkbox(&mut s.haas_enabled, "有効");
                ui.end_row();
                ui.label("Haas モデル");
                path_edit(ui, &mut s.haas_model);
                ui.end_row();
                seconds(ui, "RE-USEの処理単位", &mut s.chunk_secs);
            });
        ui.collapsing("ノイズ除去プラグインの確認", |ui| {
            ui.label(format!("利用可能: {}件", self.plugins.available.len()));
            for plugin in &self.plugins.available {
                ui.label(&plugin.name);
            }
            for issue in &self.plugins.issues {
                ui.label(issue);
            }
        });
    }
}
fn waveform_points<'a>(
    rect: egui::Rect,
    wave: impl Iterator<Item = &'a WavePoint> + Clone,
    now: u64,
    window: u64,
) -> Vec<egui::Pos2> {
    const DEFAULT_LIMIT: f32 = 1.0 / 3.0;
    let visible = wave.filter_map(|point| {
        let age = now.checked_sub(point.qpc_100ns)?;
        (age <= window).then_some((age, point.amplitude))
    });
    // 大きいピークが表示範囲にある間だけ倍率を抑える。
    let limit = visible
        .clone()
        .map(|(_, sample)| sample.abs())
        .fold(DEFAULT_LIMIT, f32::max);
    let half_height = rect.height() * 0.45;
    visible
        .map(|(age, sample)| {
            egui::pos2(
                rect.right() - rect.width() * (age as f64 / window as f64) as f32,
                rect.center().y - (sample / limit) * half_height,
            )
        })
        .collect()
}

fn seconds(ui: &mut egui::Ui, label: &str, value: &mut f64) {
    ui.label(label);
    ui.add(
        egui::DragValue::new(value)
            .speed(0.1)
            .suffix(" 秒")
            .range(0.0..=604_800.0),
    );
    ui.end_row();
}
fn path_edit(ui: &mut egui::Ui, path: &mut PathBuf) {
    let mut s = path.to_string_lossy().to_string();
    if ui
        .add(egui::TextEdit::singleline(&mut s).desired_width(530.))
        .changed()
    {
        *path = s.into();
    }
}
fn denoise_name(mode: &DenoiseMode, plugins: &PluginCatalog) -> String {
    let Some(id) = mode.plugin_id() else {
        return "ノイズ除去なし".into();
    };
    plugins
        .available
        .iter()
        .find(|plugin| plugin.id == id)
        .map(|plugin| plugin.name.clone())
        .unwrap_or_else(|| format!("未配置: {id}"))
}

fn tray_state(phase: RecorderPhase) -> TrayState {
    match phase {
        RecorderPhase::Preparing => TrayState::Preparing,
        RecorderPhase::Preview => TrayState::Preview,
        RecorderPhase::Recording => TrayState::Recording,
        RecorderPhase::Finalizing => TrayState::Finalizing,
        RecorderPhase::Finished => TrayState::Idle,
        RecorderPhase::Failed => TrayState::Failed,
    }
}

fn device_display_name(track: &TrackSettings, devices: &[DeviceInfo]) -> String {
    if track.device_id.is_empty() {
        "未選択".into()
    } else if let Some(device) = devices
        .iter()
        .find(|device| device.id == track.device_id && device.kind == track.kind)
    {
        device.name.clone()
    } else {
        format!(
            "未接続: {}",
            if track.device_name.is_empty() {
                &track.device_id
            } else {
                &track.device_name
            }
        )
    }
}

fn refresh_device_names(config: &mut Config, devices: &[DeviceInfo]) {
    for track in &mut config.tracks {
        if let Some(device) = devices
            .iter()
            .find(|device| device.id == track.device_id && device.kind == track.kind)
        {
            track.device_name.clone_from(&device.name);
        }
    }
}

fn preview_selection(config: &Config, devices: &[DeviceInfo]) -> Vec<(usize, String)> {
    config
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| {
            track.enabled
                && devices
                    .iter()
                    .any(|device| device.id == track.device_id && device.kind == track.kind)
        })
        .map(|(slot, track)| (slot, track.device_id.clone()))
        .collect()
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(error) = self.tray_monitor.as_ref().and_then(TrayMonitor::take_error) {
            if !self.message.is_empty() {
                self.message.push('\n');
            }
            self.message
                .push_str(&format!("通知領域の更新に失敗: {error}"));
        }
        if let (Some(limit), Some(recorder)) = (self.stop_after_secs, &self.recorder)
            && recorder.status().elapsed_secs >= limit
        {
            recorder.stop();
        }
        if let Some(result) =
            self.recovery
                .as_ref()
                .and_then(|receiver| match receiver.try_recv() {
                    Ok(result) => Some(result),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => Some(Err(anyhow::anyhow!(
                        "復旧処理が応答せず終了しました。元データは保持しています"
                    ))),
                })
        {
            self.recovery = None;
            let text = match result {
                Ok(report) => report.message(),
                Err(error) => format!("復旧処理を完了できませんでした: {error:#}"),
            };
            if self.message.is_empty() {
                self.message = text;
            } else {
                self.message.push('\n');
                self.message.push_str(&text);
            }
        }
        if let Some(dir) = self.smoke_dir.clone() {
            self.smoke_frames += 1;
            if self.smoke_frames == 5 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
            }
            for event in ctx.input(|i| i.events.clone()) {
                if let egui::Event::Screenshot { image, .. } = event {
                    let result = (|| -> Result<()> {
                        std::fs::create_dir_all(&dir)?;
                        let bytes: Vec<u8> =
                            image.pixels.iter().flat_map(|p| p.to_array()).collect();
                        image::save_buffer(
                            dir.join(format!("settings-{}.png", self.page)),
                            &bytes,
                            image.width() as u32,
                            image.height() as u32,
                            image::ColorType::Rgba8,
                        )?;
                        Ok(())
                    })();
                    if let Err(e) = result {
                        eprintln!("{e:#}");
                        self.exit = true;
                    } else if self.page < 3 {
                        self.page += 1;
                        self.smoke_frames = 0;
                    } else {
                        self.exit = true;
                    }
                }
            }
        }
        let _ = self.instance.take_activation_request();
        match self.desktop_requests.take() {
            Some(DesktopAction::Exit) => self.request_exit(),
            Some(DesktopAction::Show) if !self.exit => {
                self.hidden = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            _ => {}
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            if !self.exit {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                if self.tray.is_some() {
                    self.hidden = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                } else {
                    self.message = "通知領域を作成できません。終了ボタンを使ってください".into();
                }
            } else if self.recorder.is_some() || self.recovery.is_some() || self.preview.is_some() {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            }
        }
        if self.recorder.as_ref().is_some_and(Recorder::is_finished) {
            let mut recorder = self.recorder.take().unwrap();
            let status = recorder.status();
            self.message = match recorder.join() {
                Ok(()) => status.message,
                Err(e) => format!("{e:#}"),
            };
        }
        egui::TopBottomPanel::top("actions").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("TalkRecorder").strong().size(20.));
                if self.recorder.is_none() {
                    if ui
                        .add_enabled(
                            self.can_edit_settings(),
                            egui::Button::new(self.stop_after_secs.map_or_else(
                                || "録音開始".to_owned(),
                                |limit| format!("録音開始（{limit:.0}秒）"),
                            )),
                        )
                        .clicked()
                    {
                        self.recording_requested = true;
                    }
                } else if let Some(r) = &self.recorder
                    && ui
                        .add_enabled(
                            r.phase_reader()() != RecorderPhase::Finalizing,
                            egui::Button::new("停止"),
                        )
                        .clicked()
                {
                    r.stop();
                }
                if ui
                    .add_enabled(self.can_edit_settings(), egui::Button::new("設定を保存"))
                    .clicked()
                {
                    self.message = match self.config.save_atomic(&self.path) {
                        Ok(()) => {
                            self.device_settings.mark_saved(&self.config);
                            self.device_save_error = None;
                            "設定を保存しました".into()
                        }
                        Err(e) => format!("{e:#}"),
                    };
                }
                if ui
                    .add_enabled(
                        !self.exit && self.tray.is_some(),
                        egui::Button::new("トレイへ"),
                    )
                    .clicked()
                {
                    self.hidden = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                }
                if ui
                    .add_enabled(!self.exit, egui::Button::new("終了"))
                    .clicked()
                {
                    self.request_exit();
                }
                if self
                    .recorder
                    .as_ref()
                    .is_some_and(|r| r.phase_reader()() == RecorderPhase::Recording)
                {
                    ui.label(
                        RichText::new("● 録音中")
                            .strong()
                            .color(if ui.visuals().dark_mode {
                                Color32::LIGHT_RED
                            } else {
                                Color32::from_rgb(163, 27, 45)
                            }),
                    );
                } else if self.recording_requested {
                    ui.label("録音準備中");
                }
            });
            ui.horizontal(|ui| {
                for (index, label) in ["デバイス", "保存・時間", "音声処理", "録音ログ"]
                    .iter()
                    .enumerate()
                {
                    ui.selectable_value(&mut self.page, index, *label);
                }
            });
            if self.mic_reference_test
                && let Some(recorder) = &self.recorder
            {
                let seconds = recorder.status().elapsed_secs;
                let prompt = if seconds < 10.0 {
                    "静かにお待ちください（10秒から発話）"
                } else if seconds < 20.0 {
                    "話してください（1回目）"
                } else if seconds < 45.0 {
                    "静かにお待ちください"
                } else if seconds < 55.0 {
                    "話してください（2回目）"
                } else if seconds < 74.0 {
                    "静かにお待ちください（74秒で自動停止）"
                } else {
                    "録音を確定しています"
                };
                ui.label(RichText::new(prompt).strong().size(22.0));
            }
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            if let Some(r) = &self.recorder {
                let s = r.status();
                ui.label(&s.message);
                if let Some(warning) = s.audit_warning() {
                    ui.label(warning);
                }
                ui.label(format!(
                    "{} {:.1} 秒　編集後 {:.1} 秒　解析待ち {:.1} 秒",
                    if s.phase == RecorderPhase::Preview {
                        "入力確認"
                    } else {
                        "録音"
                    },
                    s.elapsed_secs,
                    s.saved_secs,
                    s.pending_secs
                ));
                if let Some(path) = &s.folder {
                    let progress = s.save_progress();
                    ui.horizontal_wrapped(|ui| {
                        if let Some(active) = progress.active {
                            let stage = match active.parts {
                                None => "保存準備中".into(),
                                Some((done, total)) if done < total => {
                                    format!("書き出し中（内部ファイル {done}/{total}）")
                                }
                                Some(_) => "ファイル確定・検証中".into(),
                            };
                            ui.label(format!("区間 {:06} {stage}", active.number));
                        }
                        ui.label(format!("保存待ち {} 区間", progress.queued));
                        ui.label(format!("保存済み {} 区間", progress.saved));
                        if progress.skipped > 0 {
                            ui.label(format!("除外 {} 区間", progress.skipped));
                        }
                        if progress.failed > 0 {
                            ui.label(format!("保存失敗 {} 区間", progress.failed));
                        }
                    });
                    ui.label(path.display().to_string());
                }
            } else {
                if self.recovery.is_some() {
                    ui.label("未完了録音を確認中");
                }
                ui.label(if self.message.is_empty() {
                    if self.recovery.is_some() {
                        ""
                    } else {
                        "停止中"
                    }
                } else {
                    &self.message
                });
            }
            if let Some(error) = &self.device_save_error {
                ui.colored_label(Color32::YELLOW, error);
            }
        });
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.page == 3 {
                self.audit_view.show(ui, &self.config.output_dir);
                return;
            }
            egui::ScrollArea::vertical().show(ui, |ui| match self.page {
                0 => self.devices(ui),
                1 => {
                    ui.add_enabled_ui(self.can_edit_settings(), |ui| self.settings(ui));
                }
                _ => {
                    ui.add_enabled_ui(self.can_edit_settings(), |ui| self.speech_settings(ui));
                }
            });
        });
        if let Some(current) = self.folder_picker.clone() {
            let mut open = true;
            let mut chosen = false;
            egui::Window::new("保存先を選択")
                .open(&mut open)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label(current.display().to_string());
                    ui.horizontal(|ui| {
                        if ui.button("上へ").clicked()
                            && let Some(p) = current.parent()
                        {
                            self.folder_picker = Some(p.into());
                        }
                        if ui.button("このフォルダーを選択").clicked() {
                            self.config.output_dir = current.clone();
                            chosen = true;
                        }
                    });
                    egui::ScrollArea::vertical()
                        .max_height(350.)
                        .show(ui, |ui| {
                            if let Ok(entries) = std::fs::read_dir(&current) {
                                for entry in entries.filter_map(Result::ok) {
                                    if entry.file_type().is_ok_and(|t| t.is_dir())
                                        && ui.button(entry.file_name().to_string_lossy()).clicked()
                                    {
                                        self.folder_picker = Some(entry.path());
                                    }
                                }
                            }
                        });
                });
            if !open || chosen {
                self.folder_picker = None;
            }
        }
        self.remember_devices();
        self.update_input_preview(ctx.input(|i| i.viewport().minimized.unwrap_or(false)));
        if self.exit && self.recorder.is_none() && self.recovery.is_none() && self.preview.is_none()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(if self.hidden { 500 } else { 100 }));
    }
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some(mut recorder) = self.recorder.take() {
            recorder.stop();
            let _ = recorder.join();
        }
        self.remember_devices();
        self.tray_monitor.take();
        self.tray.take();
    }
}

pub fn config_path_default() -> PathBuf {
    Path::new("local").join("settings.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_starts_only_for_reserved_launch_routes() {
        for origin in ["via_kotorintoys", "direct_task"] {
            assert!(auto_record_on_startup(true, origin));
            assert!(!auto_record_on_startup(false, origin));
        }
        for origin in ["standalone", "host_manual"] {
            assert!(!auto_record_on_startup(true, origin));
        }
    }

    #[test]
    fn waveform_uses_triple_gain_except_while_a_loud_peak_is_visible() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
        let center = rect.center().y;
        let quiet = 0.1;
        let draw = |first, first_time| {
            let wave = [
                WavePoint {
                    qpc_100ns: first_time,
                    amplitude: first,
                },
                WavePoint {
                    qpc_100ns: 20,
                    amplitude: quiet,
                },
            ];
            waveform_points(rect, wave.iter(), 20, 10)
        };
        let quiet_points = draw(0.0, 10);
        assert!((center - quiet_points[1].y - quiet * 45.0 * 3.0).abs() < 1e-4);
        for peak in [-0.8_f32, 0.8] {
            let loud_points = draw(peak, 10);
            assert!((loud_points[0].y - (center - peak.signum() * 45.0)).abs() < 1e-4);
            assert!(loud_points[1].y > quiet_points[1].y);
            assert_eq!(loud_points[1].x, rect.right());
        }
        assert_eq!(draw(0.8, 9), quiet_points[1..]);
    }

    #[test]
    fn waveform_history_grows_left_without_changing_the_time_scale() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(100.0, 100.0));
        let wave = [100, 110, 120].map(|qpc_100ns| WavePoint {
            qpc_100ns,
            amplitude: 0.0,
        });
        let first = waveform_points(rect, wave[..2].iter(), 110, 100);
        let later = waveform_points(rect, wave.iter(), 120, 100);
        assert_eq!(
            first.iter().map(|point| point.x).collect::<Vec<_>>(),
            [90.0, 100.0]
        );
        assert_eq!(
            later.iter().map(|point| point.x).collect::<Vec<_>>(),
            [80.0, 90.0, 100.0]
        );
        assert!(waveform_points(rect, wave.iter(), 221, 100).is_empty());
    }

    #[test]
    fn remembered_missing_device_is_distinct_from_unselected_and_never_rebound_by_name() {
        let mut config = Config::default();
        let replacement = DeviceInfo {
            id: "another-id".into(),
            name: "同じ名前".into(),
            kind: capture::DeviceKind::Microphone,
            sample_rate: 48_000,
            channels: 1,
        };
        let devices = vec![replacement];
        assert_eq!(device_display_name(&config.tracks[0], &devices), "未選択");
        config.tracks[0].device_id = "original-id".into();
        config.tracks[0].device_name = "同じ名前".into();
        refresh_device_names(&mut config, &devices);
        assert_eq!(config.tracks[0].device_id, "original-id");
        assert_eq!(
            device_display_name(&config.tracks[0], &devices),
            "未接続: 同じ名前"
        );
        let mut original = devices[0].clone();
        original.id = "original-id".into();
        original.name = "再接続したマイク".into();
        refresh_device_names(&mut config, &[original.clone()]);
        assert_eq!(config.tracks[0].device_name, original.name);
        assert_eq!(
            device_display_name(&config.tracks[0], &[original]),
            "再接続したマイク"
        );
    }

    #[test]
    fn recording_indicator_excludes_preview_preparation_and_saved_completion() {
        assert_eq!(tray_state(RecorderPhase::Recording), TrayState::Recording);
        assert_eq!(tray_state(RecorderPhase::Preview), TrayState::Preview);
        assert_eq!(tray_state(RecorderPhase::Finished), TrayState::Idle);
        assert_eq!(tray_state(RecorderPhase::Failed), TrayState::Failed);
        assert_eq!(tray_state(RecorderPhase::Preparing), TrayState::Preparing);
        assert_eq!(tray_state(RecorderPhase::Finalizing), TrayState::Finalizing);
    }
}
