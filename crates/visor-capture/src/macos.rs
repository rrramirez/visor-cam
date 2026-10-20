//! ScreenCaptureKit backend (macOS 14+).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, anyhow};
use crossbeam_channel::{Sender, TrySendError, bounded};
use screencapturekit::cm::SCFrameStatus;
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;
use screencapturekit::stream::configuration::{
    PixelFormat as ScPixelFormat, color_matrix, color_space,
};
use visor_core::{AudioBuffer, CaptureTarget, Micros, PixelFormat, Plane, Rect, VideoFrame};

use crate::camera::CameraDevice;
use crate::{
    AppInfo, Backend, DisplayInfo, ScreenRequest, ScreenSession, Shareable, SystemAudio, WindowInfo,
};

/// Frames buffered between capture and encoder before new ones are dropped.
const FRAME_QUEUE: usize = 8;
const AUDIO_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u16 = 2;

pub struct MacBackend;

impl Backend for MacBackend {
    fn shareable(&self) -> anyhow::Result<Shareable> {
        let content = SCShareableContent::get().map_err(sc_err)?;
        let own_pid = std::process::id() as i32;
        let displays = content
            .displays()
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let f = d.frame();
                let scale = SCContentFilter::create()
                    .with_display(d)
                    .with_excluding_windows(&[])
                    .build()
                    .map_or(2.0, |filter| f64::from(filter.point_pixel_scale()));
                DisplayInfo {
                    id: d.display_id(),
                    name: if i == 0 {
                        "Main display".into()
                    } else {
                        format!("Display {}", i + 1)
                    },
                    frame: Rect {
                        x: f.origin.x,
                        y: f.origin.y,
                        width: f.size.width,
                        height: f.size.height,
                    },
                    scale,
                }
            })
            .collect();
        let windows = content
            .windows()
            .iter()
            .filter(|w| w.is_on_screen() && w.window_layer() == 0)
            .filter_map(|w| {
                let app = w.owning_application()?;
                let title = w.title().unwrap_or_default();
                let f = w.frame();
                (app.process_id() != own_pid
                    && !title.is_empty()
                    && f.size.width >= 50.0
                    && f.size.height >= 50.0)
                    .then(|| WindowInfo {
                        id: u64::from(w.window_id()),
                        title,
                        app: app.application_name(),
                        pid: app.process_id(),
                    })
            })
            .collect();
        let mut apps: Vec<AppInfo> = content
            .applications()
            .iter()
            .map(|a| AppInfo {
                pid: a.process_id(),
                name: a.application_name(),
            })
            .filter(|a| !a.name.is_empty() && a.pid != own_pid)
            .collect();
        apps.sort_by_key(|a| a.name.to_lowercase());
        Ok(Shareable {
            displays,
            windows,
            apps,
        })
    }

    fn start(&self, request: &ScreenRequest) -> anyhow::Result<ScreenSession> {
        let content = SCShareableContent::get().map_err(sc_err)?;
        let displays = content.displays();
        let apps = content.applications();
        let windows = content.windows();
        let own_pid = std::process::id() as i32;
        let own_app: Vec<&SCRunningApplication> = if request.exclude_self {
            apps.iter().filter(|a| a.process_id() == own_pid).collect()
        } else {
            Vec::new()
        };
        let find_display = |id: u32| -> anyhow::Result<&SCDisplay> {
            displays
                .iter()
                .find(|d| d.display_id() == id)
                .ok_or_else(|| anyhow!("display {id} not found"))
        };
        let display_filter = |display: &SCDisplay| {
            SCContentFilter::create()
                .with_display(display)
                .with_excluding_applications(&own_app, &[])
                .build()
        };
        // The display a window or app lives on: the one containing its first window's center.
        let display_for_pid = |pid: i32| -> anyhow::Result<&SCDisplay> {
            let window = windows.iter().find(|w| {
                w.owning_application()
                    .is_some_and(|a| a.process_id() == pid)
                    && w.is_on_screen()
            });
            let center = window.map(|w| {
                let f = w.frame();
                (
                    f.origin.x + f.size.width / 2.0,
                    f.origin.y + f.size.height / 2.0,
                )
            });
            center
                .and_then(|(x, y)| {
                    displays.iter().find(|d| {
                        let f = d.frame();
                        x >= f.origin.x
                            && x < f.origin.x + f.size.width
                            && y >= f.origin.y
                            && y < f.origin.y + f.size.height
                    })
                })
                .or_else(|| displays.first())
                .ok_or_else(|| anyhow!("no displays"))
        };

        let mut source_rect = None;
        // Display-wide captures that should hide Visor's own windows.
        let mut excluding_on = None;
        let filter = match &request.target {
            CaptureTarget::Display { id } => {
                excluding_on = Some(*id);
                display_filter(find_display(*id)?)
            }
            CaptureTarget::Window { id } => {
                let window = windows
                    .iter()
                    .find(|w| u64::from(w.window_id()) == *id)
                    .ok_or_else(|| anyhow!("window {id} not found"))?;
                SCContentFilter::create().with_window(window).build()
            }
            CaptureTarget::App { pid } => {
                let app = apps
                    .iter()
                    .find(|a| a.process_id() == *pid)
                    .ok_or_else(|| anyhow!("app {pid} not found"))?;
                SCContentFilter::create()
                    .with_display(display_for_pid(*pid)?)
                    .with_including_applications(&[app], &[])
                    .build()
            }
            CaptureTarget::Area { display_id, rect } => {
                let display = find_display(*display_id)?;
                // Area rects are global; ScreenCaptureKit wants display-relative.
                let origin = display.frame().origin;
                source_rect = Some(CGRect::new(
                    rect.x - origin.x,
                    rect.y - origin.y,
                    rect.width,
                    rect.height,
                ));
                excluding_on = Some(*display_id);
                display_filter(display)
            }
        }
        .map_err(sc_err)?;

        let scale = f64::from(filter.point_pixel_scale());
        let points = source_rect.unwrap_or_else(|| filter.content_rect());
        let width = even((points.size.width * scale).round() as u32);
        let height = even((points.size.height * scale).round() as u32);

        let mut config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(ScPixelFormat::YCbCr_420v)
            .with_minimum_frame_interval(&CMTime::new(1, request.fps.max(1) as i32))
            .with_shows_cursor(request.show_cursor)
            .with_queue_depth(FRAME_QUEUE as u32)
            // Pin the output to sRGB primaries with the BT.709 matrix in video
            // range, which is what the encoder tags the stream as.
            .with_color_space_name(color_space::SRGB)?
            .with_color_matrix(color_matrix::ITU_R_709_2)?;
        if let Some(rect) = source_rect {
            config = config.with_source_rect(rect);
        }

        let (tx, rx) = bounded(FRAME_QUEUE);
        let mut stream = SCStream::new(&filter, &config).map_err(sc_err)?;
        stream
            .add_output_handler(
                move |sample: CMSampleBuffer, kind| on_screen_sample(&tx, sample, kind),
                SCStreamOutputType::Screen,
            )
            .context("adding screen output handler")?;

        // System audio comes from a separate audio-only stream so it can follow
        // its own filter (all apps, or one app) independent of the video target.
        let audio = match request.system_audio {
            SystemAudio::Off => None,
            mode => {
                let display = match &request.target {
                    CaptureTarget::Display { id } | CaptureTarget::Area { display_id: id, .. } => {
                        find_display(*id)?
                    }
                    CaptureTarget::Window { .. } | CaptureTarget::App { .. } => {
                        displays.first().ok_or_else(|| anyhow!("no displays"))?
                    }
                };
                let filter = match mode {
                    SystemAudio::App { pid } => {
                        let app = apps
                            .iter()
                            .find(|a| a.process_id() == pid)
                            .ok_or_else(|| anyhow!("app {pid} for audio not found"))?;
                        SCContentFilter::create()
                            .with_display(display)
                            .with_including_applications(&[app], &[])
                            .build()
                    }
                    _ => display_filter(display),
                }
                .map_err(sc_err)?;
                Some(start_audio_stream(&filter)?)
            }
        };

        stream
            .start_capture()
            .map_err(sc_err)
            .context("starting screen capture (is Screen Recording permission granted?)")?;
        // ScreenCaptureKit only lists an app once it has a window, so Visor
        // may not have been excludable yet (its control bar opens after
        // capture starts). Switch to the excluding filter once it can be.
        let stop_watch = Arc::new(AtomicBool::new(false));
        if let Some(display_id) =
            excluding_on.filter(|_| request.exclude_self && own_app.is_empty())
        {
            let (stream, stop) = (stream.clone(), stop_watch.clone());
            std::thread::Builder::new()
                .name("visor-sck-exclude".into())
                .spawn(move || exclude_own_windows_when_listed(&stream, display_id, &stop))
                .context("spawning exclusion watcher")?;
        }
        let (audio_rx, audio_stream) = match audio {
            Some((rx, s)) => {
                s.start_capture()
                    .map_err(sc_err)
                    .context("starting system audio capture")?;
                (Some(rx), Some(s))
            }
            None => (None, None),
        };
        tracing::info!(
            width,
            height,
            system_audio = audio_rx.is_some(),
            "screen capture started"
        );

        Ok(ScreenSession::new(
            rx,
            (width, height),
            audio_rx,
            move || {
                stop_watch.store(true, Ordering::Relaxed);
                if let Some(s) = audio_stream {
                    s.stop_capture().map_err(sc_err)?;
                }
                stream.stop_capture().map_err(sc_err)
            },
        ))
    }
}

