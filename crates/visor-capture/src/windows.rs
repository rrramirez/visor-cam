//! Windows backend (Windows 10 2004+ / 11): Windows.Graphics.Capture (via the
//! `windows-capture` crate) for the screen and WASAPI loopback (via `wasapi`)
//! for system audio.
//!
//! Ids: a display's id is its GDI device number (`\\.\DISPLAY<n>`), which is
//! stable while the display configuration is unchanged; a window's id is its
//! HWND.
//!
//! Coordinates: [`DisplayInfo::frame`] and area rects are "global logical"
//! coordinates, i.e. a monitor's physical desktop rect divided by that
//! monitor's own scale (DPI / 96). With mixed-DPI setups this space is not
//! perfectly continuous between monitors, but every rect only ever relates to
//! the one monitor it is on.
//!
//! Limitations:
//! - Graphics Capture can't capture an application as such, so
//!   [`CaptureTarget::App`] records that process's largest visible top-level
//!   window. UWP/packaged apps show up under `ApplicationFrameHost.exe`.
//! - Visor's own windows are hidden from capture only when they've been marked
//!   with [`exclude_window_from_capture`]; `exclude_self` doesn't change that.
//! - The yellow capture border can only be turned off on Windows 11, and the
//!   cursor setting is ignored on systems without `IsCursorCaptureEnabled`.

use std::ffi::c_void;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use ::windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use ::windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use ::windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
use ::windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use ::windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use ::windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor,
    MDT_EFFECTIVE_DPI, SetThreadDpiAwarenessContext,
};
use ::windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, MONITORINFOF_PRIMARY, SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE,
    WS_EX_TOOLWINDOW,
};
use ::windows::core::{BOOL, PCWSTR, PWSTR, w};
use anyhow::{Context as _, anyhow, bail};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use visor_core::{AudioBuffer, CaptureTarget, PixelFormat, Plane, Rect, VideoFrame};
use wasapi::{
    AudioCaptureClient, AudioClient, DeviceEnumerator, Direction, Handle, SampleType, StreamMode,
    WaveFormat,
};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    GraphicsCaptureItemType, MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::capture_math::{
    FrameLimiter, PixelBox, area_to_pixels, audio_packet_ts, blit_bgra, even, f32_from_le_bytes,
    hns_to_micros,
};
use crate::{
    AppInfo, Backend, DisplayInfo, Drops, ScreenRequest, ScreenSession, Shareable, SystemAudio,
    WindowInfo, host_now,
};

/// Frames buffered between capture and encoder before new ones are dropped.
const FRAME_QUEUE: usize = 8;
static SCREEN_DROPS: Drops = Drops::new("encoder is behind, dropping screen frames");
static AUDIO_DROPS: Drops = Drops::new("audio writer is behind, dropping system audio");
const AUDIO_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u16 = 2;
/// WASAPI buffer length, in 100 ns units (200 ms).
const AUDIO_BUFFER_HNS: i64 = 2_000_000;
/// Windows smaller than this (physical pixels) aren't offered as sources.
const MIN_WINDOW_SIZE: i32 = 50;

/// Hides `hwnd` from every screen capture (Graphics Capture, Desktop
/// Duplication, screenshots), so Visor's control bar and camera bubble never
/// end up in recordings. Needs Windows 10 2004+.
pub fn exclude_window_from_capture(hwnd: isize) -> anyhow::Result<()> {
    // SAFETY: plain Win32 call; an invalid handle just fails.
    unsafe { SetWindowDisplayAffinity(HWND(hwnd as *mut c_void), WDA_EXCLUDEFROMCAPTURE) }
        .context("SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE) failed")
}

pub struct WinBackend;

