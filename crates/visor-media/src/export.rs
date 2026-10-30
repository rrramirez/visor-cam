//! Renders a project's edit list to a single file: MP4 (H.264 + AAC), WebM
//! (VP9 or AV1 + Opus) or GIF.
//!
//! Output frames are produced at a constant rate along the *output* timeline;
//! each is mapped back to source time through the edit list, so video and
//! audio stay aligned across any number of cuts. Audio is mapped sample by
//! sample.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, anyhow, bail};
use ffmpeg_next::{
    self as ffmpeg, ChannelLayout, Dictionary, Packet, Rational, codec, encoder, filter, format,
    format::sample::Type, frame, software::scaling,
};
use visor_core::{EditList, Micros, Project};

use crate::audio_writer::{CHANNELS, SAMPLE_RATE};
use crate::composite::Bubble;
use crate::decode::{AudioReader, VideoReader, new_ref, reuse_or_alloc};
use crate::tempo::Tempo;
use crate::timeline::{AudioMix, FrameCursor, SampleMap, micros_to_samples};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    /// H.264 + AAC.
    Mp4,
    /// VP9 (or AV1) + Opus.
    WebM {
        av1: bool,
    },
    Gif,
}

impl ExportFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::WebM { .. } => "webm",
            Self::Gif => "gif",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// Downscale video taller than this (MP4/WebM). `None` keeps the source size.
    pub max_height: Option<u32>,
    pub gif_fps: u32,
    pub gif_max_width: u32,
    pub audio: AudioCleanup,
}

impl ExportOptions {
    pub fn new(format: ExportFormat) -> Self {
        Self {
            format,
            max_height: None,
            gif_fps: 15,
            gif_max_width: 960,
            audio: AudioCleanup::default(),
        }
    }
}

/// Optional fixes for the exported sound.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioCleanup {
    /// Take out rumble and steady background noise (fans, hum, hiss).
    pub denoise: bool,
    /// Bring the loudness to a steady -16 LUFS, the usual level for speech
    /// online, so quiet and loud passages and recordings sound alike.
    pub normalize: bool,
}

impl AudioCleanup {
    /// FFmpeg filters doing the cleanup, in order.
    pub fn filters(self) -> Vec<&'static str> {
        let mut filters = Vec::new();
        if self.denoise {
            // Rumble below speech, then spectral noise reduction that keeps
            // learning the noise floor as it changes.
            filters.extend(["highpass=f=80", "afftdn=nr=12:nf=-40:tn=1"]);
        }
        if self.normalize {
            filters.push("loudnorm=I=-16:TP=-1.5:LRA=11");
        }
        filters
    }
}

/// Exports `project` (stored in `dir`) to `out`. `progress` receives 0.0–1.0.
/// Setting `cancel` aborts; a partial file is deleted on error or cancel.
pub fn export(
    dir: &Path,
    project: &Project,
    out: &Path,
    opts: &ExportOptions,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    let result = run(dir, project, out, opts, progress, cancel);
    if result.is_err() {
        let _ = std::fs::remove_file(out);
    }
    result
}