/// Polls until Visor's process shows up in the shareable content, then makes
/// `stream` (a capture of `display_id`) exclude it.
fn exclude_own_windows_when_listed(stream: &SCStream, display_id: u32, stop: &AtomicBool) {
    let own_pid = std::process::id() as i32;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
        let Ok(content) = SCShareableContent::get() else {
            continue;
        };
        let apps = content.applications();
        let Some(own) = apps.iter().find(|a| a.process_id() == own_pid) else {
            continue;
        };
        let displays = content.displays();
        let Some(display) = displays.iter().find(|d| d.display_id() == display_id) else {
            return;
        };
        let result = SCContentFilter::create()
            .with_display(display)
            .with_excluding_applications(&[own], &[])
            .build()
            .and_then(|filter| stream.update_content_filter(&filter));
        match result {
            Ok(()) => tracing::info!("now excluding Visor's windows from capture"),
            Err(e) => tracing::warn!("could not exclude Visor's windows from capture: {e}"),
        }
        return;
    }
}

fn start_audio_stream(
    filter: &SCContentFilter,
) -> anyhow::Result<(crossbeam_channel::Receiver<AudioBuffer>, SCStream)> {
    let config = SCStreamConfiguration::new()
        // Video is required by ScreenCaptureKit; keep it tiny and rare.
        .with_width(2)
        .with_height(2)
        .with_minimum_frame_interval(&CMTime::new(1, 1))
        .with_captures_audio(true)
        .with_excludes_current_process_audio(true)
        .with_sample_rate(AUDIO_RATE as i32)
        .with_channel_count(i32::from(AUDIO_CHANNELS));
    let (tx, rx) = bounded(256);
    let mut stream = SCStream::new(filter, &config).map_err(sc_err)?;
    stream
        .add_output_handler(|_: CMSampleBuffer, _| {}, SCStreamOutputType::Screen)
        .context("audio stream video sink")?;
    stream
        .add_output_handler(
            move |sample: CMSampleBuffer, kind| on_audio_sample(&tx, sample, kind),
            SCStreamOutputType::Audio,
        )
        .context("adding audio output handler")?;
    Ok((rx, stream))
}