impl Backend for WinBackend {
    fn shareable(&self) -> anyhow::Result<Shareable> {
        let _dpi = PerMonitorDpiScope::enter();
        let own_pid = std::process::id();
        let displays = monitors()?.iter().map(MonitorDesc::info).collect();

        let mut names = std::collections::HashMap::new();
        let mut windows = Vec::new();
        for w in top_level_windows() {
            if w.pid == own_pid {
                continue;
            }
            let app = names
                .entry(w.pid)
                .or_insert_with(|| app_name(w.pid))
                .clone();
            windows.push(WindowInfo {
                id: w.hwnd.0 as usize as u64,
                title: w.title,
                app,
                pid: w.pid as i32,
            });
        }
        let mut apps: Vec<AppInfo> = names
            .into_iter()
            .map(|(pid, name)| AppInfo {
                pid: pid as i32,
                name,
            })
            .collect();
        apps.sort_by_key(|a| (a.name.to_lowercase(), a.pid));
        Ok(Shareable {
            displays,
            windows,
            apps,
        })
    }

    fn start(&self, request: &ScreenRequest) -> anyhow::Result<ScreenSession> {
        let _dpi = PerMonitorDpiScope::enter();
        let (source, crop, size) = match &request.target {
            CaptureTarget::Display { id } => {
                let m = find_monitor(*id)?;
                let (w, h) = m.size_px();
                (Source::Monitor(m.monitor), None, (even(w), even(h)))
            }
            CaptureTarget::Area { display_id, rect } => {
                let m = find_monitor(*display_id)?;
                let area = area_to_pixels(*rect, m.logical_frame(), m.scale, m.size_px())?;
                let size = (area.width, area.height);
                (Source::Monitor(m.monitor), Some(area), size)
            }
            CaptureTarget::Window { id } => {
                let hwnd = HWND(*id as usize as *mut c_void);
                // SAFETY: IsWindow accepts any value.
                if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
                    bail!("window {id:#x} not found");
                }
                let window = Window::from_raw_hwnd(hwnd.0);
                (Source::Window(window), None, window_capture_size(window)?)
            }
            CaptureTarget::App { pid } => {
                let w = top_level_windows()
                    .into_iter()
                    .filter(|w| w.pid as i32 == *pid)
                    .max_by_key(|w| w.area())
                    .ok_or_else(|| anyhow!("app {pid} has no visible window to capture"))?;
                tracing::info!(
                    title = w.title,
                    "capturing the app's largest window (Graphics Capture can't capture whole apps)"
                );
                let window = Window::from_raw_hwnd(w.hwnd.0);
                (Source::Window(window), None, window_capture_size(window)?)
            }
        };

        let (tx, rx) = bounded(FRAME_QUEUE);
        let flags = HandlerFlags {
            tx,
            size,
            crop,
            fps: request.fps,
        };
        let control = match source {
            Source::Monitor(m) => start_graphics_capture(m, request.show_cursor, false, flags),
            Source::Window(w) => start_graphics_capture(w, request.show_cursor, true, flags),
        }?;

        let audio = match request.system_audio {
            SystemAudio::Off => None,
            SystemAudio::All => Some(LoopbackSource::AllExcept(std::process::id())),
            SystemAudio::App { pid } => Some(LoopbackSource::Process(pid as u32)),
        };
        let audio = match audio.map(start_system_audio).transpose() {
            Ok(a) => a,
            Err(e) => {
                if let Err(stop_err) = control.stop() {
                    tracing::warn!("stopping screen capture: {stop_err}");
                }
                return Err(e.context("starting system audio capture"));
            }
        };
        let (audio_rx, audio_capture) = audio.unzip();
        tracing::info!(
            width = size.0,
            height = size.1,
            system_audio = audio_rx.is_some(),
            "screen capture started"
        );

        Ok(ScreenSession::new(rx, size, audio_rx, move || {
            let audio = audio_capture.map(AudioCapture::stop).transpose();
            control
                .stop()
                .map_err(|e| anyhow!("stopping screen capture: {e}"))?;
            audio.map(drop)
        }))
    }
}

enum Source {
    Monitor(Monitor),
    Window(Window),
}

// ---------------------------------------------------------------------------
// Screen (Graphics Capture)
// ---------------------------------------------------------------------------

type Control = windows_capture::capture::CaptureControl<ScreenHandler, anyhow::Error>;

