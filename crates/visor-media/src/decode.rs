//! Track readers used by the player, thumbnails and export. Timestamps are
//! source-time microseconds.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use ffmpeg_next::{
    self as ffmpeg, Rational, format, format::sample::Type, frame, media, software::scaling,
};
use visor_core::{Micros, Part};

use crate::Resampler;
use crate::audio_writer::{CHANNELS, SAMPLE_RATE};
use crate::scale::Scaler;

/// Opens a track for reading. Track files come from project folders that may
/// not have been made here, so FFmpeg may only read them as local MP4 or
/// Matroska: no playlists or other demuxers that pull in further files.
pub(crate) fn open_input(path: &Path) -> anyhow::Result<format::context::Input> {
    let mut options = ffmpeg::Dictionary::new();
    options.set("format_whitelist", "mov,matroska");
    options.set("protocol_whitelist", "file");
    format::input_with_dictionary(path, options)
        .with_context(|| format!("opening {}", path.display()))
}

/// Prepares `f` to be completely overwritten with a `fmt` `w` x `h` image:
/// keeps its buffer if it fits and nothing else (an encoder, a filter, a
/// frame in flight) still holds it, else gives it a new one.
pub(crate) fn reuse_or_alloc(f: &mut frame::Video, fmt: format::Pixel, w: u32, h: u32) {
    // An empty frame has no format and zero size.
    let fits = f.format() == fmt && f.width() == w && f.height() == h;
    // SAFETY: `f` is a valid frame (allocated, since it fits).
    if !fits || unsafe { ffmpeg::ffi::av_frame_is_writable(f.as_mut_ptr()) } == 0 {
        *f = frame::Video::new(fmt, w, h);
    }
}

/// A new reference to `f`'s buffers, for APIs that take ownership of the
/// frame they are given (like a filter graph's buffer source).
pub(crate) fn new_ref(f: &frame::Video) -> anyhow::Result<frame::Video> {
    let mut r = frame::Video::empty();
    // SAFETY: both frames are valid; `r` is empty.
    match unsafe { ffmpeg::ffi::av_frame_ref(r.as_mut_ptr(), f.as_ptr()) } {
        0 => Ok(r),
        e => Err(ffmpeg::Error::from(e).into()),
    }
}

fn to_micros(ts: i64, tb: Rational) -> Micros {
    (i128::from(ts.max(0)) * i128::from(tb.0) * 1_000_000 / i128::from(tb.1.max(1))) as Micros
}

fn from_micros(us: Micros, tb: Rational) -> i64 {
    (i128::from(us) * i128::from(tb.1) / (i128::from(tb.0.max(1)) * 1_000_000)) as i64
}

/// A stretch of a track's source time and the file covering it (none: the
/// track wasn't recorded then).
struct Span {
    start: Micros,
    /// Frames from `start` on that are past it belong to the next span;
    /// the last span has no end.
    end: Micros,
    path: Option<PathBuf>,
}

/// Spans from a track's parts. The last one is open ended, so a frame that
/// runs a little past the recorded length still shows.
fn spans(dir: &Path, parts: &[Part]) -> Vec<Span> {
    let mut spans: Vec<Span> = parts
        .iter()
        .map(|p| Span {
            start: p.start,
            end: p.end,
            path: p.file.as_ref().map(|f| dir.join(f)),
        })
        .collect();
    if let Some(last) = spans.last_mut() {
        last.end = Micros::MAX;
    }
    spans
}

/// The span holding source time `at` (the first if before all).
fn span_at(spans: &[Span], at: Micros) -> usize {
    spans.iter().rposition(|s| s.start <= at).unwrap_or(0)
}

/// One file's video stream.
struct VideoFile {
    input: format::context::Input,
    decoder: ffmpeg::decoder::Video,
    index: usize,
    time_base: Rational,
    decoded: frame::Video,
    eof: bool,
}