fn on_screen_sample(tx: &Sender<VideoFrame>, sample: CMSampleBuffer, kind: SCStreamOutputType) {
    if !matches!(kind, SCStreamOutputType::Screen)
        || sample.frame_status() != Some(SCFrameStatus::Complete)
    {
        return;
    }
    let Some(host_ts) = cmtime_micros(sample.presentation_timestamp()) else {
        return;
    };
    let Some(pixels) = sample.pixel_buffer() else {
        return;
    };
    let Ok(guard) = pixels.lock(CVPixelBufferLockFlags::READ_ONLY) else {
        return;
    };

    let mut planes = Vec::with_capacity(2);
    for i in 0..guard.plane_count() {
        // SAFETY: the buffer stays locked for the lifetime of `guard`.
        let Some(data) = (unsafe { guard.plane_data(i) }) else {
            return;
        };
        planes.push(Plane {
            data: data.to_vec(),
            stride: guard.bytes_per_row_of_plane(i),
        });
    }
    let frame = VideoFrame {
        host_ts,
        width: guard.width() as u32,
        height: guard.height() as u32,
        format: PixelFormat::Nv12,
        planes,
    };
    if let Err(TrySendError::Full(_)) = tx.try_send(frame) {
        tracing::warn!("encoder is behind; dropped a screen frame");
    }
}