fn run(
    dir: &Path,
    project: &Project,
    out: &Path,
    opts: &ExportOptions,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    ffmpeg::init()?;
    let edit = &project.edit;
    if edit.duration() == 0 {
        bail!("nothing to export: the edit keeps no time");
    }
    let gif = opts.format == ExportFormat::Gif;
    let (mut renderer, (out_w, out_h)) = Renderer::open(dir, project, opts)?;
    let fps = renderer.fps;
    let mut audio = if gif {
        None
    } else {
        let tracks: Vec<_> = [&project.tracks.mic, &project.tracks.system]
            .into_iter()
            .flatten()
            .map(|t| AudioReader::open(&dir.join(&t.file)))
            .collect::<anyhow::Result<_>>()?;
        (!tracks.is_empty()).then(|| AudioMix::new(tracks))
    };

    let speed = project.speed();
    let total_frames =
        (u128::from(project.output_duration()) * u128::from(fps)).div_ceil(1_000_000) as u64;
    // Edit (cut-aware, before speed) time at the end of output frame `k`.
    let edit_us = |k: u64| (k as f64 * 1e6 / f64::from(fps) * speed) as Micros;

    if gif {
        // Pass 1: build a palette from a sample of the frames; neighbouring
        // frames of screen video add almost no new colors.
        let mut palette_graph = gif_palette_graph(out_w, out_h, fps)?;
        for k in (0..total_frames).step_by(GIF_PALETTE_STEP) {
            check(cancel)?;
            let f = new_ref(renderer.render(k)?)?;
            palette_graph.get("in").unwrap().source().add(&f)?;
            progress(0.25 * (k + 1) as f32 / total_frames as f32);
        }
        palette_graph.get("in").unwrap().source().flush()?;
        let mut palette = frame::Video::empty();
        palette_graph
            .get("out")
            .unwrap()
            .sink()
            .frame(&mut palette)
            .context("palettegen produced nothing")?;

        // Pass 2: map frames onto the palette and encode.
        renderer.reset()?;
        let mut graph = gif_paletteuse_graph(out_w, out_h, fps, &palette)?;
        let mut muxer = Muxer::new(out, opts.format, out_w, out_h, fps, false)?;
        muxer.start()?;
        graph.get("pal").unwrap().source().add(&palette)?;
        graph.get("pal").unwrap().source().flush()?;
        let mut mapped = frame::Video::empty();
        for k in 0..total_frames {
            check(cancel)?;
            let f = new_ref(renderer.render(k)?)?;
            graph.get("in").unwrap().source().add(&f)?;
            while graph.get("out").unwrap().sink().frame(&mut mapped).is_ok() {
                muxer.video(&mapped)?;
            }
            progress(0.25 + 0.75 * (k + 1) as f32 / total_frames as f32);
        }
        graph.get("in").unwrap().source().flush()?;
        while graph.get("out").unwrap().sink().frame(&mut mapped).is_ok() {
            muxer.video(&mapped)?;
        }
        return muxer.finish();
    }

    // MP4s carry the project's thumbnail as cover art, which file managers
    // and players show for the file.
    // A nicety: the export goes ahead without it if it can't be made.
    let cover = if opts.format == ExportFormat::Mp4 {
        let t = thumbnail_time(project);
        let jpeg = renderer.render_source(t).and_then(|f| encode_jpeg(f, 2));
        renderer.reset()?;
        jpeg.inspect_err(|e| tracing::warn!("exporting without cover art: {e:#}"))
            .ok()
    } else {
        None
    };
    let mut muxer = Muxer::new(out, opts.format, out_w, out_h, fps, audio.is_some())?;
    if let Some(jpeg) = cover {
        muxer.cover(jpeg)?;
    }
    // Markers become chapters, which players list to jump between. The
    // last one runs to the end of the last frame.
    let mut chapters = project.chapters();
    if let Some(last) = chapters.last_mut() {
        last.end_us = last.end_us.max(total_frames * 1_000_000 / u64::from(fps));
    }
    muxer.chapters(&chapters)?;
    muxer.start()?;
    let segments = SampleMap::new(edit);
    let mut tempo = Tempo::with_filters(speed, &opts.audio.filters())?;
    let mut stretched = Vec::new();
    // Edit-time samples read so far (before the speed change).
    let mut audio_read = 0i64;
    // Frames are rendered (decode, scale, composite) on their own thread,
    // so that work overlaps with encoding.
    std::thread::scope(|scope| -> anyhow::Result<()> {
        let (tx, rx) = crossbeam_channel::bounded(RENDER_AHEAD);
        scope.spawn(move || {
            for k in 0..total_frames {
                let frame = renderer.render(k).and_then(new_ref);
                let failed = frame.is_err();
                // Stops when the encoder side has given up.
                if tx.send(frame).is_err() || failed || cancel.load(Ordering::Relaxed) {
                    break;
                }
            }
        });
        for k in 0..total_frames {
            check(cancel)?;
            let frame = rx.recv().context("rendering stopped")??;
            muxer.video(&frame)?;
            drop(frame);
            if let Some(mix) = &mut audio {
                // Audio through the end of this frame.
                let end = segments
                    .total
                    .min(micros_to_samples(edit_us(k + 1)))
                    .max(audio_read);
                for (src, len) in segments.pieces(audio_read, end - audio_read) {
                    stretched.clear();
                    tempo.process(&mix.read(src, len)?, &mut stretched)?;
                    muxer.audio(&stretched)?;
                }
                audio_read = end;
            }
            progress((k + 1) as f32 / total_frames as f32);
        }
        Ok(())
    })?;
    if let Some(mix) = &mut audio {
        stretched.clear();
        for (src, len) in segments.pieces(audio_read, segments.total - audio_read) {
            tempo.process(&mix.read(src, len)?, &mut stretched)?;
        }
        tempo.flush(&mut stretched)?;
        muxer.audio(&stretched)?;
    }
    muxer.finish()
}

