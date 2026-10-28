//! Edit-aware preview player for the editor.
//!
//! Positions are in *source* time so the timeline can show cut regions; while
//! playing, the player walks the *output* timeline (skipping cuts). A video
//! thread decodes screen and camera frames to RGBA at preview size and
//! publishes the latest ones; an audio thread mixes the audio tracks into a
//! cpal output stream. Both follow one wall clock started at the same
//! position, which keeps them within a few milliseconds over editor-length
//! sessions.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Context;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{Receiver, Sender, unbounded};
use ffmpeg_next::{format, frame, software::scaling};
use visor_core::{EditList, Micros, Project};

use crate::Resampler;
use crate::audio_writer::{CHANNELS, SAMPLE_RATE};
use crate::decode::{AudioReader, VideoReader};
use crate::tempo::{self, Tempo};
use crate::timeline::{AudioMix, FrameCursor, SampleMap, micros_to_samples};

/// Scaling for preview frames, on one thread: playback runs for minutes,
/// so CPU matters more than latency (seeks are dominated by decoding).
const PREVIEW_SCALING: scaling::Flags = scaling::Flags::FAST_BILINEAR;

/// An RGBA image ready for the UI.
#[derive(Clone)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

impl PreviewFrame {
    pub(crate) fn from_frame(f: &frame::Video) -> Self {
        let (w, h) = (f.width() as usize, f.height() as usize);
        let stride = f.stride(0);
        let mut rgba = Vec::with_capacity(w * h * 4);
        for row in 0..h {
            rgba.extend_from_slice(&f.data(0)[row * stride..row * stride + w * 4]);
        }
        Self {
            width: w as u32,
            height: h as u32,
            rgba,
        }
    }
}

#[derive(Debug)]
enum Cmd {
    Play,
    Pause,
    /// Source time and a sequence number reported back once rendered.
    Seek(Micros, u64),
    SetEdit(EditList),
    /// The clock already runs at the new speed; restart audio stretching.
    SetSpeed(f64),
}

#[derive(Default)]
struct Shared {
    /// Current source position.
    position: AtomicU64,
    playing: AtomicBool,
    /// Bumped whenever a new frame is published.
    generation: AtomicU64,
    /// Sequence number of the last seek the video thread has rendered.
    rendered_seek: AtomicU64,
    /// Frames published since the UI last took them.
    screen: Mutex<Option<Arc<PreviewFrame>>>,
    camera: Mutex<Option<Arc<PreviewFrame>>>,
}

pub struct Player {
    video_tx: Sender<Cmd>,
    audio_tx: Option<Sender<Cmd>>,
    shared: Arc<Shared>,
    clock: Arc<Mutex<Clock>>,
    seen_generation: u64,
    seeks_sent: u64,
    edit: EditList,
    duration: Micros,
}

impl Player {
    /// Opens the project's tracks. Screen frames are scaled to fit within
    /// `max_size`; camera frames to `camera_height` tall.
    pub fn open(
        dir: &Path,
        project: &Project,
        max_size: (u32, u32),
        camera_height: u32,
    ) -> anyhow::Result<Self> {
        let screen_track = project
            .tracks
            .screen
            .as_ref()
            .context("project has no screen track")?;
        let screen_path = dir.join(&screen_track.file);
        let (sw, sh) = VideoReader::open(&screen_path, None)?.size();
        let (pw, ph) = fit(sw, sh, max_size);
        let mut screen_reader =
            VideoReader::open(&screen_path, Some((format::Pixel::RGBA, pw, ph)))?;
        screen_reader.set_scaling(PREVIEW_SCALING, 1);
        let screen = FrameCursor::new(screen_reader);
        let camera = match &project.tracks.camera {
            Some(t) => {
                let path = dir.join(&t.file);
                let (cw, ch) = VideoReader::open(&path, None)?.size();
                let h = camera_height.min(ch).max(2) & !1;
                let w = ((u64::from(cw) * u64::from(h) / u64::from(ch.max(1))) as u32 + 1) & !1;
                let mut reader = VideoReader::open(&path, Some((format::Pixel::RGBA, w, h)))?;
                reader.set_scaling(PREVIEW_SCALING, 1);
                Some(FrameCursor::new(reader))
            }
            None => None,
        };
        let audio_paths: Vec<_> = [&project.tracks.mic, &project.tracks.system]
            .into_iter()
            .flatten()
            .map(|t| dir.join(&t.file))
            .collect();

        let shared = Arc::new(Shared::default());
        let speed = project.speed.clamp(tempo::MIN_SPEED, tempo::MAX_SPEED);
        let clock = Arc::new(Mutex::new(Clock::paused(0, speed)));
        let edit = project.edit.clone();

        let (video_tx, video_rx) = unbounded();
        {
            let (shared, clock, edit) = (shared.clone(), clock.clone(), edit.clone());
            thread::Builder::new()
                .name("visor-player-video".into())
                .spawn(move || video_loop(screen, camera, edit, shared, clock, video_rx))?;
        }
        let audio_tx = if audio_paths.is_empty() {
            None
        } else {
            let readers = audio_paths
                .iter()
                .map(|p| AudioReader::open(p))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let (tx, rx) = unbounded();
            let edit = edit.clone();
            let clock = clock.clone();
            thread::Builder::new()
                .name("visor-player-audio".into())
                .spawn(move || {
                    if let Err(e) = audio_loop(AudioMix::new(readers), edit, clock, rx) {
                        tracing::warn!("preview audio unavailable: {e:#}");
                    }
                })?;
            Some(tx)
        };

        let mut player = Self {
            video_tx,
            audio_tx,
            shared,
            clock,
            seen_generation: 0,
            seeks_sent: 0,
            duration: project.duration_us,
            edit,
        };
        player.seek(0);
        Ok(player)
    }

