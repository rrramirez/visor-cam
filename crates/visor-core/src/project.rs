//! A recording on disk: a folder holding raw tracks and `project.json`.

use std::fs;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::{EditList, Micros, RecordingConfig};

pub const PROJECT_FILE: &str = "project.json";
/// Track files inside a recording folder.
pub const SCREEN_FILE: &str = "screen.mp4";
pub const CAMERA_FILE: &str = "camera.mp4";
pub const MIC_FILE: &str = "mic.m4a";
pub const SYSTEM_FILE: &str = "system.m4a";
pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fps: Option<u32>,
    /// The file's time at which the track starts (its first frame's
    /// timestamp), for a video added from elsewhere; `None` is 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_us: Option<Micros>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Tracks {
    pub screen: Option<Track>,
    pub camera: Option<Track>,
    pub mic: Option<Track>,
    pub system: Option<Track>,
}

impl Tracks {
    pub fn get(&self, kind: TrackKind) -> Option<&Track> {
        match kind {
            TrackKind::Screen => self.screen.as_ref(),
            TrackKind::Camera => self.camera.as_ref(),
            TrackKind::Mic => self.mic.as_ref(),
            TrackKind::System => self.system.as_ref(),
        }
    }

    fn all(&self) -> impl Iterator<Item = &Track> {
        [&self.screen, &self.camera, &self.mic, &self.system]
            .into_iter()
            .flatten()
    }
}

/// Which of a recording's tracks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Screen,
    Camera,
    Mic,
    System,
}

/// A piece recorded onto the recording after it stopped (*Record more*).
/// Its tracks are files of their own, which go on from source time
/// `start_us`: the recording's length before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Take {
    pub start_us: Micros,
    /// 0 while it is being recorded.
    pub duration_us: Micros,
    pub tracks: Tracks,
    /// Added from elsewhere (another recording or a video), not recorded
    /// onto this one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub added: bool,
}

/// A stretch of source time that one file of a track covers; no file when
/// that piece was recorded without the track (blank, or silent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub start: Micros,
    pub end: Micros,
    pub file: Option<String>,
    /// The file's time at `start` (see [`Track::start_us`]).
    pub offset: Micros,
}

/// Blurs over what was recorded here that last to its end (`recorded_end`)
/// go on over the take `clip`, like the whole-video blur they usually are:
/// stretched if the take follows on, else a copy over it (footage added
/// in between keeps its own).
pub fn carry_blurs(blurs: &mut Vec<Blur>, recorded_end: Micros, clip: crate::Range) {
    let mut copies = Vec::new();
    for b in blurs
        .iter_mut()
        .filter(|b| b.start_us < recorded_end && b.end_us >= recorded_end)
    {
        if recorded_end >= clip.0 {
            b.end_us = b.end_us.max(clip.1);
        } else {
            copies.push(Blur {
                start_us: clip.0,
                end_us: clip.1,
                ..b.clone()
            });
        }
    }
    blurs.extend(copies);
}

/// The file name of a track in take `n` (the recording's first piece being
/// 1): `screen.mp4` → `screen-2.mp4`.
pub fn take_file(file: &str, n: usize) -> String {
    match file.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}-{n}.{ext}"),
        None => format!("{file}-{n}"),
    }
}

/// Where a frame of size `src` shows inside one of size `into`: scaled to
/// fit, in the middle, with black bars, as `(x, y, width, height)` in
/// pixels (even, as YUV frames need). A shape within 1% of `into`'s fills it.
pub fn fit_rect(src: (u32, u32), into: (u32, u32)) -> (u32, u32, u32, u32) {
    let (sw, sh) = (u64::from(src.0), u64::from(src.1.max(1)));
    let (w, h) = (u64::from(into.0), u64::from(into.1));
    let (a, b) = (sw * h, w * sh);
    let even = |v: u64| ((v as u32) & !1).max(2);
    let (fw, fh) = if a.abs_diff(b) * 100 <= b {
        into
    } else if a < b {
        (even(a / sh).min(into.0), into.1)
    } else {
        (into.0, even(sh * w / sw.max(1)).min(into.1))
    };
    (((into.0 - fw) / 2) & !1, ((into.1 - fh) / 2) & !1, fw, fh)
}

