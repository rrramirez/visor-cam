//! The recording engine. A [`Recorder`] starts screen (and system audio)
//! capture, subscribes to already-open mic and camera streams, and runs one
//! writer thread per track, all sharing one pause-aware [`MediaClock`].

use std::borrow::Borrow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, select};
use visor_capture::camera::CameraStream;
use visor_capture::mic::MicStream;
use visor_capture::{Backend, ScreenRequest, ScreenSession, SystemAudio, host_now};
use visor_core::project::take_file;
use visor_core::{
    AudioBuffer, MediaClock, Micros, Project, RecordingConfig, Track, Tracks, VideoFrame,
};
use visor_media::{AudioWriter, VideoWriter};

pub use visor_core::project::{CAMERA_FILE, MIC_FILE, SCREEN_FILE, SYSTEM_FILE};

/// How long to keep draining queued buffers after stop.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(250);
/// How long a track may go without writing (the screen is still, or
/// recording is paused) before what its writer holds is saved to the file,
/// so a crash loses little. Matches the tracks' fragment length.
const IDLE_FLUSH: Duration = Duration::from_millis(500);

pub struct RecorderOptions {
    pub config: RecordingConfig,
    /// Project folder to create; see [`new_project_dir`].
    pub dir: PathBuf,
    pub title: String,
    /// Open mic stream to record from (shared with the level meter).
    pub mic: Option<Arc<MicStream>>,
    /// Open camera stream to record from (shared with the bubble preview).
    pub camera: Option<Arc<CameraStream>>,
    pub exclude_self: bool,
    /// Start paused; call [`Recorder::resume`] to begin. Lets capture warm up
    /// (during a countdown) so recording starts the instant it's resumed.
    pub start_paused: bool,
    /// Record more onto the stopped recording in `dir`, as a take played
    /// after the rest (`title` is then unused).
    pub append: bool,
}

type ClockRef = Arc<Mutex<MediaClock>>;

struct Worker {
    track: &'static str,
    stop: Sender<Micros>,
    handle: JoinHandle<anyhow::Result<()>>,
}

/// Marker saves run in the background (see [`Recorder::mark`]); this keeps
/// them from overwriting a newer save, or the final one at stop.
#[derive(Default)]
struct Saves {
    /// Markers in the newest project.json written.
    markers: usize,
    /// The recording stopped or was discarded.
    done: bool,
}

pub struct Recorder {
    clock: ClockRef,
    session: Option<ScreenSession>,
    workers: Vec<Worker>,
    dir: PathBuf,
    project: Project,
    /// Recording more: the project as it was before.
    original: Option<Project>,
    /// Where this recording's time starts in the project: the recording's
    /// length before the take.
    base: Micros,
    saves: Arc<Mutex<Saves>>,
    // Keep the shared streams alive for as long as we record from them.
    _mic: Option<Arc<MicStream>>,
    _camera: Option<Arc<CameraStream>>,
}

