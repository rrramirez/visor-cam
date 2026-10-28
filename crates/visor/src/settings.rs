//! User preferences, stored as JSON in the platform config directory.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub save_dir: PathBuf,
    pub fps: u32,
    pub countdown_secs: u32,
    pub show_cursor: bool,
    /// Global shortcuts for recording (see `shortcuts`).
    pub shortcuts: bool,
    /// Index into the editor's export format list (0 MP4, 1 VP9, 2 AV1, 3 GIF).
    /// The editor starts with the last one exported.
    pub export_format: i32,
    /// Index into the editor's export size list (0 original, 1 1080p, 2 720p),
    /// likewise remembered. 1080p by default: full Retina size costs ~4x the
    /// encoding for little visible gain when shared.
    pub export_resolution: i32,
    /// Export audio cleanup, likewise remembered.
    pub export_denoise: bool,
    pub export_normalize: bool,
    pub last_camera: Option<String>,
    pub last_mic: Option<String>,
    pub system_audio: bool,
    /// Editor sidebar sections the user folded ("edit", "camera", "speed",
    /// "thumbnail").
    pub folded_sections: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        let videos = dirs::video_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Movies")))
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            save_dir: videos.join("Visor"),
            fps: 30,
            countdown_secs: 3,
            show_cursor: true,
            shortcuts: true,
            export_format: 0,
            export_resolution: 1,
            export_denoise: false,
            export_normalize: false,
            last_camera: None,
            last_mic: None,
            system_audio: false,
            folded_sections: Vec::new(),
        }
    }
}

impl Settings {
    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("Visor").join("settings.json"))
    }

    pub fn load() -> Self {
        let Some(path) = Self::path() else {
            return Self::default();
        };
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                tracing::warn!("ignoring unreadable settings ({e}); using defaults");
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let Some(path) = Self::path() else { return };
        let result = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| {
                std::fs::write(&path, serde_json::to_vec_pretty(self).unwrap_or_default())
            });
        if let Err(e) = result {
            tracing::warn!("could not save settings: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_use_defaults() {
        let s: Settings = serde_json::from_str(r#"{"fps": 60}"#).unwrap();
        assert_eq!(s.fps, 60);
        assert_eq!(s.countdown_secs, 3);
        assert!(s.shortcuts);
        assert!(!s.export_denoise);
        assert!(s.save_dir.ends_with("Visor"));
    }
}
