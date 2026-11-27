//! Videos added to a recording from elsewhere: another app's, a phone's.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, anyhow};
use ffmpeg_next::{self as ffmpeg, Packet, Rational, media};
use visor_core::project::{CAMERA_FILE, MIC_FILE, PROJECT_FILE, SCREEN_FILE, SYSTEM_FILE};
use visor_core::{Addition, EditList, Micros, Project, Take, Track, Tracks};

use crate::decode::{open_input, rotation};

/// The kinds of file a video can be added from.
pub const VIDEO_EXTENSIONS: [&str; 5] = ["mp4", "mov", "m4v", "mkv", "webm"];

/// What a video file holds, in its own time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoInfo {
    /// From `start` to the end of its last frame or sound.
    pub duration: Micros,
    /// As shown: turned upright if it says to be (a phone's).
    pub size: (u32, u32),
    /// The file's time it starts at.
    pub start: Micros,
    /// Whether it has sound Visor can play.
    pub has_audio: bool,
}

/// Whether `path` is named like a video that can be added.
pub fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Reads what's in a video to add, refusing what Visor can't play: other
/// kinds of file, no video, or video or sound it has no decoder for.
pub fn probe_video(path: &Path) -> anyhow::Result<VideoInfo> {
    ffmpeg::init()?;
    if !is_video_file(path) {
        return Err(anyhow!("Visor can add MP4, MOV, M4V, MKV and WebM videos"));
    }
    let mut input = open_input(path).map_err(|_| anyhow!("can't read this video"))?;
    let video = input
        .streams()
        .best(media::Type::Video)
        .context("no video in this file")?;
    let decoder = |s: &ffmpeg::Stream| {
        ffmpeg::codec::context::Context::from_parameters(s.parameters())
            .ok()
            .filter(|_| ffmpeg::decoder::find(s.parameters().id()).is_some())
    };
    let ctx = decoder(&video).with_context(|| {
        format!(
            "Visor can't play this video's format ({:?})",
            video.parameters().id()
        )
    })?;
    let v = ctx.decoder().video().context("can't read this video")?;
    let (w, h) = (v.width(), v.height());
    if w == 0 || h == 0 {
        return Err(anyhow!("can't read this video's frame size"));
    }
    let size = match rotation(&video.parameters()) {
        90 | 270 => (h, w),
        _ => (w, h),
    };
    let audio = input
        .streams()
        .best(media::Type::Audio)
        .filter(|a| decoder(a).is_some())
        .map(|a| a.index());
    let streams: Vec<(usize, Rational)> = std::iter::once(video.index())
        .chain(audio)
        .map(|i| (i, input.stream(i).unwrap().time_base()))
        .collect();

    // From the packets: containers' own durations and start times aren't
    // always there, nor right.
    let (mut start, mut end) = (Micros::MAX, 0);
    loop {
        let mut packet = Packet::empty();
        if packet.read(&mut input).is_err() {
            break;
        }
        let Some(&(_, tb)) = streams.iter().find(|(i, _)| *i == packet.stream()) else {
            continue;
        };
        if let Some(pts) = packet.pts().or(packet.dts()) {
            let to = |ts: i64| {
                (i128::from(ts.max(0)) * i128::from(tb.0) * 1_000_000 / i128::from(tb.1.max(1)))
                    as Micros
            };
            start = start.min(to(pts));
            end = end.max(to(pts.saturating_add(packet.duration().max(0))));
        }
    }
    if end <= start {
        return Err(anyhow!("nothing to play in this video"));
    }
    Ok(VideoInfo {
        duration: end - start,
        size,
        start,
        has_audio: audio.is_some(),
    })
}

/// Copies `src` to `dst`, telling `progress` how far it is (0.0–1.0), and
/// stopping with an error, leaving nothing at `dst`, once `cancel` is set.
/// It's written beside `dst` and renamed, so `dst` is never half a file.
pub fn copy_in(
    src: &Path,
    dst: &Path,
    progress: &mut dyn FnMut(f64),
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    let partial = dst.with_extension("partial");
    let result = copy_to(src, &partial, progress, cancel).and_then(|()| {
        // Only onto nothing, or the empty file `prepare` kept its name with.
        if fs::metadata(dst).is_ok_and(|m| m.len() > 0) {
            return Err(anyhow!("{} is already there", dst.display()));
        }
        fs::rename(&partial, dst).map_err(Into::into)
    });
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result.with_context(|| format!("copying {}", src.display()))
}

