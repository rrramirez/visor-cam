//! Per-track encoders used while recording.

use std::ffi::c_void;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use ffmpeg_next::{
    self as ffmpeg, Dictionary, Packet, Rational, codec, color, encoder, format, frame, picture,
    software::scaling,
};
use visor_core::{Micros, PixelFormat, Plane, PlaneData, VideoFrame};

use crate::scale::Scaler;

/// Encoder timestamps are in microseconds, matching [`Micros`].
const MICROS: Rational = Rational(1, 1_000_000);

/// Muxer options for recording tracks, so a crash loses at most about half
/// a second: fragmented MP4 with a fragment at least that often (not only
/// at keyframes, which come every 2 s), each written straight to the file
/// rather than held in FFmpeg's buffer, which a still screen's tiny frames
/// could take minutes to fill.
pub(crate) fn crash_safe_options() -> Dictionary<'static> {
    let mut opts = Dictionary::new();
    opts.set("movflags", "frag_keyframe+empty_moov+default_base_moof");
    opts.set("frag_duration", "500000");
    opts.set("flush_packets", "1");
    opts
}

/// Hardware encoder first, software fallback second.
const H264_ENCODERS: &[&str] = &[
    "h264_videotoolbox",
    "h264_nvenc",
    "h264_qsv",
    "h264_amf",
    // Windows Media Foundation: the GPU's encoder whatever the vendor
    // (covers Intel graphics; hardware only, see below).
    "h264_mf",
    "h264_vaapi",
    "libx264",
];

/// Encodes NV12 or BGRA frames to H.264 in a fragmented MP4, so the file
/// stays playable even if Visor crashes mid-recording.
pub struct VideoWriter {
    output: format::context::Output,
    encoder: encoder::Video,
    frame: frame::Video,
    /// RGB to NV12 converter, created on the first RGB input.
    rgb: Option<Scaler>,
    stream_time_base: Rational,
    last_pts: Option<Micros>,
    /// Nominal frame duration. Only the final sample's length depends on it;
    /// MP4 derives the others from timestamp deltas.
    frame_duration: Micros,
    encoder_name: &'static str,
}

// SAFETY: every FFmpeg object inside is owned exclusively by the writer and
// none has thread affinity; the writer is moved to, then used by, one thread.
unsafe impl Send for VideoWriter {}

