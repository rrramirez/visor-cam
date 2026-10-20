//! Camera capture through FFmpeg's input devices: avfoundation (macOS),
//! dshow (Windows) and v4l2 (Linux). Frames are converted to NV12.
//!
//! Like the mic, a [`CameraStream`] runs independently of recording so the
//! camera bubble can show a live preview; the recorder subscribes to it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, anyhow};
use crossbeam_channel::{Receiver, bounded};
use ffmpeg_next::{self as ffmpeg, Dictionary, format, frame, software::scaling};
use visor_core::{Micros, PixelFormat, Plane, VideoFrame};

use crate::{Tap, host_now};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraDevice {
    /// Id to pass to [`CameraStream::open`].
    pub id: String,
    pub name: String,
}

#[cfg(target_os = "macos")]
const INPUT_FORMAT: &str = "avfoundation";
#[cfg(target_os = "windows")]
const INPUT_FORMAT: &str = "dshow";
#[cfg(all(unix, not(target_os = "macos")))]
const INPUT_FORMAT: &str = "v4l2";

/// Preferred capture mode; cameras that reject it are opened with defaults.
const WIDTH: u32 = 1280;
const HEIGHT: u32 = 720;
const FPS: u32 = 30;

pub fn list() -> Vec<CameraDevice> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::list_cameras()
    }
    #[cfg(not(target_os = "macos"))]
    {
        list_with_ffmpeg().unwrap_or_else(|e| {
            tracing::warn!("listing cameras failed: {e:#}");
            Vec::new()
        })
    }
}

#[cfg(not(target_os = "macos"))]
fn list_with_ffmpeg() -> anyhow::Result<Vec<CameraDevice>> {
    let fmt = input_format()?;
    // SAFETY: an empty context with the device format set is all
    // avdevice_list_devices needs. It is freed with avformat_free_context, not
    // wrapped in `Input`: closing it as an input would call the device's
    // read_close on a context it never opened (v4l2 dereferences priv_data).
    unsafe {
        let ctx = ffmpeg::ffi::avformat_alloc_context();
        if ctx.is_null() {
            anyhow::bail!("out of memory");
        }
        (*ctx).iformat = fmt;
        let devices = ffmpeg::device::extensions::DeviceIter::wrap(ctx).map(|devices| {
            devices
                .map(|d| {
                    #[cfg(target_os = "windows")]
                    let id = format!("video={}", d.description());
                    #[cfg(not(target_os = "windows"))]
                    let id = d.name().to_owned();
                    CameraDevice {
                        id,
                        name: d.description().to_owned(),
                    }
                })
                .collect()
        });
        ffmpeg::ffi::avformat_free_context(ctx);
        Ok(devices?)
    }
}

fn input_format() -> anyhow::Result<*const ffmpeg::ffi::AVInputFormat> {
    ffmpeg::init()?;
    ffmpeg::device::register_all();
    let name = std::ffi::CString::new(INPUT_FORMAT)?;
    // SAFETY: valid C string; returns a static or null.
    let fmt = unsafe { ffmpeg::ffi::av_find_input_format(name.as_ptr()) };
    if fmt.is_null() {
        anyhow::bail!("FFmpeg was built without the {INPUT_FORMAT} input device");
    }
    Ok(fmt)
}

pub struct CameraStream {
    tap: Tap<Arc<VideoFrame>>,
    size: (u32, u32),
    stop: Arc<AtomicBool>,
}

impl CameraStream {
    pub fn open(id: &str) -> anyhow::Result<Self> {
        let fmt = input_format()?;
        let input = open_device(fmt, id)?;
        let stream = input
            .streams()
            .best(ffmpeg::media::Type::Video)
            .context("camera has no video stream")?;
        let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .video()?;
        let size = (decoder.width() & !1, decoder.height() & !1);
        if size.0 == 0 || size.1 == 0 {
            anyhow::bail!("camera reported an empty frame size");
        }

        let tap = Tap::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_tap, thread_stop) = (tap.clone(), stop.clone());
        let (ready_tx, ready_rx) = bounded(1);
        // The read loop blocks inside FFmpeg, so the thread is detached and
        // exits after its next frame once `stop` is set.
        thread::Builder::new()
            .name("visor-camera".into())
            .spawn(move || {
                let _ = ready_tx.send(());
                if let Err(e) = read_loop(input, decoder, size, &thread_tap, &thread_stop) {
                    tracing::error!("camera stopped: {e:#}");
                }
                thread_tap.close();
            })?;
        ready_rx.recv()?;
        tracing::info!(width = size.0, height = size.1, "camera started");
        Ok(Self { tap, size, stop })
    }

    pub fn subscribe(&self) -> Receiver<Arc<VideoFrame>> {
        self.tap.subscribe(8)
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }
}

