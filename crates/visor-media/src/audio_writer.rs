//! AAC audio track writer used while recording.

use std::path::Path;

use anyhow::{Context, anyhow};
use ffmpeg_next::{
    self as ffmpeg, ChannelLayout, Dictionary, Packet, Rational, codec, encoder, format,
    format::sample::Type, frame,
};
use visor_core::{AudioBuffer, Micros};

use crate::resample::Resampler;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
/// Timing differences under this are treated as jitter, not gaps.
const JITTER_TOLERANCE: i64 = SAMPLE_RATE as i64 / 25; // 40 ms

/// Encodes interleaved f32 buffers to 48 kHz stereo AAC in a fragmented MP4
/// (`.m4a`). Buffers are placed by their PTS: gaps are filled with silence
/// and overlaps trimmed, so the track stays aligned with video.
pub struct AudioWriter {
    output: format::context::Output,
    encoder: encoder::Audio,
    stream_time_base: Rational,
    frame_size: usize,
    resampler: Option<(u32, u16, Resampler)>,
    /// Interleaved stereo samples waiting to fill an encoder frame.
    pending: Vec<f32>,
    /// Sample position of the first pending sample.
    pending_pts: i64,
    scratch: Vec<f32>,
}

impl AudioWriter {
    pub fn create(path: &Path) -> anyhow::Result<Self> {
        ffmpeg::init()?;
        let mut output =
            format::output(path).with_context(|| format!("creating {}", path.display()))?;
        let global_header = output
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER);
        let codec =
            encoder::find(codec::Id::AAC).ok_or_else(|| anyhow!("FFmpeg has no AAC encoder"))?;
        let mut enc = codec::context::Context::new_with_codec(codec)
            .encoder()
            .audio()?;
        enc.set_rate(SAMPLE_RATE as i32);
        enc.set_channel_layout(ChannelLayout::default(i32::from(CHANNELS)));
        enc.set_format(format::Sample::F32(Type::Planar));
        enc.set_bit_rate(160_000);
        enc.set_time_base(Rational(1, SAMPLE_RATE as i32));
        if global_header {
            enc.set_flags(codec::Flags::GLOBAL_HEADER);
        }
        let encoder = enc.open_as_with(codec, Dictionary::new())?;
        let frame_size = (encoder.frame_size() as usize).max(1024);

        let mut stream = output.add_stream(codec)?;
        stream.set_parameters(&encoder);
        stream.set_time_base(Rational(1, SAMPLE_RATE as i32));
        crate::track_file::write_header(&mut output)?;
        let stream_time_base = output.stream(0).context("stream vanished")?.time_base();

        Ok(Self {
            output,
            encoder,
            stream_time_base,
            frame_size,
            resampler: None,
            pending: Vec::new(),
            pending_pts: 0,
            scratch: Vec::new(),
        })
    }

    /// Appends `buf`, whose first sample belongs at output time `pts`.
    pub fn write(&mut self, buf: &AudioBuffer, pts: Micros) -> anyhow::Result<()> {
        if buf.channels == 0 || buf.samples.is_empty() {
            return Ok(());
        }
        self.scratch.clear();
        if buf.sample_rate == SAMPLE_RATE && buf.channels == CHANNELS {
            self.scratch.extend_from_slice(&buf.samples);
        } else {
            let resampler = match &mut self.resampler {
                Some((rate, ch, r)) if *rate == buf.sample_rate && *ch == buf.channels => r,
                slot => {
                    &mut slot
                        .insert((
                            buf.sample_rate,
                            buf.channels,
                            Resampler::new(buf.sample_rate, buf.channels, SAMPLE_RATE, CHANNELS)?,
                        ))
                        .2
                }
            };
            resampler.process(&buf.samples, &mut self.scratch)?;
        }

        let target = micros_to_samples(pts);
        let next = self.pending_pts + (self.pending.len() / usize::from(CHANNELS)) as i64;
        let mut skip = 0usize;
        if target > next + JITTER_TOLERANCE || (next == 0 && target > 0) {
            // A gap (or a late start): pad with silence up to the buffer.
            self.pending.resize(
                self.pending.len() + (target - next) as usize * usize::from(CHANNELS),
                0.0,
            );
        } else if target < next - JITTER_TOLERANCE {
            // Overlap: drop what was already covered.
            skip = ((next - target) as usize * usize::from(CHANNELS)).min(self.scratch.len());
        }
        let scratch = std::mem::take(&mut self.scratch);
        self.pending.extend_from_slice(&scratch[skip..]);
        self.scratch = scratch;
        self.encode_full_frames()
    }

    /// Writes out everything encoded so far, as when no audio has come for
    /// a while, so a crash doesn't lose it.
    pub fn flush(&mut self) -> anyhow::Result<()> {
        self.drain()?;
        crate::track_file::flush(&mut self.output)
    }

    /// Pads with silence to `end`, flushes the encoder and finalizes the file.
    pub fn finish(mut self, end: Micros) -> anyhow::Result<()> {
        let end = micros_to_samples(end);
        let have = self.pending_pts + (self.pending.len() / usize::from(CHANNELS)) as i64;
        if end > have {
            self.pending.resize(
                self.pending.len() + (end - have) as usize * usize::from(CHANNELS),
                0.0,
            );
        } else {
            let keep = (end - self.pending_pts).max(0) as usize * usize::from(CHANNELS);
            self.pending.truncate(keep);
        }
        self.encode_full_frames()?;
        if !self.pending.is_empty() {
            let frames = self.pending.len() / usize::from(CHANNELS);
            self.send(frames)?;
        }
        self.encoder.send_eof()?;
        self.drain()?;
        self.output.write_trailer()?;
        Ok(())
    }

    fn encode_full_frames(&mut self) -> anyhow::Result<()> {
        while self.pending.len() >= self.frame_size * usize::from(CHANNELS) {
            self.send(self.frame_size)?;
        }
        Ok(())
    }

    /// Encodes the first `frames` pending stereo frames.
    fn send(&mut self, frames: usize) -> anyhow::Result<()> {
        let mut f = frame::Audio::new(
            format::Sample::F32(Type::Planar),
            frames,
            ChannelLayout::default(i32::from(CHANNELS)),
        );
        f.set_rate(SAMPLE_RATE);
        f.set_pts(Some(self.pending_pts));
        for ch in 0..usize::from(CHANNELS) {
            let plane = f.plane_mut::<f32>(ch);
            for (i, s) in plane.iter_mut().enumerate().take(frames) {
                *s = self.pending[i * usize::from(CHANNELS) + ch];
            }
        }
        self.pending.drain(..frames * usize::from(CHANNELS));
        self.pending_pts += frames as i64;
        self.encoder.send_frame(&f)?;
        self.drain()
    }

    fn drain(&mut self) -> anyhow::Result<()> {
        let mut packet = Packet::empty();
        while self.encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(0);
            packet.rescale_ts(Rational(1, SAMPLE_RATE as i32), self.stream_time_base);
            packet.write_interleaved(&mut self.output)?;
        }
        Ok(())
    }
}

fn micros_to_samples(us: Micros) -> i64 {
    (u128::from(us) * u128::from(SAMPLE_RATE) / 1_000_000) as i64
}