impl VideoWriter {
    pub fn create(path: &Path, width: u32, height: u32, fps: u32) -> anyhow::Result<Self> {
        ffmpeg::init()?;
        if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("frame size {width}x{height} must be even");
        }
        let mut output =
            format::output(path).with_context(|| format!("creating {}", path.display()))?;
        let global_header = output
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER);

        let mut last_err = None;
        for &name in H264_ENCODERS {
            let Some(codec) = encoder::find_by_name(name) else {
                continue;
            };
            match open_h264(codec, name, width, height, fps, global_header) {
                Ok(encoder) => {
                    let mut stream = output.add_stream(codec)?;
                    stream.set_parameters(&encoder);
                    stream.set_time_base(MICROS);
                    output.write_header_with(crash_safe_options())?;
                    let stream_time_base = output
                        .stream(0)
                        .ok_or_else(|| anyhow!("stream vanished"))?
                        .time_base();
                    tracing::info!(encoder = name, width, height, "video writer ready");
                    return Ok(Self {
                        output,
                        encoder,
                        frame: frame::Video::new(format::Pixel::NV12, width, height),
                        rgb: None,
                        stream_time_base,
                        last_pts: None,
                        frame_duration: 1_000_000 / Micros::from(fps.max(1)),
                        encoder_name: name,
                    });
                }
                Err(e) => {
                    tracing::debug!(encoder = name, error = %e, "encoder unavailable");
                    last_err = Some(e);
                }
            }
        }
        Err(anyhow!(
            "no usable H.264 encoder (last error: {last_err:?})"
        ))
    }

    pub fn encoder_name(&self) -> &'static str {
        self.encoder_name
    }

    /// Encodes `src` at `pts`. Frames with a non-increasing PTS are dropped.
    /// `keyframe` forces an I-frame (used after resume).
    ///
    /// The first frame is always placed at 0 so every track starts together,
    /// even if capture delivered its first frame slightly after the start.
    pub fn write(&mut self, src: &VideoFrame, pts: Micros, keyframe: bool) -> anyhow::Result<()> {
        let pts = if self.last_pts.is_none() { 0 } else { pts };
        if self.last_pts.is_some_and(|last| pts <= last) {
            return Ok(());
        }
        let (w, h) = (self.frame.width() as usize, self.frame.height() as usize);
        if src.width as usize != w || src.height as usize != h {
            bail!(
                "frame size changed from {w}x{h} to {}x{}",
                src.width,
                src.height
            );
        }
        // The encoder may still reference the previous frame's buffer.
        // SAFETY: valid, owned AVFrame.
        if unsafe { ffmpeg::ffi::av_frame_make_writable(self.frame.as_mut_ptr()) } < 0 {
            bail!("could not make video frame writable");
        }
        match (src.format, src.planes.as_slice()) {
            (PixelFormat::Nv12, [y, uv]) => {
                // Y plane: h rows of w bytes. CbCr plane: h/2 rows of w bytes.
                copy_plane(&mut self.frame, 0, &y.data, y.stride, w, h);
                copy_plane(&mut self.frame, 1, &uv.data, uv.stride, w, h / 2);
            }
            (PixelFormat::Bgra | PixelFormat::Rgbx, [plane]) => {
                let fmt = if src.format == PixelFormat::Bgra {
                    format::Pixel::BGRA
                } else {
                    format::Pixel::RGBA
                };
                let input = (fmt, w as u32, h as u32);
                if self.rgb.as_ref().is_none_or(|s| s.input() != input) {
                    // Half the cores: at 4K60 one core can't keep up.
                    let threads = std::thread::available_parallelism().map_or(2, |n| n.get() / 2);
                    self.rgb = Some(Scaler::new(
                        input,
                        (format::Pixel::NV12, w as u32, h as u32),
                        scaling::Flags::FAST_BILINEAR,
                        threads.max(1),
                    )?);
                }
                let src_frame = borrow_plane(plane, fmt, w, h)?;
                self.rgb
                    .as_mut()
                    .unwrap()
                    .run(&src_frame, &mut self.frame)?;
            }
            _ => bail!(
                "unsupported frame layout {:?} with {} planes",
                src.format,
                src.planes.len()
            ),
        }

        self.frame.set_pts(Some(pts as i64));
        self.frame.set_kind(if keyframe {
            picture::Type::I
        } else {
            picture::Type::None
        });
        self.encoder.send_frame(&self.frame)?;
        self.last_pts = Some(pts);
        self.drain()
    }

    /// Flushes the encoder and finalizes the file. Capture only delivers frames
    /// when the screen changes, so the last frame is repeated to make the video
    /// end at `end`, the full recording length.
    pub fn finish(mut self, end: Micros) -> anyhow::Result<()> {
        let hold = end.saturating_sub(self.frame_duration);
        if self.last_pts.is_some_and(|last| hold > last) {
            self.frame.set_pts(Some(hold as i64));
            self.frame.set_kind(picture::Type::None);
            self.encoder.send_frame(&self.frame)?;
            self.drain()?;
        }
        self.encoder.send_eof()?;
        self.drain()?;
        self.output.write_trailer()?;
        Ok(())
    }

    fn drain(&mut self) -> anyhow::Result<()> {
        let mut packet = Packet::empty();
        while self.encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(0);
            packet.set_duration(self.frame_duration as i64);
            packet.rescale_ts(MICROS, self.stream_time_base);
            packet.write_interleaved(&mut self.output)?;
        }
        Ok(())
    }
}

