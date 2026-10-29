//! Finding dead time: stretches where nothing changes on screen and nothing
//! is heard, which make a demo drag. The editor offers to cut them, and
//! agents can ask for them.

use std::path::Path;

use ffmpeg_next::format;
use visor_core::{EditList, Micros, Project, Range};

use crate::decode::{AudioReader, VideoReader};

/// Screen frames are compared this wide, in grey: enough to see typing or
/// the pointer move, little enough to be quick.
const SCAN_WIDTH: u32 = 640;
/// A pixel changed if its grey level moved by more than this (out of 255),
/// past compression noise.
const PIXEL_CHANGE: u8 = 24;
/// The screen changed if this many pixels did since it last changed: a
/// few typed characters, or the pointer moving (where it was, and where it
/// is). A spinner turning or a clock ticking in place differs from the
/// last change by its few pixels only, however long it goes on.
const CHANGED_PIXELS: usize = 32;
/// A change that adds up to `CHANGED_PIXELS` began with its first changed
/// pixel, at most this long before: typing is busy from its first key, not
/// only every few keys.
const LOOKBACK: Micros = 1_500_000;
/// Sound is measured in windows this long.
const WINDOW: Micros = 50_000;
/// Sound counts once it lasts this many windows, so a lone click doesn't.
const MIN_SOUND_WINDOWS: usize = 2;
/// Sound is heard if its level is this many times the recording's quietest
/// (its background noise), and at least `MIN_LEVEL`.
const ABOVE_NOISE: f32 = 4.0;
const MIN_LEVEL: f32 = 0.004;
/// Kept on each side of something happening, so cuts don't clip it.
const MARGIN: Micros = 300_000;
/// The shortest dead time worth cutting, unless asked otherwise.
pub const MIN_LEN: Micros = 2_000_000;

/// The stretches of the recording in `dir` at least `min_len` long where
/// the screen stays still and nothing is heard, leaving out what the edit
/// already cuts. Decodes the whole recording, so run it off the UI thread.
pub fn find(dir: &Path, project: &Project, min_len: Micros) -> anyhow::Result<Vec<Range>> {
    let mut active = Vec::new();
    if let Some(track) = &project.tracks.screen {
        screen_changes(&dir.join(&track.file), &mut active)?;
    }
    let audio: Vec<_> = [&project.tracks.mic, &project.tracks.system]
        .into_iter()
        .flatten()
        .map(|t| dir.join(&t.file))
        .collect();
    sounds(&audio, project.duration_us, &mut active)?;
    Ok(quiet_stretches(
        active,
        project.duration_us,
        &project.edit,
        min_len,
    ))
}

/// Adds the times the screen changed, against the last frame that did, so
/// slow changes add up too.
fn screen_changes(path: &Path, active: &mut Vec<Range>) -> anyhow::Result<()> {
    let (w, h) = VideoReader::open(path, None)?.size();
    let sw = SCAN_WIDTH.min(w.max(2)) & !1;
    let sh = ((u64::from(h) * u64::from(sw) / u64::from(w.max(1))) as u32).max(2) & !1;
    let mut reader = VideoReader::open(path, Some((format::Pixel::GRAY8, sw, sh)))?;
    let (sw, sh) = (sw as usize, sh as usize);
    let mut reference: Option<Vec<u8>> = None;
    // When the first pixel changed since the reference.
    let mut first_change = None;
    let mut pixels = vec![0u8; sw * sh];
    while let Some((at, frame)) = reader.next_frame()? {
        let (data, stride) = (frame.data(0), frame.stride(0));
        for (row, out) in pixels.chunks_exact_mut(sw).enumerate() {
            out.copy_from_slice(&data[row * stride..row * stride + sw]);
        }
        let Some(r) = &reference else {
            // The first frame is where it starts, not something happening.
            reference = Some(pixels.clone());
            continue;
        };
        let changed = r
            .iter()
            .zip(&pixels)
            .filter(|(a, b)| a.abs_diff(**b) > PIXEL_CHANGE)
            .take(CHANGED_PIXELS)
            .count();
        if changed > 0 && first_change.is_none() {
            first_change = Some(at);
        }
        if changed == CHANGED_PIXELS {
            let from = first_change.unwrap_or(at).max(at.saturating_sub(LOOKBACK));
            active.push(Range(from, at));
            reference = Some(pixels.clone());
            first_change = None;
        }
    }
    Ok(())
}