impl Recorder {
    pub fn start(backend: &dyn Backend, opts: RecorderOptions) -> anyhow::Result<Self> {
        let RecorderOptions {
            config,
            dir,
            title,
            mic,
            camera,
            exclude_self,
            start_paused,
            append,
        } = opts;
        let original = if append {
            let project = Project::load(&dir)
                .with_context(|| format!("opening the recording in {}", dir.display()))?;
            if project.recording_take().is_some() {
                return Err(anyhow!("the recording is still being recorded onto"));
            }
            Some(project)
        } else {
            std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
            None
        };
        // A take's files are numbered: screen-2.mp4…
        let take = original.as_ref().map(|p| p.takes.len() + 2);
        let file = |base: &str| take.map_or_else(|| base.to_string(), |n| take_file(base, n));

        let system_audio = match (config.audio.system, config.audio.app_pid) {
            (_, Some(pid)) => SystemAudio::App { pid },
            (true, None) => SystemAudio::All,
            (false, None) => SystemAudio::Off,
        };
        let session = backend.start(&ScreenRequest {
            target: config.target.clone(),
            fps: config.fps,
            show_cursor: config.show_cursor,
            system_audio,
            exclude_self,
        })?;

        let mut tracks = Tracks::default();
        let (width, height) = session.size;
        tracks.screen = Some(Track {
            file: file(SCREEN_FILE),
            width: Some(width),
            height: Some(height),
            fps: Some(config.fps),
        });

        // Writers are created before the clock starts so their setup time
        // isn't recorded as blank video.
        let screen_writer =
            VideoWriter::create(&dir.join(file(SCREEN_FILE)), width, height, config.fps)?;
        let camera_writer = match &camera {
            Some(cam) => {
                let (w, h) = cam.size();
                tracks.camera = Some(Track {
                    file: file(CAMERA_FILE),
                    width: Some(w),
                    height: Some(h),
                    fps: Some(30),
                });
                Some(VideoWriter::create(&dir.join(file(CAMERA_FILE)), w, h, 30)?)
            }
            None => None,
        };
        let mic_writer = match &mic {
            Some(_) => {
                tracks.mic = Some(Track {
                    file: file(MIC_FILE),
                    width: None,
                    height: None,
                    fps: None,
                });
                Some(AudioWriter::create(&dir.join(file(MIC_FILE)))?)
            }
            None => None,
        };
        let system_writer = match &session.audio {
            Some(_) => {
                tracks.system = Some(Track {
                    file: file(SYSTEM_FILE),
                    width: None,
                    height: None,
                    fps: None,
                });
                Some(AudioWriter::create(&dir.join(file(SYSTEM_FILE)))?)
            }
            None => None,
        };

        let mut project = match &original {
            Some(p) => p.clone(),
            None => Project::new(title, visor_core::time::rfc3339_now(), 0),
        };
        let base = project.duration_us;
        project.sources = Some(config.clone());
        if original.is_some() {
            project.begin_take(tracks);
        } else {
            project.tracks = tracks;
        }

        let mut start_clock = MediaClock::start(host_now());
        if start_paused {
            start_clock.pause(host_now());
        }
        let clock: ClockRef = Arc::new(Mutex::new(start_clock));
        let mut workers = vec![spawn_video(
            "screen",
            session.video.clone(),
            screen_writer,
            clock.clone(),
        )?];
        if let (Some(cam), Some(writer)) = (&camera, camera_writer) {
            workers.push(spawn_video(
                "camera",
                cam.subscribe(),
                writer,
                clock.clone(),
            )?);
        }
        if let (Some(mic), Some(writer)) = (&mic, mic_writer) {
            workers.push(spawn_audio("mic", mic.subscribe(), writer, clock.clone())?);
        }
        if let (Some(rx), Some(writer)) = (session.audio.clone(), system_writer) {
            workers.push(spawn_audio("system", rx, writer, clock.clone())?);
        }
        // Written now with no duration (or the take's), so a recording interrupted by a crash
        // can be found and recovered (see visor_media::recover).
        if let Err(e) = project.save(&dir) {
            tracing::warn!("could not write the project file: {e}");
        }
        tracing::info!(dir = %dir.display(), tracks = workers.len(), "recording started");

        Ok(Self {
            clock,
            session: Some(session),
            workers,
            dir,
            project,
            original,
            base,
            saves: Arc::default(),
            _mic: mic,
            _camera: camera,
        })
    }

    pub fn pause(&self) {
        self.clock.lock().unwrap().pause(host_now());
    }

    pub fn resume(&self) {
        self.clock.lock().unwrap().resume(host_now());
    }

    pub fn is_paused(&self) -> bool {
        self.clock.lock().unwrap().is_paused()
    }

    /// Recorded time so far, excluding pauses.
    pub fn elapsed(&self) -> Micros {
        self.clock.lock().unwrap().elapsed(host_now())
    }

