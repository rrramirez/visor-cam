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
    /// Index into the editor's export format list (0 MP4, 1 VP9, 2 AV1, 3 GIF).
    pub export_format: i32,
    pub last_camera: Option<String>,
    pub last_mic: Option<String>,
    pub system_audio: bool,
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
            export_format: 0,
            last_camera: None,
            last_mic: None,
            system_audio: false,
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
        assert!(s.save_dir.ends_with("Visor"));
    }
}