fn copy_to(
    src: &Path,
    dst: &Path,
    progress: &mut dyn FnMut(f64),
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    let mut from = fs::File::open(src)?;
    let total = from.metadata()?.len().max(1);
    let mut to = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut done = 0u64;
    progress(0.0);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled").into());
        }
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        to.write_all(&buf[..n])?;
        done += n as u64;
        progress((done as f64 / total as f64).min(1.0));
    }
    to.sync_all()?;
    progress(1.0);
    Ok(())
}

/// What adding some recordings and videos to a recording takes: what each
/// adds ([`Project::add`], in order) and the files to copy in first.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub additions: Vec<Addition>,
    /// Each file to copy, and its name in the recording's folder.
    pub copies: Vec<(PathBuf, String)>,
}

/// Whether `path` is a Visor recording's folder.
pub fn is_recording(path: &Path) -> bool {
    path.join(PROJECT_FILE).is_file()
}

/// Reads `paths` (Visor recording folders and video files), to add to the
/// recording `project` in `dir`, refusing any it can't: this recording
/// itself, one being recorded, a video it can't play. Their files get the
/// next free take numbers ([`Project::free_take_number`]), kept as empty
/// files in `dir` until [`Prepared::copy`] fills them in, so nothing else
/// takes them meanwhile; if it fails, they're removed.
pub fn prepare(dir: &Path, project: &Project, paths: &[PathBuf]) -> anyhow::Result<Prepared> {
    let mut prepared = Prepared {
        additions: Vec::new(),
        copies: Vec::new(),
    };
    let result = prepare_into(dir, project, paths, &mut prepared);
    if result.is_err() {
        for name in prepared.files() {
            let _ = fs::remove_file(dir.join(name));
        }
    }
    result.map(|()| prepared)
}

/// Keeps `name` in `dir` for a copy of `src`: an empty file, made only if
/// nothing has the name.
fn reserve(
    dir: &Path,
    copies: &mut Vec<(PathBuf, String)>,
    src: PathBuf,
    name: String,
) -> anyhow::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(&name))
        .with_context(|| format!("can't make {name} in the recording"))?;
    copies.push((src, name));
    Ok(())
}

fn prepare_into(
    dir: &Path,
    project: &Project,
    paths: &[PathBuf],
    prepared: &mut Prepared,
) -> anyhow::Result<()> {
    let mut next = project.free_take_number(dir);
    let here = fs::canonicalize(dir).ok();
    for path in paths {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let addition = if is_recording(path) {
            if fs::canonicalize(path).ok() == here {
                return Err(anyhow!("can't add a recording to itself"));
            }
            recording(dir, path, &mut next, &mut prepared.copies)
        } else {
            video(dir, path, &mut next, &mut prepared.copies)
        }
        .with_context(|| format!("adding {name}"))?;
        prepared.additions.push(addition);
    }
    Ok(())
}

impl Prepared {
    /// Copies the files into `dir`, telling `progress` how far it is over
    /// all of them. On failure or `cancel`, all of its files are removed.
    pub fn copy(
        &self,
        dir: &Path,
        progress: &mut dyn FnMut(f64),
        cancel: &AtomicBool,
    ) -> anyhow::Result<()> {
        let sizes: Vec<u64> = self
            .copies
            .iter()
            .map(|(src, _)| fs::metadata(src).map_or(0, |m| m.len()))
            .collect();
        let total = sizes.iter().sum::<u64>().max(1) as f64;
        let mut done = 0u64;
        for (i, (src, name)) in self.copies.iter().enumerate() {
            let size = sizes[i] as f64;
            let result = copy_in(
                src,
                &dir.join(name),
                &mut |p| {
                    progress((done as f64 + p * size) / total);
                },
                cancel,
            );
            if let Err(e) = result {
                // Those copied, and those kept for the rest.
                for name in self.files() {
                    let _ = fs::remove_file(dir.join(name));
                }
                return Err(e);
            }
            done += sizes[i];
        }
        progress(1.0);
        Ok(())
    }

    /// The names of the files it copies in.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.copies.iter().map(|(_, n)| n.as_str())
    }
}