    /// Notes this moment of the recording. Returns the recorded time it
    /// marks (in the whole recording, when recording more).
    pub fn mark(&mut self, text: impl Into<String>) -> Micros {
        let at_us = self.base + self.elapsed();
        self.project.markers.push(visor_core::Marker {
            at_us,
            text: text.into(),
        });
        // Saved now (still with no duration) so a recording recovered after
        // a crash keeps its markers; in the background, as the folder may be
        // slow to write to.
        let (project, dir, saves) = (self.project.clone(), self.dir.clone(), self.saves.clone());
        thread::spawn(move || {
            let mut saves = saves.lock().unwrap_or_else(|e| e.into_inner());
            let markers = project.markers.len();
            if saves.done || saves.markers >= markers {
                return;
            }
            match project.save(&dir) {
                Ok(()) => saves.markers = markers,
                Err(e) => tracing::warn!("could not save the marker: {e}"),
            }
        });
        at_us
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stops all tracks, finalizes files and writes `project.json`.
    pub fn stop(mut self) -> anyhow::Result<(PathBuf, Project)> {
        let stop_at = host_now();
        let duration = self.clock.lock().unwrap().elapsed(stop_at);
        let failed = self.join_workers(stop_at)?;
        if failed.contains(&"screen") {
            if self.original.is_some() {
                self.forget_take()?;
            }
            return Err(anyhow!("the screen track failed to record"));
        }
        let tracks = match self.project.takes.last_mut() {
            Some(take) if self.original.is_some() => &mut take.tracks,
            _ => &mut self.project.tracks,
        };
        for track in failed {
            match track {
                "camera" => tracks.camera = None,
                "mic" => tracks.mic = None,
                "system" => tracks.system = None,
                _ => {}
            }
        }
        if self.original.is_some() {
            if duration == 0 {
                self.remove_take_files();
            }
            self.project.finish_take(duration, Vec::new());
        } else {
            self.project.duration_us = duration;
            self.project.edit = visor_core::EditList::full(duration);
        }
        {
            let mut saves = self.saves.lock().unwrap_or_else(|e| e.into_inner());
            saves.done = true;
            self.project.save(&self.dir)?;
        }
        tracing::info!(duration_ms = duration / 1000, "recording saved");
        Ok((std::mem::take(&mut self.dir), self.project.clone()))
    }

    /// Stops recording and deletes the project folder, or when recording
    /// more, only the take: the recording is left as it was.
    pub fn discard(mut self) -> anyhow::Result<()> {
        self.saves.lock().unwrap_or_else(|e| e.into_inner()).done = true;
        self.join_workers(host_now())?;
        if self.original.is_some() {
            return self.forget_take();
        }
        std::fs::remove_dir_all(&self.dir)
            .with_context(|| format!("removing {}", self.dir.display()))
    }

    /// Recording more: puts the project back as it was and deletes the
    /// take's files.
    fn forget_take(&mut self) -> anyhow::Result<()> {
        self.saves.lock().unwrap_or_else(|e| e.into_inner()).done = true;
        self.remove_take_files();
        if let Some(original) = &self.original {
            original.save(&self.dir)?;
            self.project = original.clone();
        }
        Ok(())
    }

    fn remove_take_files(&self) {
        let Some(take) = self.project.recording_take() else {
            return;
        };
        let t = &take.tracks;
        for track in [&t.screen, &t.camera, &t.mic, &t.system]
            .into_iter()
            .flatten()
        {
            let path = self.dir.join(&track.file);
            if let Err(e) = std::fs::remove_file(&path)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!("could not remove {}: {e}", path.display());
            }
        }
    }

    /// Signals every worker, stops capture and waits. Returns failed tracks.
    fn join_workers(&mut self, stop_at: Micros) -> anyhow::Result<Vec<&'static str>> {
        for w in &self.workers {
            let _ = w.stop.send(stop_at);
        }
        let mut failed = Vec::new();
        for w in self.workers.drain(..) {
            match w.handle.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::error!(track = w.track, "track failed: {e:#}");
                    failed.push(w.track);
                }
                Err(_) => {
                    tracing::error!(track = w.track, "track writer panicked");
                    failed.push(w.track);
                }
            }
        }
        if let Some(session) = self.session.take() {
            session.stop()?;
        }
        Ok(failed)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        if !self.workers.is_empty() {
            let _ = self.join_workers(host_now());
        }
    }
}