fn start_graphics_capture<T>(
    item: T,
    show_cursor: bool,
    is_window: bool,
    flags: HandlerFlags,
) -> anyhow::Result<Control>
where
    T: TryInto<GraphicsCaptureItemType> + Send + 'static,
{
    if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
        bail!("Windows Graphics Capture is not available (needs Windows 10 2004 or later)");
    }
    let cursor = if GraphicsCaptureApi::is_cursor_settings_supported().unwrap_or(false) {
        if show_cursor {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::WithoutCursor
        }
    } else {
        tracing::warn!("this Windows version can't toggle cursor capture; using the default");
        CursorCaptureSettings::Default
    };
    // Windows 11 only; Windows 10 always draws the yellow border.
    let border = if GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
        DrawBorderSettings::WithoutBorder
    } else {
        DrawBorderSettings::Default
    };
    // Include a window's menus and popups (Windows 11 only).
    let secondary =
        if is_window && GraphicsCaptureApi::is_secondary_windows_supported().unwrap_or(false) {
            SecondaryWindowSettings::Include
        } else {
            SecondaryWindowSettings::Default
        };
    let settings = Settings::new(
        item,
        cursor,
        border,
        secondary,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        flags,
    );
    ScreenHandler::start_free_threaded(settings)
        .map_err(|e| anyhow!("starting Windows Graphics Capture: {e}"))
}

struct HandlerFlags {
    tx: Sender<VideoFrame>,
    /// Output frame size; constant for the session.
    size: (u32, u32),
    /// Region of the captured surface to keep (area capture).
    crop: Option<PixelBox>,
    fps: u32,
}

struct ScreenHandler {
    /// Dropped when the captured window closes, which ends the video stream.
    tx: Option<Sender<VideoFrame>>,
    size: (u32, u32),
    crop: Option<PixelBox>,
    limiter: FrameLimiter,
}

impl GraphicsCaptureApiHandler for ScreenHandler {
    type Flags = HandlerFlags;
    type Error = anyhow::Error;

    fn new(ctx: Context<Self::Flags>) -> anyhow::Result<Self> {
        let f = ctx.flags;
        Ok(Self {
            tx: Some(f.tx),
            size: f.size,
            crop: f.crop,
            limiter: FrameLimiter::new(f.fps),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> anyhow::Result<()> {
        // Errors returned from here would end the capture; log and skip instead.
        let Some(tx) = &self.tx else {
            return Ok(());
        };
        let host_ts = match frame.timestamp() {
            Ok(t) => hns_to_micros(t.Duration),
            Err(e) => {
                tracing::warn!("frame without timestamp: {e}");
                return Ok(());
            }
        };
        if !self.limiter.accept(host_ts) {
            return Ok(());
        }
        let (width, height) = self.size;
        let wanted = self.crop.unwrap_or(PixelBox {
            x: 0,
            y: 0,
            width,
            height,
        });
        let Some(region) = wanted.clamp_to(frame.width(), frame.height()) else {
            return Ok(());
        };
        let mut buffer = match frame.buffer_crop(
            region.x,
            region.y,
            region.x + region.width,
            region.y + region.height,
        ) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("reading a captured frame failed: {e}");
                return Ok(());
            }
        };
        let (bw, bh) = (buffer.width(), buffer.height());
        let pitch = buffer.row_pitch() as usize;
        let raw = buffer.as_raw_buffer();
        let plane = if (bw, bh) == self.size && raw.len() >= pitch * bh as usize {
            Plane {
                data: raw[..pitch * bh as usize].to_vec().into(),
                stride: pitch,
            }
        } else {
            // The source changed size (window resized, display mode change):
            // keep the session's size by cropping or padding.
            Plane {
                data: blit_bgra(raw, pitch, (bw, bh), self.size).into(),
                stride: width as usize * 4,
            }
        };
        let frame = VideoFrame {
            host_ts,
            width,
            height,
            format: PixelFormat::Bgra,
            planes: vec![plane],
        };
        match tx.try_send(frame) {
            Err(TrySendError::Full(_)) => SCREEN_DROPS.record(),
            Err(TrySendError::Disconnected(_)) => self.tx = None,
            Ok(()) => {}
        }
        Ok(())
    }

    fn on_closed(&mut self) -> anyhow::Result<()> {
        tracing::info!("captured item closed; ending the video stream");
        self.tx = None;
        Ok(())
    }
}

/// The size Graphics Capture will deliver for `window`, made even.
fn window_capture_size(window: Window) -> anyhow::Result<(u32, u32)> {
    let item: GraphicsCaptureItemType =
        window.try_into().context("this window can't be captured")?;
    let GraphicsCaptureItemType::Window((item, _)) = item else {
        bail!("unexpected capture item type");
    };
    let size = item.Size().context("querying the window's size")?;
    if size.Width <= 0 || size.Height <= 0 {
        bail!("window has no visible area");
    }
    Ok((even(size.Width as u32), even(size.Height as u32)))
}

// ---------------------------------------------------------------------------
// Monitors and windows
// ---------------------------------------------------------------------------

/// Makes Win32 calls on this thread report physical pixels regardless of the
/// process's DPI awareness, restoring the previous mode on drop.
struct PerMonitorDpiScope(DPI_AWARENESS_CONTEXT);

impl PerMonitorDpiScope {
    fn enter() -> Self {
        // SAFETY: changes only this thread's awareness; restored on drop.
        Self(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) })
    }
}

