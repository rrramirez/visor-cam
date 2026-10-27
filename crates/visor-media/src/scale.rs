//! A swscale context that splits each frame across threads.
//!
//! `ffmpeg-next`'s scaler calls `sws_scale`, which always runs on one thread;
//! only the frame API (`sws_scale_frame`) uses the context's `threads`. At
//! Retina sizes, scaling is most of the cost of preview playback and export.

use std::ffi::{CStr, c_void};

use anyhow::bail;
use ffmpeg_next::{self as ffmpeg, ffi, format, frame, software::scaling};

pub(crate) struct Scaler {
    ctx: *mut ffi::SwsContext,
    input: (format::Pixel, u32, u32),
}

// SAFETY: the context is owned and has no thread affinity; it is used from
// one thread at a time.
unsafe impl Send for Scaler {}

impl Scaler {
    /// Converts `input` frames to `output` (format, width, height).
    /// `threads` 0 picks one per core.
    pub fn new(
        input: (format::Pixel, u32, u32),
        output: (format::Pixel, u32, u32),
        flags: scaling::Flags,
        threads: usize,
    ) -> anyhow::Result<Self> {
        // SAFETY: plain allocation; checked for null.
        let ctx = unsafe { ffi::sws_alloc_context() };
        if ctx.is_null() {
            bail!("could not allocate a scaler");
        }
        let scaler = Self { ctx, input };
        let pix = |p: format::Pixel| ffi::AVPixelFormat::from(p) as i64;
        let options: [(&CStr, i64); 8] = [
            (c"srcw", i64::from(input.1)),
            (c"srch", i64::from(input.2)),
            (c"src_format", pix(input.0)),
            (c"dstw", i64::from(output.1)),
            (c"dsth", i64::from(output.2)),
            (c"dst_format", pix(output.0)),
            (c"sws_flags", i64::from(flags.bits())),
            (c"threads", threads as i64),
        ];
        for (name, value) in options {
            // SAFETY: `ctx` is a valid SwsContext (an AVClass object).
            let r = unsafe { ffi::av_opt_set_int(ctx as *mut c_void, name.as_ptr(), value, 0) };
            if r < 0 {
                bail!("scaler option {name:?}: {}", ffmpeg::Error::from(r));
            }
        }
        // SAFETY: options set above; no filters.
        let r = unsafe { ffi::sws_init_context(ctx, std::ptr::null_mut(), std::ptr::null_mut()) };
        if r < 0 {
            bail!("could not set up the scaler: {}", ffmpeg::Error::from(r));
        }
        Ok(scaler)
    }

    pub fn input(&self) -> (format::Pixel, u32, u32) {
        self.input
    }

    /// Scales `src` into `dst`, allocating `dst` if it's empty. `dst` must
    /// be writable and of the output format and size.
    pub fn run(&mut self, src: &frame::Video, dst: &mut frame::Video) -> anyhow::Result<()> {
        if (src.format(), src.width(), src.height()) != self.input {
            bail!("scaler input changed");
        }
        // SAFETY: both frames are valid; the context was initialised for
        // `src`'s format and size.
        let r = unsafe { ffi::sws_scale_frame(self.ctx, dst.as_mut_ptr(), src.as_ptr()) };
        if r < 0 {
            bail!("scaling failed: {}", ffmpeg::Error::from(r));
        }
        Ok(())
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` and freed only here.
        unsafe { ffi::sws_freeContext(self.ctx) };
    }
}