/// Another recording, or a video, to add at the end of a project
/// ([`Project::add`]), in its own source time.
#[derive(Debug, Clone, PartialEq)]
pub struct Addition {
    pub duration_us: Micros,
    /// Its pieces from its time 0, their files named as in the folder it's
    /// added to.
    pub pieces: Vec<Take>,
    pub edit: EditList,
    pub markers: Vec<Marker>,
    pub blurs: Vec<Blur>,
    /// Its frame size, to keep its blurs over what they hid when it shows
    /// with black bars.
    pub size: Option<(u32, u32)>,
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
    /// The whole recording's length, takes included.
    pub duration_us: Micros,
    /// The first piece's tracks, from source time 0.
    pub tracks: Tracks,
    /// Pieces recorded after it, in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub takes: Vec<Take>,
    /// What it was recorded from, to record more from the same.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<RecordingConfig>,
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
    /// Areas of the screen hidden under a blur, e.g. a password.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blurs: Vec<Blur>,
}

/// An area of the screen blurred for a stretch of the recording. The area
/// is normalized to the screen frame (0.0–1.0), like the camera bubble;
/// the camera bubble goes on top of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Blur {
    /// Source time it starts, and where it ends (exclusive).
    pub start_us: Micros,
    pub end_us: Micros,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Blur {
    /// Whether it lasts over source time `t`.
    pub fn covers(&self, t: Micros) -> bool {
        (self.start_us..self.end_us).contains(&t)
    }

    /// Whether it blurs the frame shown from `pts` until `next` (`None`
    /// for the last frame): wherever the frame is shown, if it is on
    /// screen at any moment the blur lasts. So a frame that comes up a
    /// little before the blur starts, as a recording's frames don't keep
    /// to a fixed rate, is never seen unblurred.
    pub fn covers_frame(&self, pts: Micros, next: Option<Micros>) -> bool {
        pts < self.end_us && next.is_none_or(|n| n > self.start_us)
    }

    /// The area in a `width` x `height` frame as pixel edges `(x0, y0, x1,
    /// y1)`, kept inside the frame (`project.json` may say anything), or
    /// `None` if nothing of it is left.
    pub fn pixels(&self, width: u32, height: u32) -> Option<(usize, usize, usize, usize)> {
        let edge = |v: f64, size: u32| {
            let v = if v.is_finite() { v } else { 0.0 };
            (v.clamp(0.0, 1.0) * f64::from(size)).round() as usize
        };
        let (x0, y0) = (edge(self.x, width), edge(self.y, height));
        let x1 = edge(self.x + self.width, width);
        let y1 = edge(self.y + self.height, height);
        (x1 > x0 && y1 > y0).then_some((x0, y0, x1, y1))
    }
}

/// A note at a moment of the recording, e.g. "Step 2: open settings".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub at_us: Micros,
    #[serde(default)]
    pub text: String,
}

/// A chapter of the exported video, in output time.
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub start_us: Micros,
    pub end_us: Micros,
    pub title: String,
}

/// The shortest chapter: a marker this close after the last chapter's
/// start (in the video as exported) takes its place, and one this close to
/// the end starts none.
const MIN_CHAPTER: Micros = 1_000_000;