/// A file's name for track `base` (`screen.mp4`) in take `n`, keeping the
/// file's own extension: `screen-3.mov`.
fn numbered(base: &str, n: usize, file: &str) -> String {
    let stem = base.split_once('.').map_or(base, |(s, _)| s);
    match file.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => format!("{stem}-{n}.{ext}"),
        _ => format!("{stem}-{n}"),
    }
}

/// Another Visor recording, as it's edited: each of its pieces a take.
fn recording(
    dir: &Path,
    src: &Path,
    next: &mut usize,
    copies: &mut Vec<(PathBuf, String)>,
) -> anyhow::Result<Addition> {
    if crate::recover::is_interrupted(src) {
        return Err(anyhow!(
            "it's being recorded, or wasn't saved properly (open it in Visor first)"
        ));
    }
    let theirs = Project::load(src).context("can't read the recording")?;
    if theirs.duration_us == 0 {
        return Err(anyhow!("nothing is recorded in it"));
    }
    let first_end = theirs
        .takes
        .first()
        .map_or(theirs.duration_us, |t| t.start_us);
    let pieces = std::iter::once((0, first_end, &theirs.tracks))
        .chain(
            theirs
                .takes
                .iter()
                .map(|t| (t.start_us, t.duration_us, &t.tracks)),
        )
        .filter(|&(_, len, _)| len > 0);
    let pieces = pieces
        .map(|(start_us, duration_us, tracks)| {
            let n = *next;
            *next += 1;
            // A file two tracks share (a video's picture and sound) is
            // copied once.
            let mut names: HashMap<String, String> = HashMap::new();
            let mut rename = |track: &Option<Track>, base: &str| -> anyhow::Result<Option<Track>> {
                let Some(t) = track else { return Ok(None) };
                let file = src.join(&t.file);
                if !file.is_file() {
                    return Err(anyhow!("its file {} is missing", t.file));
                }
                let name = match names.get(&t.file) {
                    Some(name) => name.clone(),
                    None => {
                        let name = numbered(base, n, &t.file);
                        reserve(dir, copies, file, name.clone())?;
                        names.insert(t.file.clone(), name.clone());
                        name
                    }
                };
                Ok(Some(Track {
                    file: name,
                    ..t.clone()
                }))
            };
            let tracks = Tracks {
                screen: rename(&tracks.screen, SCREEN_FILE)?,
                camera: rename(&tracks.camera, CAMERA_FILE)?,
                mic: rename(&tracks.mic, MIC_FILE)?,
                system: rename(&tracks.system, SYSTEM_FILE)?,
            };
            Ok(Take {
                start_us,
                duration_us,
                tracks,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let size = theirs
        .tracks
        .screen
        .as_ref()
        .and_then(|t| Some((t.width?, t.height?)));
    Ok(Addition {
        duration_us: theirs.duration_us,
        pieces,
        edit: theirs.edit,
        markers: theirs.markers,
        blurs: theirs.blurs,
        size,
    })
}

/// A video file: one take, its picture and sound from the same file.
fn video(
    dir: &Path,
    src: &Path,
    next: &mut usize,
    copies: &mut Vec<(PathBuf, String)>,
) -> anyhow::Result<Addition> {
    if !is_video_file(src) {
        return Err(anyhow!("Visor can add MP4, MOV, M4V, MKV and WebM videos"));
    }
    let n = *next;
    *next += 1;
    let file_name = src
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("video.mp4");
    let name = numbered(SCREEN_FILE, n, file_name);
    // Its number is kept before reading it, which can take a while.
    reserve(dir, copies, src.to_path_buf(), name.clone())?;
    let info = probe_video(src)?;
    let start_us = (info.start > 0).then_some(info.start);
    let tracks = Tracks {
        screen: Some(Track {
            file: name.clone(),
            width: Some(info.size.0),
            height: Some(info.size.1),
            fps: None,
            start_us,
        }),
        mic: info.has_audio.then(|| Track {
            file: name,
            start_us,
            ..Track::default()
        }),
        ..Tracks::default()
    };
    Ok(Addition {
        duration_us: info.duration,
        pieces: vec![Take {
            start_us: 0,
            duration_us: info.duration,
            tracks,
        }],
        edit: EditList::full(info.duration),
        markers: Vec::new(),
        blurs: Vec::new(),
        size: Some(info.size),
    })
}