/// Frames rendered ahead of the encoder.
const RENDER_AHEAD: usize = 3;

/// GIF palettes are built from every this many frames.
const GIF_PALETTE_STEP: usize = 3;

/// Produces composited output frames (YUV420P at output size).
struct Renderer<'a> {
    edit: &'a EditList,
    blurs: &'a [visor_core::Blur],
    fps: u32,
    speed: f64,
    screen: FrameCursor,
    camera: Option<(FrameCursor, Bubble)>,
    /// Screen frame with the camera bubble on it; reused across frames.
    composed: frame::Video,
    /// The screen frame with blurs on it, and which frame (its source time)
    /// under which blurs, so a frame shown again isn't blurred again.
    blurred: frame::Video,
    blurred_key: Option<(Micros, Vec<usize>)>,
}

impl<'a> Renderer<'a> {
    /// A renderer for `project`'s export with `opts`, and the output size.
    fn open(
        dir: &Path,
        project: &'a Project,
        opts: &ExportOptions,
    ) -> anyhow::Result<(Self, (u32, u32))> {
        let screen_track = project
            .tracks
            .screen
            .as_ref()
            .context("project has no screen track")?;
        let screen_path = dir.join(&screen_track.file);
        let (src_w, src_h) = VideoReader::open(&screen_path, None)?.size();
        let (out_w, out_h) = output_size(src_w, src_h, opts);
        let fps = if opts.format == ExportFormat::Gif {
            opts.gif_fps.max(1)
        } else {
            screen_track.fps.unwrap_or(30).clamp(1, 120)
        };

        let mut screen_reader =
            VideoReader::open(&screen_path, Some((format::Pixel::YUV420P, out_w, out_h)))?;
        // Half the cores, like the encoder.
        screen_reader.set_scaling(scaling::Flags::BILINEAR, encoder_threads());
        let camera = match &project.tracks.camera {
            Some(track) => match Bubble::new(&project.camera_overlay, out_w, out_h) {
                Some(bubble) => {
                    let path = dir.join(&track.file);
                    let (cw, ch) = VideoReader::open(&path, None)?.size();
                    let (sw, sh) = bubble.camera_scale(cw, ch);
                    let reader = VideoReader::open(&path, Some((format::Pixel::YUV420P, sw, sh)))?;
                    Some((FrameCursor::new(reader), bubble))
                }
                None => None,
            },
            None => None,
        };
        let renderer = Self {
            edit: &project.edit,
            blurs: &project.blurs,
            fps,
            speed: project.speed(),
            screen: FrameCursor::new(screen_reader),
            camera,
            composed: frame::Video::empty(),
            blurred: frame::Video::empty(),
            blurred_key: None,
        };
        Ok((renderer, (out_w, out_h)))
    }

    /// Output frame `k`, stamped with pts `k`.
    fn render(&mut self, k: u64) -> anyhow::Result<&frame::Video> {
        // Output frame time, sped up or slowed down into edit time.
        let t_out = (k as f64 * 1e6 / f64::from(self.fps) * self.speed) as Micros;
        let t_src = self
            .edit
            .to_source(t_out)
            .unwrap_or_else(|| self.edit.ranges().last().map_or(0, |r| r.1));
        let frame = self.render_source(t_src)?;
        frame.set_pts(Some(k as i64));
        Ok(frame)
    }

