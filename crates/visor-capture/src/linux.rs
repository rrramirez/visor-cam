//! Linux backend: xdg-desktop-portal ScreenCast + PipeWire. Works on Wayland
//! and X11 desktops that run a portal implementation (GNOME, KDE, wlroots…).
//!
//! # How it maps onto [`Backend`]
//!
//! - **Screen.** [`Backend::start`] opens a ScreenCast portal session
//!   ([`ashpd`]) and consumes the portal's PipeWire node through the remote
//!   returned by `OpenPipeWireRemote`. `start` blocks until the user has
//!   answered the portal's dialog, so call it off the UI thread.
//! - **Targets.** The portal cannot be told *which* monitor or window to
//!   record; its own picker chooses. `Display` and `Area` request a monitor
//!   source and `Window` and `App` request a window source; the ids in the
//!   target are ignored. `App` therefore records one window of the app, picked
//!   by the user. `Area` crops the chosen monitor: the rect is in global
//!   logical coordinates and is made monitor-relative with the portal stream's
//!   position and size, then scaled to the stream's pixel size.
//! - **Frames.** BGRx/BGRA is negotiated preferentially (RGBx/RGBA are
//!   accepted and swizzled) and delivered as [`PixelFormat::Bgra`]. The output
//!   size is fixed when the stream first negotiates; if a window stream is
//!   later resized, frames are cropped or padded (black, top-left anchored) to
//!   that size because the encoder needs a constant size. Buffers must be
//!   mappable memory (MemPtr/MemFd); DMA-BUF is not requested.
//! - **Frame rate.** The stream's max framerate is requested as `fps` and any
//!   extra frames are dropped by [`FrameLimiter`].
//! - **Cursor.** Embedded in the frames when `show_cursor`, hidden otherwise.
//!   If the portal lacks the requested mode its default is used (metadata
//!   cursors are not drawn).
//! - **Timestamps.** `spa_meta_header.pts` (CLOCK_MONOTONIC ns) when present
//!   and plausible, else the arrival time from [`host_now`].
//! - **System audio** comes from the user's PipeWire daemon directly, not
//!   through the portal (so it is not available inside a Flatpak sandbox
//!   without PipeWire access). [`SystemAudio::All`] records the default sink's
//!   monitor, which includes Visor's own sounds. [`SystemAudio::App`] finds
//!   the app's `Stream/Output/Audio` nodes by `application.process.id` (the
//!   pid or any descendant, so multi-process browsers work) and links their
//!   output ports to Visor's capture stream itself, following streams that
//!   appear later; the app keeps playing to its speakers.
//! - **Shareable content.** The portal cannot enumerate anything, so
//!   [`Backend::shareable`] returns one placeholder display (id 0, "Choose in
//!   system dialog") sized from the first connected DRM connector's preferred
//!   mode, no windows, and as apps the processes currently playing audio
//!   (useful for per-app audio).
//! - **`exclude_self`** cannot be honoured: the portal has no way to exclude
//!   windows. The app hides its camera bubble when recording a display here.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{ResponseError, Session};
use ashpd::enumflags2::BitFlags;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use futures_lite::future::block_on;
use pipewire as pw;
use pw::context::ContextRc;
use pw::main_loop::MainLoopRc;
use pw::properties::properties;
use pw::spa;
use pw::stream::{StreamBox, StreamFlags, StreamState};
use spa::buffer::meta::{MetaHeader, MetaVideoCrop};
use spa::param::ParamType;
use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::{self, Pod, Value, serialize::PodSerializer};
use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Id, Rectangle, SpaTypes};
use visor_core::{AudioBuffer, CaptureTarget, Micros, PixelFormat, Plane, Rect, VideoFrame};

use crate::{
    AppInfo, Backend, DisplayInfo, Drops, ScreenRequest, ScreenSession, Shareable, SystemAudio,
    host_now,
};

/// Frames buffered between capture and encoder before new ones are dropped.
const FRAME_QUEUE: usize = 8;
static SCREEN_DROPS: Drops = Drops::new("encoder is behind, dropping screen frames");
static AUDIO_DROPS: Drops = Drops::new("audio writer is behind, dropping system audio");
const AUDIO_QUEUE: usize = 256;
const AUDIO_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u32 = 2;
/// How long to wait for PipeWire to negotiate a video format after the
/// portal dialog has been answered.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(15);
/// Buffer timestamps further than this from "now" are not trusted.
const MAX_PTS_SKEW: Micros = 1_000_000;

pub struct LinuxBackend;

impl Backend for LinuxBackend {
    fn shareable(&self) -> anyhow::Result<Shareable> {
        let (width, height) = primary_drm_mode().unwrap_or((1920, 1080));
        let displays = vec![DisplayInfo {
            id: 0,
            name: "Choose in system dialog".into(),
            frame: Rect {
                x: 0.0,
                y: 0.0,
                width: f64::from(width),
                height: f64::from(height),
            },
            scale: 1.0,
        }];
        let apps = audio_apps().unwrap_or_else(|e| {
            tracing::debug!("listing PipeWire audio apps failed: {e:#}");
            Vec::new()
        });
        Ok(Shareable {
            displays,
            windows: Vec::new(),
            apps,
        })
    }