impl VideoFile {
    fn open(path: &Path) -> anyhow::Result<Self> {
        ffmpeg::init()?;
        let input = open_input(path)?;
        let stream = input
            .streams()
            .best(media::Type::Video)
            .context("no video stream")?;
        let (index, time_base) = (stream.index(), stream.time_base());
        let mut ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?;
        ctx.set_threading(ffmpeg::threading::Config {
            kind: ffmpeg::threading::Type::Frame,
            count: 0,
        });
        let decoder = ctx.decoder().video()?;
        Ok(Self {
            input,
            decoder,
            index,
            time_base,
            decoded: frame::Video::empty(),
            eof: false,
        })
    }

    fn duration(&self) -> Micros {
        to_micros(
            self.input.duration(),
            Rational(1, ffmpeg::ffi::AV_TIME_BASE),
        )
    }

    fn seek(&mut self, at: Micros) -> anyhow::Result<()> {
        let ts = from_micros(at, Rational(1, ffmpeg::ffi::AV_TIME_BASE));
        self.input.seek(ts, ..ts + 1)?;
        self.decoder.flush();
        self.eof = false;
        Ok(())
    }

    fn next_decoded(&mut self) -> anyhow::Result<Option<(Micros, frame::Video)>> {
        loop {
            if self.decoder.receive_frame(&mut self.decoded).is_ok() {
                let pts = self.decoded.timestamp().or(self.decoded.pts()).unwrap_or(0);
                let at = to_micros(pts, self.time_base);
                let raw = std::mem::replace(&mut self.decoded, frame::Video::empty());
                return Ok(Some((at, raw)));
            }
            if self.eof {
                return Ok(None);
            }
            match self.input.packets().find(|(s, _)| s.index() == self.index) {
                Some((_, packet)) => self.decoder.send_packet(&packet)?,
                None => {
                    self.eof = true;
                    self.decoder.send_eof()?;
                }
            }
        }
    }
}

/// Decodes a video track, optionally converting every frame to a fixed
/// pixel format and size. A track recorded in pieces (takes) reads as one,
/// in source time; a piece of another shape is fitted inside the target
/// with black bars.
pub struct VideoReader {
    spans: Vec<Span>,
    /// The span being read.
    pos: usize,
    /// The open file and the span it belongs to.
    file: Option<(usize, VideoFile)>,
    size: (u32, u32),
    duration: Micros,
    target: Option<(format::Pixel, u32, u32)>,
    /// The scaler and the size it scales to.
    scaler: Option<(Scaler, (u32, u32))>,
    scale_flags: scaling::Flags,
    scale_threads: usize,
    /// A frame scaled to fit, before its bars are added.
    fitted: frame::Video,
}

// SAFETY: the reader exclusively owns its FFmpeg objects, none of which has
// thread affinity; it is used from one thread at a time.
unsafe impl Send for VideoReader {}

impl VideoReader {
    /// `target`: convert to this format and size (`None` = decoder output).
    pub fn open(path: &Path, target: Option<(format::Pixel, u32, u32)>) -> anyhow::Result<Self> {
        let file = VideoFile::open(path)?;
        let duration = file.duration();
        let spans = vec![Span {
            start: 0,
            end: Micros::MAX,
            path: Some(path.to_path_buf()),
        }];
        Ok(Self::new(spans, 0, file, duration, target))
    }

    /// A track over its `parts` (files in `dir`), read as one. Its size is
    /// the first file's.
    pub fn open_parts(
        dir: &Path,
        parts: &[Part],
        target: Option<(format::Pixel, u32, u32)>,
    ) -> anyhow::Result<Self> {
        let duration = parts.last().map_or(0, |p| p.end);
        let spans = spans(dir, parts);
        let first = spans
            .iter()
            .position(|s| s.path.is_some())
            .context("no file for the track")?;
        let file = VideoFile::open(spans[first].path.as_ref().unwrap())?;
        Ok(Self::new(spans, first, file, duration, target))
    }