    pub fn play(&mut self) {
        // From the next kept time if a preview left it in a cut; from the
        // beginning when at the end.
        let pos = self.position();
        let from = snap_to_kept(&self.edit, pos);
        if self
            .edit
            .to_output(from)
            .is_none_or(|o| o + 50_000 >= self.edit.duration())
        {
            self.seek(self.edit.ranges().first().map_or(0, |r| r.0));
        } else if from != pos {
            self.seek(from);
        }
        self.send(Cmd::Play);
        self.shared.playing.store(true, Ordering::Relaxed);
    }

    pub fn pause(&mut self) {
        self.send(Cmd::Pause);
        self.shared.playing.store(false, Ordering::Relaxed);
    }

    pub fn toggle(&mut self) {
        if self.is_playing() {
            self.pause()
        } else {
            self.play()
        }
    }

    pub fn is_playing(&self) -> bool {
        self.shared.playing.load(Ordering::Relaxed)
    }

    /// Jumps to source time `t`. Inside a cut, lands on the next kept frame.
    pub fn seek(&mut self, t: Micros) {
        let t = snap_to_kept(&self.edit, t.min(self.duration));
        self.shared.position.store(t, Ordering::Relaxed);
        self.seeks_sent += 1;
        self.send(Cmd::Seek(t, self.seeks_sent));
    }

    /// Shows the frame at source time `t` even if it was cut, e.g. while a
    /// cut's edge is dragged, pausing playback. Playing from there starts
    /// at the next kept time.
    pub fn preview(&mut self, t: Micros) {
        if self.is_playing() {
            self.pause();
        }
        let t = t.min(self.duration.saturating_sub(1));
        self.shared.position.store(t, Ordering::Relaxed);
        self.seeks_sent += 1;
        self.send(Cmd::Seek(t, self.seeks_sent));
    }

    pub fn set_edit(&mut self, edit: EditList) {
        self.edit = edit.clone();
        if let Some(tx) = &self.audio_tx {
            let _ = tx.send(Cmd::SetEdit(edit.clone()));
        }
        let _ = self.video_tx.send(Cmd::SetEdit(edit));
        let pos = self.position();
        self.seek(pos);
    }

    /// Plays at `speed` (0.5–2.0) from now on, audio pitch unchanged.
    pub fn set_speed(&mut self, speed: f64) {
        let speed = speed.clamp(tempo::MIN_SPEED, tempo::MAX_SPEED);
        {
            let mut c = self.clock.lock().unwrap();
            *c = c.rebased(c.now(), speed);
        }
        self.send(Cmd::SetSpeed(speed));
    }

    /// Current source position.
    pub fn position(&self) -> Micros {
        self.shared.position.load(Ordering::Relaxed)
    }

    pub fn duration(&self) -> Micros {
        self.duration
    }

    /// Whether nothing is in flight: paused, with every seek rendered. The
    /// UI can stop polling until it next changes something.
    pub fn is_idle(&self) -> bool {
        !self.is_playing()
            && self.shared.rendered_seek.load(Ordering::Acquire) == self.seeks_sent
            && self.shared.generation.load(Ordering::Acquire) == self.seen_generation
    }

    /// Screen and camera frames that changed since the last call (`None`
    /// for one that didn't), or `None` if neither did.
    #[allow(clippy::type_complexity)]
    pub fn take_frames(
        &mut self,
    ) -> Option<(Option<Arc<PreviewFrame>>, Option<Arc<PreviewFrame>>)> {
        let generation = self.shared.generation.load(Ordering::Acquire);
        if generation == self.seen_generation {
            return None;
        }
        self.seen_generation = generation;
        let screen = self.shared.screen.lock().unwrap().take();
        let camera = self.shared.camera.lock().unwrap().take();
        Some((screen, camera))
    }