    fn start(&self, request: &ScreenRequest) -> anyhow::Result<ScreenSession> {
        if request.exclude_self {
            tracing::debug!("exclude_self is not supported by the ScreenCast portal");
        }
        let (source, area) = match &request.target {
            CaptureTarget::Display { .. } => (SourceType::Monitor, None),
            CaptureTarget::Area { rect, .. } => (SourceType::Monitor, Some(*rect)),
            CaptureTarget::Window { .. } | CaptureTarget::App { .. } => (SourceType::Window, None),
        };
        let portal = block_on(open_portal(source, request.show_cursor))?;
        let close_portal = |portal: &Portal| {
            if let Err(e) = block_on(portal.session.close()) {
                tracing::warn!("closing the ScreenCast portal session: {e}");
            }
        };

        let plan = CropPlan {
            area,
            stream_pos: portal.stream_pos,
            stream_size: portal.stream_size,
        };
        let (tx, rx) = bounded(FRAME_QUEUE);
        let video = match spawn_video(portal.fd, portal.node_id, plan, request.fps, tx) {
            Ok(v) => v,
            Err(e) => {
                close_portal(&portal.handles);
                return Err(e);
            }
        };
        let (video_thread, size) = video;

        let audio = match request.system_audio {
            SystemAudio::Off => None,
            mode => match spawn_audio(mode) {
                Ok(a) => Some(a),
                Err(e) => {
                    video_thread.stop();
                    close_portal(&portal.handles);
                    return Err(e);
                }
            },
        };
        let (audio_rx, audio_thread) = audio.unzip();
        tracing::info!(
            width = size.0,
            height = size.1,
            system_audio = audio_rx.is_some(),
            "screen capture started"
        );

        let handles = portal.handles;
        Ok(ScreenSession::new(rx, size, audio_rx, move || {
            if let Some(t) = audio_thread {
                t.stop();
            }
            video_thread.stop();
            block_on(handles.session.close())
                .map_err(|e| anyhow!("closing the ScreenCast portal session: {e}"))
        }))
    }
}

// ---------------------------------------------------------------------------
// Portal

/// Keeps the portal session alive; closing it ends the screencast.
struct Portal {
    // Field order matters for drop: the session before its proxy.
    session: Session<Screencast>,
    _proxy: Screencast,
}

struct PortalStream {
    handles: Portal,
    fd: OwnedFd,
    node_id: u32,
    stream_pos: Option<(i32, i32)>,
    stream_size: Option<(i32, i32)>,
}

async fn open_portal(source: SourceType, show_cursor: bool) -> anyhow::Result<PortalStream> {
    let proxy = Screencast::new()
        .await
        .context("connecting to the ScreenCast portal (is xdg-desktop-portal running?)")?;
    let session = proxy
        .create_session(Default::default())
        .await
        .context("creating a ScreenCast session")?;
    match negotiate(&proxy, &session, source, show_cursor).await {
        Ok((stream, fd)) => Ok(PortalStream {
            node_id: stream.pipe_wire_node_id(),
            stream_pos: stream.position(),
            stream_size: stream.size(),
            fd,
            handles: Portal {
                session,
                _proxy: proxy,
            },
        }),
        Err(e) => {
            let _ = session.close().await;
            Err(e)
        }
    }
}

async fn negotiate(
    proxy: &Screencast,
    session: &Session<Screencast>,
    source: SourceType,
    show_cursor: bool,
) -> anyhow::Result<(ashpd::desktop::screencast::Stream, OwnedFd)> {
    let available = proxy.available_cursor_modes().await.unwrap_or_default();
    let wanted = if show_cursor {
        CursorMode::Embedded
    } else {
        CursorMode::Hidden
    };
    let cursor = available.contains(wanted).then_some(wanted);
    if cursor.is_none() {
        tracing::warn!(?wanted, ?available, "cursor mode unsupported by the portal");
    }
    proxy
        .select_sources(
            session,
            SelectSourcesOptions::default()
                .set_cursor_mode(cursor)
                .set_sources(BitFlags::from(source))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await
        .and_then(|r| r.response())
        .context("selecting ScreenCast sources")?;
    let streams = proxy
        .start(session, None, Default::default())
        .await
        .and_then(|r| r.response())
        .map_err(|e| match e {
            ashpd::Error::Response(ResponseError::Cancelled) => {
                anyhow!("screen recording was cancelled in the system dialog")
            }
            e => anyhow!("starting the ScreenCast session: {e}"),
        })?;
    let stream = streams
        .streams()
        .first()
        .cloned()
        .context("the portal returned no streams")?;
    let fd = proxy
        .open_pipe_wire_remote(session, Default::default())
        .await
        .context("opening the portal's PipeWire remote")?;
    Ok((stream, fd))
}

// ---------------------------------------------------------------------------
// PipeWire threads

/// A thread running a PipeWire main loop until told to quit.
struct PwThread {
    quit: pw::channel::Sender<()>,
    handle: JoinHandle<()>,
}

impl PwThread {
    fn spawn<R: Send + 'static>(
        name: &str,
        body: impl FnOnce(pw::channel::Receiver<()>, Sender<anyhow::Result<R>>) -> anyhow::Result<()>
        + Send
        + 'static,
    ) -> anyhow::Result<(Self, Receiver<anyhow::Result<R>>)> {
        let (quit, quit_rx) = pw::channel::channel();
        let (ready_tx, ready_rx) = bounded(1);
        let handle = thread::Builder::new().name(name.into()).spawn(move || {
            let fail = ready_tx.clone();
            if let Err(e) = body(quit_rx, ready_tx) {
                tracing::error!("PipeWire thread failed: {e:#}");
                // Only lands if the caller is still waiting for readiness.
                let _ = fail.try_send(Err(e));
            }
        })?;
        Ok((Self { quit, handle }, ready_rx))
    }

    fn stop(self) {
        let _ = self.quit.send(());
        if self.handle.join().is_err() {
            tracing::error!("PipeWire thread panicked");
        }
    }
}

