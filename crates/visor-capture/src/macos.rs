//! ScreenCaptureKit backend (macOS 13+).

use anyhow::{Context, anyhow};
use crossbeam_channel::{Sender, TrySendError, bounded};
use screencapturekit::cv::CVPixelBufferLockFlags;
use screencapturekit::prelude::*;
use screencapturekit::shareable_content::SCDisplay;
use screencapturekit::stream::configuration::{PixelFormat as ScPixelFormat, color_matrix, color_space};
use screencapturekit::cm::SCFrameStatus;
use visor_core::{CaptureTarget, Micros, PixelFormat, Plane, RecordingConfig, VideoFrame};

use crate::{AppInfo, Backend, DisplayInfo, Session, Shareable, WindowInfo};

/// Frames buffered between capture and encoder before new ones are dropped.
const FRAME_QUEUE: usize = 8;

#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

pub struct MacBackend {
    timebase: (u64, u64),
}

impl MacBackend {
    pub fn new() -> Self {
        let mut info = MachTimebaseInfo { numer: 1, denom: 1 };
        // SAFETY: plain out-parameter call into libSystem.
        unsafe { mach_timebase_info(&mut info) };
        Self { timebase: (u64::from(info.numer), u64::from(info.denom)) }
    }
}

impl Backend for MacBackend {
    fn shareable(&self) -> anyhow::Result<Shareable> {
        let content = SCShareableContent::get().map_err(sc_err)?;
        Ok(Shareable {
            displays: content
                .displays()
                .iter()
                .map(|d| DisplayInfo { id: d.display_id(), width: d.width(), height: d.height() })
                .collect(),
            windows: content
                .windows()
                .iter()
                .filter(|w| w.is_on_screen() && w.window_layer() == 0)
                .map(|w| WindowInfo {
                    id: u64::from(w.window_id()),
                    title: w.title().unwrap_or_default(),
                    app: w.owning_application().map(|a| a.application_name()).unwrap_or_default(),
                })
                .collect(),
            apps: content
                .applications()
                .iter()
                .map(|a| AppInfo { pid: a.process_id(), name: a.application_name() })
                .filter(|a| !a.name.is_empty())
                .collect(),
        })
    }

    fn start(&self, config: &RecordingConfig) -> anyhow::Result<Session> {
        let content = SCShareableContent::get().map_err(sc_err)?;
        let displays = content.displays();
        let find_display = |id: u32| -> anyhow::Result<&SCDisplay> {
            displays.iter().find(|d| d.display_id() == id).ok_or_else(|| anyhow!("display {id} not found"))
        };
        let main_display = || displays.first().ok_or_else(|| anyhow!("no displays"));

        let mut source_rect = None;
        let filter = match &config.target {
            CaptureTarget::Display { id } => {
                SCContentFilter::create().with_display(find_display(*id)?).with_excluding_windows(&[]).build()
            }
            CaptureTarget::Window { id } => {
                let windows = content.windows();
                let window = windows
                    .iter()
                    .find(|w| u64::from(w.window_id()) == *id)
                    .ok_or_else(|| anyhow!("window {id} not found"))?;
                SCContentFilter::create().with_window(window).build()
            }
            CaptureTarget::App { pid } => {
                let apps = content.applications();
                let app = apps
                    .iter()
                    .find(|a| a.process_id() == *pid)
                    .ok_or_else(|| anyhow!("app with pid {pid} not found"))?;
                SCContentFilter::create().with_display(main_display()?).with_including_applications(&[app], &[]).build()
            }
            CaptureTarget::Area { display_id, rect } => {
                source_rect = Some(CGRect::new(rect.x, rect.y, rect.width, rect.height));
                SCContentFilter::create().with_display(find_display(*display_id)?).with_excluding_windows(&[]).build()
            }
        }
        .map_err(sc_err)?;

        let scale = f64::from(filter.point_pixel_scale());
        let points = source_rect.unwrap_or_else(|| filter.content_rect());
        let width = even((points.size.width * scale).round() as u32);
        let height = even((points.size.height * scale).round() as u32);

        let mut sc_config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(ScPixelFormat::YCbCr_420v)
            .with_minimum_frame_interval(&CMTime::new(1, config.fps.max(1) as i32))
            .with_shows_cursor(config.show_cursor)
            .with_queue_depth(FRAME_QUEUE as u32)
            // Pin the output to sRGB primaries with the BT.709 matrix in video
            // range, which is what the encoder tags the stream as.
            .with_color_space_name(color_space::SRGB)?
            .with_color_matrix(color_matrix::ITU_R_709_2)?;
        if let Some(rect) = source_rect {
            sc_config = sc_config.with_source_rect(rect);
        }

        let (tx, rx) = bounded(FRAME_QUEUE);
        let mut stream = SCStream::new(&filter, &sc_config).map_err(sc_err)?;
        stream
            .add_output_handler(move |sample: CMSampleBuffer, kind| on_screen_sample(&tx, sample, kind), SCStreamOutputType::Screen)
            .context("adding screen output handler")?;
        stream.start_capture().map_err(sc_err).context("starting capture (is Screen Recording permission granted?)")?;
        tracing::info!(width, height, "screen capture started");

        Ok(Session {
            screen: rx,
            screen_size: (width, height),
            camera: None,
            audio: Vec::new(),
            stop: Box::new(move || stream.stop_capture().map_err(sc_err)),
        })
    }

    fn host_now(&self) -> Micros {
        // SAFETY: no preconditions.
        let ticks = unsafe { mach_absolute_time() };
        let (numer, denom) = self.timebase;
        (u128::from(ticks) * u128::from(numer) / u128::from(denom) / 1_000) as Micros
    }
}

fn on_screen_sample(tx: &Sender<VideoFrame>, sample: CMSampleBuffer, kind: SCStreamOutputType) {
    if !matches!(kind, SCStreamOutputType::Screen) || sample.frame_status() != Some(SCFrameStatus::Complete) {
        return;
    }
    let Some(host_ts) = cmtime_micros(sample.presentation_timestamp()) else { return };
    let Some(pixels) = sample.pixel_buffer() else { return };
    let Ok(guard) = pixels.lock(CVPixelBufferLockFlags::READ_ONLY) else { return };

    let mut planes = Vec::with_capacity(2);
    for i in 0..guard.plane_count() {
        // SAFETY: the buffer stays locked for the lifetime of `guard`.
        let Some(data) = (unsafe { guard.plane_data(i) }) else { return };
        planes.push(Plane { data: data.to_vec(), stride: guard.bytes_per_row_of_plane(i) });
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
