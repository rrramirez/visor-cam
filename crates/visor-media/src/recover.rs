//! Recovery of recordings that never finished, e.g. because the app crashed
//! or was killed while recording.
//!
//! Tracks are fragmented MP4 written as they go, so everything up to the last
//! complete fragment is readable. What a crash loses is the final project
//! file: the recorder writes it with no duration when recording starts and
//! fills it in on stop (older versions wrote it only on stop). Recovery
//! measures the tracks that can be read and writes a complete project.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use ffmpeg_next::{self as ffmpeg, Packet, Rational, media};
use visor_core::project::{CAMERA_FILE, MIC_FILE, PROJECT_FILE, SCREEN_FILE, SYSTEM_FILE, Track};
use visor_core::{EditList, Micros, Project};

/// Whether `dir` holds a recording that was never finished: a screen track
/// with no project file, or a project file without a duration.
pub fn is_interrupted(dir: &Path) -> bool {
    if !dir.join(SCREEN_FILE).is_file() {
        return false;
    }
    match Project::load(dir) {
        Ok(p) => p.duration_us == 0,
        Err(_) => !dir.join(PROJECT_FILE).exists(),
    }
}

/// Loads the project in `dir`, recovering it first if it was interrupted.
pub fn load_or_recover(dir: &Path) -> anyhow::Result<Project> {
    if is_interrupted(dir) {
        return recover(dir);
    }
    Project::load(dir).with_context(|| format!("loading {}", dir.display()))
}

/// Interrupted recordings directly under `root`. A recording in progress
/// looks interrupted too, so list these before recording can start.
pub fn find_interrupted(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|dir| dir.is_dir() && is_interrupted(dir))
        .collect()
}

/// Recovers every interrupted recording directly under `root`. Returns the
/// folders recovered; failures are logged and skipped.
pub fn recover_all(root: &Path) -> Vec<PathBuf> {
    find_interrupted(root)
        .into_iter()
        .filter(|dir| match recover(dir) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!("could not recover {}: {e:#}", dir.display());
                false
            }
        })
        .collect()
}

/// Rebuilds and saves the project for an interrupted recording in `dir`.
pub fn recover(dir: &Path) -> anyhow::Result<Project> {
    crate::init()?;
    let mut project = Project::load(dir).unwrap_or_else(|_| {
        let title = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Recovered recording".into());
        Project::new(title, visor_core::time::rfc3339_now(), 0)
    });

    let screen = probe(&dir.join(SCREEN_FILE)).context("the screen track is unreadable")?;
    if screen.duration == 0 {
        bail!("the screen track is empty");
    }
    let video_track = |file: &str, info: &TrackInfo| Track {
        file: file.into(),
        width: info.size.map(|s| s.0),
        height: info.size.map(|s| s.1),
        fps: Some(30),
    };
    project.tracks.screen = Some(Track {
        fps: project
            .tracks
            .screen
            .as_ref()
            .and_then(|t| t.fps)
            .or(Some(30)),
        ..video_track(SCREEN_FILE, &screen)
    });
    project.tracks.camera = probe(&dir.join(CAMERA_FILE))
        .ok()
        .filter(|i| i.duration > 0)
        .map(|i| video_track(CAMERA_FILE, &i));
    let audio_track = |file: &str| {
        probe(&dir.join(file))
            .ok()
            .filter(|i| i.duration > 0)
            .map(|_| Track {
                file: file.into(),
                width: None,
                height: None,
                fps: None,
            })
    };
    project.tracks.mic = audio_track(MIC_FILE);
    project.tracks.system = audio_track(SYSTEM_FILE);

    project.duration_us = screen.duration;
    project.edit = EditList::full(screen.duration);
    project.save(dir)?;
    tracing::info!(
        dir = %dir.display(),
        duration_ms = screen.duration / 1000,
        "recovered an interrupted recording"
    );
    Ok(project)
}

struct TrackInfo {
    duration: Micros,
    size: Option<(u32, u32)>,
}

/// Duration (end of the last readable packet) and, for video, frame size.
/// Reads packets rather than trusting the container, which a crash leaves
/// without a duration; a truncated last fragment just ends the scan.
fn probe(path: &Path) -> anyhow::Result<TrackInfo> {
    let mut input = crate::decode::open_input(path)?;
    let stream = input
        .streams()
        .best(media::Type::Video)
        .or_else(|| input.streams().best(media::Type::Audio))
        .context("no audio or video stream")?;
    let index = stream.index();
    let time_base = stream.time_base();
    let size = (stream.parameters().medium() == media::Type::Video)
        .then(|| {
            ffmpeg::codec::context::Context::from_parameters(stream.parameters())
                .and_then(|c| c.decoder().video())
                .ok()
                .map(|v| (v.width(), v.height()))
        })
        .flatten();

    let mut end = 0i64;
    loop {
        // A fresh packet per read: av_read_frame doesn't free the old one.
        let mut packet = Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {}
            Err(ffmpeg::Error::Eof) => break,
            // A crash can leave a partial fragment at the end.
            Err(_) => break,
        }
        if packet.stream() != index {
            continue;
        }
        if let Some(pts) = packet.pts().or(packet.dts()) {
            end = end.max(pts.saturating_add(packet.duration().max(0)));
        }
    }
    Ok(TrackInfo {
        duration: to_micros(end, time_base),
        size,
    })
}

fn to_micros(ts: i64, tb: Rational) -> Micros {
    (i128::from(ts.max(0)) * i128::from(tb.0) * 1_000_000 / i128::from(tb.1.max(1))) as Micros
}