/// Lets the main loop be quit from another thread via `quit`.
fn attach_quit<'l>(
    main_loop: &'l MainLoopRc,
    quit: pw::channel::Receiver<()>,
) -> pw::channel::AttachedReceiver<'l, ()> {
    let weak = main_loop.downgrade();
    quit.attach(main_loop.loop_(), move |()| {
        if let Some(main_loop) = weak.upgrade() {
            main_loop.quit();
        }
    })
}

fn spawn_video(
    fd: OwnedFd,
    node_id: u32,
    plan: CropPlan,
    fps: u32,
    tx: Sender<VideoFrame>,
) -> anyhow::Result<(PwThread, (u32, u32))> {
    let (thread, ready) = PwThread::spawn("visor-pw-video", move |quit, ready| {
        run_video(fd, node_id, plan, fps, tx, quit, ready)
    })?;
    match ready.recv_timeout(NEGOTIATION_TIMEOUT) {
        Ok(Ok(size)) => Ok((thread, size)),
        Ok(Err(e)) => {
            thread.stop();
            Err(e.context("starting the PipeWire screen stream"))
        }
        Err(_) => {
            thread.stop();
            Err(anyhow!(
                "PipeWire screen stream did not negotiate a format in time"
            ))
        }
    }
}

struct VideoState {
    plan: CropPlan,
    ready: Option<Sender<anyhow::Result<(u32, u32)>>>,
    tx: Sender<VideoFrame>,
    limiter: FrameLimiter,
    format: Option<(SrcFormat, u32, u32)>,
    /// Fixed output size and the source region it is cut from (Area only).
    out: Option<(u32, u32)>,
    area_px: Option<PixelRect>,
    warned_unmapped: bool,
    was_streaming: bool,
}

fn run_video(
    fd: OwnedFd,
    node_id: u32,
    plan: CropPlan,
    fps: u32,
    tx: Sender<VideoFrame>,
    quit: pw::channel::Receiver<()>,
    ready: Sender<anyhow::Result<(u32, u32)>>,
) -> anyhow::Result<()> {
    pw::init();
    let main_loop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&main_loop, None)?;
    let core = context
        .connect_fd_rc(fd, None)
        .context("connecting to the portal's PipeWire remote")?;
    let _quit = attach_quit(&main_loop, quit);

    let stream = StreamBox::new(
        &core,
        "visor-screen",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;
    let state = VideoState {
        plan,
        ready: Some(ready),
        tx,
        limiter: FrameLimiter::new(fps),
        format: None,
        out: None,
        area_px: None,
        warned_unmapped: false,
        was_streaming: false,
    };
    let weak = main_loop.downgrade();
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(move |_, st, old, new| {
            tracing::debug!(?old, ?new, "screen stream state");
            match new {
                StreamState::Streaming => st.was_streaming = true,
                StreamState::Error(e) => {
                    tracing::error!("screen stream error: {e}");
                    if let Some(ready) = st.ready.take() {
                        let _ = ready.send(Err(anyhow!("PipeWire stream error: {e}")));
                    }
                    if let Some(l) = weak.upgrade() {
                        l.quit();
                    }
                }
                // The portal session ended (e.g. the user stopped sharing).
                StreamState::Unconnected if st.was_streaming => {
                    if let Some(l) = weak.upgrade() {
                        l.quit();
                    }
                }
                _ => {}
            }
        })
        .param_changed(|stream, st, id, param| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else { return };
            if let Err(e) = st.on_format(stream, param) {
                tracing::error!("screen stream format: {e:#}");
                if let Some(ready) = st.ready.take() {
                    let _ = ready.send(Err(e));
                }
            }
        })
        .process(|stream, st| st.on_process(stream))
        .register()?;

    let format = serialize_pod(video_format_object(fps))?;
    let mut params = [Pod::from_bytes(&format).context("bad format pod")?];
    stream.connect(
        Direction::Input,
        Some(node_id),
        StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    main_loop.run();
    let _ = stream.disconnect();
    Ok(())
}

impl VideoState {
    fn on_format(&mut self, stream: &pw::stream::Stream, param: &Pod) -> anyhow::Result<()> {
        let (media_type, media_subtype) = spa::param::format_utils::parse_format(param)
            .map_err(|e| anyhow!("parsing format: {e:?}"))?;
        if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
            return Ok(());
        }
        let mut info = VideoInfoRaw::new();
        info.parse(param)
            .map_err(|e| anyhow!("parsing video format: {e:?}"))?;
        let fmt = SrcFormat::from_spa(info.format())
            .ok_or_else(|| anyhow!("unsupported pixel format {:?}", info.format()))?;
        let Rectangle { width, height } = info.size();
        tracing::info!(
            ?fmt,
            width,
            height,
            framerate = ?info.framerate(),
            max_framerate = ?info.max_framerate(),
            "screen stream format"
        );
        self.format = Some((fmt, width, height));

        if self.out.is_none() {
            let out = match self.plan.area {
                Some(rect) => {
                    let px = area_crop(
                        &rect,
                        self.plan.stream_pos,
                        self.plan.stream_size,
                        (width, height),
                    )
                    .ok_or_else(|| anyhow!("the area is outside the chosen monitor"))?;
                    self.area_px = Some(px);
                    (px.width, px.height)
                }
                None => (even(width), even(height)),
            };
            self.out = Some(out);
            if let Some(ready) = self.ready.take() {
                let _ = ready.send(Ok(out));
            }
        }