    fn new(
        spans: Vec<Span>,
        first: usize,
        file: VideoFile,
        duration: Micros,
        target: Option<(format::Pixel, u32, u32)>,
    ) -> Self {
        let size = (file.decoder.width(), file.decoder.height());
        Self {
            spans,
            pos: first,
            file: Some((first, file)),
            size,
            duration,
            target,
            scaler: None,
            scale_flags: scaling::Flags::BILINEAR,
            scale_threads: 0,
            fitted: frame::Video::empty(),
        }
    }

    /// How frames are converted to the target: the filter, and threads
    /// (0 = one per core). Defaults to bilinear on every core, for export;
    /// live preview trades a little of both for less CPU.
    pub(crate) fn set_scaling(&mut self, flags: scaling::Flags, threads: usize) {
        (self.scale_flags, self.scale_threads) = (flags, threads);
        self.scaler = None;
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Track duration: the container's, or for parts where the last ends.
    pub fn duration(&self) -> Micros {
        self.duration
    }

    /// Whether the track has frames at source time `t`: not in a piece
    /// recorded without it.
    pub fn shows(&self, t: Micros) -> bool {
        self.spans[span_at(&self.spans, t)].path.is_some()
    }

    /// Which piece source time `t` is in.
    pub(crate) fn piece(&self, t: Micros) -> usize {
        span_at(&self.spans, t)
    }

    /// Positions the reader at the keyframe at or before `at`. Following
    /// [`next_frame`](Self::next_frame) calls start there; skip frames before `at` to land
    /// exactly. In a piece without the track, at the next piece with it.
    pub fn seek(&mut self, at: Micros) -> anyhow::Result<()> {
        let i = span_at(&self.spans, at);
        let Some(j) = (i..self.spans.len()).find(|&j| self.spans[j].path.is_some()) else {
            self.pos = self.spans.len();
            return Ok(());
        };
        let local = if j == i { at - self.spans[j].start } else { 0 };
        self.pos = j;
        self.open_pos()?.seek(local)
    }

    /// The file of the span being read, opened from its start if it isn't.
    fn open_pos(&mut self) -> anyhow::Result<&mut VideoFile> {
        if self.file.as_ref().is_none_or(|(i, _)| *i != self.pos) {
            let path = self.spans[self.pos].path.as_ref().unwrap();
            self.file = Some((self.pos, VideoFile::open(path)?));
        }
        Ok(&mut self.file.as_mut().unwrap().1)
    }

    /// The next decoded frame and its source time, or `None` at the end.
    pub fn next_frame(&mut self) -> anyhow::Result<Option<(Micros, frame::Video)>> {
        match self.next_decoded()? {
            Some((at, raw)) => Ok(Some((at, self.convert(raw)?))),
            None => Ok(None),
        }
    }

    /// The next frame as the decoder produced it (not converted to the
    /// target), so callers can skip frames without paying for scaling.
    pub(crate) fn next_decoded(&mut self) -> anyhow::Result<Option<(Micros, frame::Video)>> {
        while self.pos < self.spans.len() {
            if self.spans[self.pos].path.is_some() {
                let Span { start, end, .. } = self.spans[self.pos];
                if let Some((t, raw)) = self.open_pos()?.next_decoded()? {
                    let at = start.saturating_add(t);
                    if at < end {
                        return Ok(Some((at, raw)));
                    }
                }
            }
            // On to the next piece, from its start.
            self.pos += 1;
            self.file = None;
        }
        Ok(None)
    }

    /// Decodes forward to the last frame at or before `at` (or the first
    /// frame after it if none). Call after [`seek`](Self::seek).
    pub fn frame_at(&mut self, at: Micros) -> anyhow::Result<Option<(Micros, frame::Video)>> {
        let mut best = None;
        while let Some((pts, f)) = self.next_decoded()? {
            if pts > at && best.is_some() {
                break;
            }
            let past = pts > at;
            best = Some((pts, f));
            if past {
                break;
            }
        }
        match best {
            Some((pts, raw)) => Ok(Some((pts, self.convert(raw)?))),
            None => Ok(None),
        }
    }

    pub(crate) fn convert(&mut self, raw: frame::Video) -> anyhow::Result<frame::Video> {
        if self.target.is_none() {
            return Ok(raw);
        }
        let mut out = frame::Video::empty();
        self.convert_into(&raw, &mut out)?;
        Ok(out)
    }

    /// Converts a frame from [`next_decoded`](Self::next_decoded) to the
    /// target into `out`, reusing its buffer when nothing else holds it.
    pub(crate) fn convert_into(
        &mut self,
        raw: &frame::Video,
        out: &mut frame::Video,
    ) -> anyhow::Result<()> {
        let Some((fmt, w, h)) = self.target else {
            *out = raw.clone();
            return Ok(());
        };
        let d = raw;
        let input = (d.format(), d.width(), d.height());
        let fit = match bars(fmt) {
            Some(_) => fit_inside((d.width(), d.height()), (w, h)),
            None => (w, h),
        };
        if self
            .scaler
            .as_ref()
            .is_none_or(|(s, to)| s.input() != input || *to != fit)
        {
            let scaler = Scaler::new(
                input,
                (fmt, fit.0, fit.1),
                self.scale_flags,
                self.scale_threads,
            )?;
            self.scaler = Some((scaler, fit));
        }
        let scaler = &mut self.scaler.as_mut().unwrap().0;
        if fit == (w, h) {
            reuse_or_alloc(out, fmt, w, h);
            scaler.run(d, out)?;
        } else {
            reuse_or_alloc(&mut self.fitted, fmt, fit.0, fit.1);
            scaler.run(d, &mut self.fitted)?;
            reuse_or_alloc(out, fmt, w, h);
            letterbox(&self.fitted, out);
        }
        out.set_pts(d.pts());
        Ok(())
    }
}

/// How to paint black in each plane of `fmt`: bytes per pixel, chroma
/// subsampling (as a shift) and the bytes of a black pixel. `None` for
/// formats Visor doesn't convert to, which are stretched instead.
fn bars(fmt: format::Pixel) -> Option<&'static [(usize, u32, &'static [u8])]> {
    match fmt {
        format::Pixel::RGBA => Some(&[(4, 0, &[0, 0, 0, 255])]),
        format::Pixel::YUV420P => Some(&[(1, 0, &[16]), (1, 1, &[128]), (1, 1, &[128])]),
        format::Pixel::GRAY8 => Some(&[(1, 0, &[0])]),
        _ => None,
    }
}

