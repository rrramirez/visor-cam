use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use crossbeam_channel::{RecvTimeoutError, select};
use visor_capture::Backend;
use visor_core::{CaptureTarget, MediaClock, Project, Rect, RecordingConfig, Track};
use visor_media::VideoWriter;

#[derive(Parser)]
#[command(name = "visor-cli", about = "Headless Visor recorder", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List displays, windows and apps that can be recorded.
    List,
    /// Record into a new project folder.
    ///
    /// While recording, type `p` + Enter to pause/resume and Enter alone to stop.
    Record {
        /// Display id (see `list`). Defaults to the main display.
        #[arg(long, conflicts_with_all = ["window", "app"])]
        display: Option<u32>,
        #[arg(long, conflicts_with = "app")]
        window: Option<u64>,
        /// Process id of an app to record (all its windows).
        #[arg(long)]
        app: Option<i32>,
        /// Area of the display as `x,y,width,height` in points.
        #[arg(long, value_parser = parse_rect, conflicts_with_all = ["window", "app"])]
        area: Option<Rect>,
        #[arg(long, default_value_t = 60)]
        fps: u32,
        #[arg(long)]
        no_cursor: bool,
        /// Stop automatically after this many seconds.
        #[arg(long)]
        seconds: Option<u64>,
        /// Project folder to create.
        #[arg(long)]
        out: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let backend = visor_capture::backend()?;
    match Cli::parse().command {
        Command::List => list(backend.as_ref()),
        Command::Record { display, window, app, area, fps, no_cursor, seconds, out } => {
            let display_id = match display {
                Some(id) => id,
                None => backend.shareable()?.displays.first().context("no displays")?.id,
            };
            let target = match (window, app, area) {
                (Some(id), _, _) => CaptureTarget::Window { id },
                (_, Some(pid), _) => CaptureTarget::App { pid },
                (_, _, Some(rect)) => CaptureTarget::Area { display_id, rect },
                _ => CaptureTarget::Display { id: display_id },
            };
            let config = RecordingConfig { target, fps, show_cursor: !no_cursor, ..RecordingConfig::display(display_id) };
            record(backend.as_ref(), &config, seconds.map(Duration::from_secs), &out)
        }
    }
}

fn list(backend: &dyn Backend) -> anyhow::Result<()> {
    let s = backend.shareable()?;
    println!("Displays:");
    for d in &s.displays {
        println!("  {:>10}  {}x{}", d.id, d.width, d.height);
    }
    println!("Windows:");
    for w in s.windows.iter().filter(|w| !w.title.is_empty()) {
        println!("  {:>10}  {} — {}", w.id, w.app, w.title);
    }
    println!("Apps:");
    for a in &s.apps {
        println!("  {:>10}  {}", a.pid, a.name);
    }
    Ok(())
}

enum Control {
    TogglePause,
    Stop,
}

fn record(backend: &dyn Backend, config: &RecordingConfig, limit: Option<Duration>, out: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    let session = backend.start(config)?;
    let (width, height) = session.screen_size;
    let mut writer = VideoWriter::create(&out.join("screen.mp4"), width, height, config.fps)?;

    let clock = Arc::new(Mutex::new(MediaClock::start(backend.host_now())));
    let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded();
    {
        let ctl_tx = ctl_tx.clone();
        thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let msg = match line.as_deref().map(str::trim) {
                    Ok("p") => Control::TogglePause,
                    _ => Control::Stop,
                };
                if ctl_tx.send(msg).is_err() {
                    return;
                }
            }
            let _ = ctl_tx.send(Control::Stop);
        });
    }
    if let Some(limit) = limit {
        thread::spawn(move || {
            thread::sleep(limit);
            let _ = ctl_tx.send(Control::Stop);
        });
    }

    eprintln!("Recording {width}x{height} with {}. `p`+Enter pauses/resumes, Enter stops.", writer.encoder_name());
    let mut force_keyframe = true;
    let mut frames = 0u64;
    loop {
        select! {
            recv(session.screen) -> frame => {
                let Ok(frame) = frame else { break };
                let Some(pts) = clock.lock().unwrap().pts(frame.host_ts) else { continue };
                writer.write(&frame, pts, std::mem::take(&mut force_keyframe))?;
                frames += 1;
            }
            recv(ctl_rx) -> msg => match msg {
                Ok(Control::TogglePause) => {
                    let mut c = clock.lock().unwrap();
                    if c.is_paused() {
                        c.resume(backend.host_now());
                        force_keyframe = true;
                        eprintln!("Resumed.");
                    } else {
                        c.pause(backend.host_now());
                        eprintln!("Paused.");
                    }
                }
                Ok(Control::Stop) | Err(_) => break,
            },
        }
    }

    let stop_at = backend.host_now();
    let screen = session.screen.clone();
    session.stop()?;
    // Encode frames captured before the stop that are still queued.
    loop {
        match screen.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) if frame.host_ts <= stop_at => {
                if let Some(pts) = clock.lock().unwrap().pts(frame.host_ts) {
                    writer.write(&frame, pts, false)?;
                    frames += 1;
                }
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }
    let duration = clock.lock().unwrap().elapsed(stop_at);
    writer.finish(duration)?;

    let title = out.file_name().map_or_else(|| "Untitled".into(), |n| n.to_string_lossy().into_owned());
    let mut project = Project::new(title, visor_core::time::rfc3339_now(), duration);
    project.tracks.screen =
        Some(Track { file: "screen.mp4".into(), width: Some(width), height: Some(height), fps: Some(config.fps) });
    project.save(out)?;

    eprintln!("Saved {frames} frames ({:.1}s) to {}", duration as f64 / 1e6, out.display());
    Ok(())
}

fn parse_rect(s: &str) -> Result<Rect, String> {
    let v: Vec<f64> = s.split(',').map(|p| p.trim().parse::<f64>().map_err(|e| e.to_string())).collect::<Result<_, _>>()?;
    match v[..] {
        [x, y, width, height] if width > 0.0 && height > 0.0 => Ok(Rect { x, y, width, height }),
        _ => Err("expected x,y,width,height with positive size".into()),
    }
}
