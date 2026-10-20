//! Camera capture through FFmpeg's input devices: avfoundation (macOS),
//! dshow (Windows) and v4l2 (Linux). Frames are converted to NV12.
//!
//! Like the mic, a [`CameraStream`] runs independently of recording so the
//! camera bubble can show a live preview; the recorder subscribes to it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

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

/// Sizes to ask the camera for, best first. The bubble never needs more than
/// 1080p, and a 4K camera feed costs ~9x the memory and encoding of 720p.
const SIZES: [(u32, u32); 6] = [
    (1280, 720),
    (1920, 1080),
    (1600, 900),
    (960, 540),
    (1280, 960),
    (640, 480),
];
/// Larger camera frames are scaled down to fit this.
const MAX_SIZE: (u32, u32) = (1920, 1080);
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
    use std::ffi::{CStr, c_char};

    let fmt = input_format()?;
    let mut list: *mut ffmpeg::ffi::AVDeviceInfoList = std::ptr::null_mut();
    // SAFETY: unlike avdevice_list_devices on a bare context,
    // avdevice_list_input_sources allocates the device's private data, which
    // dshow's device listing dereferences. The list is freed below.
    let ret = unsafe {
        ffmpeg::ffi::avdevice_list_input_sources(
            fmt,
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut list,
        )
    };
    if ret < 0 {
        anyhow::bail!(
            "listing {INPUT_FORMAT} devices: {}",
            ffmpeg::Error::from(ret)
        );
    }
    let text = |p: *const c_char| {
        // SAFETY: FFmpeg device strings are null or NUL-terminated.
        (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    };
    let mut cameras = Vec::new();
    // SAFETY: `list` holds `nb_devices` valid entries until it is freed.
    unsafe {
        if !list.is_null() {
            for i in 0..(*list).nb_devices.max(0) as usize {
                let d = &**(*list).devices.add(i);
                // dshow lists microphones too; keep devices that provide video.
                let types = if d.media_types.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(d.media_types, d.nb_media_types.max(0) as usize)
                };
                if !types.is_empty()
                    && !types.contains(&ffmpeg::ffi::AVMediaType::AVMEDIA_TYPE_VIDEO)
                {
                    continue;
                }
                let name = text(d.device_name).unwrap_or_default();
                let description = text(d.device_description).unwrap_or_else(|| name.clone());
                #[cfg(target_os = "windows")]
                let id = format!("video={description}");
                #[cfg(not(target_os = "windows"))]
                let id = name;
                cameras.push(CameraDevice {
                    id,
                    name: description,
                });
            }
        }
        ffmpeg::ffi::avdevice_free_list_devices(&mut list);
    }
    Ok(cameras)
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
        if decoder.width() < 2 || decoder.height() < 2 {
            anyhow::bail!("camera reported an empty frame size");
        }
        let size = fit_within((decoder.width(), decoder.height()), MAX_SIZE);

        let tap = Tap::default();
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_tap, thread_stop) = (tap.clone(), stop.clone());
        let (ready_tx, ready_rx) = bounded(1);
        // The read loop can block inside FFmpeg, so the thread is detached and
        // exits after its next read once `stop` is set.
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
    let sized = |(w, h): (u32, u32), nv12: bool| {
        let mut d = Dictionary::new();
        d.set("framerate", &FPS.to_string());
        d.set("video_size", &format!("{w}x{h}"));
        if nv12 && cfg!(target_os = "macos") {
            d.set("pixel_format", "nv12");
        }
        d
    };
    let rate_only = || {
        let mut d = Dictionary::new();
        d.set("framerate", &FPS.to_string());
        d
    };
    let attempts = SIZES
        .iter()
        .map(|&s| sized(s, true))
        .chain(SIZES.iter().map(|&s| sized(s, false)))
        .chain([rate_only(), Dictionary::new()]);
    let mut last_err = None;
    for opts in attempts {
        match format::open_with(id, &format, opts) {
            Ok(ctx) => return Ok(ctx.input()),
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow!(
        "could not open camera {id:?}: {last_err:?} (is camera permission granted?)"
    ))
}

/// `size` scaled down (keeping its aspect) to fit `max`, with even sides.
fn fit_within((w, h): (u32, u32), (max_w, max_h): (u32, u32)) -> (u32, u32) {
    let scale = f64::min(
        1.0,
        f64::min(max_w as f64 / w as f64, max_h as f64 / h as f64),
    );
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(w as f64 * scale), even(h as f64 * scale))
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

    // Read packets directly: `Input::packets()` ends at the first EAGAIN,
    // which capture devices return whenever no frame is ready yet. Each read
    // needs a fresh packet: av_read_frame overwrites a packet without
    // freeing what it held, so reusing one leaks every frame.
    while !stop.load(Ordering::Relaxed) {
        let mut packet = ffmpeg::Packet::empty();
        match packet.read(&mut input) {
            Ok(()) => {}
            Err(ffmpeg::Error::Other {
                errno: ffmpeg::error::EAGAIN,
            }) => {
                thread::sleep(Duration::from_millis(2));
                continue;
            }
            Err(ffmpeg::Error::InvalidData) => continue,
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => return Err(e).context("reading from camera"),
        }
        if packet.stream() != index {
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

#[cfg(test)]
mod tests {
    use super::fit_within;

    #[test]
    fn large_camera_frames_are_scaled_to_fit() {
        assert_eq!(fit_within((3840, 2160), (1920, 1080)), (1920, 1080));
        assert_eq!(fit_within((4032, 3024), (1920, 1080)), (1440, 1080));
        assert_eq!(fit_within((1280, 720), (1920, 1080)), (1280, 720));
        assert_eq!(fit_within((641, 481), (1920, 1080)), (640, 480));
    }
}