/// The even size that a `src` frame scales to inside `into` keeping its
/// shape; `into` itself when the shapes are within 1%.
fn fit_inside(src: (u32, u32), into: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (u64::from(src.0), u64::from(src.1.max(1)));
    let (w, h) = (u64::from(into.0), u64::from(into.1));
    let (a, b) = (sw * h, w * sh);
    if a.abs_diff(b) * 100 <= b {
        return into;
    }
    let even = |v: u64| ((v as u32) & !1).max(2);
    if a < b {
        (even(a / sh).min(into.0), into.1)
    } else {
        (into.0, even(sh * w / sw.max(1)).min(into.1))
    }
}

/// Paints `out` black and puts `src` (no larger) in its middle.
fn letterbox(src: &frame::Video, out: &mut frame::Video) {
    let planes = bars(out.format()).expect("a format with bars");
    let (w, h) = (out.width(), out.height());
    let (x, y) = (((w - src.width()) / 2) & !1, ((h - src.height()) / 2) & !1);
    for (p, &(bpp, shift, black)) in planes.iter().enumerate() {
        let sub = |v: u32| ((v + (1 << shift) - 1) >> shift) as usize;
        let (pw, ph) = (sub(w), sub(h));
        let (sw, sh) = (sub(src.width()), sub(src.height()));
        let (ox, oy) = ((x >> shift) as usize, (y >> shift) as usize);
        let (stride, sstride) = (out.stride(p), src.stride(p));
        let from = src.data(p);
        let to = out.data_mut(p);
        let paint = |row: &mut [u8]| {
            for px in row.chunks_exact_mut(bpp) {
                px.copy_from_slice(black);
            }
        };
        for r in 0..ph {
            let row = &mut to[r * stride..r * stride + pw * bpp];
            if r < oy || r >= oy + sh {
                paint(row);
                continue;
            }
            paint(&mut row[..ox * bpp]);
            paint(&mut row[(ox + sw) * bpp..]);
            let s = (r - oy) * sstride;
            row[ox * bpp..(ox + sw) * bpp].copy_from_slice(&from[s..s + sw * bpp]);
        }
    }
}