        // Ask for mappable memory plus the header (timestamps) and crop metas.
        let pods = [
            buffers_object(),
            meta_object::<MetaHeader>(),
            meta_object::<MetaVideoCrop>(),
        ]
        .into_iter()
        .map(serialize_pod)
        .collect::<anyhow::Result<Vec<_>>>()?;
        let mut params: Vec<&Pod> = pods.iter().filter_map(|b| Pod::from_bytes(b)).collect();
        stream.update_params(&mut params)?;
        Ok(())
    }

    fn on_process(&mut self, stream: &pw::stream::Stream) {
        let Some(mut buffer) = stream.dequeue_buffer() else {
            return;
        };
        let (Some((fmt, width, height)), Some(out)) = (self.format, self.out) else {
            return;
        };
        let now = host_now();
        let pts = buffer.find_meta::<MetaHeader>().map(|h| h.pts());
        let host_ts = pts.map_or(now, |pts| pts_to_host(pts, now));
        let crop = buffer
            .find_meta::<MetaVideoCrop>()
            .map(|c| c.meta_region())
            .filter(|r| r.is_valid())
            .map(|r| {
                let (p, s) = (r.position(), r.size());
                PixelRect {
                    x: p.x.max(0) as u32,
                    y: p.y.max(0) as u32,
                    width: s.width,
                    height: s.height,
                }
            });
        let Some(data) = buffer.datas_mut().first_mut() else {
            return;
        };
        let chunk = data.chunk();
        if chunk.size() == 0 || chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED) {
            return;
        }
        if !self.limiter.accept(host_ts) {
            return;
        }
        let stride = match chunk.stride() {
            s if s > 0 => s as usize,
            _ => width as usize * 4,
        };
        let offset = chunk.offset() as usize;
        let Some(bytes) = data.data() else {
            if !self.warned_unmapped {
                self.warned_unmapped = true;
                tracing::error!("screen buffers are not mappable (DMA-BUF?); dropping frames");
            }
            return;
        };
        let Some(src) = bytes.get(offset..) else {
            return;
        };
        let full = PixelRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        let region = self.area_px.or(crop).map_or(full, |r| r.intersect(&full));
        let pixels = crop_pixels(src, stride, region, out);
        let frame = VideoFrame {
            host_ts,
            width: out.0,
            height: out.1,
            // The encoder converts either byte order; no swizzle here.
            format: if fmt.is_rgb_order() {
                PixelFormat::Rgbx
            } else {
                PixelFormat::Bgra
            },
            planes: vec![Plane {
                data: pixels.into(),
                stride: out.0 as usize * 4,
            }],
        };
        if let Err(TrySendError::Full(_)) = self.tx.try_send(frame) {
            SCREEN_DROPS.record();
        }
    }
}

fn video_format_object(fps: u32) -> pod::Object {
    let fps = fps.max(1);
    pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        // Screencasts are variable-rate ("0/1"); the cap goes in maxFramerate.
        pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 0, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
        pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    )
}

fn buffers_object() -> pod::Object {
    let mappable = (1 << spa::sys::SPA_DATA_MemPtr) | (1 << spa::sys::SPA_DATA_MemFd);
    pod::Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![pod::Property::new(
            spa::sys::SPA_PARAM_BUFFERS_dataType,
            Value::Choice(pod::ChoiceValue::Int(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Flags {
                    default: mappable,
                    flags: Vec::new(),
                },
            ))),
        )],
    }
}

fn meta_object<T: spa::buffer::meta::Metadata>() -> pod::Object {
    pod::Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: ParamType::Meta.as_raw(),
        properties: vec![
            pod::Property::new(spa::sys::SPA_PARAM_META_type, Value::Id(Id(T::META_TYPE))),
            pod::Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Int(std::mem::size_of::<T>() as i32),
            ),
        ],
    }
}

fn serialize_pod(object: pod::Object) -> anyhow::Result<Vec<u8>> {
    Ok(
        PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(object))
            .map_err(|e| anyhow!("serializing SPA pod: {e:?}"))?
            .0
            .into_inner(),
    )
}

// ---------------------------------------------------------------------------
// System audio

fn spawn_audio(mode: SystemAudio) -> anyhow::Result<(Receiver<AudioBuffer>, PwThread)> {
    let (tx, rx) = bounded(AUDIO_QUEUE);
    let (thread, ready) = PwThread::spawn("visor-pw-audio", move |quit, ready| {
        run_audio(mode, tx, quit, ready)
    })?;
    match ready.recv_timeout(NEGOTIATION_TIMEOUT) {
        Ok(Ok(())) => Ok((rx, thread)),
        Ok(Err(e)) => {
            thread.stop();
            Err(e.context("starting PipeWire system audio capture"))
        }
        Err(_) => {
            thread.stop();
            Err(anyhow!("PipeWire system audio did not start in time"))
        }
    }
}

struct AudioState {
    tx: Sender<AudioBuffer>,
    rate: u32,
    channels: u32,
}