/// Playback speeds a project can have; others are taken as the nearest.
/// What one `atempo` filter handles: the preview and export rely on it.
pub const MIN_SPEED: f64 = 0.5;
pub const MAX_SPEED: f64 = 2.0;

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
            takes: Vec::new(),
            sources: None,
            edit: EditList::full(duration_us),
            camera_overlay: CameraOverlay::default(),
            speed: 1.0,
            thumbnail_us: None,
            markers: Vec::new(),
            blurs: Vec::new(),
        }
    }

    /// The files of track `kind` over source time, in order: the first
    /// piece's, then each take's.
    pub fn parts(&self, kind: TrackKind) -> Vec<Part> {
        let first_end = self.takes.first().map_or(self.duration_us, |t| t.start_us);
        std::iter::once((0, first_end, &self.tracks))
            .chain(
                self.takes
                    .iter()
                    .map(|t| (t.start_us, t.start_us + t.duration_us, &t.tracks)),
            )
            .filter(|(start, end, _)| end > start)
            .map(|(start, end, tracks)| Part {
                start,
                end,
                file: tracks.get(kind).map(|t| t.file.clone()),
                offset: tracks.get(kind).and_then(|t| t.start_us).unwrap_or(0),
            })
            .collect()
    }

    /// Whether any piece of the recording has track `kind`.
    pub fn has_track(&self, kind: TrackKind) -> bool {
        self.tracks.get(kind).is_some() || self.takes.iter().any(|t| t.tracks.get(kind).is_some())
    }

    /// Where what was recorded onto this recording ends: its first piece,
    /// or the last take recorded (not added) after it.
    pub fn recorded_end(&self) -> Micros {
        let first = self.takes.first().map_or(self.duration_us, |t| t.start_us);
        self.takes
            .iter()
            .filter(|t| !t.added && t.duration_us > 0)
            .map(|t| t.start_us + t.duration_us)
            .fold(first, Micros::max)
    }

    /// The take being recorded, if one is.
    pub fn recording_take(&self) -> Option<&Take> {
        self.takes.last().filter(|t| t.duration_us == 0)
    }

    /// Starts recording a take after what is recorded, into `tracks`;
    /// returns its number (2 for the first take), which names its files
    /// ([`take_file`]).
    pub fn begin_take(&mut self, tracks: Tracks) -> usize {
        self.takes.push(Take {
            start_us: self.duration_us,
            duration_us: 0,
            tracks,
            added: false,
        });
        self.takes.len() + 1
    }

    /// Ends the take being recorded, `duration` long: the recording grows by
    /// it, it plays as the last clip, and `markers` (in the take's own time)
    /// are added. Blurs that lasted to the end go on over it, like the
    /// whole-video blur they usually are. One with nothing recorded is
    /// dropped.
    pub fn finish_take(&mut self, duration: Micros, markers: Vec<Marker>) {
        if duration == 0 {
            self.drop_take();
            return;
        }
        let recorded_end = self.recorded_end();
        let Some(take) = self.takes.last_mut().filter(|t| t.duration_us == 0) else {
            return;
        };
        take.duration_us = duration;
        let start = take.start_us;
        self.duration_us = start + duration;
        let clip = crate::Range(start, start + duration);
        self.edit.append(clip);
        carry_blurs(&mut self.blurs, recorded_end, clip);
        self.markers.extend(markers.into_iter().map(|m| Marker {
            at_us: start + m.at_us.min(duration),
            ..m
        }));
    }

    /// Forgets the take being recorded (its files are the caller's). False
    /// if none is.
    pub fn drop_take(&mut self) -> bool {
        let recording = self.recording_take().is_some();
        if recording {
            self.takes.pop();
        }
        recording
    }

    /// Adds another recording or a video after everything: its pieces as
    /// takes, its clips (in its order) as the last ones, its markers and
    /// blurs. Blurs here don't go on over it: it's other footage. Returns
    /// the source time it starts at.
    pub fn add(&mut self, addition: Addition) -> Micros {
        let start = self.duration_us;
        self.takes.extend(addition.pieces.into_iter().map(|t| Take {
            start_us: start + t.start_us,
            added: true,
            ..t
        }));
        self.duration_us = start + addition.duration_us;
        self.edit.append_edit(&addition.edit, start);
        self.markers
            .extend(addition.markers.into_iter().map(|m| Marker {
                at_us: start + m.at_us,
                ..m
            }));
        self.markers.sort_by_key(|m| m.at_us);
        let frame = self
            .tracks
            .screen
            .as_ref()
            .and_then(|t| Some((t.width?, t.height?)));
        let place = match (addition.size, frame) {
            (Some(src), Some(into)) if into.0 > 0 && into.1 > 0 => {
                let (x, y, w, h) = fit_rect(src, into);
                let (fw, fh) = (f64::from(into.0), f64::from(into.1));
                Some((
                    f64::from(x) / fw,
                    f64::from(y) / fh,
                    f64::from(w) / fw,
                    f64::from(h) / fh,
                ))
            }
            _ => None,
        };
        self.blurs.extend(addition.blurs.into_iter().map(|b| {
            let (x, y, w, h) = place.unwrap_or((0.0, 0.0, 1.0, 1.0));
            Blur {
                start_us: start + b.start_us,
                end_us: start + b.end_us,
                x: x + b.x * w,
                y: y + b.y * h,
                width: b.width * w,
                height: b.height * h,
            }
        }));
        start
    }

    /// Takes out what was added from source time `at` on, as it is now:
    /// the takes, clips, markers and blurs from there, which
    /// [`add`](Self::add) puts back.
    pub fn take_addition(&mut self, at: Micros) -> Addition {
        let (pieces, takes) = std::mem::take(&mut self.takes)
            .into_iter()
            .partition(|t| t.start_us >= at);
        self.takes = takes;
        let pieces: Vec<Take> = pieces;
        let (markers, kept) = std::mem::take(&mut self.markers)
            .into_iter()
            .partition(|m| m.at_us >= at);
        self.markers = kept;
        let markers: Vec<Marker> = markers;
        let (blurs, kept) = std::mem::take(&mut self.blurs)
            .into_iter()
            .partition(|b| b.start_us >= at);
        self.blurs = kept;
        let blurs: Vec<Blur> = blurs;
        let duration_us = self.duration_us.saturating_sub(at);
        self.duration_us = at.min(self.duration_us);
        Addition {
            duration_us,
            pieces: pieces
                .into_iter()
                .map(|t| Take {
                    start_us: t.start_us - at,
                    ..t
                })
                .collect(),
            edit: self.edit.split_off(at),
            markers: markers
                .into_iter()
                .map(|m| Marker {
                    at_us: m.at_us - at,
                    ..m
                })
                .collect(),
            blurs: blurs
                .into_iter()
                .map(|b| Blur {
                    start_us: b.start_us - at,
                    end_us: b.end_us - at,
                    ..b
                })
                .collect(),
            size: None,
        }
    }

    /// The lowest number (2 or more) to name a new take's files with
    /// ([`take_file`]) that no file of this project nor in `dir` has: one
    /// of an addition that was undone may still be there, to redo it.
    pub fn free_take_number(&self, dir: &Path) -> usize {
        let mut names: Vec<String> = std::iter::once(&self.tracks)
            .chain(self.takes.iter().map(|t| &t.tracks))
            .flat_map(Tracks::all)
            .map(|t| t.file.clone())
            .collect();
        if let Ok(entries) = fs::read_dir(dir) {
            names.extend(
                entries
                    .flatten()
                    .filter_map(|e| e.file_name().into_string().ok()),
            );
        }
        let stems: Vec<&str> = names
            .iter()
            .map(|n| n.split_once('.').map_or(n.as_str(), |(stem, _)| stem))
            .collect();
        (2..)
            .find(|n| {
                let tail = format!("-{n}");
                !stems.iter().any(|s| s.ends_with(&tail))
            })
            .unwrap_or(2)
    }

    /// Length of the exported video: the kept time, sped up or slowed down.
    pub fn output_duration(&self) -> Micros {
        (self.edit.duration() as f64 / self.speed()).round() as Micros
    }

    /// The speed the video plays and exports at.
    pub fn speed(&self) -> f64 {
        self.speed.clamp(MIN_SPEED, MAX_SPEED)
    }

    /// Where source time `source` is in the export, after cuts and speed;
    /// inside a cut, where the video goes on.
    pub fn output_time(&self, source: Micros) -> Micros {
        (self.edit.kept_before(source) as f64 / self.speed()).round() as Micros
    }

    /// The exported video's chapters: one from each marker, in output time,
    /// and before the first one a chapter named after the recording. None
    /// without markers. Markers in what's trimmed off either end start none.
    pub fn chapters(&self) -> Vec<Chapter> {
        let end = self.output_duration();
        let first_kept = self.edit.ranges().iter().map(|r| r.0).min().unwrap_or(0);
        // In the order they play, which moved clips change.
        let mut markers: Vec<(Micros, &Marker)> = self
            .markers
            .iter()
            .filter(|m| m.at_us >= first_kept)
            .map(|m| (self.output_time(m.at_us), m))
            .collect();
        markers.sort_by_key(|&(at, m)| (at, m.at_us));
        // FFmpeg takes C strings: a marker's text could hold a NUL.
        let clean = |text: &str| text.replace('\0', "").trim().to_string();
        let mut starts = vec![(0, clean(&self.title))];
        for (at, m) in markers {
            if at + MIN_CHAPTER > end {
                break;
            }
            let (mut start, mut title) = (at, clean(&m.text));
            if starts.last().is_some_and(|(t, _)| at - t < MIN_CHAPTER) {
                // The later marker, about what follows, names it, unless
                // it says nothing.
                let (last, last_title) = starts.pop().unwrap();
                start = last;
                if title.is_empty() {
                    title = last_title;
                }
            }
            starts.push((start, title));
        }
        if starts.len() < 2 {
            return Vec::new();
        }
        let mut chapters: Vec<Chapter> = Vec::new();
        for (i, (start_us, title)) in starts.into_iter().enumerate() {
            if let Some(last) = chapters.last_mut() {
                last.end_us = start_us;
            }
            chapters.push(Chapter {
                start_us,
                end_us: end,
                title: if title.is_empty() {
                    format!("Chapter {}", i + 1)
                } else {
                    title
                },
            });
        }
        chapters
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
        let takes = self.takes.iter().map(|t| &t.tracks);
        for track in std::iter::once(&self.tracks)
            .chain(takes)
            .flat_map(Tracks::all)
        {
            if !is_plain_file_name(&track.file) {
                return Err(ProjectError::Invalid(
                    "track file outside the project folder",
                ));
            }
        }
        let mut end = 0;
        for take in &self.takes {
            if take.start_us < end || take.start_us + take.duration_us > self.duration_us {
                return Err(ProjectError::Invalid("takes out of order or out of bounds"));
            }
            end = take.start_us + take.duration_us;
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
    // Taken unless it is this folder under another case (on case-insensitive
    // file systems), which a rename can still change.
    let target = numbered(&name)
        .map(|n| root.join(n))
        .find(|t| t.symlink_metadata().is_err() || t.canonicalize().ok().as_ref() == Some(&dir))
        .expect("numbered names never run out");
    fs::rename(&dir, &target)?;
    Ok(Some(target))
}

/// `name`, then `name 2`, `name 3`…: the names to try when one is taken.
pub fn numbered(name: &str) -> impl Iterator<Item = String> + '_ {
    std::iter::once(name.to_string()).chain((2u64..).map(move |n| format!("{name} {n}")))
}

