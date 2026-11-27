//! Videos added to a recording from elsewhere: another app's, a phone's.

use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, anyhow};
use ffmpeg_next::{self as ffmpeg, Packet, Rational, media};
use visor_core::Micros;

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
    let result = copy_to(src, &partial, progress, cancel)
        .and_then(|()| fs::rename(&partial, dst).map_err(Into::into));
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