fn run_audio(
    mode: SystemAudio,
    tx: Sender<AudioBuffer>,
    quit: pw::channel::Receiver<()>,
    ready: Sender<anyhow::Result<()>>,
) -> anyhow::Result<()> {
    pw::init();
    let main_loop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&main_loop, None)?;
    let core = context
        .connect_rc(None)
        .context("connecting to the PipeWire daemon")?;
    let _quit = attach_quit(&main_loop, quit);

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
        *pw::keys::NODE_NAME => "visor-system-audio",
        *pw::keys::APP_NAME => "Visor",
    };
    let mut flags = StreamFlags::MAP_BUFFERS;
    let app_pid = match mode {
        SystemAudio::App { pid } => {
            // Visor links the app's ports itself; keep the session manager out.
            props.insert("node.autoconnect", "false");
            Some(pid)
        }
        _ => {
            props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
            flags |= StreamFlags::AUTOCONNECT;
            None
        }
    };
    let stream = StreamBox::new(&core, "visor-system-audio", props)?;

    let linker = app_pid.map(|pid| Rc::new(RefCell::new(AppLinker::new(pid))));
    let state = AudioState {
        tx,
        rate: AUDIO_RATE,
        channels: AUDIO_CHANNELS,
    };
    let stream_linker = linker.clone();
    let stream_core = core.clone();
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(move |stream, _, old, new| {
            tracing::debug!(?old, ?new, "system audio stream state");
            if let StreamState::Error(e) = &new {
                tracing::error!("system audio stream error: {e}");
            }
            if let (Some(linker), StreamState::Paused | StreamState::Streaming) =
                (&stream_linker, &new)
            {
                let mut linker = linker.borrow_mut();
                linker.own_node = Some(stream.node_id());
                linker.relink(&stream_core);
            }
        })
        .param_changed(|_, st, id, param| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else { return };
            let mut info = spa::param::audio::AudioInfoRaw::new();
            if info.parse(param).is_ok() {
                st.rate = info.rate().max(1);
                st.channels = info.channels().max(1);
                tracing::info!(
                    rate = st.rate,
                    channels = st.channels,
                    "system audio format"
                );
            }
        })
        .process(|stream, st| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
            let Some(bytes) = data.data().and_then(|b| b.get(offset..offset + size)) else {
                return;
            };
            let samples = f32_le_samples(bytes);
            let frames = samples.len() as u64 / u64::from(st.channels);
            if frames == 0 {
                return;
            }
            // The buffer's first sample was captured one buffer-length ago.
            let duration = frames * 1_000_000 / u64::from(st.rate);
            let buffer = AudioBuffer {
                host_ts: host_now().saturating_sub(duration),
                sample_rate: st.rate,
                channels: st.channels as u16,
                samples,
            };
            if let Err(TrySendError::Full(_)) = st.tx.try_send(buffer) {
                AUDIO_DROPS.record();
            }
        })
        .register()?;

    // Keep the registry (and its listener) alive for the app linker.
    let _registry = match &linker {
        Some(linker) => {
            let registry = core.get_registry_rc()?;
            let (on_add, on_remove) = (linker.clone(), linker.clone());
            let add_core = core.clone();
            let listener = registry
                .add_listener_local()
                .global(move |g| {
                    let Some(props) = g.props else { return };
                    let mut linker = on_add.borrow_mut();
                    if linker.on_global(g.id, &g.type_, props) {
                        linker.relink(&add_core);
                    }
                })
                .global_remove(move |id| on_remove.borrow_mut().on_global_remove(id))
                .register();
            Some((registry, listener))
        }
        None => None,
    };

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(AUDIO_RATE);
    info.set_channels(AUDIO_CHANNELS);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let format = serialize_pod(pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    })?;
    let mut params = [Pod::from_bytes(&format).context("bad format pod")?];
    stream.connect(Direction::Input, None, flags, &mut params)?;
    let _ = ready.send(Ok(()));
    main_loop.run();
    let _ = stream.disconnect();
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PortInfo {
    node: u32,
    output: bool,
    channel: String,
}

/// Links an application's output streams to Visor's capture stream, following
/// streams and ports as they come and go.
struct AppLinker {
    pid: i32,
    own_pid: i32,
    own_node: Option<u32>,
    app_nodes: HashSet<u32>,
    ports: HashMap<u32, PortInfo>,
    links: HashMap<(u32, u32), pw::link::Link>,
}

impl AppLinker {
    fn new(pid: i32) -> Self {
        Self {
            pid,
            own_pid: std::process::id() as i32,
            own_node: None,
            app_nodes: HashSet::new(),
            ports: HashMap::new(),
            links: HashMap::new(),
        }
    }

    /// Records a registry global; returns whether links may need updating.
    fn on_global(
        &mut self,
        id: u32,
        kind: &pw::types::ObjectType,
        props: &spa::utils::dict::DictRef,
    ) -> bool {
        match kind {
            pw::types::ObjectType::Node => {
                let is_output_stream = props.get("media.class") == Some("Stream/Output/Audio");
                let pid = props
                    .get("application.process.id")
                    .and_then(|p| p.parse::<i32>().ok());
                match pid {
                    Some(pid)
                        if is_output_stream
                            && pid != self.own_pid
                            && is_same_or_descendant(pid, self.pid) =>
                    {
                        tracing::info!(node = id, pid, "found app audio stream");
                        self.app_nodes.insert(id)
                    }
                    _ => false,
                }
            }
            pw::types::ObjectType::Port => {
                let (Some(node), Some(dir)) = (
                    props.get("node.id").and_then(|n| n.parse().ok()),
                    props.get("port.direction"),
                ) else {
                    return false;
                };
                if props.get("port.monitor") == Some("true") {
                    return false;
                }
                let channel = props.get("audio.channel").unwrap_or("").to_owned();
                self.ports.insert(
                    id,
                    PortInfo {
                        node,
                        output: dir == "out",
                        channel,
                    },
                );
                true
            }
            _ => false,
        }
    }

    fn on_global_remove(&mut self, id: u32) {
        self.app_nodes.remove(&id);
        if self.ports.remove(&id).is_some() {
            self.links.retain(|&(out, inp), _| out != id && inp != id);
        }
    }