impl Drop for PerMonitorDpiScope {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: restores the context returned by the earlier call.
            unsafe { SetThreadDpiAwarenessContext(self.0) };
        }
    }
}

struct MonitorDesc {
    id: u32,
    name: String,
    monitor: Monitor,
    /// Desktop rect in physical pixels.
    rect: RECT,
    scale: f64,
    primary: bool,
}

impl MonitorDesc {
    fn size_px(&self) -> (u32, u32) {
        (
            (self.rect.right - self.rect.left).max(0) as u32,
            (self.rect.bottom - self.rect.top).max(0) as u32,
        )
    }

    fn logical_frame(&self) -> Rect {
        let (w, h) = self.size_px();
        Rect {
            x: f64::from(self.rect.left) / self.scale,
            y: f64::from(self.rect.top) / self.scale,
            width: f64::from(w) / self.scale,
            height: f64::from(h) / self.scale,
        }
    }

    fn info(&self) -> DisplayInfo {
        DisplayInfo {
            id: self.id,
            name: self.name.clone(),
            frame: self.logical_frame(),
            scale: self.scale,
        }
    }
}

/// All monitors, primary first. Call inside a [`PerMonitorDpiScope`].
fn monitors() -> anyhow::Result<Vec<MonitorDesc>> {
    let list = Monitor::enumerate().map_err(|e| anyhow!("enumerating monitors: {e}"))?;
    let mut out = Vec::with_capacity(list.len());
    for (i, monitor) in list.into_iter().enumerate() {
        let handle = HMONITOR(monitor.as_raw_hmonitor());
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: valid out-pointer with cbSize set.
        if !unsafe { GetMonitorInfoW(handle, &mut info) }.as_bool() {
            continue;
        }
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        // SAFETY: plain out-parameter call.
        let scale = unsafe { GetDpiForMonitor(handle, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
            .map_or(1.0, |()| f64::from(dpi_x.max(1)) / 96.0);
        let id = monitor.index().map_or(i as u32 + 1, |n| n as u32);
        let name = monitor
            .name()
            .ok()
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| format!("Display {id}"));
        out.push(MonitorDesc {
            id,
            name,
            monitor,
            rect: info.rcMonitor,
            scale,
            primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
    }
    out.sort_by_key(|m| (!m.primary, m.id));
    Ok(out)
}

fn find_monitor(id: u32) -> anyhow::Result<MonitorDesc> {
    monitors()?
        .into_iter()
        .find(|m| m.id == id)
        .ok_or_else(|| anyhow!("display {id} not found"))
}

struct WindowDesc {
    hwnd: HWND,
    title: String,
    pid: u32,
    rect: RECT,
}

impl WindowDesc {
    fn area(&self) -> i64 {
        i64::from(self.rect.right - self.rect.left) * i64::from(self.rect.bottom - self.rect.top)
    }
}

/// Visible, titled, unowned, non-tool top-level windows in Z order.
fn top_level_windows() -> Vec<WindowDesc> {
    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        // SAFETY: lparam is the &mut Vec passed to EnumWindows below.
        let out = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
        out.push(hwnd);
        BOOL(1)
    }
    let mut hwnds: Vec<HWND> = Vec::new();
    // SAFETY: the callback only runs during this call, while `hwnds` is alive.
    if let Err(e) = unsafe { EnumWindows(Some(collect), LPARAM(&raw mut hwnds as isize)) } {
        tracing::warn!("enumerating windows failed: {e}");
    }
    hwnds.into_iter().filter_map(describe_window).collect()
}

fn describe_window(hwnd: HWND) -> Option<WindowDesc> {
    // SAFETY: read-only queries on a window handle; stale handles just fail.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return None;
        }
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex_style & WS_EX_TOOLWINDOW.0 != 0 {
            return None;
        }
        if GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.is_invalid()) {
            return None;
        }
        // Cloaked windows (other virtual desktops, suspended UWP apps) aren't on screen.
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return None;
        }
        let mut rect = RECT::default();
        GetWindowRect(hwnd, &mut rect).ok()?;
        if rect.right - rect.left < MIN_WINDOW_SIZE || rect.bottom - rect.top < MIN_WINDOW_SIZE {
            return None;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return None;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
        let title = String::from_utf16_lossy(&buf[..n]).trim().to_owned();
        if title.is_empty() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        Some(WindowDesc {
            hwnd,
            title,
            pid,
            rect,
        })
    }
}