/// Adds the stretches where any of the audio tracks is heard.
fn sounds(
    paths: &[impl AsRef<Path>],
    duration: Micros,
    active: &mut Vec<Range>,
) -> anyhow::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    let windows = (duration / WINDOW) as usize + 1;
    // The loudest track's level in each window.
    let mut levels = vec![0f32; windows];
    for path in paths {
        let mut sums = vec![(0f64, 0u32); windows];
        let mut reader = AudioReader::open(path.as_ref())?;
        while let Some((at, samples)) = reader.next_chunk()? {
            for (i, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
                let t = at + i as u64 * 1_000_000 / 48_000;
                let w = ((t / WINDOW) as usize).min(windows - 1);
                let s = f64::from((pair[0] + pair[1]) / 2.0);
                sums[w].0 += s * s;
                sums[w].1 += 1;
            }
        }
        for (level, (sum, n)) in levels.iter_mut().zip(sums) {
            if n > 0 {
                *level = level.max((sum / f64::from(n)).sqrt() as f32);
            }
        }
    }
    let threshold = loudness_threshold(&levels);
    let mut run_start = None;
    for (i, &level) in levels.iter().chain([&0.0]).enumerate() {
        match (level > threshold, run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(start)) => {
                if i - start >= MIN_SOUND_WINDOWS {
                    active.push(Range(start as u64 * WINDOW, i as u64 * WINDOW));
                }
                run_start = None;
            }
            _ => {}
        }
    }
    Ok(())
}

/// The level above which something is heard: well over the background
/// noise, taken as the level of the quietest tenth of the recording.
fn loudness_threshold(levels: &[f32]) -> f32 {
    let mut sorted = levels.to_vec();
    sorted.sort_by(f32::total_cmp);
    let noise = sorted.get(sorted.len() / 10).copied().unwrap_or(0.0);
    (noise * ABOVE_NOISE).max(MIN_LEVEL)
}

/// The gaps between `active` stretches, less a margin by each, within
/// what `edit` keeps and at least `min_len` long.
fn quiet_stretches(
    mut active: Vec<Range>,
    duration: Micros,
    edit: &EditList,
    min_len: Micros,
) -> Vec<Range> {
    active.sort_by_key(|r| r.0);
    let mut gaps = Vec::new();
    // The start of the recording, before anything happens, can go whole.
    let mut quiet_from = Some(0);
    for r in &active {
        if let Some(from) = quiet_from {
            let to = r.0.saturating_sub(MARGIN);
            if to > from {
                gaps.push(Range(from, to));
            }
        }
        let from = r.1 + MARGIN;
        quiet_from = Some(quiet_from.map_or(from, |f| f.max(from)));
    }
    if let Some(from) = quiet_from.filter(|&f| f < duration) {
        gaps.push(Range(from, duration));
    }
    let mut out = Vec::new();
    for gap in gaps {
        for kept in edit.ranges() {
            let piece = Range(gap.0.max(kept.0), gap.1.min(kept.1));
            if piece.1 > piece.0 && piece.len() >= min_len {
                out.push(piece);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Micros = 1_000_000;

    #[test]
    fn gaps_between_activity_less_margins() {
        let edit = EditList::full(20 * S);
        // Busy 1–2 s, a click at 8 s, talking 9–10 s.
        let active = vec![Range(9 * S, 10 * S), Range(S, 2 * S), Range(8 * S, 8 * S)];
        assert_eq!(
            quiet_stretches(active, 20 * S, &edit, 2 * S),
            [
                // 2.3–7.7 s, and the end; not the 0.7 s before 1 s, nor
                // the 0.4 s between the click and the talking.
                Range(2 * S + MARGIN, 8 * S - MARGIN),
                Range(10 * S + MARGIN, 20 * S),
            ]
        );
    }

    #[test]
    fn the_start_and_what_is_cut_already() {
        let mut edit = EditList::full(20 * S);
        edit.cut(4 * S, 6 * S);
        let active = vec![Range(10 * S, 10 * S)];
        // Quiet until 9.7 s, less the cut 4–6 s, and after 10.3 s.
        assert_eq!(
            quiet_stretches(active, 20 * S, &edit, 3 * S),
            [
                Range(0, 4 * S),
                Range(6 * S, 10 * S - MARGIN),
                Range(10 * S + MARGIN, 20 * S),
            ]
        );
        // Nothing happens at all: all of it, around the cut.
        assert_eq!(
            quiet_stretches(Vec::new(), 20 * S, &edit, 3 * S),
            [Range(0, 4 * S), Range(6 * S, 20 * S)]
        );
    }

    #[test]
    fn heard_means_well_above_the_noise() {
        let mut levels = vec![0.002; 90];
        levels.extend([0.05; 10]);
        let threshold = loudness_threshold(&levels);
        assert!(threshold > 0.002 && threshold < 0.05);
        // Digital silence: the floor still applies.
        assert_eq!(loudness_threshold(&[0.0; 10]), MIN_LEVEL);
    }
}