/// One file's audio stream, as 48 kHz stereo.
struct AudioFile {
    input: format::context::Input,
    decoder: ffmpeg::decoder::Audio,
    index: usize,
    time_base: Rational,
    resampler: Option<Resampler>,
    decoded: frame::Audio,
    eof: bool,
}

impl AudioFile {
    fn open(path: &Path) -> anyhow::Result<Self> {
        ffmpeg::init()?;
        let input = open_input(path)?;
        let stream = input
            .streams()
            .best(media::Type::Audio)
            .context("no audio stream")?;
        let (index, time_base) = (stream.index(), stream.time_base());
        let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()?;
        Ok(Self {
            input,
            decoder,
            index,
            time_base,
            resampler: None,
            decoded: frame::Audio::empty(),
            eof: false,
        })
    }

    fn duration(&self) -> Micros {
        to_micros(
            self.input.duration(),
            Rational(1, ffmpeg::ffi::AV_TIME_BASE),
        )
    }

    fn seek(&mut self, at: Micros) -> anyhow::Result<()> {
        let ts = from_micros(at, Rational(1, ffmpeg::ffi::AV_TIME_BASE));
        self.input.seek(ts, ..ts + 1)?;
        self.decoder.flush();
        self.resampler = None;
        self.eof = false;
        Ok(())
    }

    fn next_chunk(&mut self) -> anyhow::Result<Option<(Micros, Vec<f32>)>> {
        loop {
            if self.decoder.receive_frame(&mut self.decoded).is_ok() {
                let pts = self.decoded.timestamp().or(self.decoded.pts()).unwrap_or(0);
                let at = to_micros(pts, self.time_base);
                return Ok(Some((at, self.interleave()?)));
            }
            if self.eof {
                return Ok(None);
            }
            match self.input.packets().find(|(s, _)| s.index() == self.index) {
                Some((_, packet)) => self.decoder.send_packet(&packet)?,
                None => {
                    self.eof = true;
                    self.decoder.send_eof()?;
                }
            }
        }
    }

    fn interleave(&mut self) -> anyhow::Result<Vec<f32>> {
        let f = &self.decoded;
        let channels = f.channel_layout().channels() as usize;
        let n = f.samples();
        let mut interleaved = Vec::with_capacity(n * channels);
        match f.format() {
            format::Sample::F32(Type::Planar) => {
                for i in 0..n {
                    for c in 0..channels {
                        interleaved.push(f.plane::<f32>(c)[i]);
                    }
                }
            }
            format::Sample::F32(Type::Packed) => {
                interleaved.extend_from_slice(&f.plane::<f32>(0)[..n * channels]);
            }
            format::Sample::I16(Type::Planar) => {
                for i in 0..n {
                    for c in 0..channels {
                        interleaved.push(f32::from(f.plane::<i16>(c)[i]) / 32_768.0);
                    }
                }
            }
            format::Sample::I16(Type::Packed) => {
                interleaved.extend(
                    f.plane::<i16>(0)[..n * channels]
                        .iter()
                        .map(|&s| f32::from(s) / 32_768.0),
                );
            }
            other => return Err(anyhow!("unsupported decoded sample format {other:?}")),
        }
        if f.rate() == SAMPLE_RATE && channels == usize::from(CHANNELS) {
            return Ok(interleaved);
        }
        let resampler = match &mut self.resampler {
            Some(r) => r,
            None => self.resampler.insert(Resampler::new(
                f.rate(),
                channels as u16,
                SAMPLE_RATE,
                CHANNELS,
            )?),
        };
        let mut out = Vec::new();
        resampler.process(&interleaved, &mut out)?;
        Ok(out)
    }
}

