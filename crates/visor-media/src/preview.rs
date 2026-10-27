//! Timeline thumbnails and waveform data for the editor.

use std::path::Path;

use ffmpeg_next::format;
use visor_core::Micros;

use crate::decode::{AudioReader, VideoReader};
use crate::player::PreviewFrame;
use crate::timeline::FrameCursor;

/// `count` evenly spaced thumbnails `height` pixels tall, with their source
/// times. Slow-ish (decodes from the previous keyframe, or straight through
/// when thumbnails are close together), so run it off the UI thread.
pub fn thumbnails(
    path: &Path,
    duration: Micros,
    count: usize,
    height: u32,
) -> anyhow::Result<Vec<(Micros, PreviewFrame)>> {
    let (w, h) = VideoReader::open(path, None)?.size();
    let th = height.max(2) & !1;
    let tw = ((u64::from(w) * u64::from(th) / u64::from(h.max(1))) as u32 + 1) & !1;
    let mut cursor = FrameCursor::new(VideoReader::open(
        path,
        Some((format::Pixel::RGBA, tw, th)),
    )?);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let t = duration * (2 * i as u64 + 1) / (2 * count.max(1) as u64);
        if let Some((pts, f)) = cursor.at(t)? {
            out.push((pts, PreviewFrame::from_frame(f)));
        }
    }
    Ok(out)
}

/// Peak level (0.0–1.0) per bucket across all `paths`, over `duration` of
/// source time.
pub fn waveform(paths: &[&Path], duration: Micros, buckets: usize) -> anyhow::Result<Vec<f32>> {
    let mut peaks = vec![0f32; buckets];
    if buckets == 0 || duration == 0 {
        return Ok(peaks);
    }
    for path in paths {
        let mut reader = AudioReader::open(path)?;
        while let Some((at, samples)) = reader.next_chunk()? {
            for (i, pair) in samples.chunks_exact(2).enumerate() {
                let t = at + (i as u64 * 1_000_000 / 48_000);
                let b = ((u128::from(t) * buckets as u128 / u128::from(duration)) as usize)
                    .min(buckets - 1);
                peaks[b] = peaks[b].max(pair[0].abs().max(pair[1].abs()));
            }
        }
    }
    Ok(peaks)
}
