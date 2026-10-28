//! A recording on disk: a folder holding raw tracks and `project.json`.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{EditList, Micros};

pub const PROJECT_FILE: &str = "project.json";
/// Track files inside a recording folder.
pub const SCREEN_FILE: &str = "screen.mp4";
pub const CAMERA_FILE: &str = "camera.mp4";
pub const MIC_FILE: &str = "mic.m4a";
pub const SYSTEM_FILE: &str = "system.m4a";
pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tracks {
    pub screen: Option<Track>,
    pub camera: Option<Track>,
    pub mic: Option<Track>,
    pub system: Option<Track>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BubbleShape {
    Circle,
    RoundedRect,
}

/// Camera bubble placement, normalized to the screen frame (0.0–1.0).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CameraOverlay {
    pub visible: bool,
    pub shape: BubbleShape,
    pub x: f64,
    pub y: f64,
    /// Bubble width as a fraction of the screen width.
    pub size: f64,
    /// Flip the camera horizontally, as the live bubble shows it while
    /// recording (selfie view). The camera track itself is stored unflipped.
    #[serde(default = "mirror_default")]
    pub mirror: bool,
}

fn mirror_default() -> bool {
    true
}

impl Default for CameraOverlay {
    fn default() -> Self {
        Self {
            visible: true,
            shape: BubbleShape::Circle,
            x: 0.03,
            y: 0.75,
            size: 0.2,
            mirror: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub title: String,
    /// RFC 3339.
    pub created_at: String,
    pub duration_us: Micros,
    pub tracks: Tracks,
    pub edit: EditList,
    #[serde(default)]
    pub camera_overlay: CameraOverlay,
    /// Playback speed of the whole video (0.5–2.0), applied after the edit.
    #[serde(default = "speed_default")]
    pub speed: f64,
    /// Source time of the frame used as the video's thumbnail; `None` uses
    /// the first frame kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_us: Option<Micros>,
}

fn speed_default() -> f64 {
    1.0
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid project.json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported project version {0}")]
    Version(u32),
    #[error("invalid project.json: {0}")]
    Invalid(&'static str),
}

impl Project {
    pub fn new(
        title: impl Into<String>,
        created_at: impl Into<String>,
        duration_us: Micros,
    ) -> Self {
        Self {
            version: CURRENT_VERSION,
            title: title.into(),
            created_at: created_at.into(),
            duration_us,
            tracks: Tracks::default(),
            edit: EditList::full(duration_us),
            camera_overlay: CameraOverlay::default(),
            speed: 1.0,
            thumbnail_us: None,
        }
    }

    /// Length of the exported video: the kept time, sped up or slowed down.
    pub fn output_duration(&self) -> Micros {
        (self.edit.duration() as f64 / self.speed.max(0.01)).round() as Micros
    }

    pub fn load(dir: &Path) -> Result<Self, ProjectError> {
        let project: Self = serde_json::from_slice(&fs::read(dir.join(PROJECT_FILE))?)?;
        if project.version > CURRENT_VERSION {
            return Err(ProjectError::Version(project.version));
        }
        project.validate()?;
        Ok(project)
    }

    /// Project folders can come from elsewhere (shared, unzipped), so
    /// `project.json` is untrusted: track files must stay inside the folder
    /// and the edit list must be well formed.
    fn validate(&self) -> Result<(), ProjectError> {
        let t = &self.tracks;
        for track in [&t.screen, &t.camera, &t.mic, &t.system]
            .into_iter()
            .flatten()
        {
            if !is_plain_file_name(&track.file) {
                return Err(ProjectError::Invalid(
                    "track file outside the project folder",
                ));
            }
        }
        if !self.edit.is_valid(self.duration_us) {
            return Err(ProjectError::Invalid(
                "edit ranges out of order or out of bounds",
            ));
        }
        Ok(())
    }

    /// Writes atomically (temp file + rename) so a crash never leaves a
    /// truncated `project.json`. The temp file is created fresh, never
    /// opened through an existing symlink.
    pub fn save(&self, dir: &Path) -> Result<(), ProjectError> {
        let tmp = dir.join(format!("{PROJECT_FILE}.tmp"));
        match fs::remove_file(&tmp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(self)?)?;
        drop(file);
        fs::rename(tmp, dir.join(PROJECT_FILE))?;
        Ok(())
    }
}

/// A bare file name: no directories, no `..`, not absolute.
fn is_plain_file_name(name: &str) -> bool {
    let mut parts = Path::new(name).components();
    matches!(parts.next(), Some(Component::Normal(_))) && parts.next().is_none()
}

/// Lists project folders under `root`, newest first by folder name. This is a
/// plain directory scan; there is no library database.
pub fn list_projects(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join(PROJECT_FILE).is_file())
        .collect();
    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    Ok(dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("visor-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let mut p = Project::new("Demo", "2026-10-19T18:42:00Z", 10_000_000);
        p.tracks.screen = Some(Track {
            file: "screen.mp4".into(),
            width: Some(1920),
            height: Some(1080),
            fps: Some(60),
        });
        p.edit.cut(2_000_000, 3_000_000);
        p.save(&dir).unwrap();

        assert_eq!(Project::load(&dir).unwrap(), p);
        assert_eq!(
            list_projects(dir.parent().unwrap())
                .unwrap()
                .iter()
                .filter(|d| **d == dir)
                .count(),
            1
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_untrusted_paths_and_ranges() {
        assert!(is_plain_file_name("screen.mp4"));
        for bad in ["", "..", "../x.mp4", "/etc/passwd", "a/b.mp4", "."] {
            assert!(!is_plain_file_name(bad), "{bad:?}");
        }

        let mut p = Project::new("Demo", "2026-10-19T18:42:00Z", 100);
        p.tracks.screen = Some(Track {
            file: "../other/screen.mp4".into(),
            width: None,
            height: None,
            fps: None,
        });
        assert!(p.validate().is_err());
        p.tracks.screen = None;
        assert!(p.validate().is_ok());
        p.duration_us = 50;
        assert!(p.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn save_does_not_follow_a_symlinked_temp_file() {
        let dir = std::env::temp_dir().join(format!("visor-link-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim");
        fs::write(&victim, b"keep").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("project.json.tmp")).unwrap();

        Project::new("Demo", "2026-10-19T18:42:00Z", 10)
            .save(&dir)
            .unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"keep");
        assert!(
            !fs::symlink_metadata(dir.join(PROJECT_FILE))
                .unwrap()
                .is_symlink()
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
