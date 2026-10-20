//! A recording on disk: a folder holding raw tracks and `project.json`.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{EditList, Micros};

pub const PROJECT_FILE: &str = "project.json";
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
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid project.json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unsupported project version {0}")]
    Version(u32),
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
        }
    }

    pub fn load(dir: &Path) -> Result<Self, ProjectError> {
        let project: Self = serde_json::from_slice(&fs::read(dir.join(PROJECT_FILE))?)?;
        if project.version > CURRENT_VERSION {
            return Err(ProjectError::Version(project.version));
        }
        Ok(project)
    }

    /// Writes atomically (temp file + rename) so a crash never leaves a
    /// truncated `project.json`.
    pub fn save(&self, dir: &Path) -> Result<(), ProjectError> {
        let tmp = dir.join(format!("{PROJECT_FILE}.tmp"));
        fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        fs::rename(tmp, dir.join(PROJECT_FILE))?;
        Ok(())
    }
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
}
