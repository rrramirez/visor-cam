//! Thumbnails for the Recordings window, cached outside the recordings.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use anyhow::Context;
use visor_core::Project;
use visor_core::project::PROJECT_FILE;

use crate::export::{frame_jpeg, thumbnail_time};

/// The recording in `dir`'s thumbnail, `height` pixels tall, as a JPEG in
/// `cache`: its chosen thumbnail frame (else the first kept one), as the
/// export shows it. Rendered once per state of `project.json`; a new
/// rendering replaces the recording's older ones. Slow the first time, so
/// call it off the UI thread.
pub fn cached_thumbnail(dir: &Path, cache: &Path, height: u32) -> anyhow::Result<PathBuf> {
    let meta = fs::metadata(dir.join(PROJECT_FILE)).context("no project.json")?;
    let folder = hash(&(dir, height));
    let state = hash(&(meta.modified().ok(), meta.len()));
    let path = cache.join(format!("{folder:016x}-{state:016x}.jpg"));
    if path.is_file() {
        return Ok(path);
    }
    let project = Project::load(dir)?;
    let jpeg = frame_jpeg(dir, &project, thumbnail_time(&project), height)?;
    fs::create_dir_all(cache)?;
    // Written whole, then named: a half-written file is never shown.
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, jpeg)?;
    fs::rename(&tmp, &path)?;
    let mine = format!("{folder:016x}-");
    for old in fs::read_dir(cache)?.flatten() {
        let name = old.file_name().to_string_lossy().into_owned();
        if name.starts_with(&mine) && old.path() != path {
            let _ = fs::remove_file(old.path());
        }
    }
    Ok(path)
}

/// Removes cached thumbnails of recordings other than `dirs` (trashed,
/// moved away).
pub fn prune_thumbnails(cache: &Path, dirs: &[PathBuf], height: u32) {
    let keep: Vec<String> = dirs
        .iter()
        .map(|d| format!("{:016x}-", hash(&(d.as_path(), height))))
        .collect();
    let Ok(entries) = fs::read_dir(cache) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.ends_with(".jpg") && !keep.iter().any(|k| name.starts_with(k.as_str())) {
            let _ = fs::remove_file(e.path());
        }
    }
}

fn hash(value: &impl Hash) -> u64 {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    h.finish()
}