    fn relink(&mut self, core: &pw::core::CoreRc) {
        let Some(own) = self.own_node else { return };
        let ports = |pred: &dyn Fn(&PortInfo) -> bool| -> Vec<(u32, u32, String)> {
            let mut v: Vec<_> = self
                .ports
                .iter()
                .filter(|(_, p)| pred(p))
                .map(|(&id, p)| (id, p.node, p.channel.clone()))
                .collect();
            v.sort();
            v
        };
        let outs = ports(&|p| p.output && self.app_nodes.contains(&p.node));
        let ins = ports(&|p| !p.output && p.node == own);
        let outs: Vec<(u32, &str)> = outs.iter().map(|(id, _, c)| (*id, c.as_str())).collect();
        let ins_simple: Vec<(u32, &str)> = ins.iter().map(|(id, _, c)| (*id, c.as_str())).collect();
        for (out, inp) in plan_links(&outs, &ins_simple) {
            if self.links.contains_key(&(out, inp)) {
                continue;
            }
            let out_node = self.ports[&out].node;
            let result = core.create_object::<pw::link::Link>(
                "link-factory",
                &properties! {
                    "link.output.node" => out_node.to_string(),
                    "link.output.port" => out.to_string(),
                    "link.input.node" => own.to_string(),
                    "link.input.port" => inp.to_string(),
                    "object.linger" => "false",
                },
            );
            match result {
                Ok(link) => {
                    tracing::debug!(out, inp, "linked app audio port");
                    self.links.insert((out, inp), link);
                }
                Err(e) => tracing::warn!("linking app audio port {out} -> {inp}: {e}"),
            }
        }
    }
}

/// Pairs app output ports with capture input ports by channel position. Mono
/// or unpositioned outputs go to every input; channels the capture stream
/// doesn't have (e.g. surround) are dropped.
fn plan_links(outs: &[(u32, &str)], ins: &[(u32, &str)]) -> Vec<(u32, u32)> {
    let mut links = Vec::new();
    for &(out, channel) in outs {
        let matching: Vec<u32> = ins
            .iter()
            .filter(|(_, c)| *c == channel)
            .map(|&(id, _)| id)
            .collect();
        if !matching.is_empty() {
            links.extend(matching.into_iter().map(|inp| (out, inp)));
        } else if channel.is_empty() || channel == "MONO" {
            links.extend(ins.iter().map(|&(inp, _)| (out, inp)));
        }
    }
    links
}

/// Processes currently playing audio, by name, from the PipeWire registry.
fn audio_apps() -> anyhow::Result<Vec<AppInfo>> {
    let (tx, rx) = bounded(1);
    thread::Builder::new()
        .name("visor-pw-list".into())
        .spawn(move || {
            let _ = tx.send(list_audio_apps());
        })?;
    rx.recv_timeout(Duration::from_secs(2))
        .map_err(|_| anyhow!("PipeWire did not answer"))?
}

fn list_audio_apps() -> anyhow::Result<Vec<AppInfo>> {
    pw::init();
    let main_loop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&main_loop, None)?;
    let core = context.connect_rc(None)?;
    let registry = core.get_registry_rc()?;
    let found: Rc<RefCell<Vec<AppInfo>>> = Rc::default();
    let sink = found.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |g| {
            let Some(props) = g.props else { return };
            if g.type_ != pw::types::ObjectType::Node
                || props.get("media.class") != Some("Stream/Output/Audio")
            {
                return;
            }
            let Some(pid) = props
                .get("application.process.id")
                .and_then(|p| p.parse().ok())
            else {
                return;
            };
            let name = props
                .get("application.name")
                .or_else(|| props.get("node.name"))
                .unwrap_or("Unknown")
                .to_owned();
            sink.borrow_mut().push(AppInfo { pid, name });
        })
        .register();
    let pending = core.sync(0)?;
    let weak = main_loop.downgrade();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE
                && seq == pending
                && let Some(l) = weak.upgrade()
            {
                l.quit();
            }
        })
        .register();
    main_loop.run();

    let own = std::process::id() as i32;
    let mut apps = found.take();
    apps.retain(|a| a.pid != own);
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps.dedup_by_key(|a| a.pid);
    Ok(apps)
}

// ---------------------------------------------------------------------------
// Pure helpers

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SrcFormat {
    Bgrx,
    Bgra,
    Rgbx,
    Rgba,
}

impl SrcFormat {
    fn from_spa(f: VideoFormat) -> Option<Self> {
        Some(match f {
            VideoFormat::BGRx => Self::Bgrx,
            VideoFormat::BGRA => Self::Bgra,
            VideoFormat::RGBx => Self::Rgbx,
            VideoFormat::RGBA => Self::Rgba,
            _ => return None,
        })
    }