/// Creates a new folder in `root` named `name`, or the first free name
/// from [`numbered`]. Creating it (rather than checking that the name is
/// free) claims it, so two recordings can't end up in one folder.
pub fn create_unique_dir(root: &Path, name: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    for candidate in numbered(name) {
        let dir = root.join(candidate);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!("numbered names never run out")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Range;

    #[test]
    fn blurs_cover_frames_up_at_any_moment_they_last() {
        let blur = Blur {
            start_us: 5_000_000,
            end_us: 6_000_000,
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        // Up a little before it starts, or until a little after it ends.
        assert!(blur.covers_frame(4_980_000, Some(5_010_000)));
        assert!(blur.covers_frame(5_990_000, Some(6_020_000)));
        assert!(blur.covers_frame(5_990_000, None));
        // Gone before it starts, or up only once it's over.
        assert!(!blur.covers_frame(4_950_000, Some(5_000_000)));
        assert!(!blur.covers_frame(6_000_000, Some(6_030_000)));
    }

    #[test]
    fn chapters_follow_markers_through_cuts_and_speed() {
        const S: Micros = 1_000_000;
        let mut p = Project::new("Safari tour", "", 60 * S);
        assert!(p.chapters().is_empty());
        let marker = |at_us, text: &str| Marker {
            at_us,
            text: text.into(),
        };
        p.markers = vec![
            marker(30 * S, "Settings"),
            marker(10 * S, "Open Safari"),
            // Just before the cut below, and both in it: the last one
            // with text names the chapter.
            marker(39 * S + S * 8 / 10, "Mistake"),
            marker(41 * S, "Oops"),
            marker(42 * S, " \0"),
            // Less than a second before the end, and past it.
            marker(54 * S + S / 2, "Too late"),
            marker(58 * S, "Gone"),
        ];
        p.edit.cut(40 * S, 45 * S);
        p.edit.cut(55 * S, 60 * S);
        p.speed = 2.0;
        let chapter = |start: f64, end: f64, title: &str| Chapter {
            start_us: (start * 1e6) as Micros,
            end_us: (end * 1e6) as Micros,
            title: title.into(),
        };
        assert_eq!(
            p.chapters(),
            [
                chapter(0.0, 5.0, "Safari tour"),
                chapter(5.0, 15.0, "Open Safari"),
                chapter(15.0, 19.9, "Settings"),
                chapter(19.9, 25.0, "Oops"),
            ]
        );
        // A speed out of range plays (and exports) at the nearest allowed.
        let chapters = p.chapters();
        p.speed = 5.0;
        assert_eq!(p.chapters(), chapters);
        // Markers trimmed off the start start nothing.
        let mut trimmed = p.clone();
        trimmed.edit.cut(0, 12 * S);
        assert_eq!(trimmed.chapters()[0], chapter(0.0, 9.0, "Safari tour"));
        // A first marker right at the start starts the video.
        p.markers[1].at_us = S;
        assert_eq!(p.chapters()[0], chapter(0.0, 15.0, "Open Safari"));
    }

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

    fn track(file: &str) -> Option<Track> {
        Some(Track {
            file: file.into(),
            width: None,
            height: None,
            fps: None,
            start_us: None,
        })
    }

    #[test]
    fn takes_continue_the_recording_with_their_own_files() {
        let mut p = Project::new("Demo", "2026-10-19T18:42:00Z", 25);
        p.tracks.screen = track(SCREEN_FILE);
        p.tracks.camera = track(CAMERA_FILE);
        p.takes.push(Take {
            start_us: 20,
            duration_us: 5,
            tracks: Tracks {
                screen: track("screen-2.mp4"),
                ..Tracks::default()
            },
            added: false,
        });
        let part = |start, end, file: Option<&str>| Part {
            start,
            end,
            file: file.map(String::from),
            offset: 0,
        };
        assert_eq!(
            p.parts(TrackKind::Screen),
            [
                part(0, 20, Some(SCREEN_FILE)),
                part(20, 25, Some("screen-2.mp4"))
            ]
        );
        // No camera in the second take: nothing to show there.
        assert_eq!(
            p.parts(TrackKind::Camera),
            [part(0, 20, Some(CAMERA_FILE)), part(20, 25, None)]
        );
        assert!(p.has_track(TrackKind::Camera));
        assert!(!p.has_track(TrackKind::Mic));
        assert_eq!(take_file(MIC_FILE, 3), "mic-3.m4a");

        // A take being recorded has no length yet, and no part.
        p.takes.push(Take {
            start_us: 25,
            duration_us: 0,
            tracks: Tracks::default(),
            added: false,
        });
        assert_eq!(p.parts(TrackKind::Screen).len(), 2);

        let dir = std::env::temp_dir().join(format!("visor-takes-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        p.sources = Some(crate::RecordingConfig::display(1));
        p.save(&dir).unwrap();
        assert_eq!(Project::load(&dir).unwrap(), p);

        // Takes come from project.json too: their files stay in the folder,
        // and they stay inside the recording.
        let mut bad = p.clone();
        bad.takes[0].tracks.mic = track("../mic.m4a");
        bad.save(&dir).unwrap();
        assert!(Project::load(&dir).is_err());
        let mut bad = p.clone();
        bad.takes[0].duration_us = 10;
        bad.save(&dir).unwrap();
        assert!(Project::load(&dir).is_err());

        // Projects from before takes load with none, and save as before.
        let old = Project::new("Old", "2026-10-19T18:42:00Z", 10);
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("takes") && !json.contains("sources"));
        let loaded: Project = serde_json::from_str(&json).unwrap();
        assert!(loaded.takes.is_empty() && loaded.sources.is_none());
        assert_eq!(loaded.parts(TrackKind::Screen), [part(0, 10, None)]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_finished_take_plays_as_a_clip_at_the_end() {
        let mut p = Project::new("Demo", "2026-10-19T18:42:00Z", 20);
        p.edit.cut(5, 8);
        p.edit.split(12);
        p.edit.move_clip(2, 0);
        p.markers.push(Marker {
            at_us: 3,
            text: "Before".into(),
        });
        let blur = |start_us, end_us| Blur {
            start_us,
            end_us,
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 0.5,
        };
        p.blurs = vec![blur(0, 20), blur(2, 6)];
        let before = p.clone();

        // Being recorded: nothing changes but the pending take.
        let tracks = Tracks {
            screen: track("screen-2.mp4"),
            ..Tracks::default()
        };
        assert_eq!(p.begin_take(tracks.clone()), 2);
        assert_eq!(p.recording_take().map(|t| t.start_us), Some(20));
        assert_eq!((p.duration_us, &p.edit), (20, &before.edit));
        // Discarded: as it was.
        assert!(p.drop_take());
        assert_eq!(p, before);

        p.begin_take(tracks);
        let marker = Marker {
            at_us: 2,
            text: "In the take".into(),
        };
        p.finish_take(5, vec![marker]);
        assert_eq!(p.recording_take(), None);
        assert_eq!(p.duration_us, 25);
        assert_eq!(p.takes[0].duration_us, 5);
        assert_eq!(
            p.edit.ranges(),
            [Range(12, 20), Range(0, 5), Range(8, 12), Range(20, 25)]
        );
        assert_eq!(p.markers.last().map(|m| m.at_us), Some(22));
        // A blur lasting to the end goes on over the take; others don't.
        assert_eq!(
            p.blurs
                .iter()
                .map(|b| (b.start_us, b.end_us))
                .collect::<Vec<_>>(),
            [(0, 25), (2, 6)]
        );
        assert!(p.edit.is_valid(p.duration_us));

        // A take with nothing in it is dropped.
        let mut q = before.clone();
        q.begin_take(Tracks::default());
        q.finish_take(0, Vec::new());
        assert_eq!(q, before);
    }

    #[test]
    fn an_added_recording_comes_with_its_edit_markers_and_blurs() {
        let size = |file: &str, w, h| {
            Some(Track {
                width: Some(w),
                height: Some(h),
                ..track(file).unwrap()
            })
        };
        let mut p = Project::new("Demo", "2026-10-19T18:42:00Z", 20);
        p.tracks.screen = size(SCREEN_FILE, 1600, 900);
        p.markers.push(Marker {
            at_us: 3,
            text: "Here".into(),
        });
        let blur = |start_us, end_us, x, y, width, height| Blur {
            start_us,
            end_us,
            x,
            y,
            width,
            height,
        };
        p.blurs.push(blur(0, 20, 0.0, 0.0, 0.5, 0.5));
        let before = p.clone();

        // A square recording, 10 long, recorded more onto at 6, its first
        // 2 trimmed off; a marker and a blur in its own time.
        let mut edit = EditList::full(10);
        edit.cut(0, 2);
        let take = |start_us, duration_us, file: &str| Take {
            start_us,
            duration_us,
            tracks: Tracks {
                screen: size(file, 800, 800),
                ..Tracks::default()
            },
            added: false,
        };
        let added = Addition {
            duration_us: 10,
            pieces: vec![take(0, 6, "screen-2.mp4"), take(6, 4, "screen-3.mp4")],
            edit,
            markers: vec![Marker {
                at_us: 7,
                text: "There".into(),
            }],
            blurs: vec![blur(4, 9, 0.5, 0.0, 0.5, 1.0)],
            size: Some((800, 800)),
        };
        assert_eq!(p.add(added), 20);
        assert_eq!(p.duration_us, 30);
        assert_eq!(p.edit.ranges(), [Range(0, 20), Range(22, 30)]);
        assert_eq!(
            p.takes
                .iter()
                .map(|t| (t.start_us, t.duration_us))
                .collect::<Vec<_>>(),
            [(20, 6), (26, 4)]
        );
        assert_eq!(
            p.parts(TrackKind::Screen)[1..]
                .iter()
                .map(|part| part.file.clone().unwrap())
                .collect::<Vec<_>>(),
            ["screen-2.mp4", "screen-3.mp4"]
        );
        assert_eq!(p.markers.last().map(|m| m.at_us), Some(27));
        // This one's blur stays on this one; the added one is on the right
        // half of the 900x900 square in the middle of the 1600x900 frame.
        assert_eq!(p.blurs[0], before.blurs[0]);
        let b = &p.blurs[1];
        assert_eq!((b.start_us, b.end_us), (24, 29));
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        let (x, w) = ((350.0 + 450.0) / 1600.0, 450.0 / 1600.0);
        assert!(near(b.x, x) && near(b.y, 0.0) && near(b.width, w) && near(b.height, 1.0));
        assert!(p.validate().is_ok());

        // Taken out again (to undo it): as before; put back: as added.
        let mut q = p.clone();
        let part = q.take_addition(20);
        assert_eq!(q, before);
        assert_eq!(part.size, None);
        q.add(part);
        assert_eq!(q, p);

        // Recording more after it: this recording's whole-video blur
        // covers the new take too, the added one's doesn't.
        assert!(p.takes.iter().all(|t| t.added));
        assert_eq!(p.recorded_end(), 20);
        let mut r = p.clone();
        r.begin_take(Tracks::default());
        r.finish_take(5, Vec::new());
        assert!(!r.takes.last().unwrap().added);
        let spans: Vec<_> = r
            .blurs
            .iter()
            .map(|b| (b.start_us, b.end_us, b.x))
            .collect();
        assert_eq!(spans, [(0, 20, 0.0), (24, 29, b.x), (30, 35, 0.0)]);
        assert_eq!(r.recorded_end(), 35);

        // Its files' numbers aren't taken again by a take recorded later.
        let dir = std::env::temp_dir().join(format!("visor-added-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        assert_eq!(before.free_take_number(&dir), 2);
        assert_eq!(p.free_take_number(&dir), 4);
        // Nor those of files left in the folder (an addition undone).
        fs::write(dir.join("screen-2.mov"), b"").unwrap();
        assert_eq!(before.free_take_number(&dir), 3);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_file_starting_late_says_where_it_starts() {
        let mut t = track("screen-2.mp4").unwrap();
        assert!(!serde_json::to_string(&t).unwrap().contains("start_us"));
        t.start_us = Some(1_400_000);
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Track>(&json).unwrap(), t);
    }

    #[test]
    fn frames_of_another_shape_fit_in_the_middle() {
        assert_eq!(fit_rect((800, 800), (1600, 900)), (350, 0, 900, 900));
        assert_eq!(fit_rect((1600, 900), (800, 800)), (0, 174, 800, 450));
        assert_eq!(fit_rect((1280, 720), (1920, 1080)), (0, 0, 1920, 1080));
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
            start_us: None,
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
            start_us: None,
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
