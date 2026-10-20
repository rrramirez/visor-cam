use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use visor_capture::Backend;
use visor_capture::camera::{self, CameraStream};
use visor_capture::mic::{self, MicStream};
use visor_core::{AudioConfig, CaptureTarget, RecordingConfig, Rect};
use visor_recorder::{Recorder, RecorderOptions};

#[derive(Parser)]
#[command(name = "visor-cli", about = "Headless Visor recorder", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List displays, windows, apps, microphones and cameras.
    List,
    /// Record into a new project folder.
    ///
    /// While recording, type `p` + Enter to pause/resume and Enter alone to stop.
    Record(RecordArgs),
}

#[derive(clap::Args)]
struct RecordArgs {
    /// Display id (see `list`). Defaults to the main display.
    #[arg(long, conflicts_with_all = ["window", "app"])]
    display: Option<u32>,
    #[arg(long, conflicts_with = "app")]
    window: Option<u64>,
    /// Process id of an app to record (all its windows).
    #[arg(long)]
    app: Option<i32>,
    /// Area as global `x,y,width,height` in points.
    #[arg(long, value_parser = parse_rect, conflicts_with_all = ["window", "app"])]
    area: Option<Rect>,
    #[arg(long, default_value_t = 30)]
    fps: u32,
    #[arg(long)]
    no_cursor: bool,
    /// Record a microphone: `default` or an id from `list`.
    #[arg(long)]
    mic: Option<String>,
    /// Record system audio.
    #[arg(long)]
    system_audio: bool,
    /// Record only this app's audio (process id).
    #[arg(long)]
    app_audio: Option<i32>,
    /// Record a camera: `default` or an id from `list`.
    #[arg(long)]
    camera: Option<String>,
    /// Stop automatically after this many seconds.
    #[arg(long)]
    seconds: Option<u64>,
    /// Project folder to create.
    #[arg(long)]
    out: PathBuf,
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
        Command::Record(args) => record(backend.as_ref(), args),
    }
}

fn list(backend: &dyn Backend) -> anyhow::Result<()> {
    let s = backend.shareable()?;
    println!("Displays:");
    for d in &s.displays {
        println!(
            "  {:>10}  {} ({}x{} pt @{}x)",
            d.id, d.name, d.frame.width, d.frame.height, d.scale
        );
    }
    println!("Windows:");
    for w in &s.windows {
        println!("  {:>10}  {} — {}", w.id, w.app, w.title);
    }
    println!("Apps:");
    for a in &s.apps {
        println!("  {:>10}  {}", a.pid, a.name);
    }
    println!("Microphones:");
    for m in mic::list() {
        println!(
            "  {}{}  ({})",
            m.name,
            if m.is_default { " [default]" } else { "" },
            m.id
        );
    }
    println!("Cameras:");
    for c in camera::list() {
        println!("  {}  ({})", c.name, c.id);
    }
    Ok(())
}

enum Control {
    TogglePause,
    Stop,
}

fn record(backend: &dyn Backend, args: RecordArgs) -> anyhow::Result<()> {
    let display_id = match args.display {
        Some(id) => id,
        None => {
            backend
                .shareable()?
                .displays
                .first()
                .context("no displays")?
                .id
        }
    };
    let target = match (args.window, args.app, args.area) {
        (Some(id), _, _) => CaptureTarget::Window { id },
        (_, Some(pid), _) => CaptureTarget::App { pid },
        (_, _, Some(rect)) => CaptureTarget::Area { display_id, rect },
        _ => CaptureTarget::Display { id: display_id },
    };
    let mic = match args.mic.as_deref() {
        None => None,
        Some("default") => Some(Arc::new(MicStream::open(None)?)),
        Some(id) => Some(Arc::new(MicStream::open(Some(id))?)),
    };
    let camera = match args.camera.as_deref() {
        None => None,
        Some("default") => {
            let first = camera::list()
                .into_iter()
                .next()
                .context("no cameras found")?;
            Some(Arc::new(CameraStream::open(&first.id)?))
        }
        Some(id) => Some(Arc::new(CameraStream::open(id)?)),
    };
    let config = RecordingConfig {
        target,
        fps: args.fps,
        show_cursor: !args.no_cursor,
        audio: AudioConfig {
            mic: args.mic.clone(),
            system: args.system_audio,
            app_pid: args.app_audio,
        },
        camera: args.camera.clone(),
        countdown_secs: 0,
    };
    let title = args
        .out
        .file_name()
        .map_or_else(|| "Untitled".into(), |n| n.to_string_lossy().into_owned());
    let recorder = Recorder::start(
        backend,
        RecorderOptions {
            config,
            dir: args.out,
            title,
            mic,
            camera,
            exclude_self: false,
        },
    )?;

    let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded();
    {
        let ctl_tx = ctl_tx.clone();
        thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let msg = if matches!(line.as_deref().map(str::trim), Ok("p")) {
                    Control::TogglePause
                } else {
                    Control::Stop
                };
                if ctl_tx.send(msg).is_err() {
                    return;
                }
            }
            let _ = ctl_tx.send(Control::Stop);
        });
    }
    if let Some(secs) = args.seconds {
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(secs));
            let _ = ctl_tx.send(Control::Stop);
        });
    }

    eprintln!("Recording. `p`+Enter pauses/resumes, Enter stops.");
    while let Ok(Control::TogglePause) = ctl_rx.recv() {
        if recorder.is_paused() {
            recorder.resume();
            eprintln!("Resumed.");
        } else {
            recorder.pause();
            eprintln!("Paused at {:.1}s.", recorder.elapsed() as f64 / 1e6);
        }
    }
    let (dir, project) = recorder.stop()?;
    eprintln!(
        "Saved {:.1}s to {}",
        project.duration_us as f64 / 1e6,
        dir.display()
    );
    Ok(())
}

fn parse_rect(s: &str) -> Result<Rect, String> {
    let v: Vec<f64> = s
        .split(',')
        .map(|p| p.trim().parse::<f64>().map_err(|e| e.to_string()))
        .collect::<Result<_, _>>()?;
    match v[..] {
        [x, y, width, height] if width > 0.0 && height > 0.0 => Ok(Rect {
            x,
            y,
            width,
            height,
        }),
        _ => Err("expected x,y,width,height with positive size".into()),
    }
}