    fn is_rgb_order(self) -> bool {
        matches!(self, Self::Rgbx | Self::Rgba)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl PixelRect {
    fn intersect(&self, other: &PixelRect) -> PixelRect {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = (self.x + self.width).min(other.x + other.width);
        let y1 = (self.y + self.height).min(other.y + other.height);
        PixelRect {
            x: x0,
            y: y0,
            width: x1.saturating_sub(x0),
            height: y1.saturating_sub(y0),
        }
    }
}

/// What to cut out of the portal stream: everything, or an area of a monitor.
#[derive(Debug, Clone, Copy)]
struct CropPlan {
    area: Option<Rect>,
    /// The monitor's position and size in global logical coordinates, as
    /// reported by the portal.
    stream_pos: Option<(i32, i32)>,
    stream_size: Option<(i32, i32)>,
}

/// Maps a global logical `rect` onto the pixels of a monitor stream whose
/// logical position/size the portal reported (`None`: origin at 0,0 and no
/// scaling). The result is clamped to the stream and has even dimensions, or
/// is `None` if less than 2×2 pixels remain.
fn area_crop(
    rect: &Rect,
    stream_pos: Option<(i32, i32)>,
    stream_size: Option<(i32, i32)>,
    pixels: (u32, u32),
) -> Option<PixelRect> {
    let (px, py) = stream_pos.unwrap_or((0, 0));
    let (lw, lh) = stream_size
        .filter(|&(w, h)| w > 0 && h > 0)
        .map_or((f64::from(pixels.0), f64::from(pixels.1)), |(w, h)| {
            (f64::from(w), f64::from(h))
        });
    let (sx, sy) = (f64::from(pixels.0) / lw, f64::from(pixels.1) / lh);
    let clamp_x = |v: f64| v.clamp(0.0, f64::from(pixels.0));
    let clamp_y = |v: f64| v.clamp(0.0, f64::from(pixels.1));
    let x0 = clamp_x(((rect.x - f64::from(px)) * sx).round());
    let y0 = clamp_y(((rect.y - f64::from(py)) * sy).round());
    let x1 = clamp_x(((rect.x + rect.width - f64::from(px)) * sx).round());
    let y1 = clamp_y(((rect.y + rect.height - f64::from(py)) * sy).round());
    let (width, height) = ((x1 - x0) as u32 & !1, (y1 - y0) as u32 & !1);
    (width >= 2 && height >= 2).then_some(PixelRect {
        x: x0 as u32,
        y: y0 as u32,
        width,
        height,
    })
}

/// Copies `region` of a packed 32-bit frame into a tightly packed BGRA frame
/// of `out` size, swizzling RGB-ordered input. The region is anchored
/// top-left: it is cropped if larger than `out` and padded with black if
/// smaller. Rows missing from a short `src` are left black.
/// Copies `region` of a 4-byte-per-pixel image into a tightly packed
/// `out`-sized one, padding with black if the region is smaller.
fn crop_pixels(src: &[u8], stride: usize, region: PixelRect, out: (u32, u32)) -> Vec<u8> {
    let (out_w, out_h) = (out.0 as usize, out.1 as usize);
    let w = (region.width as usize).min(out_w);
    let h = (region.height as usize).min(out_h);
    let rows = (0..h).map_while(|row| {
        let start = (region.y as usize + row) * stride + region.x as usize * 4;
        src.get(start..start + w * 4)
    });
    if (w, h) == (out_w, out_h) {
        let mut dst = Vec::with_capacity(out_w * out_h * 4);
        rows.for_each(|line| dst.extend_from_slice(line));
        // A short source leaves the rest black.
        dst.resize(out_w * out_h * 4, 0);
        return dst;
    }
    let mut dst = vec![0u8; out_w * out_h * 4];
    for (row, line) in rows.enumerate() {
        dst[row * out_w * 4..][..w * 4].copy_from_slice(line);
    }
    dst
}

/// Drops frames that arrive faster than `fps`, with a quarter-interval of
/// tolerance for jitter so a 60 Hz source limited to 30 fps keeps every other
/// frame.
#[derive(Debug)]
struct FrameLimiter {
    interval: Micros,
    next: Option<Micros>,
}

impl FrameLimiter {
    fn new(fps: u32) -> Self {
        Self {
            interval: 1_000_000 / Micros::from(fps.max(1)),
            next: None,
        }
    }

    fn accept(&mut self, ts: Micros) -> bool {
        if self.next.is_some_and(|next| ts + self.interval / 4 < next) {
            return false;
        }
        // Stay on the ideal grid unless we fell more than a frame behind.
        let base = match self.next {
            Some(next) if ts < next + self.interval => next,
            _ => ts,
        };
        self.next = Some(base + self.interval);
        true
    }
}

/// A buffer's `spa_meta_header.pts` (CLOCK_MONOTONIC ns) in host-clock µs, or
/// `now` if it is missing or implausible.
fn pts_to_host(pts_ns: i64, now: Micros) -> Micros {
    if pts_ns <= 0 {
        return now;
    }
    let us = (pts_ns / 1_000) as Micros;
    if us.abs_diff(now) <= MAX_PTS_SKEW {
        us
    } else {
        now
    }
}

fn f32_le_samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|&b| f32::from_le_bytes(b))
        .collect()
}

fn even(n: u32) -> u32 {
    (n & !1).max(2)
}

/// The parent pid from the contents of `/proc/<pid>/stat`.
fn parse_ppid(stat: &str) -> Option<i32> {
    // "pid (comm) state ppid ..."; comm may contain spaces and parentheses.
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// Whether `pid` is `ancestor` or one of its descendants.
fn is_same_or_descendant(mut pid: i32, ancestor: i32) -> bool {
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        match std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .as_deref()
            .and_then(parse_ppid)
        {
            Some(ppid) => pid = ppid,
            None => return false,
        }
    }
    false
}

/// `"1920x1080"` (optionally with a suffix such as `i`) to pixels.
fn parse_drm_mode(line: &str) -> Option<(u32, u32)> {
    let (w, h) = line.trim().split_once('x')?;
    let h: String = h.chars().take_while(char::is_ascii_digit).collect();
    Some((w.parse().ok()?, h.parse().ok()?))
}

