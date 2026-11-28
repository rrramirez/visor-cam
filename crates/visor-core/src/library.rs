//! The recordings in the save folder, as the Recordings window and agents
//! list them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::Micros;
use crate::project::{PROJECT_FILE, Project, list_projects, recorded_at};

/// A recording in the library.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub dir: PathBuf,
    /// Its name; the folder's when it has none or can't be read.
    pub title: String,
    pub recorded_at: Option<SystemTime>,
    /// When `project.json` last changed: an edit, a rename of the title.
    pub changed_at: Option<SystemTime>,
    /// `None` when `project.json` can't be read.
    pub info: Option<Info>,
}

/// What a readable recording holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    /// Everything recorded, takes and additions included.
    pub recorded_us: Micros,
    /// The video as exported: what's kept, at its speed.
    pub video_us: Micros,
    pub markers: usize,
}

/// The recordings under `root`, newest first, by their real paths (as
/// editors and agents know them, through any link to `root`).
pub fn entries(root: &Path) -> io::Result<Vec<Entry>> {
    let root = root.canonicalize()?;
    Ok(list_projects(&root)?
        .into_iter()
        .map(|d| entry(&d))
        .collect())
}

/// The library's entry for the recording in `dir`.
pub fn entry(dir: &Path) -> Entry {
    let folder = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let project = Project::load(dir).ok();
    let title = project
        .as_ref()
        .map(|p| p.title.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or(folder);
    Entry {
        dir: dir.to_path_buf(),
        title,
        recorded_at: recorded_at(dir),
        changed_at: fs::metadata(dir.join(PROJECT_FILE))
            .and_then(|m| m.modified())
            .ok(),
        info: project.map(|p| Info {
            recorded_us: p.duration_us,
            video_us: p.output_duration(),
            markers: p.markers.len(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_recordings_newest_first_with_their_video_length() {
        let root = std::env::temp_dir().join(format!("visor-library-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let make = |folder: &str, title: &str| {
            let dir = root.join(folder);
            fs::create_dir_all(&dir).unwrap();
            let mut p = Project::new(title, "2026-11-27T10:00:00Z", 10_000_000);
            p.edit.cut(2_000_000, 4_000_000);
            p.speed = 2.0;
            p.save(&dir).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            dir
        };
        make("2026-11-25 Recording", "");
        make("Intro", "Intro");
        let broken = root.join("Broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join(PROJECT_FILE), "{").unwrap();
        // Not a recording.
        fs::create_dir_all(root.join("exports")).unwrap();

        let list = entries(&root).unwrap();
        let titles: Vec<&str> = list.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, ["Broken", "Intro", "2026-11-25 Recording"]);
        assert_eq!(list[0].info, None);
        assert_eq!(
            list[1].info,
            Some(Info {
                recorded_us: 10_000_000,
                video_us: 4_000_000,
                markers: 0,
            })
        );
        assert!(list[1].changed_at.is_some() && list[1].recorded_at.is_some());
        // Reached through a link (macOS's /tmp, a linked folder): listed by
        // their real paths, as editors know them.
        #[cfg(unix)]
        {
            let link = root.with_extension("link");
            let _ = fs::remove_file(&link);
            std::os::unix::fs::symlink(&root, &link).unwrap();
            let real = root.canonicalize().unwrap();
            assert!(
                entries(&link)
                    .unwrap()
                    .iter()
                    .all(|e| e.dir.starts_with(&real))
            );
            fs::remove_file(&link).unwrap();
        }
        fs::remove_dir_all(&root).unwrap();
    }
}