fn on_audio_sample(tx: &Sender<AudioBuffer>, sample: CMSampleBuffer, kind: SCStreamOutputType) {
    if !matches!(kind, SCStreamOutputType::Audio) {
        return;
    }
    let Some(host_ts) = cmtime_micros(sample.presentation_timestamp()) else {
        return;
    };
    let Some(desc) = sample.format_description() else {
        return;
    };
    if !desc.audio_is_float() || desc.audio_bits_per_channel() != Some(32) {
        tracing::warn!("unexpected system audio format; dropping buffer");
        return;
    }
    let rate = desc.audio_sample_rate().map_or(AUDIO_RATE, |r| r as u32);
    let channels = desc
        .audio_channel_count()
        .unwrap_or(u32::from(AUDIO_CHANNELS)) as usize;
    let Ok(list) = sample.audio_buffer_list() else {
        return;
    };
    let buffers: Vec<&[f32]> = (&list)
        .into_iter()
        .map(|b| bytemuck_f32(b.data()))
        .collect();

    // ScreenCaptureKit delivers non-interleaved float (one buffer per channel);
    // handle interleaved too, just in case.
    let samples = if buffers.len() == channels && channels > 1 {
        let frames = buffers.iter().map(|b| b.len()).min().unwrap_or(0);
        let mut out = Vec::with_capacity(frames * channels);
        for i in 0..frames {
            out.extend(buffers.iter().map(|b| b[i]));
        }
        out
    } else {
        buffers.concat()
    };
    let buffer = AudioBuffer {
        host_ts,
        sample_rate: rate,
        channels: channels as u16,
        samples,
    };
    if let Err(TrySendError::Full(_)) = tx.try_send(buffer) {
        tracing::warn!("audio writer is behind; dropped a system audio buffer");
    }
}

fn bytemuck_f32(bytes: &[u8]) -> &[f32] {
    // SAFETY: CoreAudio buffers are 4-byte aligned float32 data; any trailing
    // partial sample is excluded by the integer division.
    unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<f32>(), bytes.len() / 4) }
}

/// Host-clock `CMTime` to microseconds.
fn cmtime_micros(t: CMTime) -> Option<Micros> {
    (t.is_valid() && t.timescale > 0 && t.value >= 0)
        .then(|| (i128::from(t.value) * 1_000_000 / i128::from(t.timescale)) as Micros)
}

fn even(n: u32) -> u32 {
    (n & !1).max(2)
}

fn sc_err(e: SCError) -> anyhow::Error {
    anyhow!("ScreenCaptureKit: {e}")
}

pub(crate) fn list_cameras() -> Vec<CameraDevice> {
    use objc2_av_foundation::{AVCaptureDevice, AVMediaTypeVideo};
    let Some(media) = (unsafe { AVMediaTypeVideo }) else {
        return Vec::new();
    };
    #[allow(deprecated)]
    let devices = unsafe { AVCaptureDevice::devicesWithMediaType(media) };
    devices
        .iter()
        .map(|d| {
            // FFmpeg's avfoundation input selects devices by localized name.
            let name = unsafe { d.localizedName() }.to_string();
            CameraDevice {
                id: name.clone(),
                name,
            }
        })
        .collect()
}