    /// The composited frame shown at source time `t_src`: blurred areas,
    /// then the camera bubble on top.
    fn render_source(&mut self, t_src: Micros) -> anyhow::Result<&mut frame::Video> {
        let (pts, next) = self
            .screen
            .span_at(t_src)?
            .context("screen track has no frames")?;
        // Blurred by when the frame is up, not the time asked for.
        let active: Vec<usize> = (0..self.blurs.len())
            .filter(|&i| self.blurs[i].covers_frame(pts, next))
            .collect();
        let (_, screen) = self
            .screen
            .at(t_src)?
            .context("screen track has no frames")?;
        let camera = match &mut self.camera {
            Some((cam, bubble)) => cam.at(t_src)?.map(|(_, f)| (&*bubble, f)),
            None => None,
        };
        // The same frame under the same blurs is blurred once, as a still
        // screen's frame is shown again and again.
        let blurred = !active.is_empty();
        let key = (pts, active);
        if blurred && self.blurred_key.as_ref() != Some(&key) {
            copy_frame(&mut self.blurred, screen);
            let on: Vec<_> = key.1.iter().map(|&i| &self.blurs[i]).collect();
            crate::blur::blur_yuv420p(&mut self.blurred, &on);
            self.blurred_key = Some(key);
        }
        // Passed on as is if no bubble goes on it; else copied, as the same
        // frame may be shown again.
        let Some((bubble, cam_frame)) = camera else {
            return Ok(if blurred { &mut self.blurred } else { screen });
        };
        copy_frame(
            &mut self.composed,
            if blurred { &self.blurred } else { screen },
        );
        bubble.blend(&mut self.composed, cam_frame);
        Ok(&mut self.composed)
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        self.screen.reset()?;
        if let Some((cam, _)) = &mut self.camera {
            cam.reset()?;
        }
        Ok(())
    }
}

/// Copies `src` into `dst`, reusing `dst`'s buffer when it fits and nothing
/// else holds it.
fn copy_frame(dst: &mut frame::Video, src: &frame::Video) {
    reuse_or_alloc(dst, src.format(), src.width(), src.height());
    dst.clone_from(src);
}

/// Source time of the project's thumbnail: the chosen frame while it's still
/// in the edit, otherwise the first frame kept.
pub fn thumbnail_time(project: &Project) -> Micros {
    let first = project.edit.ranges().first().map_or(0, |r| r.0);
    project
        .thumbnail_us
        .filter(|&t| project.edit.to_output(t).is_some())
        .unwrap_or(first)
}

/// Saves the project's thumbnail, composited like the export (camera bubble
/// included) at the recording's full size, as a JPEG at `out`.
pub fn save_thumbnail(dir: &Path, project: &Project, out: &Path) -> anyhow::Result<()> {
    let jpeg = render_jpeg(dir, project, thumbnail_time(project), None, 2)?;
    std::fs::write(out, jpeg).with_context(|| format!("writing {}", out.display()))
}

/// The frame at source time `t`, cut or not, composited like the export
/// and at most `max_height` tall, as a JPEG file's bytes: for an agent to
/// see what was recorded. Good enough to read text, and a fraction of the
/// thumbnail's size.
pub fn frame_jpeg(
    dir: &Path,
    project: &Project,
    t: Micros,
    max_height: u32,
) -> anyhow::Result<Vec<u8>> {
    render_jpeg(dir, project, t, Some(max_height), 5)
}

/// The frame at source time `t`, composited like the export (camera bubble
/// included), as a JPEG file's bytes at `quality` (see [`encode_jpeg`]).
fn render_jpeg(
    dir: &Path,
    project: &Project,
    t: Micros,
    max_height: Option<u32>,
    quality: i32,
) -> anyhow::Result<Vec<u8>> {
    ffmpeg::init()?;
    let mut opts = ExportOptions::new(ExportFormat::Mp4);
    opts.max_height = max_height;
    let mut renderer = Renderer::open(dir, project, &opts)?.0;
    encode_jpeg(renderer.render_source(t)?, quality)
}