/// Creates a folder under `root` named after a recording's name, as the
/// editor names folders (`Demo`, then `Demo 2`…), or after the time if the
/// name makes no folder name.
pub fn named_project_dir(root: &Path, title: &str) -> std::io::Result<PathBuf> {
    match visor_core::project::folder_name(title) {
        Some(name) => visor_core::project::create_unique_dir(root, &name),
        None => new_project_dir(root, "Recording"),
    }
}

/// Creates a folder under `root` named after the local time and title,
/// e.g. `2026-10-20 14.03.27 Recording`.
pub fn new_project_dir(root: &Path, title: &str) -> std::io::Result<PathBuf> {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H.%M.%S").to_string();
    let name = visor_core::project::folder_name(&format!("{stamp} {title}")).unwrap_or(stamp);
    visor_core::project::create_unique_dir(root, &name)
}

fn spawn_video<F: Borrow<VideoFrame> + Send + 'static>(
    track: &'static str,
    rx: Receiver<F>,
    mut writer: VideoWriter,
    clock: ClockRef,
) -> anyhow::Result<Worker> {
    let (stop_tx, stop_rx) = bounded(1);
    let handle = thread::Builder::new()
        .name(format!("visor-{track}"))
        .spawn(move || {
            let mut epoch = usize::MAX;
            let write = |writer: &mut VideoWriter, frame: &F| -> anyhow::Result<bool> {
                let frame = frame.borrow();
                let (pts, now_epoch) = {
                    let c = clock.lock().unwrap();
                    (c.pts(frame.host_ts), c.epoch())
                };
                let Some(pts) = pts else {
                    return Ok(false);
                };
                let keyframe = now_epoch != epoch;
                epoch = now_epoch;
                writer.write(frame, pts, keyframe)?;
                Ok(true)
            };
            let stop_at =
                run_until_stopped(&rx, &stop_rx, &mut writer, write, VideoWriter::flush, |f| {
                    f.borrow().host_ts
                })?;
            let duration = clock.lock().unwrap().elapsed(stop_at);
            writer.finish(duration)
        })?;
    Ok(Worker {
        track,
        stop: stop_tx,
        handle,
    })
}

fn spawn_audio<B: Borrow<AudioBuffer> + Send + 'static>(
    track: &'static str,
    rx: Receiver<B>,
    mut writer: AudioWriter,
    clock: ClockRef,
) -> anyhow::Result<Worker> {
    let (stop_tx, stop_rx) = bounded(1);
    let handle = thread::Builder::new()
        .name(format!("visor-{track}"))
        .spawn(move || {
            let write = |writer: &mut AudioWriter, buf: &B| -> anyhow::Result<bool> {
                let buf = buf.borrow();
                let Some(pts) = clock.lock().unwrap().pts(buf.host_ts) else {
                    return Ok(false);
                };
                writer.write(buf, pts)?;
                Ok(true)
            };
            let stop_at =
                run_until_stopped(&rx, &stop_rx, &mut writer, write, AudioWriter::flush, |b| {
                    b.borrow().host_ts
                })?;
            let duration = clock.lock().unwrap().elapsed(stop_at);
            writer.finish(duration)
        })?;
    Ok(Worker {
        track,
        stop: stop_tx,
        handle,
    })
}