/// Decodes an audio track to interleaved 48 kHz stereo f32. A track
/// recorded in pieces (takes) reads as one, in source time, with nothing
/// where a piece was recorded without it.
pub struct AudioReader {
    spans: Vec<Span>,
    pos: usize,
    file: Option<(usize, AudioFile)>,
    duration: Micros,
}

// SAFETY: as for VideoReader.
unsafe impl Send for AudioReader {}

impl AudioReader {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let file = AudioFile::open(path)?;
        let duration = file.duration();
        let spans = vec![Span {
            start: 0,
            end: Micros::MAX,
            path: Some(path.to_path_buf()),
        }];
        Ok(Self {
            spans,
            pos: 0,
            file: Some((0, file)),
            duration,
        })
    }

    /// A track over its `parts` (files in `dir`), read as one.
    pub fn open_parts(dir: &Path, parts: &[Part]) -> anyhow::Result<Self> {
        let duration = parts.last().map_or(0, |p| p.end);
        let spans = spans(dir, parts);
        let first = spans
            .iter()
            .position(|s| s.path.is_some())
            .context("no file for the track")?;
        let file = AudioFile::open(spans[first].path.as_ref().unwrap())?;
        Ok(Self {
            spans,
            pos: first,
            file: Some((first, file)),
            duration,
        })
    }

    /// Track duration: the container's, or for parts where the last ends.
    pub fn duration(&self) -> Micros {
        self.duration
    }

    pub fn seek(&mut self, at: Micros) -> anyhow::Result<()> {
        let i = span_at(&self.spans, at);
        let Some(j) = (i..self.spans.len()).find(|&j| self.spans[j].path.is_some()) else {
            self.pos = self.spans.len();
            return Ok(());
        };
        let local = if j == i { at - self.spans[j].start } else { 0 };
        self.pos = j;
        self.open_pos()?.seek(local)
    }

    fn open_pos(&mut self) -> anyhow::Result<&mut AudioFile> {
        if self.file.as_ref().is_none_or(|(i, _)| *i != self.pos) {
            let path = self.spans[self.pos].path.as_ref().unwrap();
            self.file = Some((self.pos, AudioFile::open(path)?));
        }
        Ok(&mut self.file.as_mut().unwrap().1)
    }

    /// The next chunk of samples (interleaved stereo at 48 kHz) and the source
    /// time of its first sample, or `None` at the end.
    pub fn next_chunk(&mut self) -> anyhow::Result<Option<(Micros, Vec<f32>)>> {
        while self.pos < self.spans.len() {
            if self.spans[self.pos].path.is_some() {
                let Span { start, end, .. } = self.spans[self.pos];
                if let Some((t, chunk)) = self.open_pos()?.next_chunk()? {
                    let at = start.saturating_add(t);
                    if at < end {
                        return Ok(Some((at, chunk)));
                    }
                }
            }
            self.pos += 1;
            self.file = None;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn micros_roundtrip() {
        let tb = Rational(1, 90_000);
        assert_eq!(to_micros(90_000, tb), 1_000_000);
        assert_eq!(from_micros(1_000_000, tb), 90_000);
        assert_eq!(to_micros(-5, tb), 0);
    }
}
