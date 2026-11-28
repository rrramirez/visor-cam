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
    /// Lets AI agents control recording (see `app::agent`). Off until the
    /// user turns it on: any program they run could then record the screen.
    pub agent_control: bool,
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
    /// The voiceover panel's own choices, once it has been used (the
    /// recorder's aren't changed by it).
    pub voiceover: Option<VoiceoverDevices>,
}

/// What the voiceover panel records from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceoverDevices {
    pub voice: bool,
    pub mic: Option<String>,
    pub camera_on: bool,
    pub camera: Option<String>,
}

impl VoiceoverDevices {
    /// The panel's choices: as last left, or the first time, the
    /// recorder's (voice on, the camera if it records one).
    pub fn current(s: &Settings) -> Self {
        s.voiceover.clone().unwrap_or_else(|| Self {
            voice: true,
            mic: s.last_mic.clone(),
            camera_on: s.last_camera.is_some(),
            camera: s.last_camera.clone(),
        })
    }
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
            agent_control: false,
            export_format: 0,
            export_resolution: 1,
            export_denoise: false,
            export_normalize: false,
            last_camera: None,
            last_mic: None,
            system_audio: false,
            folded_sections: Vec::new(),
            voiceover: None,
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
        assert!(!s.agent_control);
        assert!(!s.export_denoise);
        assert!(s.save_dir.ends_with("Visor"));
    }

    #[test]
    fn the_voiceover_panel_keeps_its_own_choices() {
        let mut s = Settings {
            last_mic: Some("usb".into()),
            ..Settings::default()
        };
        // First time: the recorder's, voice on, no camera as it has none.
        let first = VoiceoverDevices::current(&s);
        assert_eq!(
            first,
            VoiceoverDevices {
                voice: true,
                mic: Some("usb".into()),
                camera_on: false,
                camera: None
            }
        );
        // Then its own, camera off included, whatever the recorder uses.
        s.voiceover = Some(VoiceoverDevices {
            voice: false,
            camera_on: false,
            camera: Some("cam".into()),
            ..first
        });
        s.last_camera = Some("other".into());
        let again = VoiceoverDevices::current(&s);
        assert!(!again.voice && !again.camera_on);
        assert_eq!(again.camera.as_deref(), Some("cam"));
    }
}