/// A process's display name: its exe's FileDescription, else the exe name.
fn app_name(pid: u32) -> String {
    let Some(path) = process_image_path(pid) else {
        return format!("Process {pid}");
    };
    file_description(&path)
        .or_else(|| {
            Path::new(&path)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .unwrap_or(path)
}

fn process_image_path(pid: u32) -> Option<String> {
    // SAFETY: the handle is closed before returning; the buffer outlives the call.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = vec![0u16; 1024];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(process);
        result.ok()?;
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn file_description(path: &str) -> Option<String> {
    let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
    let file = PCWSTR(wide.as_ptr());
    // SAFETY: version-info buffers are sized by GetFileVersionInfoSizeW, and
    // pointers from VerQueryValueW point into `data`, which outlives them.
    unsafe {
        let size = GetFileVersionInfoSizeW(file, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(file, None, size, data.as_mut_ptr().cast()).ok()?;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let (lang, codepage) = if VerQueryValueW(
            data.as_ptr().cast(),
            w!("\\VarFileInfo\\Translation"),
            &mut ptr,
            &mut len,
        )
        .as_bool()
            && len >= 4
            && !ptr.is_null()
        {
            let t = ptr.cast::<u16>();
            (t.read_unaligned(), t.add(1).read_unaligned())
        } else {
            (0x0409, 0x04b0) // US English, Unicode
        };
        let key: Vec<u16> = format!("\\StringFileInfo\\{lang:04x}{codepage:04x}\\FileDescription")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        if !VerQueryValueW(
            data.as_ptr().cast(),
            PCWSTR(key.as_ptr()),
            &mut ptr,
            &mut len,
        )
        .as_bool()
            || len == 0
            || ptr.is_null()
        {
            return None;
        }
        let chars = std::slice::from_raw_parts(ptr.cast::<u16>(), len as usize);
        let end = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
        let name = String::from_utf16_lossy(&chars[..end]).trim().to_owned();
        (!name.is_empty()).then_some(name)
    }
}

// ---------------------------------------------------------------------------
// System audio (WASAPI loopback)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum LoopbackSource {
    /// Everything played on the default output, except this process tree.
    AllExcept(u32),
    /// Only this process tree.
    Process(u32),
}

struct AudioCapture {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

impl AudioCapture {
    fn stop(self) -> anyhow::Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread
            .join()
            .map_err(|_| anyhow!("system audio thread panicked"))
    }
}

fn start_system_audio(
    source: LoopbackSource,
) -> anyhow::Result<(Receiver<AudioBuffer>, AudioCapture)> {
    let (tx, rx) = bounded(256);
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = bounded(1);
    let thread_stop = stop.clone();
    // COM objects stay on the thread that created them.
    let thread = thread::Builder::new()
        .name("visor-system-audio".into())
        .spawn(move || {
            let com = wasapi::initialize_mta().ok();
            match open_loopback(source) {
                Ok(stream) => {
                    let _ = ready_tx.send(Ok(()));
                    if let Err(e) = audio_loop(&stream, &tx, &thread_stop) {
                        tracing::error!("system audio stopped: {e:#}");
                    }
                    let _ = stream.client.stop_stream();
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
            if com.is_ok() {
                wasapi::deinitialize();
            }
        })?;
    ready_rx.recv().context("system audio thread exited")??;
    tracing::info!(?source, "system audio capture started");
    Ok((rx, AudioCapture { stop, thread }))
}

struct LoopbackStream {
    client: AudioClient,
    capture: AudioCaptureClient,
    event: Handle,
}

fn open_loopback(source: LoopbackSource) -> anyhow::Result<LoopbackStream> {
    match source {
        LoopbackSource::Process(pid) => AudioClient::new_application_loopback_client(pid, true)
            .map_err(anyhow::Error::from)
            .and_then(init_loopback)
            .with_context(|| format!("capturing audio of process {pid}")),
        LoopbackSource::AllExcept(pid) => {
            // Process loopback in exclude mode keeps Visor's own sounds out.
            match AudioClient::new_application_loopback_client(pid, false)
                .map_err(anyhow::Error::from)
                .and_then(init_loopback)
            {
                Ok(stream) => Ok(stream),
                Err(e) => {
                    tracing::warn!(
                        "process loopback unavailable ({e:#}); falling back to endpoint \
                         loopback, which also records Visor's own sounds"
                    );
                    let device = DeviceEnumerator::new()?
                        .get_default_device(&Direction::Render)
                        .context("no default audio output")?;
                    init_loopback(device.get_iaudioclient()?)
                }
            }
        }
    }
}

fn init_loopback(mut client: AudioClient) -> anyhow::Result<LoopbackStream> {
    let format = WaveFormat::new(
        32,
        32,
        &SampleType::Float,
        AUDIO_RATE as usize,
        usize::from(AUDIO_CHANNELS),
        None,
    );
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: AUDIO_BUFFER_HNS,
    };
    // A render endpoint (or the process-loopback virtual device) opened for
    // capture is a loopback stream.
    client.initialize_client(&format, &Direction::Capture, &mode)?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;
    Ok(LoopbackStream {
        client,
        capture,
        event,
    })
}

fn audio_loop(
    stream: &LoopbackStream,
    tx: &Sender<AudioBuffer>,
    stop: &AtomicBool,
) -> anyhow::Result<()> {
    let block = usize::from(AUDIO_CHANNELS) * 4;
    let mut bytes = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        // Endpoint loopback signals nothing while nothing plays, so a timeout
        // is normal; it also bounds how long a stop request waits.
        let _ = stream.event.wait_for_event(100);
        loop {
            let frames = match stream.capture.get_next_packet_size()? {
                Some(n) if n > 0 => n,
                _ => break,
            };
            bytes.resize(frames as usize * block, 0);
            let (read, info) = stream.capture.read_from_device(&mut bytes)?;
            if read == 0 {
                break;
            }
            let host_ts = audio_packet_ts(
                info.timestamp,
                info.flags.timestamp_error,
                host_now(),
                read,
                AUDIO_RATE,
            );
            let samples = if info.flags.silent {
                vec![0.0; read as usize * usize::from(AUDIO_CHANNELS)]
            } else {
                f32_from_le_bytes(&bytes[..read as usize * block])
            };
            let buffer = AudioBuffer {
                host_ts,
                sample_rate: AUDIO_RATE,
                channels: AUDIO_CHANNELS,
                samples,
            };
            match tx.try_send(buffer) {
                Err(TrySendError::Full(_)) => AUDIO_DROPS.record(),
                Err(TrySendError::Disconnected(_)) => return Ok(()),
                Ok(()) => {}
            }
        }
    }
    Ok(())
}