    fn send(&self, cmd: Cmd) {
        if let Some(tx) = &self.audio_tx {
            let _ = tx.send(match &cmd {
                Cmd::Play => Cmd::Play,
                Cmd::Pause => Cmd::Pause,
                Cmd::Seek(t, seq) => Cmd::Seek(*t, *seq),
                Cmd::SetEdit(e) => Cmd::SetEdit(e.clone()),
                Cmd::SetSpeed(s) => Cmd::SetSpeed(*s),
            });
        }
        let _ = self.video_tx.send(cmd);
    }
}

fn fit(w: u32, h: u32, (max_w, max_h): (u32, u32)) -> (u32, u32) {
    let scale = (f64::from(max_w) / f64::from(w))
        .min(f64::from(max_h) / f64::from(h))
        .min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(f64::from(w) * scale), even(f64::from(h) * scale))
}

/// `t` if kept, else the start of the next kept range (or the last kept time).
fn snap_to_kept(edit: &EditList, t: Micros) -> Micros {
    if edit.to_output(t).is_some() {
        return t;
    }
    edit.ranges()
        .iter()
        .find(|r| r.0 >= t)
        .map(|r| r.0)
        .or_else(|| edit.ranges().last().map(|r| r.1.saturating_sub(1)))
        .unwrap_or(0)
}

/// Shared playback clock in *output* time (after cuts, before speed), which
/// advances `speed` times as fast as the wall clock.
#[derive(Clone, Copy)]
struct Clock {
    base: Micros,
    started: Option<Instant>,
    speed: f64,
}

impl Clock {
    fn paused(at: Micros, speed: f64) -> Self {
        Self {
            base: at,
            started: None,
            speed,
        }
    }

    fn running(at: Micros, speed: f64) -> Self {
        Self {
            base: at,
            started: Some(Instant::now()),
            speed,
        }
    }

    /// The same clock (running or paused) from `at`, at `speed`.
    fn rebased(&self, at: Micros, speed: f64) -> Self {
        if self.started.is_some() {
            Self::running(at, speed)
        } else {
            Self::paused(at, speed)
        }
    }

    fn now(&self) -> Micros {
        self.base
            + self.started.map_or(0, |s| {
                (s.elapsed().as_micros() as f64 * self.speed) as Micros
            })
    }
}

