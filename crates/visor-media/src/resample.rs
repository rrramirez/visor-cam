//! Streaming sample-rate and channel conversion (libswresample), interleaved
//! f32 in and out.

use anyhow::bail;
use ffmpeg_next::{
    self as ffmpeg, ChannelLayout, format::Sample, format::sample::Type, software::resampling,
};

pub struct Resampler {
    ctx: resampling::Context,
    in_channels: usize,
    out_channels: usize,
}

// SAFETY: SwrContext has no thread affinity; Resampler is never shared.
unsafe impl Send for Resampler {}

impl Resampler {
    pub fn new(
        in_rate: u32,
        in_channels: u16,
        out_rate: u32,
        out_channels: u16,
    ) -> anyhow::Result<Self> {
        if in_channels == 0 || out_channels == 0 {
            bail!("channel count must be positive");
        }
        let packed = Sample::F32(Type::Packed);
        let ctx = resampling::Context::get(
            packed,
            ChannelLayout::default(i32::from(in_channels)),
            in_rate,
            packed,
            ChannelLayout::default(i32::from(out_channels)),
            out_rate,
        )?;
        Ok(Self {
            ctx,
            in_channels: usize::from(in_channels),
            out_channels: usize::from(out_channels),
        })
    }

    /// Converts `input` (interleaved) and appends the result to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> anyhow::Result<()> {
        let in_frames = input.len() / self.in_channels;
        self.convert(input.as_ptr(), in_frames, out)
    }

    /// Drains samples buffered inside the resampler.
    pub fn flush(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()> {
        self.convert(std::ptr::null(), 0, out)
    }

    fn convert(
        &mut self,
        input: *const f32,
        in_frames: usize,
        out: &mut Vec<f32>,
    ) -> anyhow::Result<()> {
        // SAFETY: the context is initialized; buffers are sized from
        // swr_get_out_samples and both layouts are packed (one plane).
        unsafe {
            let ctx = self.ctx.as_mut_ptr();
            let capacity = ffmpeg::ffi::swr_get_out_samples(ctx, in_frames as i32);
            if capacity <= 0 {
                return Ok(());
            }
            let start = out.len();
            out.resize(start + capacity as usize * self.out_channels, 0.0);
            let mut out_ptr = out.as_mut_ptr().add(start).cast::<u8>();
            let in_ptr = input.cast::<u8>();
            let in_planes = if input.is_null() {
                std::ptr::null()
            } else {
                &raw const in_ptr
            };
            let got = ffmpeg::ffi::swr_convert(
                ctx,
                &raw mut out_ptr,
                capacity,
                in_planes,
                in_frames as i32,
            );
            if got < 0 {
                out.truncate(start);
                bail!("swr_convert failed: {}", ffmpeg::Error::from(got));
            }
            out.truncate(start + got as usize * self.out_channels);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upmixes_and_resamples() {
        let mut r = Resampler::new(24_000, 1, 48_000, 2).unwrap();
        let input = vec![0.5f32; 2_400]; // 100 ms mono
        let mut out = Vec::new();
        r.process(&input, &mut out).unwrap();
        r.flush(&mut out).unwrap();
        let frames = out.len() / 2;
        assert!((4_700..=4_900).contains(&frames), "got {frames} frames");
        // Settled samples keep the level in both channels.
        let mid = &out[4_000..4_002];
        assert!(
            mid.iter()
                .all(|s| (s - 0.5).abs() < 0.05 || (s - 0.354).abs() < 0.05),
            "{mid:?}"
        );
    }
}