/// The preferred mode of the first connected DRM connector.
fn primary_drm_mode() -> Option<(u32, u32)> {
    let mut connectors: Vec<_> = std::fs::read_dir("/sys/class/drm")
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.join("modes").exists())
        .collect();
    connectors.sort();
    connectors.into_iter().find_map(|p| {
        let status = std::fs::read_to_string(p.join("status")).ok()?;
        if status.trim() != "connected" {
            return None;
        }
        let modes = std::fs::read_to_string(p.join("modes")).ok()?;
        parse_drm_mode(modes.lines().next()?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f64, y: f64, width: f64, height: f64) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn area_crop_is_monitor_relative_and_scaled() {
        // Second monitor at x=1920 (logical 1280x800 at 2x → 2560x1600 px).
        let px = area_crop(
            &rect(2020.0, 50.0, 300.0, 201.0),
            Some((1920, 0)),
            Some((1280, 800)),
            (2560, 1600),
        );
        assert_eq!(
            px,
            Some(PixelRect {
                x: 200,
                y: 100,
                width: 600,
                height: 402
            })
        );
    }

    #[test]
    fn area_crop_clamps_and_evens() {
        let px = area_crop(&rect(-10.0, 1000.0, 105.0, 200.0), None, None, (1920, 1080));
        assert_eq!(
            px,
            Some(PixelRect {
                x: 0,
                y: 1000,
                width: 94,
                height: 80
            })
        );
        assert_eq!(
            area_crop(&rect(5000.0, 0.0, 10.0, 10.0), None, None, (1920, 1080)),
            None
        );
    }

    #[test]
    fn crop_keeps_byte_order_and_crops_region() {
        // 3x2 RGBA frame with stride padding of 4 bytes.
        let stride = 16;
        let mut src = vec![0u8; stride * 2];
        for y in 0..2 {
            for x in 0..3 {
                let i = y * stride + x * 4;
                src[i..i + 4].copy_from_slice(&[10 * x as u8, 1, 100 + y as u8, 255]);
            }
        }
        let region = PixelRect {
            x: 1,
            y: 0,
            width: 2,
            height: 2,
        };
        let out = crop_pixels(&src, stride, region, (2, 2));
        assert_eq!(
            out,
            [
                10, 1, 100, 255, 20, 1, 100, 255, //
                10, 1, 101, 255, 20, 1, 101, 255,
            ]
        );
    }

    #[test]
    fn copy_pads_smaller_region_with_black() {
        let src = vec![7u8; 2 * 2 * 4];
        let full = PixelRect {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let out = crop_pixels(&src, 8, full, (4, 3));
        assert_eq!(out.len(), 4 * 3 * 4);
        assert_eq!(&out[..8], &[7; 8]);
        assert_eq!(&out[8..16], &[0; 8]);
        assert!(out[32..].iter().all(|&b| b == 0));
    }

    #[test]
    fn copy_tolerates_short_source() {
        let src = vec![1u8; 4 * 4]; // only one row of a 4-wide frame
        let full = PixelRect {
            x: 0,
            y: 0,
            width: 4,
            height: 2,
        };
        let out = crop_pixels(&src, 16, full, (4, 2));
        assert!(out[..16].iter().all(|&b| b == 1));
        assert!(out[16..].iter().all(|&b| b == 0));
    }

    #[test]
    fn limiter_halves_60hz_to_30fps() {
        let mut l = FrameLimiter::new(30);
        let kept: Vec<_> = (0..12)
            .map(|i| i * 16_667)
            .filter(|&ts| l.accept(ts))
            .collect();
        assert_eq!(kept.len(), 6, "{kept:?}");
        assert_eq!(kept[..3], [0, 33_334, 66_668]);
    }

    #[test]
    fn limiter_passes_slower_sources_and_recovers_after_gaps() {
        let mut l = FrameLimiter::new(60);
        assert!((0..10).all(|i| l.accept(i * 33_333)));
        assert!(l.accept(5_000_000));
        assert!(!l.accept(5_001_000));
        assert!(l.accept(5_016_667));
    }

    #[test]
    fn pts_conversion_uses_monotonic_ns_or_falls_back() {
        let now = 10_000_000;
        assert_eq!(pts_to_host(9_990_000_123, now), 9_990_000);
        assert_eq!(pts_to_host(0, now), now);
        assert_eq!(pts_to_host(-5, now), now);
        // Not CLOCK_MONOTONIC (e.g. realtime): far off, ignored.
        assert_eq!(pts_to_host(1_700_000_000_000_000_000, now), now);
    }

    #[test]
    fn f32_samples_decode_little_endian() {
        let bytes: Vec<u8> = [0.5f32, -1.0]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .chain([1, 2])
            .collect();
        assert_eq!(f32_le_samples(&bytes), [0.5, -1.0]);
    }

    #[test]
    fn links_match_channels_and_fan_out_mono() {
        let ins = [(10, "FL"), (11, "FR")];
        assert_eq!(
            plan_links(&[(1, "FL"), (2, "FR"), (3, "LFE")], &ins),
            [(1, 10), (2, 11)]
        );
        assert_eq!(plan_links(&[(4, "MONO")], &ins), [(4, 10), (4, 11)]);
        assert_eq!(plan_links(&[(5, "")], &ins), [(5, 10), (5, 11)]);
    }

    #[test]
    fn parses_proc_stat_ppid() {
        assert_eq!(parse_ppid("1234 (my (weird) app) S 42 1234 ..."), Some(42));
        assert_eq!(parse_ppid("garbage"), None);
        let me = std::process::id() as i32;
        assert!(is_same_or_descendant(me, me));
        // Every process descends from init.
        assert!(is_same_or_descendant(me, 1) || !std::path::Path::new("/proc/1").exists());
    }

    #[test]
    fn parses_drm_modes() {
        assert_eq!(parse_drm_mode("2560x1440\n"), Some((2560, 1440)));
        assert_eq!(parse_drm_mode("1920x1080i"), Some((1920, 1080)));
        assert_eq!(parse_drm_mode("bogus"), None);
    }

    #[test]
    fn format_pods_serialize() {
        for object in [
            video_format_object(30),
            buffers_object(),
            meta_object::<MetaHeader>(),
            meta_object::<MetaVideoCrop>(),
        ] {
            let bytes = serialize_pod(object).unwrap();
            assert!(Pod::from_bytes(&bytes).is_some());
        }
    }
}
