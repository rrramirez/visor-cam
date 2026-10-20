//! The recording engine. A [`Recorder`] starts screen (and system audio)
//! capture, subscribes to already-open mic and camera streams, and runs one
//! writer thread per track, all sharing one pause-aware [`MediaClock`].

use std::borrow::Borrow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, anyhow};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded, select};
use visor_capture::camera::CameraStream;
use visor_capture::mic::MicStream;
use visor_capture::{Backend, ScreenRequest, ScreenSession, SystemAudio, host_now};
use visor_core::{AudioBuffer, MediaClock, Micros, Project, RecordingConfig, Track, VideoFrame};
use visor_media::{AudioWriter, VideoWriter};

pub const SCREEN_FILE: &str = "screen.mp4";
pub const CAMERA_FILE: &str = "camera.mp4";
pub const MIC_FILE: &str = "mic.m4a";
pub const SYSTEM_FILE: &str = "system.m4a";

/// How long to keep draining queued buffers after stop.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

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
}

type ClockRef = Arc<Mutex<MediaClock>>;

struct Worker {
    track: &'static str,
    stop: Sender<Micros>,
    handle: JoinHandle<anyhow::Result<()>>,
}

pub struct Recorder {
    clock: ClockRef,
    session: Option<ScreenSession>,
    workers: Vec<Worker>,
    dir: PathBuf,
    project: Project,
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
        } = opts;
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

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

        let mut project = Project::new(title, visor_core::time::rfc3339_now(), 0);
        let (width, height) = session.size;
        project.tracks.screen = Some(Track {
            file: SCREEN_FILE.into(),
            width: Some(width),
            height: Some(height),
            fps: Some(config.fps),
        });

        // Writers are created before the clock starts so their setup time
        // isn't recorded as blank video.
        let screen_writer = VideoWriter::create(&dir.join(SCREEN_FILE), width, height, config.fps)?;
        let camera_writer = match &camera {
            Some(cam) => {
                let (w, h) = cam.size();
                project.tracks.camera = Some(Track {
                    file: CAMERA_FILE.into(),
                    width: Some(w),
                    height: Some(h),
                    fps: Some(30),
                });
                Some(VideoWriter::create(&dir.join(CAMERA_FILE), w, h, 30)?)
            }
            None => None,
        };
        let mic_writer = match &mic {
            Some(_) => {
                project.tracks.mic = Some(Track {
                    file: MIC_FILE.into(),
                    width: None,
                    height: None,
                    fps: None,
                });
                Some(AudioWriter::create(&dir.join(MIC_FILE))?)
            }
            None => None,
        };
        let system_writer = match &session.audio {
            Some(_) => {
                project.tracks.system = Some(Track {
                    file: SYSTEM_FILE.into(),
                    width: None,
                    height: None,
                    fps: None,
                });
                Some(AudioWriter::create(&dir.join(SYSTEM_FILE))?)
            }
            None => None,
        };

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
        tracing::info!(dir = %dir.display(), tracks = workers.len(), "recording started");

        Ok(Self {
            clock,
            session: Some(session),
            workers,
            dir,
            project,
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

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Stops all tracks, finalizes files and writes `project.json`.
    pub fn stop(mut self) -> anyhow::Result<(PathBuf, Project)> {
        let stop_at = host_now();
        let duration = self.clock.lock().unwrap().elapsed(stop_at);
        let failed = self.join_workers(stop_at)?;
        for track in failed {
            match track {
                "screen" => return Err(anyhow!("the screen track failed to record")),
                "camera" => self.project.tracks.camera = None,
                "mic" => self.project.tracks.mic = None,
                "system" => self.project.tracks.system = None,
                _ => {}
            }
        }
        self.project.duration_us = duration;
        self.project.edit = visor_core::EditList::full(duration);
        self.project.save(&self.dir)?;
        tracing::info!(duration_ms = duration / 1000, "recording saved");
        Ok((std::mem::take(&mut self.dir), self.project.clone()))
    }

    /// Stops recording and deletes the project folder.
    pub fn discard(mut self) -> anyhow::Result<()> {
        self.join_workers(host_now())?;
        std::fs::remove_dir_all(&self.dir)
            .with_context(|| format!("removing {}", self.dir.display()))
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

/// A new folder under `root` named after the local time and title, e.g.
/// `2026-10-20 14.03.27 Recording`.
pub fn new_project_dir(root: &Path, title: &str) -> PathBuf {
    let stamp = chrono::Local::now().format("%Y-%m-%d %H.%M.%S");
    let safe: String = title
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let base = root.join(format!("{stamp} {}", safe.trim()));
    let mut dir = base.clone();
    let mut n = 2;
    while dir.exists() {
        dir = PathBuf::from(format!("{} {n}", base.display()));
        n += 1;
    }
    dir
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
            let mut write = |frame: &VideoFrame, writer: &mut VideoWriter| -> anyhow::Result<()> {
                let (pts, now_epoch) = {
                    let c = clock.lock().unwrap();
                    (c.pts(frame.host_ts), c.epoch())
                };
                if let Some(pts) = pts {
                    let keyframe = now_epoch != epoch;
                    epoch = now_epoch;
                    writer.write(frame, pts, keyframe)?;
                }
                Ok(())
            };
            let stop_at = run_until_stopped(
                &rx,
                &stop_rx,
                |f| write(f.borrow(), &mut writer),
                |f| f.borrow().host_ts,
            )?;
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
            let mut write = |buf: &AudioBuffer| -> anyhow::Result<()> {
                let pts = clock.lock().unwrap().pts(buf.host_ts);
                if let Some(pts) = pts {
                    writer.write(buf, pts)?;
                }
                Ok(())
            };
            let stop_at =
                run_until_stopped(&rx, &stop_rx, |b| write(b.borrow()), |b| b.borrow().host_ts)?;
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
/// captured before that time. Returns the stop time.
fn run_until_stopped<T>(
    rx: &Receiver<T>,
    stop_rx: &Receiver<Micros>,
    mut write: impl FnMut(&T) -> anyhow::Result<()>,
    timestamp: impl Fn(&T) -> Micros,
) -> anyhow::Result<Micros> {
    let stop_at = loop {
        select! {
            recv(rx) -> item => match item {
                Ok(item) => write(&item)?,
                // Source ended (e.g. camera unplugged): wait for the stop signal.
                Err(_) => break stop_rx.recv().unwrap_or_else(|_| host_now()),
            },
            recv(stop_rx) -> at => break at.unwrap_or_else(|_| host_now()),
        }
    };
    loop {
        match rx.recv_timeout(DRAIN_TIMEOUT) {
            Ok(item) if timestamp(&item) <= stop_at => write(&item)?,
            Ok(_) | Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(stop_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_dirs_are_unique_and_sanitized() {
        let root = std::env::temp_dir().join(format!("visor-dirs-{}", std::process::id()));
        let a = new_project_dir(&root, "Demo: a/b");
        assert!(
            a.file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("Demo- a-b")
        );
        std::fs::create_dir_all(&a).unwrap();
        let b = new_project_dir(&root, "Demo: a/b");
        assert_ne!(a, b);
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
            |&t| {
                seen.push(t);
                Ok(())
            },
            |&t| t,
        )
        .unwrap();
        assert_eq!(at, 25);
        assert!(seen.iter().all(|&t| t <= 25 || t == 30), "{seen:?}");
        assert!(!seen.contains(&40));
    }
}
