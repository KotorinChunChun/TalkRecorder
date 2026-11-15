pub mod analysis_recovery;
pub mod capture;
pub mod clock_sync;
pub mod compact;
pub mod config;
pub mod denoise_plugins;
mod desktop_controls;
mod device_settings;
pub mod disk_space;
pub mod editor;
pub mod engine;
pub mod finalize;
pub mod journal;
pub mod maintenance;
#[cfg(windows)]
mod management;
pub mod power;
pub mod recovery;
pub mod single_instance;
pub mod speech;
pub mod storage;
pub mod sync_audio;
mod tray_status;
pub mod ui;

pub const APP_NAME: &str = "TalkRecorder";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SpeechState {
    Speech,
    Silence,
    Unknown,
}
