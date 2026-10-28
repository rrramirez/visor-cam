//! A recording on disk: a folder holding raw tracks and `project.json`.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

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
    /// Points of interest noted while recording, in source time.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub markers: Vec<Marker>,
}

/// A note at a moment of the recording, e.g. "Step 2: open settings".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub at_us: Micros,
    #[serde(default)]
    pub text: String,
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
            markers: Vec::new(),
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

/// Lists project folders under `root`, newest first by when they were
/// recorded. This is a plain directory scan; there is no library database.
pub fn list_projects(root: &Path) -> io::Result<Vec<PathBuf>> {
    // Folders are renamed after their recordings, so names don't sort by
    // date; the folder's creation time does, survives renames, and costs a
    // stat rather than reading every project.json.
    let mut dirs: Vec<(Option<SystemTime>, PathBuf)> = fs::read_dir(root)?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join(PROJECT_FILE).is_file())
        .map(|p| (recorded_at(&p), p))
        .collect();
    dirs.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.file_name().cmp(&a.1.file_name()))
    });
    Ok(dirs.into_iter().map(|(_, p)| p).collect())
}

/// When the recording in `dir` was made: its folder's creation time, where
/// the file system keeps one.
pub fn recorded_at(dir: &Path) -> Option<SystemTime> {
    fs::metadata(dir).and_then(|m| m.created()).ok()
}

/// Longest folder or file name stem, in bytes: file systems allow 255
/// bytes, leaving room for " 2" and extensions like " thumbnail.jpg".
const MAX_NAME_BYTES: usize = 200;

/// A folder (or file) name for a recording titled `title`: characters that
/// aren't allowed in file names (on any platform) become "-", leading or
/// trailing dots and spaces go, and long titles are cut short. `None` if
/// nothing usable is left.
pub fn folder_name(title: &str) -> Option<String> {
    let mut cleaned = String::new();
    for c in title.chars() {
        let c = if c.is_control()
            || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
        {
            '-'
        } else {
            c
        };
        if cleaned.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        cleaned.push(c);
    }
    let name = cleaned.trim_matches(|c: char| c == '.' || c.is_whitespace());
    if name.is_empty() {
        return None;
    }
    // Windows reserves device names, with any extension ("COM1.mp4" too):
    // set the stem apart.
    let (stem, rest) = name.split_at(name.find('.').unwrap_or(name.len()));
    let device = stem.trim_end().to_ascii_uppercase();
    let reserved = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (device.len() == 4
            && (device.starts_with("COM") || device.starts_with("LPT"))
            && device.as_bytes()[3].is_ascii_digit());
    Some(if reserved {
        format!("{} recording{rest}", stem.trim_end())
    } else {
        name.to_string()
    })
}

/// Renames the recording folder `dir` after its project's title, if it is
/// directly inside `root` (the recordings folder) and named otherwise. A
/// taken name gets " 2", " 3", …. Returns the new path when it moved.
pub fn rename_to_title(dir: &Path, root: &Path) -> io::Result<Option<PathBuf>> {
    if dir.is_symlink() {
        return Ok(None);
    }
    let dir = dir.canonicalize()?;
    let root = root.canonicalize()?;
    if dir.parent() != Some(root.as_path()) {
        return Ok(None);
    }
    let title = Project::load(&dir)
        .map_err(|e| io::Error::other(e.to_string()))?
        .title;
    let Some(name) = folder_name(&title) else {
        return Ok(None);
    };
    if dir.file_name() == Some(name.as_ref()) {
        return Ok(None);
    }
    let mut target = root.join(&name);
    let mut n = 2;
    // Taken unless it is this folder under another case (on case-insensitive
    // file systems), which a rename can still change.
    while target.symlink_metadata().is_ok() && target.canonicalize().ok().as_ref() != Some(&dir) {
        target = root.join(format!("{name} {n}"));
        n += 1;
    }
    fs::rename(&dir, &target)?;
    Ok(Some(target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(root: &Path, folder: &str, title: &str, created: &str) -> PathBuf {
        let dir = root.join(folder);
        fs::create_dir_all(&dir).unwrap();
        Project::new(title, created, 1_000_000).save(&dir).unwrap();
        dir
    }

    #[test]
    fn folder_names_are_safe() {
        assert_eq!(folder_name(" Demo: a/b? ").as_deref(), Some("Demo- a-b-"));
        assert_eq!(folder_name("..hidden.").as_deref(), Some("hidden"));
        assert_eq!(folder_name(" . "), None);
        assert_eq!(folder_name("con").as_deref(), Some("con recording"));
        // Reserved with any extension: the suffix goes on the stem.
        assert_eq!(
            folder_name("Com1.mp4").as_deref(),
            Some("Com1 recording.mp4")
        );
        assert_eq!(
            folder_name("nul .txt").as_deref(),
            Some("nul recording.txt")
        );
        assert_eq!(folder_name("Console").as_deref(), Some("Console"));
        assert_eq!(folder_name(&"x".repeat(300)).unwrap().len(), 200);
        // Bytes, not characters: 3-byte characters stop at 66 of them.
        let cjk = folder_name(&"録".repeat(100)).unwrap();
        assert_eq!(cjk.chars().count(), 66);
    }

    #[test]
    fn folders_follow_titles_inside_the_recordings_folder() {
        let root = std::env::temp_dir().join(format!("visor-rename-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let a = recording(
            &root,
            "2026-10-01 10.00.00 Recording",
            "Demo",
            "2026-10-01T10:00:00Z",
        );
        let b = recording(
            &root,
            "2026-10-02 10.00.00 Recording",
            "Demo",
            "2026-10-02T10:00:00Z",
        );
        let root = root.canonicalize().unwrap();

        let a = rename_to_title(&a, &root).unwrap().unwrap();
        assert_eq!(a, root.join("Demo"));
        // Taken: numbered. Already named: left alone.
        let b = rename_to_title(&b, &root).unwrap().unwrap();
        assert_eq!(b, root.join("Demo 2"));
        assert_eq!(rename_to_title(&a, &root).unwrap(), None);
        // Newest first, whatever the names.
        assert_eq!(list_projects(&root).unwrap(), [b.clone(), a.clone()]);

        // A case-only change isn't blocked by the folder itself.
        let mut p = Project::load(&a).unwrap();
        p.title = "DEMO".into();
        p.save(&a).unwrap();
        let renamed = rename_to_title(&a, &root).unwrap().unwrap();
        assert_eq!(renamed.file_name().unwrap(), "DEMO");

        // Outside the recordings folder: never moved.
        let elsewhere = recording(&root.join("nested"), "x", "Moved?", "2026-10-03T10:00:00Z");
        assert_eq!(rename_to_title(&elsewhere, &root).unwrap(), None);
        assert!(elsewhere.exists());
        fs::remove_dir_all(&root).unwrap();
    }

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
        p.markers.push(Marker {
            at_us: 4_000_000,
            text: "Step 2: open settings".into(),
        });
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
