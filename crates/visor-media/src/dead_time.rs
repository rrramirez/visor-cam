//! Finding dead time: stretches where nothing changes on screen and nothing
//! is heard, which make a demo drag. The editor offers to cut them, and
//! agents can ask for them.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::bail;

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
/// Screen frames are compared at most this often: a change of the
/// screen shows in its next frame anyway, and it's quicker.
const SAMPLE: Micros = 100_000;
/// A change that adds up to `CHANGED_PIXELS` began with its first changed
/// pixel, at most this long before: typing is busy from its first key, not
/// only every few keys.
const LOOKBACK: Micros = 1_500_000;
/// A smaller change of at least this many pixels (a box ticked) happened
/// if the screen then stays still this long. A clock ticking or a spinner
/// turning never does.
const SMALL_PIXELS: usize = 3;
const SETTLE: Micros = 1_500_000;
/// Sound is measured in windows this long.
const WINDOW: Micros = 50_000;
/// Sound counts once it lasts this many windows, so a lone click doesn't.
const MIN_SOUND_WINDOWS: usize = 2;
/// Sound is heard if its level is this many times its track's quietest (its
/// background noise), and at least `MIN_LEVEL`.
const ABOVE_NOISE: f32 = 4.0;
const MIN_LEVEL: f32 = 0.004;
/// Kept on each side of something happening, so cuts don't clip it.
const MARGIN: Micros = 300_000;
/// The shortest dead time worth cutting, unless asked otherwise.
pub const MIN_LEN: Micros = 2_000_000;

/// The stretches of the recording in `dir` at least `min_len` long where
/// the screen stays still and nothing is heard, leaving out what the edit
/// already cuts. Decodes the whole recording, so run it off the UI thread;
/// it fails soon after `cancel` is set.
pub fn find(
    dir: &Path,
    project: &Project,
    min_len: Micros,
    cancel: &AtomicBool,
) -> anyhow::Result<Vec<Range>> {
    let mut active = Vec::new();
    if let Some(track) = &project.tracks.screen {
        let size = track.width.zip(track.height);
        screen_changes(&dir.join(&track.file), size, cancel, &mut active)?;
    }
    let audio: Vec<_> = [&project.tracks.mic, &project.tracks.system]
        .into_iter()
        .flatten()
        .map(|t| dir.join(&t.file))
        .collect();
    sounds(&audio, project.duration_us, cancel, &mut active)?;
    Ok(quiet_stretches(
        active,
        project.duration_us,
        &project.edit,
        min_len,
    ))
}

fn check(cancel: &AtomicBool) -> anyhow::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("cancelled");
    }
    Ok(())
}

/// Adds the times the screen changed, against the last frame that did, so
/// slow changes add up too. `size` is the track's, if known.
fn screen_changes(
    path: &Path,
    size: Option<(u32, u32)>,
    cancel: &AtomicBool,
    active: &mut Vec<Range>,
) -> anyhow::Result<()> {
    let (w, h) = match size {
        Some(size) => size,
        None => VideoReader::open(path, None)?.size(),
    };
    let sw = SCAN_WIDTH.min(w.max(2)) & !1;
    let sh = ((u64::from(h) * u64::from(sw) / u64::from(w.max(1))) as u32).max(2) & !1;
    let mut reader = VideoReader::open(path, Some((format::Pixel::GRAY8, sw, sh)))?;
    let (sw, sh) = (sw as usize, sh as usize);
    let mut scan = ScreenScan::default();
    let mut pixels = vec![0u8; sw * sh];
    let mut next_sample = 0;
    while let Some((at, raw)) = reader.next_decoded()? {
        if at < next_sample {
            continue;
        }
        check(cancel)?;
        next_sample = at + SAMPLE;
        let frame = reader.convert(raw)?;
        let (data, stride) = (frame.data(0), frame.stride(0));
        for (row, out) in pixels.chunks_exact_mut(sw).enumerate() {
            out.copy_from_slice(&data[row * stride..row * stride + sw]);
        }
        scan.sample(at, &pixels, active);
    }
    Ok(())
}

/// Finds when the screen changed, from frames in order.
#[derive(Default)]
struct ScreenScan {
    /// The frame when the screen last changed.
    reference: Option<Vec<u8>>,
    /// The frame before.
    previous: Vec<u8>,
    /// When the screen first differed from the reference, and has since.
    first_change: Option<Micros>,
    /// When a small change was made, if the screen has kept still since.
    settling: Option<Micros>,
}

impl ScreenScan {
    fn sample(&mut self, at: Micros, pixels: &[u8], active: &mut Vec<Range>) {
        let Some(reference) = &self.reference else {
            // The first frame is where it starts, not something happening.
            self.reference = Some(pixels.to_vec());
            self.previous = pixels.to_vec();
            return;
        };
        let changed = differing(reference, pixels, CHANGED_PIXELS);
        let moved = differing(&self.previous, pixels, 1) > 0;
        self.previous.copy_from_slice(pixels);
        if changed == CHANGED_PIXELS {
            let from = self
                .first_change
                .unwrap_or(at)
                .max(at.saturating_sub(LOOKBACK));
            active.push(Range(from, at));
            self.reference = Some(pixels.to_vec());
            (self.first_change, self.settling) = (None, None);
        } else if changed == 0 {
            // Back as it was (a cursor blinking): nothing changed after all.
            (self.first_change, self.settling) = (None, None);
        } else {
            self.first_change.get_or_insert(at);
            if moved {
                self.settling = (changed >= SMALL_PIXELS).then_some(at);
            } else if let Some(since) = self.settling.filter(|&t| at - t >= SETTLE) {
                active.push(Range(since, since));
                self.settling = None;
            }
        }
    }
}

