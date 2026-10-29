//! Track readers used by the player, thumbnails and export. Timestamps are
//! source-time microseconds.

use std::path::Path;

use anyhow::{Context, anyhow};
use ffmpeg_next::{
    self as ffmpeg, Rational, format, format::sample::Type, frame, media, software::scaling,
};
use visor_core::Micros;

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

/// Decodes a video track, optionally converting every frame to a fixed
/// pixel format and size.
pub struct VideoReader {
    input: format::context::Input,
    decoder: ffmpeg::decoder::Video,
    index: usize,
    time_base: Rational,
    target: Option<(format::Pixel, u32, u32)>,
    scaler: Option<Scaler>,
    scale_flags: scaling::Flags,
    scale_threads: usize,
    decoded: frame::Video,
    eof: bool,
}

// SAFETY: the reader exclusively owns its FFmpeg objects, none of which has
// thread affinity; it is used from one thread at a time.
unsafe impl Send for VideoReader {}

impl VideoReader {
    /// `target`: convert to this format and size (`None` = decoder output).
    pub fn open(path: &Path, target: Option<(format::Pixel, u32, u32)>) -> anyhow::Result<Self> {
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
            target,
            scaler: None,
            scale_flags: scaling::Flags::BILINEAR,
            scale_threads: 0,
            decoded: frame::Video::empty(),
            eof: false,
        })
    }

    /// How frames are converted to the target: the filter, and threads
    /// (0 = one per core). Defaults to bilinear on every core, for export;
    /// live preview trades a little of both for less CPU.
    pub(crate) fn set_scaling(&mut self, flags: scaling::Flags, threads: usize) {
        (self.scale_flags, self.scale_threads) = (flags, threads);
        self.scaler = None;
    }

    pub fn size(&self) -> (u32, u32) {
        (self.decoder.width(), self.decoder.height())
    }

    /// Track duration from the container.
    pub fn duration(&self) -> Micros {
        to_micros(
            self.input.duration(),
            Rational(1, ffmpeg::ffi::AV_TIME_BASE),
        )
    }

    /// Positions the reader at the keyframe at or before `at`. Following
    /// [`next_frame`](Self::next_frame) calls start there; skip frames before `at` to land
    /// exactly.
    pub fn seek(&mut self, at: Micros) -> anyhow::Result<()> {
        let ts = from_micros(at, Rational(1, ffmpeg::ffi::AV_TIME_BASE));
        self.input.seek(ts, ..ts + 1)?;
        self.decoder.flush();
        self.eof = false;
        Ok(())
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
        if self.scaler.as_ref().is_none_or(|s| s.input() != input) {
            self.scaler = Some(Scaler::new(
                input,
                (fmt, w, h),
                self.scale_flags,
                self.scale_threads,
            )?);
        }
        reuse_or_alloc(out, fmt, w, h);
        self.scaler.as_mut().unwrap().run(d, out)?;
        out.set_pts(d.pts());
        Ok(())
    }
}

/// Decodes an audio track to interleaved 48 kHz stereo f32.
pub struct AudioReader {
    input: format::context::Input,
    decoder: ffmpeg::decoder::Audio,
    index: usize,
    time_base: Rational,
    resampler: Option<Resampler>,
    decoded: frame::Audio,
    eof: bool,
}

// SAFETY: as for VideoReader.
unsafe impl Send for AudioReader {}

impl AudioReader {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
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

    pub fn duration(&self) -> Micros {
        to_micros(
            self.input.duration(),
            Rational(1, ffmpeg::ffi::AV_TIME_BASE),
        )
    }

    pub fn seek(&mut self, at: Micros) -> anyhow::Result<()> {
        let ts = from_micros(at, Rational(1, ffmpeg::ffi::AV_TIME_BASE));
        self.input.seek(ts, ..ts + 1)?;
        self.decoder.flush();
        self.resampler = None;
        self.eof = false;
        Ok(())
    }

    /// The next chunk of samples (interleaved stereo at 48 kHz) and the source
    /// time of its first sample, or `None` at the end.
    pub fn next_chunk(&mut self) -> anyhow::Result<Option<(Micros, Vec<f32>)>> {
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
