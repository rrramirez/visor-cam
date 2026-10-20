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
    format::sample::Type, frame,
};
use visor_core::{EditList, Micros, Project};

use crate::audio_writer::{CHANNELS, SAMPLE_RATE};
use crate::composite::Bubble;
use crate::decode::{AudioReader, VideoReader};
use crate::tempo::{self, Tempo};
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
}

impl ExportOptions {
    pub fn new(format: ExportFormat) -> Self {
        Self {
            format,
            max_height: None,
            gif_fps: 15,
            gif_max_width: 960,
        }
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
    let screen_track = project
        .tracks
        .screen
        .as_ref()
        .context("project has no screen track")?;
    let screen_path = dir.join(&screen_track.file);
    let (src_w, src_h) = VideoReader::open(&screen_path, None)?.size();

    let gif = opts.format == ExportFormat::Gif;
    let (out_w, out_h) = output_size(src_w, src_h, opts);
    let fps = if gif {
        opts.gif_fps.max(1)
    } else {
        screen_track.fps.unwrap_or(30).clamp(1, 120)
    };

    let screen = FrameCursor::new(VideoReader::open(
        &screen_path,
        Some((format::Pixel::YUV420P, out_w, out_h)),
    )?);
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

    let speed = project.speed.clamp(tempo::MIN_SPEED, tempo::MAX_SPEED);
    let total_frames =
        (u128::from(project.output_duration()) * u128::from(fps)).div_ceil(1_000_000) as u64;
    // Edit (cut-aware, before speed) time at the end of output frame `k`.
    let edit_us = |k: u64| (k as f64 * 1e6 / f64::from(fps) * speed) as Micros;
    let mut renderer = Renderer {
        edit,
        fps,
        speed,
        screen,
        camera,
    };

    if gif {
        // Pass 1: build an optimal palette over every frame.
        let mut palette_graph = gif_palette_graph(out_w, out_h, fps)?;
        for k in 0..total_frames {
            check(cancel)?;
            let f = renderer.render(k)?;
            palette_graph.get("in").unwrap().source().add(&f)?;
            progress(0.5 * (k + 1) as f32 / total_frames as f32);
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
        graph.get("pal").unwrap().source().add(&palette)?;
        graph.get("pal").unwrap().source().flush()?;
        let mut mapped = frame::Video::empty();
        for k in 0..total_frames {
            check(cancel)?;
            let f = renderer.render(k)?;
            graph.get("in").unwrap().source().add(&f)?;
            while graph.get("out").unwrap().sink().frame(&mut mapped).is_ok() {
                muxer.video(&mapped)?;
            }
            progress(0.5 + 0.5 * (k + 1) as f32 / total_frames as f32);
        }
        graph.get("in").unwrap().source().flush()?;
        while graph.get("out").unwrap().sink().frame(&mut mapped).is_ok() {
            muxer.video(&mapped)?;
        }
        return muxer.finish();
    }

    let mut muxer = Muxer::new(out, opts.format, out_w, out_h, fps, audio.is_some())?;
    let segments = SampleMap::new(edit);
    let mut tempo = Tempo::new(speed)?;
    let mut stretched = Vec::new();
    // Edit-time samples read so far (before the speed change).
    let mut audio_read = 0i64;
    for k in 0..total_frames {
        check(cancel)?;
        muxer.video(&renderer.render(k)?)?;
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

/// Produces composited output frames (YUV420P at output size).
struct Renderer<'a> {
    edit: &'a EditList,
    fps: u32,
    speed: f64,
    screen: FrameCursor,
    camera: Option<(FrameCursor, Bubble)>,
}

impl Renderer<'_> {
    /// Output frame `k`, stamped with pts `k`.
    fn render(&mut self, k: u64) -> anyhow::Result<frame::Video> {
        // Output frame time, sped up or slowed down into edit time.
        let t_out = (k as f64 * 1e6 / f64::from(self.fps) * self.speed) as Micros;
        let t_src = self
            .edit
            .to_source(t_out)
            .unwrap_or_else(|| self.edit.ranges().last().map_or(0, |r| r.1));
        let mut composed = self
            .screen
            .at(t_src)?
            .context("screen track has no frames")?
            .clone();
        if let Some((cam, bubble)) = &mut self.camera
            && let Some(cam_frame) = cam.at(t_src)?
        {
            bubble.blend(&mut composed, cam_frame);
        }
        composed.set_pts(Some(k as i64));
        Ok(composed)
    }

    fn reset(&mut self) -> anyhow::Result<()> {
        self.screen.reset()?;
        if let Some((cam, _)) = &mut self.camera {
            cam.reset()?;
        }
        Ok(())
    }
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

        let mut opts = Dictionary::new();
        if fmt == ExportFormat::Mp4 {
            opts.set("movflags", "+faststart");
        }
        output.write_header_with(opts)?;
        let video_stream_tb = output.stream(0).context("no video stream")?.time_base();
        let mut muxer = Self {
            output,
            video,
            video_tb,
            video_stream_tb,
            audio,
        };
        if let Some(a) = &mut muxer.audio {
            a.stream_tb = muxer
                .output
                .stream(1)
                .context("no audio stream")?
                .time_base();
        }
        Ok(muxer)
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
            opts.set("preset", "fast");
            opts.set("crf", "22");
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
