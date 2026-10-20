//! What the user asked to record.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Identifiers are backend-specific (CGDirectDisplayID, HWND, PID, portal
/// node id...) and treated as opaque here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CaptureTarget {
    Display { id: u32 },
    Window { id: u64 },
    App { pid: i32 },
    /// A region of a display, in that display's logical coordinates.
    Area { display_id: u32, rect: Rect },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Microphone device id; `None` disables the mic.
    pub mic: Option<String>,
    pub system: bool,
    /// Restrict system audio to one application.
    pub app_pid: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordingConfig {
    pub target: CaptureTarget,
    pub fps: u32,
    pub show_cursor: bool,
    pub audio: AudioConfig,
    /// Camera device id; `None` disables the camera.
    pub camera: Option<String>,
    pub countdown_secs: u32,
}

impl RecordingConfig {
    pub fn display(id: u32) -> Self {
        Self {
            target: CaptureTarget::Display { id },
            fps: 60,
            show_cursor: true,
            audio: AudioConfig::default(),
            camera: None,
            countdown_secs: 3,
        }
    }
}
