//! Capture: screen + system audio per OS backend, plus cross-platform mic
//! ([`mic`]) and camera ([`camera`]) streams.
//!
//! Every buffer is stamped with [`host_now`]'s clock so tracks recorded by
//! different sources line up.

use std::sync::{Arc, Mutex};

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use visor_core::{AudioBuffer, CaptureTarget, Rect, VideoFrame};

pub mod camera;
#[cfg(any(target_os = "windows", test))]
mod capture_math;
mod host_clock;
pub mod mic;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub mod macos_permissions;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use self::windows::exclude_window_from_capture;

#[cfg(all(unix, not(target_os = "macos")))]
mod linux;

pub use host_clock::host_now;

/// Which system audio to record alongside the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemAudio {
    Off,
    /// Everything the system plays, except Visor itself.
    All,
    /// Only the given application's audio.
    App {
        pid: i32,
    },
}

#[derive(Debug, Clone)]
pub struct ScreenRequest {
    pub target: CaptureTarget,
    pub fps: u32,
    pub show_cursor: bool,
    pub system_audio: SystemAudio,
    /// Hide Visor's own windows (control bar, camera bubble) from the capture.
    pub exclude_self: bool,
}

/// A running screen capture. Call [`ScreenSession::stop`] to end it.
pub struct ScreenSession {
    pub video: Receiver<VideoFrame>,
    /// Pixel size of the frames `video` will deliver.
    pub size: (u32, u32),
    pub audio: Option<Receiver<AudioBuffer>>,
    stop: Box<dyn FnOnce() -> anyhow::Result<()> + Send>,
}

impl ScreenSession {
    pub fn new(
        video: Receiver<VideoFrame>,
        size: (u32, u32),
        audio: Option<Receiver<AudioBuffer>>,
        stop: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    ) -> Self {
        Self {
            video,
            size,
            audio,
            stop: Box::new(stop),
        }
    }

    pub fn stop(self) -> anyhow::Result<()> {
        (self.stop)()
    }
}

/// Things that can be recorded, as listed in the source picker.
#[derive(Debug, Clone, Default)]
pub struct Shareable {
    pub displays: Vec<DisplayInfo>,
    pub windows: Vec<WindowInfo>,
    pub apps: Vec<AppInfo>,
}

#[derive(Debug, Clone)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    /// Position and size in global logical coordinates (top-left origin).
    pub frame: Rect,
    /// Physical pixels per logical point.
    pub scale: f64,
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub app: String,
    pub pid: i32,
}

#[derive(Debug, Clone)]
pub struct AppInfo {
    pub pid: i32,
    pub name: String,
}

pub trait Backend: Send + Sync {
    fn shareable(&self) -> anyhow::Result<Shareable>;
    fn start(&self, request: &ScreenRequest) -> anyhow::Result<ScreenSession>;
}

/// The capture backend for the current OS.
pub fn backend() -> anyhow::Result<Arc<dyn Backend>> {
    #[cfg(target_os = "macos")]
    return Ok(Arc::new(macos::MacBackend));
    #[cfg(target_os = "windows")]
    return Ok(Arc::new(self::windows::WinBackend));
    #[cfg(all(unix, not(target_os = "macos")))]
    return Ok(Arc::new(linux::LinuxBackend));
    #[cfg(not(any(unix, target_os = "windows")))]
    anyhow::bail!("no capture backend for this OS yet");
}

/// Fans one producer out to any number of subscribers. Slow subscribers lose
/// items instead of blocking the producer; dropped receivers are pruned.
pub struct Tap<T> {
    subscribers: Arc<Mutex<Vec<Sender<T>>>>,
}

impl<T> Clone for Tap<T> {
    fn clone(&self) -> Self {
        Self {
            subscribers: self.subscribers.clone(),
        }
    }
}

impl<T> Default for Tap<T> {
    fn default() -> Self {
        Self {
            subscribers: Arc::default(),
        }
    }
}

impl<T: Clone> Tap<T> {
    pub fn subscribe(&self, capacity: usize) -> Receiver<T> {
        let (tx, rx) = bounded(capacity);
        self.subscribers.lock().unwrap().push(tx);
        rx
    }

    pub fn send(&self, item: T) {
        self.subscribers
            .lock()
            .unwrap()
            .retain(|tx| match tx.try_send(item.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) => {
                    tracing::trace!("tap subscriber is behind; dropped an item");
                    true
                }
                Err(TrySendError::Disconnected(_)) => false,
            });
    }

    /// Disconnects every subscriber, ending their streams.
    pub fn close(&self) {
        self.subscribers.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_fans_out_and_prunes() {
        let tap = Tap::default();
        let a = tap.subscribe(4);
        let b = tap.subscribe(4);
        tap.send(1);
        drop(b);
        tap.send(2);
        assert_eq!(a.try_iter().collect::<Vec<_>>(), [1, 2]);
        assert_eq!(tap.subscribers.lock().unwrap().len(), 1);
        tap.close();
        assert!(a.recv().is_err());
    }
}