/// One frame (video-range YUV 4:2:0) as a JPEG file's bytes, at `quality`
/// (FFmpeg's qscale: 2 is best, higher is smaller).
fn encode_jpeg(f: &frame::Video, quality: i32) -> anyhow::Result<Vec<u8>> {
    let codec = encoder::find(codec::Id::MJPEG).context("FFmpeg has no JPEG encoder")?;
    let (w, h) = (f.width(), f.height());
    // JPEG viewers assume full range: stretch video range (16–235) to
    // 0–255, or blacks would show grey and whites dim.
    let mut full = frame::Video::empty();
    scaling::Context::get(
        f.format(),
        w,
        h,
        format::Pixel::YUVJ420P,
        w,
        h,
        scaling::Flags::POINT,
    )?
    .run(f, &mut full)?;
    full.set_pts(Some(0));
    let mut enc = codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()?;
    enc.set_width(w);
    enc.set_height(h);
    enc.set_format(format::Pixel::YUVJ420P);
    enc.set_color_range(ffmpeg::color::Range::JPEG);
    enc.set_time_base(Rational(1, 30));
    enc.set_flags(codec::Flags::QSCALE);
    enc.set_global_quality(quality * ffmpeg::ffi::FF_QP2LAMBDA);
    let mut enc = enc.open()?;
    enc.send_frame(&full)?;
    enc.send_eof()?;
    let mut packet = Packet::empty();
    enc.receive_packet(&mut packet)
        .context("the JPEG encoder produced nothing")?;
    Ok(packet.data().context("empty JPEG")?.to_vec())
}

fn check(cancel: &AtomicBool) -> anyhow::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(anyhow!("export cancelled"))
    } else {
        Ok(())
    }
}

fn output_size(w: u32, h: u32, opts: &ExportOptions) -> (u32, u32) {
    let even = |v: u32| (v & !1).max(2);
    if opts.format == ExportFormat::Gif {
        let ow = w.min(opts.gif_max_width);
        return (
            even(ow),
            even((u64::from(h) * u64::from(ow) / u64::from(w.max(1))) as u32),
        );
    }
    match opts.max_height {
        Some(max) if h > max => (
            even((u64::from(w) * u64::from(max) / u64::from(h)) as u32),
            even(max),
        ),
        _ => (even(w), even(h)),
    }
}

struct Muxer {
    output: format::context::Output,
    video: encoder::Video,
    video_tb: Rational,
    video_stream_tb: Rational,
    audio: Option<AudioOut>,
    format: ExportFormat,
    /// Cover art stream index and JPEG, written once the header is.
    cover: Option<(usize, Vec<u8>)>,
}

struct AudioOut {
    encoder: encoder::Audio,
    stream_tb: Rational,
    frame_size: usize,
    planar: bool,
    pending: Vec<f32>,
    pts: i64,
}

// SAFETY: owned FFmpeg objects without thread affinity, used by one thread.
unsafe impl Send for Muxer {}