fn open_h264(
    codec: ffmpeg::Codec,
    name: &str,
    width: u32,
    height: u32,
    fps: u32,
    global_header: bool,
) -> Result<encoder::Video, ffmpeg::Error> {
    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()?;
    enc.set_width(width);
    enc.set_height(height);
    enc.set_format(format::Pixel::NV12);
    // Matches the capture backends: BT.709 matrix, video range.
    enc.set_colorspace(color::Space::BT709);
    enc.set_color_range(color::Range::MPEG);
    enc.set_color_primaries(color::Primaries::BT709);
    enc.set_color_transfer_characteristic(color::TransferCharacteristic::BT709);
    enc.set_time_base(MICROS);
    enc.set_frame_rate(Some(Rational(fps as i32, 1)));
    enc.set_gop(fps * 2);
    enc.set_max_b_frames(0);
    // Screen content compresses well; ~0.05 bits per pixel per frame is sharp.
    enc.set_bit_rate((u64::from(width) * u64::from(height) * u64::from(fps) / 20) as usize);
    if global_header {
        enc.set_flags(codec::Flags::GLOBAL_HEADER);
    }

    let mut opts = Dictionary::new();
    match name {
        "h264_videotoolbox" => {
            opts.set("realtime", "1");
            opts.set("allow_sw", "1");
        }
        "libx264" => {
            opts.set("preset", "veryfast");
            opts.set("tune", "zerolatency");
        }
        "h264_mf" => {
            // Windows' software encoder is slower than x264; fall through to
            // it when there's no hardware encoder.
            opts.set("hw_encoding", "1");
            opts.set("scenario", "display_remoting");
        }
        _ => {}
    }
    enc.open_with(opts)
}

/// A frame that reads `plane`'s pixels where they are, instead of copying
/// them into a staging frame. The frame's buffer keeps the plane alive.
fn borrow_plane(
    plane: &Plane,
    fmt: format::Pixel,
    width: usize,
    height: usize,
) -> anyhow::Result<frame::Video> {
    let row = width * 4;
    if height == 0 || plane.stride < row || plane.data.len() < plane.stride * (height - 1) + row {
        bail!(
            "{width}x{height} frame doesn't fit its {} byte plane",
            plane.data.len()
        );
    }
    unsafe extern "C" fn release(opaque: *mut c_void, _data: *mut u8) {
        // SAFETY: `opaque` is the box leaked below, freed once, here.
        drop(unsafe { Box::from_raw(opaque.cast::<PlaneData>()) });
    }
    let data = Box::new(plane.data.clone());
    let (ptr, len) = (data.as_ptr().cast_mut(), data.len());
    let opaque = Box::into_raw(data);
    // SAFETY: the buffer is read-only and owns the PlaneData it points into.
    let buf = unsafe {
        ffmpeg::ffi::av_buffer_create(
            ptr,
            len,
            Some(release),
            opaque.cast(),
            ffmpeg::ffi::AV_BUFFER_FLAG_READONLY,
        )
    };
    if buf.is_null() {
        // SAFETY: not handed to FFmpeg.
        drop(unsafe { Box::from_raw(opaque) });
        bail!("could not wrap a frame buffer");
    }
    let mut f = frame::Video::empty();
    // SAFETY: an empty frame takes ownership of `buf`; the fields describe
    // the plane checked above.
    unsafe {
        let raw = f.as_mut_ptr();
        (*raw).buf[0] = buf;
        (*raw).data[0] = ptr;
        (*raw).linesize[0] = plane.stride as i32;
        (*raw).format = ffmpeg::ffi::AVPixelFormat::from(fmt) as i32;
        (*raw).width = width as i32;
        (*raw).height = height as i32;
    }
    Ok(f)
}

fn copy_plane(
    dst: &mut frame::Video,
    plane: usize,
    src: &[u8],
    src_stride: usize,
    row_bytes: usize,
    rows: usize,
) {
    let dst_stride = dst.stride(plane);
    let dst_data = dst.data_mut(plane);
    for row in 0..rows {
        let s = &src[row * src_stride..][..row_bytes];
        dst_data[row * dst_stride..][..row_bytes].copy_from_slice(s);
    }
}