fn video_loop(
    mut screen: FrameCursor,
    mut camera: Option<FrameCursor>,
    mut edit: EditList,
    shared: Arc<Shared>,
    clock: Arc<Mutex<Clock>>,
    rx: Receiver<Cmd>,
) {
    let mut playing = false;
    let mut pending_render: Option<Micros> = None;
    let mut pending_seq = 0;
    // Source times of the frames last published, to publish only changes.
    let mut shown_screen = None;
    let mut shown_camera = None;
    loop {
        // Block while idle; poll while playing.
        let cmd = if playing || pending_render.is_some() {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(crossbeam_channel::TryRecvError::Empty) => None,
                Err(crossbeam_channel::TryRecvError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            }
        };
        if let Some(cmd) = cmd {
            match cmd {
                Cmd::Play => {
                    let src = shared.position.load(Ordering::Relaxed);
                    let out = edit.kept_before(src);
                    let mut c = clock.lock().unwrap();
                    *c = Clock::running(out, c.speed);
                    playing = true;
                }
                Cmd::Pause => {
                    playing = false;
                    let mut c = clock.lock().unwrap();
                    *c = Clock::paused(c.now(), c.speed);
                }
                Cmd::Seek(t, seq) => {
                    let out = edit.kept_before(t);
                    let mut c = clock.lock().unwrap();
                    *c = c.rebased(out, c.speed);
                    pending_render = Some(t);
                    pending_seq = seq;
                }
                Cmd::SetEdit(e) => edit = e,
                // The Player rebased the shared clock already.
                Cmd::SetSpeed(_) => {}
            }
            continue;
        }

        let target = if playing {
            // Playback supersedes any scrub request queued before it.
            pending_render = None;
            let out = clock.lock().unwrap().now();
            if out >= edit.duration() {
                playing = false;
                shared.playing.store(false, Ordering::Relaxed);
                let end = edit.ranges().last().map_or(0, |r| r.1.saturating_sub(1));
                let mut c = clock.lock().unwrap();
                *c = Clock::paused(edit.duration(), c.speed);
                end
            } else {
                edit.to_source(out).unwrap_or(0)
            }
        } else {
            pending_render
                .take()
                .unwrap_or_else(|| shared.position.load(Ordering::Relaxed))
        };
        shared.position.store(target, Ordering::Relaxed);

        let mut changed = false;
        match screen.at(target) {
            Ok(Some((pts, f))) if shown_screen != Some(pts) => {
                let frame = Arc::new(PreviewFrame::from_frame(f));
                *shared.screen.lock().unwrap() = Some(frame);
                shown_screen = Some(pts);
                changed = true;
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("preview decode failed: {e:#}"),
        }
        if let Some(cam) = &mut camera
            && let Ok(Some((pts, f))) = cam.at(target)
            && shown_camera != Some(pts)
        {
            let frame = Arc::new(PreviewFrame::from_frame(f));
            *shared.camera.lock().unwrap() = Some(frame);
            shown_camera = Some(pts);
            changed = true;
        }
        if changed {
            shared.generation.fetch_add(1, Ordering::Release);
        }
        shared.rendered_seek.store(pending_seq, Ordering::Release);
        if playing {
            thread::sleep(Duration::from_millis(8));
        }
    }
}

/// Seconds of audio kept queued for the device.
const AUDIO_AHEAD: f64 = 0.15;

fn audio_loop(
    mut mix: AudioMix,
    mut edit: EditList,
    clock: Arc<Mutex<Clock>>,
    rx: Receiver<Cmd>,
) -> anyhow::Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no audio output device")?;
    let config = device.default_output_config()?;
    let (rate, channels) = (config.sample_rate(), config.channels());
    let mut resampler = if rate != SAMPLE_RATE || channels != CHANNELS {
        Some(Resampler::new(SAMPLE_RATE, CHANNELS, rate, channels)?)
    } else {
        None
    };

    let ring: Arc<Mutex<VecDeque<f32>>> = Arc::default();
    let cb_ring = ring.clone();
    let stream = device.build_output_stream(
        config.config(),
        move |out: &mut [f32], _| {
            let mut ring = cb_ring.lock().unwrap();
            for s in out.iter_mut() {
                *s = ring.pop_front().unwrap_or(0.0);
            }
        },
        // Underruns are expected while paused (nothing is queued).
        |e| tracing::debug!("audio output: {e}"),
        None,
    )?;
    stream.play()?;

    let mut map = SampleMap::new(&edit);
    let mut tempo = Tempo::new(clock.lock().unwrap().speed)?;
    let mut stretched = Vec::new();
    let mut playing = false;
    // Next output sample to queue.
    let mut cursor = 0i64;
    let ahead = (AUDIO_AHEAD * f64::from(rate)) as usize * usize::from(channels);
    let mut converted = Vec::new();
    loop {
        // Returns when the Player (and so the sender) is dropped.
        let cmd = if playing {
            match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(c) => Some(c),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return Ok(()),
            }
        } else {
            match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return Ok(()),
            }
        };
        match cmd {
            Some(Cmd::Play) => {
                ring.lock().unwrap().clear();
                let c = *clock.lock().unwrap();
                cursor = micros_to_samples(c.now());
                tempo = Tempo::new(c.speed)?;
                playing = true;
            }
            Some(Cmd::Pause) => {
                playing = false;
                ring.lock().unwrap().clear();
            }
            Some(Cmd::Seek(t, _)) => {
                ring.lock().unwrap().clear();
                cursor = micros_to_samples(edit.kept_before(t));
                tempo = Tempo::new(clock.lock().unwrap().speed)?;
            }
            Some(Cmd::SetSpeed(speed)) => {
                ring.lock().unwrap().clear();
                cursor = micros_to_samples(clock.lock().unwrap().now());
                tempo = Tempo::new(speed)?;
            }
            Some(Cmd::SetEdit(e)) => {
                edit = e;
                map = SampleMap::new(&edit);
            }
            None => {}
        }
        if !playing {
            continue;
        }
        while ring.lock().unwrap().len() < ahead && cursor < map.total {
            let chunk = 1_024i64.min(map.total - cursor);
            for (src, len) in map.pieces(cursor, chunk) {
                stretched.clear();
                tempo.process(&mix.read(src, len)?, &mut stretched)?;
                match &mut resampler {
                    Some(r) => {
                        converted.clear();
                        r.process(&stretched, &mut converted)?;
                        ring.lock().unwrap().extend(converted.iter().copied());
                    }
                    None => ring.lock().unwrap().extend(stretched.iter().copied()),
                }
            }
            cursor += chunk;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snaps_into_kept_ranges() {
        let mut e = EditList::full(10_000_000);
        e.cut(2_000_000, 4_000_000);
        assert_eq!(snap_to_kept(&e, 1_000_000), 1_000_000);
        assert_eq!(snap_to_kept(&e, 3_000_000), 4_000_000);
        e.trim(0, 8_000_000);
        assert_eq!(snap_to_kept(&e, 9_000_000), 7_999_999);
    }

    #[test]
    fn fits_preview_size() {
        assert_eq!(fit(3456, 2234, (1280, 720)), (1114, 720));
        assert_eq!(fit(640, 360, (1280, 720)), (640, 360));
    }
}