impl Muxer {
    fn new(
        path: &Path,
        fmt: ExportFormat,
        width: u32,
        height: u32,
        fps: u32,
        with_audio: bool,
    ) -> anyhow::Result<Self> {
        let mut output =
            format::output(path).with_context(|| format!("creating {}", path.display()))?;
        let global_header = output
            .format()
            .flags()
            .contains(format::Flags::GLOBAL_HEADER);
        let video_tb = Rational(1, fps as i32);

        let candidates: &[&str] = match fmt {
            ExportFormat::Mp4 => &[
                "libx264",
                "h264_videotoolbox",
                "h264_nvenc",
                "h264_qsv",
                "h264_vaapi",
            ],
            ExportFormat::WebM { av1: false } => &["libvpx-vp9"],
            ExportFormat::WebM { av1: true } => &["libsvtav1", "libaom-av1"],
            ExportFormat::Gif => &["gif"],
        };
        let mut chosen = None;
        let mut last_err = None;
        for &name in candidates {
            let Some(codec) = encoder::find_by_name(name) else {
                continue;
            };
            match open_video(codec, name, fmt, width, height, fps, global_header) {
                Ok(enc) => {
                    chosen = Some((codec, enc));
                    break;
                }
                Err(e) => last_err = Some(format!("{name}: {e}")),
            }
        }
        let (codec, video) =
            chosen.ok_or_else(|| anyhow!("no usable video encoder for {fmt:?} ({last_err:?})"))?;
        let mut stream = output.add_stream(codec)?;
        stream.set_parameters(&video);
        stream.set_time_base(video_tb);

        let audio = if with_audio {
            let (name, planar) = match fmt {
                ExportFormat::WebM { .. } => ("libopus", false),
                _ => ("aac", true),
            };
            let codec = encoder::find_by_name(name).ok_or_else(|| anyhow!("no {name} encoder"))?;
            let mut enc = codec::context::Context::new_with_codec(codec)
                .encoder()
                .audio()?;
            enc.set_rate(SAMPLE_RATE as i32);
            enc.set_channel_layout(ChannelLayout::default(i32::from(CHANNELS)));
            enc.set_format(format::Sample::F32(if planar {
                Type::Planar
            } else {
                Type::Packed
            }));
            enc.set_bit_rate(if planar { 160_000 } else { 128_000 });
            enc.set_time_base(Rational(1, SAMPLE_RATE as i32));
            if global_header {
                enc.set_flags(codec::Flags::GLOBAL_HEADER);
            }
            let encoder = enc.open_as_with(codec, Dictionary::new())?;
            let frame_size = match encoder.frame_size() {
                0 => 1024,
                n => n as usize,
            };
            let mut stream = output.add_stream(codec)?;
            stream.set_parameters(&encoder);
            stream.set_time_base(Rational(1, SAMPLE_RATE as i32));
            Some(AudioOut {
                encoder,
                stream_tb: Rational(1, 1),
                frame_size,
                planar,
                pending: Vec::new(),
                pts: 0,
            })
        } else {
            None
        };

        Ok(Self {
            output,
            video,
            video_tb,
            video_stream_tb: video_tb,
            audio,
            format: fmt,
            cover: None,
        })
    }

    /// Adds `jpeg` as the file's cover art. Call before [`start`](Self::start).
    fn cover(&mut self, jpeg: Vec<u8>) -> anyhow::Result<()> {
        let mut stream = self.output.add_stream(encoder::find(codec::Id::MJPEG))?;
        // SAFETY: the stream's own parameters and disposition, set before the
        // header is written, as FFmpeg expects for attached pictures.
        unsafe {
            let st = stream.as_mut_ptr();
            let par = (*st).codecpar;
            (*par).codec_type = ffmpeg::ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
            (*par).codec_id = ffmpeg::ffi::AVCodecID::AV_CODEC_ID_MJPEG;
            (*par).width = self.video.width() as i32;
            (*par).height = self.video.height() as i32;
            (*st).disposition |= ffmpeg::ffi::AV_DISPOSITION_ATTACHED_PIC;
        }
        self.cover = Some((stream.index(), jpeg));
        Ok(())
    }

    /// Adds chapters. Call before [`start`](Self::start). MP4s keep them
    /// as QuickTime does, in a text track of their own (disabled, so it
    /// isn't shown as subtitles), and as Nero chapters.
    fn chapters(&mut self, chapters: &[visor_core::Chapter]) -> anyhow::Result<()> {
        for (id, c) in (1..).zip(chapters) {
            self.output.add_chapter(
                id,
                Rational(1, 1_000_000),
                c.start_us as i64,
                c.end_us as i64,
                &c.title,
            )?;
        }
        Ok(())
    }

