//! Capture backends. Each OS backend turns a [`RecordingConfig`] into streams
//! of host-timestamped [`VideoFrame`]s and [`AudioBuffer`]s.

use crossbeam_channel::Receiver;
use visor_core::{AudioBuffer, Micros, RecordingConfig, VideoFrame};

#[cfg(target_os = "macos")]
mod macos;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioKind {
    Mic,
    System,
}

/// A running capture session. Call [`Session::stop`] to end it.
pub struct Session {
    pub screen: Receiver<VideoFrame>,
    /// Pixel size of the frames `screen` will deliver.
    pub screen_size: (u32, u32),
    pub camera: Option<Receiver<VideoFrame>>,
    pub audio: Vec<(AudioKind, Receiver<AudioBuffer>)>,
    stop: Box<dyn FnOnce() -> anyhow::Result<()> + Send>,
}

impl Session {
    pub fn stop(self) -> anyhow::Result<()> {
        (self.stop)()
    }
}

/// Something that can be recorded, as listed in the source picker.
#[derive(Debug, Clone, Default)]
pub struct Shareable {
    pub displays: Vec<DisplayInfo>,
    pub windows: Vec<WindowInfo>,
    pub apps: Vec<AppInfo>,
}

#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub id: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub app: String,
}

#[derive(Debug, Clone)]
pub struct AppInfo {
    pub pid: i32,
    pub name: String,
}

pub trait Backend {
    fn shareable(&self) -> anyhow::Result<Shareable>;
    fn start(&self, config: &RecordingConfig) -> anyhow::Result<Session>;
    /// Current host monotonic time, in the same clock as `host_ts`.
    fn host_now(&self) -> Micros;
}

/// The capture backend for the current OS.
pub fn backend() -> anyhow::Result<Box<dyn Backend>> {
    #[cfg(target_os = "macos")]
    return Ok(Box::new(macos::MacBackend::new()));
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("no capture backend for this OS yet");
}
