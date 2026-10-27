//! Edit-aware building blocks shared by export and the preview player.

use std::collections::VecDeque;

use ffmpeg_next::frame;
use visor_core::{EditList, Micros};

use crate::audio_writer::{CHANNELS, SAMPLE_RATE};
use crate::decode::{AudioReader, VideoReader};

/// Source time to decode through before seeking instead.
const SEEK_THRESHOLD: Micros = 2_000_000;
const AUDIO_SEEK_THRESHOLD: i64 = 2 * SAMPLE_RATE as i64;

pub(crate) fn micros_to_samples(us: Micros) -> i64 {
    (u128::from(us) * u128::from(SAMPLE_RATE) / 1_000_000) as i64
}

/// Maps output sample positions to source sample positions for an edit list.
pub(crate) struct SampleMap {
    /// (output start, source start, length) in samples.
    ranges: Vec<(i64, i64, i64)>,
    pub total: i64,
}

impl SampleMap {
    pub fn new(edit: &EditList) -> Self {
        let mut ranges = Vec::new();
        let mut out = 0;
        for r in edit.ranges() {
            let (a, b) = (micros_to_samples(r.0), micros_to_samples(r.1));
            if b > a {
                ranges.push((out, a, b - a));
                out += b - a;
            }
        }
        Self { ranges, total: out }
    }

    /// Source pieces `(source start, length)` covering output samples
    /// `[start, start + len)`.
    pub fn pieces(&self, start: i64, len: i64) -> Vec<(i64, usize)> {
        let end = start + len;
        self.ranges
            .iter()
            .filter_map(|&(o, s, n)| {
                let (a, b) = (start.max(o), end.min(o + n));
                (b > a).then(|| (s + (a - o), (b - a) as usize))
            })
            .collect()
    }
}

/// Holds the latest decoded frame at or before a requested time, seeking
/// when a request jumps backwards or far ahead. Frames are kept as decoded;
/// only the one returned is converted, so frames skipped on the way to a
/// target cost a decode but no scaling.
pub(crate) struct FrameCursor {
    reader: VideoReader,
    current: Option<(Micros, frame::Video)>,
    upcoming: Option<(Micros, frame::Video)>,
    ended: bool,
    fresh: bool,
    /// `current` converted to the reader's target; reused across frames.
    out: frame::Video,
    out_pts: Option<Micros>,
}

impl FrameCursor {
    pub fn new(reader: VideoReader) -> Self {
        Self {
            reader,
            current: None,
            upcoming: None,
            ended: false,
            fresh: true,
            out: frame::Video::empty(),
            out_pts: None,
        }
    }

    /// Rewinds to the start for another pass.
    pub fn reset(&mut self) -> anyhow::Result<()> {
        self.reader.seek(0)?;
        self.current = None;
        self.upcoming = None;
        self.ended = false;
        self.fresh = true;
        Ok(())
    }

    /// The frame shown at `t` and its source time. The time only changes
    /// when the frame does.
    pub fn at(&mut self, t: Micros) -> anyhow::Result<Option<(Micros, &mut frame::Video)>> {
        let behind = self.current.as_ref().is_some_and(|(pts, _)| t < *pts);
        let far = self
            .current
            .as_ref()
            .is_some_and(|(pts, _)| t > pts + SEEK_THRESHOLD)
            || (self.fresh && t > SEEK_THRESHOLD);
        if behind || far {
            self.reader.seek(t)?;
            self.current = None;
            self.upcoming = None;
            self.ended = false;
        }
        self.fresh = false;
        loop {
            if self.upcoming.is_none() && !self.ended {
                self.upcoming = self.reader.next_decoded()?;
                self.ended = self.upcoming.is_none();
            }
            match &self.upcoming {
                Some((pts, _)) if *pts <= t || self.current.is_none() => {
                    let first_after = *pts > t;
                    self.current = self.upcoming.take();
                    if first_after {
                        break;
                    }
                }
                _ => break,
            }
        }
        let Some((pts, raw)) = &self.current else {
            return Ok(None);
        };
        if self.out_pts != Some(*pts) {
            self.out_pts = None;
            self.reader.convert_into(raw, &mut self.out)?;
            self.out_pts = Some(*pts);
        }
        Ok(Some((*pts, &mut self.out)))
    }
}

/// Mixes several 48 kHz stereo tracks, addressed by source sample position.
pub(crate) struct AudioMix {
    tracks: Vec<MixTrack>,
}

struct MixTrack {
    reader: AudioReader,
    /// Interleaved stereo samples starting at `start` (in sample frames).
    buffer: VecDeque<f32>,
    start: i64,
    ended: bool,
    primed: bool,
}

impl AudioMix {
    pub fn new(readers: Vec<AudioReader>) -> Self {
        let tracks = readers
            .into_iter()
            .map(|reader| MixTrack {
                reader,
                buffer: VecDeque::new(),
                start: 0,
                ended: false,
                primed: false,
            })
            .collect();
        Self { tracks }
    }

    /// `len` mixed stereo frames starting at source sample `at`.
    pub fn read(&mut self, at: i64, len: usize) -> anyhow::Result<Vec<f32>> {
        let ch = usize::from(CHANNELS);
        let mut mixed = vec![0f32; len * ch];
        for t in &mut self.tracks {
            let end_of_buffer = t.start + (t.buffer.len() / ch) as i64;
            if t.primed && (at < t.start || at > end_of_buffer + AUDIO_SEEK_THRESHOLD) {
                t.reader
                    .seek((at as u128 * 1_000_000 / u128::from(SAMPLE_RATE)) as Micros)?;
                t.buffer.clear();
                t.primed = false;
                t.ended = false;
            }
            // Fill until the buffer covers [at, at + len).
            while !t.ended
                && (!t.primed || t.start + ((t.buffer.len() / ch) as i64) < at + len as i64)
            {
                match t.reader.next_chunk()? {
                    Some((pts, chunk)) => {
                        let pos = micros_to_samples(pts);
                        if !t.primed {
                            t.start = pos;
                            t.primed = true;
                        } else {
                            let expected = t.start + (t.buffer.len() / ch) as i64;
                            if pos > expected + 480 {
                                t.buffer.extend(std::iter::repeat_n(
                                    0.0,
                                    (pos - expected) as usize * ch,
                                ));
                            }
                        }
                        t.buffer.extend(chunk);
                    }
                    None => t.ended = true,
                }
            }
            // Drop samples before `at`.
            if at > t.start {
                let drop = ((at - t.start) as usize * ch).min(t.buffer.len());
                t.buffer.drain(..drop);
                t.start += (drop / ch) as i64;
            }
            let offset = (t.start - at).max(0) as usize * ch;
            for (dst, src) in mixed.iter_mut().skip(offset).zip(t.buffer.iter()) {
                *dst += *src;
            }
        }
        for s in &mut mixed {
            *s = s.clamp(-1.0, 1.0);
        }
        Ok(mixed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_map_spans_cuts() {
        let mut edit = EditList::full(1_000_000); // 48_000 samples
        edit.cut(250_000, 500_000);
        let map = SampleMap::new(&edit);
        assert_eq!(map.total, 36_000);
        assert_eq!(
            map.pieces(11_000, 2_000),
            vec![(11_000, 1_000), (24_000, 1_000)]
        );
        assert_eq!(map.pieces(35_000, 5_000), vec![(47_000, 1_000)]);
    }
}