/// How many pixels differ between two frames, counting up to `max`.
fn differing(a: &[u8], b: &[u8], max: usize) -> usize {
    a.iter()
        .zip(b)
        .filter(|(a, b)| a.abs_diff(**b) > PIXEL_CHANGE)
        .take(max)
        .count()
}

/// Adds the stretches where any of the audio tracks is heard, each above
/// its own background noise: music playing doesn't drown out talking.
fn sounds(
    paths: &[impl AsRef<Path>],
    duration: Micros,
    cancel: &AtomicBool,
    active: &mut Vec<Range>,
) -> anyhow::Result<()> {
    let windows = (duration / WINDOW) as usize + 1;
    let mut heard = vec![false; windows];
    for path in paths {
        let mut sums = vec![(0f64, 0u32); windows];
        let mut reader = AudioReader::open(path.as_ref())?;
        while let Some((at, samples)) = reader.next_chunk()? {
            check(cancel)?;
            for (i, pair) in samples.as_chunks::<2>().0.iter().enumerate() {
                let t = at + i as u64 * 1_000_000 / 48_000;
                let w = ((t / WINDOW) as usize).min(windows - 1);
                let s = f64::from((pair[0] + pair[1]) / 2.0);
                sums[w].0 += s * s;
                sums[w].1 += 1;
            }
        }
        let levels: Vec<f32> = sums
            .into_iter()
            .map(|(sum, n)| (sum / f64::from(n.max(1))).sqrt() as f32)
            .collect();
        add_heard(&levels, &mut heard);
    }
    let mut run_start = None;
    for (i, &heard) in heard.iter().chain([&false]).enumerate() {
        match (heard, run_start) {
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

/// Marks where a track with these `levels` is heard, above its own noise.
fn add_heard(levels: &[f32], heard: &mut [bool]) {
    let threshold = loudness_threshold(levels);
    for (heard, &level) in heard.iter_mut().zip(levels) {
        *heard |= level > threshold;
    }
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
    let mut quiet_from = 0;
    for r in &active {
        let to = r.0.saturating_sub(MARGIN);
        if to > quiet_from {
            gaps.push(Range(quiet_from, to));
        }
        quiet_from = quiet_from.max(r.1 + MARGIN);
    }
    if quiet_from < duration {
        gaps.push(Range(quiet_from, duration));
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

    #[test]
    fn talking_is_heard_over_louder_music() {
        let mut heard = vec![false; 100];
        // Music playing throughout, then talking, quieter, now and then.
        add_heard(&[0.1; 100], &mut heard);
        let talk: Vec<f32> = (0..100)
            .map(|i| if i % 10 < 3 { 0.03 } else { 0.001 })
            .collect();
        add_heard(&talk, &mut heard);
        let expected: Vec<bool> = (0..100).map(|i| i % 10 < 3).collect();
        assert_eq!(heard, expected);
    }

    /// Feeds `scan` a frame every 100 ms from `from` to `to` seconds,
    /// made by `frame` from the time.
    fn frames(
        scan: &mut ScreenScan,
        from: f64,
        to: f64,
        frame: impl Fn(f64) -> Vec<u8>,
        active: &mut Vec<Range>,
    ) {
        for tenth in (from * 10.0) as u64..(to * 10.0) as u64 {
            scan.sample(tenth * 100_000, &frame(tenth as f64 / 10.0), active);
        }
    }

    fn screen(lit: usize) -> Vec<u8> {
        let mut pixels = vec![0; 1000];
        pixels[..lit].fill(255);
        pixels
    }

    #[test]
    fn a_box_ticked_counts_a_clock_ticking_doesnt() {
        // Still, then a box ticked at 2 s (5 pixels), nothing after.
        let (mut scan, mut active) = (ScreenScan::default(), Vec::new());
        frames(
            &mut scan,
            0.0,
            6.0,
            |t| screen(if t < 2.0 { 0 } else { 5 }),
            &mut active,
        );
        assert_eq!(active, [Range(2 * S, 2 * S)]);

        // A clock ticking every second (5 pixels, back and forth), then
        // something happening at 6 s: only that counts, from the clock's
        // last tick at the most.
        let (mut scan, mut active) = (ScreenScan::default(), Vec::new());
        let clock = |t: f64| {
            let mut pixels = screen(if t < 6.0 { 0 } else { 100 });
            pixels[500 + (t as usize % 2) * 5..][..5].fill(255);
            pixels
        };
        frames(&mut scan, 0.0, 8.0, clock, &mut active);
        assert_eq!(active, [Range(5 * S, 6 * S)]);
    }
}