/// Feeds items to `write` until a stop time arrives, then drains items
/// captured before that time. Returns the stop time. `write` says whether
/// it wrote the item (nothing is written while paused); once nothing has
/// been written for [`IDLE_FLUSH`], `flush` saves what the writer holds.
fn run_until_stopped<T, W>(
    rx: &Receiver<T>,
    stop_rx: &Receiver<Micros>,
    writer: &mut W,
    mut write: impl FnMut(&mut W, &T) -> anyhow::Result<bool>,
    flush: impl Fn(&mut W) -> anyhow::Result<()>,
    timestamp: impl Fn(&T) -> Micros,
) -> anyhow::Result<Micros> {
    // Only a safety step: a failure leaves the track recording.
    let flush_or_warn = |writer: &mut W| {
        if let Err(e) = flush(writer) {
            tracing::warn!("could not save a track's latest frames: {e:#}");
        }
    };
    let mut last_write = None;
    let stop_at = loop {
        select! {
            recv(rx) -> item => match item {
                Ok(item) => {
                    if write(writer, &item)? {
                        last_write = Some(Instant::now());
                    }
                }
                // Source ended (e.g. camera unplugged): wait for the stop signal.
                Err(_) => {
                    if last_write.take().is_some() {
                        flush_or_warn(writer);
                    }
                    break stop_rx.recv().unwrap_or_else(|_| host_now());
                }
            },
            recv(stop_rx) -> at => break at.unwrap_or_else(|_| host_now()),
            default(IDLE_FLUSH) => {}
        }
        if last_write.is_some_and(|t: Instant| t.elapsed() >= IDLE_FLUSH) {
            flush_or_warn(writer);
            last_write = None;
        }
    };
    loop {
        match rx.recv_timeout(DRAIN_TIMEOUT) {
            Ok(item) if timestamp(&item) <= stop_at => {
                write(writer, &item)?;
            }
            Ok(_) | Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(stop_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A screen that sends a frame every 20 ms until stopped.
    struct FakeScreen((u32, u32));

    impl Backend for FakeScreen {
        fn shareable(&self) -> anyhow::Result<visor_capture::Shareable> {
            Ok(visor_capture::Shareable::default())
        }

        fn start(&self, _: &ScreenRequest) -> anyhow::Result<ScreenSession> {
            let (w, h) = self.0;
            let (tx, rx) = bounded(64);
            let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let on = running.clone();
            thread::spawn(move || {
                while on.load(std::sync::atomic::Ordering::Relaxed) {
                    let frame = VideoFrame {
                        host_ts: host_now(),
                        width: w,
                        height: h,
                        format: visor_core::PixelFormat::Nv12,
                        planes: vec![
                            visor_core::Plane {
                                data: vec![100u8; (w * h) as usize].into(),
                                stride: w as usize,
                            },
                            visor_core::Plane {
                                data: vec![128u8; (w * h / 2) as usize].into(),
                                stride: w as usize,
                            },
                        ],
                    };
                    if tx.send(frame).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            });
            Ok(ScreenSession::new(rx, (w, h), None, move || {
                running.store(false, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            }))
        }
    }

    fn options(dir: &Path, append: bool) -> RecorderOptions {
        RecorderOptions {
            config: RecordingConfig::display(1),
            dir: dir.to_path_buf(),
            title: "Demo".into(),
            mic: None,
            camera: None,
            exclude_self: false,
            start_paused: false,
            append,
        }
    }

    #[test]
    fn records_more_onto_a_recording_as_a_clip_at_the_end() {
        let dir = std::env::temp_dir().join(format!("visor-more-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rec = Recorder::start(&FakeScreen((64, 64)), options(&dir, false)).unwrap();
        thread::sleep(Duration::from_millis(300));
        let (_, first) = rec.stop().unwrap();
        assert_eq!(first.sources, Some(RecordingConfig::display(1)));
        let d1 = first.duration_us;

        // Discarded: only the take goes.
        let rec = Recorder::start(&FakeScreen((48, 32)), options(&dir, true)).unwrap();
        assert!(Project::load(&dir).unwrap().recording_take().is_some());
        thread::sleep(Duration::from_millis(200));
        rec.discard().unwrap();
        assert_eq!(Project::load(&dir).unwrap(), first);
        assert!(!dir.join("screen-2.mp4").exists());

        let mut rec = Recorder::start(&FakeScreen((48, 32)), options(&dir, true)).unwrap();
        thread::sleep(Duration::from_millis(100));
        let at = rec.mark("In the take");
        thread::sleep(Duration::from_millis(200));
        let (_, p) = rec.stop().unwrap();
        assert_eq!(Project::load(&dir).unwrap(), p);
        assert_eq!(p.takes.len(), 1);
        let take = &p.takes[0];
        assert_eq!(take.start_us, d1);
        assert_eq!(take.tracks.screen.as_ref().unwrap().file, "screen-2.mp4");
        assert_eq!(take.tracks.screen.as_ref().unwrap().width, Some(48));
        assert!(dir.join("screen-2.mp4").is_file());
        assert_eq!(p.duration_us, d1 + take.duration_us);
        assert_eq!(
            p.edit.ranges().last(),
            Some(&visor_core::Range(d1, p.duration_us))
        );
        // Markers are in the recording's time.
        assert!(at > d1);
        assert_eq!(p.markers.last().map(|m| m.at_us), Some(at));
        // The first piece is untouched.
        assert_eq!(p.tracks, first.tracks);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn idle_tracks_are_flushed() {
        // Items 1-3 are written, then 4-9 aren't (paused) though they keep
        // coming: the track is idle all the same, and is flushed once.
        let (tx, rx) = bounded(16);
        let (stop_tx, stop_rx) = bounded(1);
        let feeder = thread::spawn(move || {
            for i in 1..10u64 {
                tx.send(i).unwrap();
                thread::sleep(Duration::from_millis(100));
            }
            thread::sleep(IDLE_FLUSH);
            stop_tx.send(100).unwrap();
        });
        let mut log = Vec::new();
        run_until_stopped(
            &rx,
            &stop_rx,
            &mut log,
            |log, &i| {
                log.push(format!("{i}"));
                Ok(i <= 3)
            },
            |log| {
                log.push("flush".into());
                Ok(())
            },
            |&i| i,
        )
        .unwrap();
        feeder.join().unwrap();
        let flushes: Vec<_> = (0..log.len()).filter(|&i| log[i] == "flush").collect();
        assert_eq!(flushes.len(), 1, "{log:?}");
        // Half a second after item 3: around when item 8 arrives.
        assert!((7..=10).contains(&flushes[0]), "{log:?}");
    }

    #[test]
    fn project_dirs_are_created_unique_and_sanitized() {
        let root = std::env::temp_dir().join(format!("visor-dirs-{}", std::process::id()));
        let a = new_project_dir(&root, "Demo: a/b").unwrap();
        assert!(a.is_dir());
        assert!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("Demo- a-b")
        );
        let b = new_project_dir(&root, "Demo: a/b").unwrap();
        assert_ne!(a, b);
        let named = named_project_dir(&root, "Safari demo: v2").unwrap();
        assert_eq!(named, root.join("Safari demo- v2"));
        // Taken as soon as it's chosen, before anything is recorded in it.
        assert_eq!(
            named_project_dir(&root, "Safari demo: v2").unwrap(),
            root.join("Safari demo- v2 2")
        );
        assert!(
            named_project_dir(&root, " .. ")
                .unwrap()
                .to_string_lossy()
                .ends_with(" Recording")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn drains_only_items_before_stop() {
        let (tx, rx) = bounded(16);
        let (stop_tx, stop_rx) = bounded(1);
        for ts in [10, 20, 30] {
            tx.send(ts).unwrap();
        }
        stop_tx.send(25).unwrap();
        tx.send(40).unwrap();
        let mut seen = Vec::new();
        // select! may pick the stop first; either way only ts <= 25 are written.
        let at = run_until_stopped(
            &rx,
            &stop_rx,
            &mut seen,
            |seen, &t| {
                seen.push(t);
                Ok(true)
            },
            |_| Ok(()),
            |&t| t,
        )
        .unwrap();
        assert_eq!(at, 25);
        assert!(seen.iter().all(|&t| t <= 25 || t == 30), "{seen:?}");
        assert!(!seen.contains(&40));
    }
}