    /// Writes the header (and cover art); frames can follow.
    fn start(&mut self) -> anyhow::Result<()> {
        let mut opts = Dictionary::new();
        if self.format == ExportFormat::Mp4 {
            opts.set("movflags", "+faststart");
        }
        self.output.write_header_with(opts)?;
        self.video_stream_tb = self
            .output
            .stream(0)
            .context("no video stream")?
            .time_base();
        if let Some(a) = &mut self.audio {
            a.stream_tb = self
                .output
                .stream(1)
                .context("no audio stream")?
                .time_base();
        }
        if let Some((index, jpeg)) = self.cover.take() {
            let mut packet = Packet::copy(&jpeg);
            packet.set_stream(index);
            packet.set_flags(ffmpeg::packet::Flags::KEY);
            packet.write_interleaved(&mut self.output)?;
        }
        Ok(())
    }

    fn video(&mut self, f: &frame::Video) -> anyhow::Result<()> {
        self.video.send_frame(f)?;
        self.drain_video()
    }

    fn audio(&mut self, samples: &[f32]) -> anyhow::Result<()> {
        let Some(a) = &mut self.audio else {
            return Ok(());
        };
        a.pending.extend_from_slice(samples);
        let ch = usize::from(CHANNELS);
        while a.pending.len() >= a.frame_size * ch {
            Self::send_audio(a, a.frame_size, &mut self.output)?;
        }
        Ok(())
    }

    fn send_audio(
        a: &mut AudioOut,
        frames: usize,
        output: &mut format::context::Output,
    ) -> anyhow::Result<()> {
        let ch = usize::from(CHANNELS);
        let fmt = format::Sample::F32(if a.planar { Type::Planar } else { Type::Packed });
        let mut f = frame::Audio::new(fmt, frames, ChannelLayout::default(i32::from(CHANNELS)));
        f.set_rate(SAMPLE_RATE);
        f.set_pts(Some(a.pts));
        if a.planar {
            for c in 0..ch {
                for (i, s) in f.plane_mut::<f32>(c).iter_mut().enumerate().take(frames) {
                    *s = a.pending[i * ch + c];
                }
            }
        } else {
            // Packed: one plane holding all channels.
            let data = f.data_mut(0);
            for (i, s) in a.pending[..frames * ch].iter().enumerate() {
                data[i * 4..i * 4 + 4].copy_from_slice(&s.to_le_bytes());
            }
        }
        a.pending.drain(..frames * ch);
        a.pts += frames as i64;
        a.encoder.send_frame(&f)?;
        Self::drain_audio(a, output)
    }

    fn drain_video(&mut self) -> anyhow::Result<()> {
        let mut packet = Packet::empty();
        while self.video.receive_packet(&mut packet).is_ok() {
            packet.set_stream(0);
            packet.rescale_ts(self.video_tb, self.video_stream_tb);
            packet.write_interleaved(&mut self.output)?;
        }
        Ok(())
    }

    fn drain_audio(a: &mut AudioOut, output: &mut format::context::Output) -> anyhow::Result<()> {
        let mut packet = Packet::empty();
        while a.encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(1);
            packet.rescale_ts(Rational(1, SAMPLE_RATE as i32), a.stream_tb);
            packet.write_interleaved(output)?;
        }
        Ok(())
    }

    fn finish(mut self) -> anyhow::Result<()> {
        self.video.send_eof()?;
        self.drain_video()?;
        if let Some(a) = &mut self.audio {
            let rest = a.pending.len() / usize::from(CHANNELS);
            if rest > 0 {
                // Encoders with a fixed frame size need a full final frame.
                a.pending.resize(a.frame_size * usize::from(CHANNELS), 0.0);
                Self::send_audio(a, a.frame_size, &mut self.output)?;
            }
            a.encoder.send_eof()?;
            Self::drain_audio(a, &mut self.output)?;
        }
        self.output.write_trailer()?;
        Ok(())
    }
}

/// Encoder threads for software encoding: half the logical cores (at least 2).
fn encoder_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(4, |n| n.get() / 2)
        .max(2)
}