impl Drop for CameraStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.tap.close();
    }
}

fn open_device(
    fmt: *const ffmpeg::ffi::AVInputFormat,
    id: &str,
) -> anyhow::Result<format::context::Input> {
    // SAFETY: `fmt` is a static AVInputFormat from av_find_input_format.
    let format = format::format::Format::Input(unsafe { format::Input::wrap(fmt as *mut _) });
    let preferred = || {
        let mut d = Dictionary::new();
        d.set("framerate", &FPS.to_string());
        d.set("video_size", &format!("{WIDTH}x{HEIGHT}"));
        #[cfg(target_os = "macos")]
        d.set("pixel_format", "nv12");
        d
    };
    let rate_only = || {
        let mut d = Dictionary::new();
        d.set("framerate", &FPS.to_string());
        d
    };
    let mut last_err = None;
    for opts in [preferred(), rate_only(), Dictionary::new()] {
        match format::open_with(id, &format, opts) {
            Ok(ctx) => return Ok(ctx.input()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow!(
        "could not open camera {id:?}: {last_err:?} (is camera permission granted?)"
    ))
}

fn read_loop(
    mut input: format::context::Input,
    mut decoder: ffmpeg::decoder::Video,
    (width, height): (u32, u32),
    tap: &Tap<Arc<VideoFrame>>,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    let index = input
        .streams()
        .best(ffmpeg::media::Type::Video)
        .context("no video stream")?
        .index();
    let time_base = input.stream(index).context("no video stream")?.time_base();
    let mut scaler: Option<scaling::Context> = None;
    let mut decoded = frame::Video::empty();
    let mut nv12 = frame::Video::new(format::Pixel::NV12, width, height);
    // Device timestamps are smooth but in the device's own clock; anchor the
    // first one to the host clock and keep the device's spacing from there.
    let mut anchor: Option<(i64, Micros)> = None;

    for (stream, packet) in input.packets() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if stream.index() != index {
            continue;
        }
        let arrived = host_now();
        decoder.send_packet(&packet)?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            let host_ts = match (decoded.pts().or(packet.pts()), anchor) {
                (Some(pts), Some((pts0, host0))) => {
                    let delta = pts.saturating_sub(pts0).max(0) as i128
                        * i128::from(time_base.0)
                        * 1_000_000
                        / i128::from(time_base.1.max(1));
                    host0 + delta as Micros
                }
                (Some(pts), None) => {
                    anchor = Some((pts, arrived));
                    arrived
                }
                (None, _) => arrived,
            };
            let out = if decoded.format() == format::Pixel::NV12
                && decoded.width() & !1 == width
                && decoded.height() & !1 == height
            {
                &decoded
            } else {
                let sws = match &mut scaler {
                    Some(s) => s,
                    None => scaler.insert(scaling::Context::get(
                        decoded.format(),
                        decoded.width(),
                        decoded.height(),
                        format::Pixel::NV12,
                        width,
                        height,
                        scaling::Flags::BILINEAR,
                    )?),
                };
                sws.run(&decoded, &mut nv12)?;
                &nv12
            };
            tap.send(Arc::new(to_video_frame(out, width, height, host_ts)));
        }
    }
    Ok(())
}

fn to_video_frame(f: &frame::Video, width: u32, height: u32, host_ts: Micros) -> VideoFrame {
    let plane = |i: usize, rows: usize| {
        let stride = f.stride(i);
        Plane {
            data: f.data(i)[..stride * rows].to_vec(),
            stride,
        }
    };
    VideoFrame {
        host_ts,
        width,
        height,
        format: PixelFormat::Nv12,
        planes: vec![plane(0, height as usize), plane(1, height as usize / 2)],
    }
}