fn open_video(
    codec: ffmpeg::Codec,
    name: &str,
    fmt: ExportFormat,
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
    enc.set_time_base(Rational(1, fps as i32));
    enc.set_frame_rate(Some(Rational(fps as i32, 1)));
    enc.set_format(if fmt == ExportFormat::Gif {
        format::Pixel::PAL8
    } else {
        format::Pixel::YUV420P
    });
    if fmt != ExportFormat::Gif {
        enc.set_gop(fps * 4);
        enc.set_colorspace(ffmpeg::color::Space::BT709);
        enc.set_color_range(ffmpeg::color::Range::MPEG);
        enc.set_color_primaries(ffmpeg::color::Primaries::BT709);
        enc.set_color_transfer_characteristic(ffmpeg::color::TransferCharacteristic::BT709);
    }
    if global_header {
        enc.set_flags(codec::Flags::GLOBAL_HEADER);
    }
    let mut opts = Dictionary::new();
    match name {
        "libx264" => {
            // Screen video is mostly static: veryfast loses almost nothing
            // against slower presets there (SSIM 0.997 vs 0.998 on a real
            // recording) at about half the CPU, and the files come out
            // about as small.
            opts.set("preset", "veryfast");
            opts.set("crf", "22");
            // Half the cores: about as fast as using all of them, at half
            // the power, so the fans stay down during long exports.
            opts.set("threads", &encoder_threads().to_string());
        }
        "h264_videotoolbox" | "h264_nvenc" | "h264_qsv" | "h264_vaapi" => {
            enc.set_bit_rate((u64::from(width) * u64::from(height) * u64::from(fps) / 12) as usize);
            enc.set_max_b_frames(0);
        }
        "libvpx-vp9" => {
            enc.set_bit_rate(0);
            opts.set("crf", "33");
            opts.set("deadline", "good");
            opts.set("cpu-used", "4");
            opts.set("row-mt", "1");
        }
        "libsvtav1" => {
            opts.set("preset", "10");
            opts.set("crf", "35");
        }
        "libaom-av1" => {
            opts.set("cpu-used", "8");
            opts.set("crf", "35");
            opts.set("row-mt", "1");
        }
        _ => {}
    }
    enc.open_with(opts)
}

fn buffer_args(width: u32, height: u32, fmt: format::Pixel, fps: u32) -> String {
    let pix = ffmpeg::ffi::AVPixelFormat::from(fmt) as i32;
    format!("video_size={width}x{height}:pix_fmt={pix}:time_base=1/{fps}:pixel_aspect=1/1")
}

fn gif_palette_graph(width: u32, height: u32, fps: u32) -> anyhow::Result<filter::Graph> {
    let mut graph = filter::Graph::new();
    graph.add(
        &filter::find("buffer").unwrap(),
        "in",
        &buffer_args(width, height, format::Pixel::YUV420P, fps),
    )?;
    graph.add(&filter::find("buffersink").unwrap(), "out", "")?;
    graph
        .output("in", 0)?
        .input("out", 0)?
        .parse("palettegen=stats_mode=diff")?;
    graph.validate()?;
    Ok(graph)
}

fn gif_paletteuse_graph(
    width: u32,
    height: u32,
    fps: u32,
    palette: &frame::Video,
) -> anyhow::Result<filter::Graph> {
    let mut graph = filter::Graph::new();
    graph.add(
        &filter::find("buffer").unwrap(),
        "in",
        &buffer_args(width, height, format::Pixel::YUV420P, fps),
    )?;
    graph.add(
        &filter::find("buffer").unwrap(),
        "pal",
        &buffer_args(palette.width(), palette.height(), palette.format(), fps),
    )?;
    graph.add(&filter::find("buffersink").unwrap(), "out", "")?;
    graph
        .output("in", 0)?
        .output("pal", 0)?
        .input("out", 0)?
        .parse("[in][pal]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle")?;
    graph.validate()?;
    Ok(graph)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_size_respects_limits() {
        let mp4 = ExportOptions {
            max_height: Some(1080),
            ..ExportOptions::new(ExportFormat::Mp4)
        };
        assert_eq!(output_size(3456, 2234, &mp4), (1670, 1080));
        assert_eq!(output_size(1280, 720, &mp4), (1280, 720));
        let gif = ExportOptions::new(ExportFormat::Gif);
        assert_eq!(output_size(3456, 2234, &gif), (960, 620));
    }
}
