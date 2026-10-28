//! Visor: an offline screen recorder.
//!
//! Usage:
//!   visor                         menu bar app; opens the recorder panel
//!   visor --edit <folder>         open a recording in the editor
//!   visor --record [--stop-after S]
//!                                 start recording right away with the saved
//!                                 preferences (optionally stopping after S s)

// A windowed app on Windows (no console window); logs go to visor.log.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod editor;
mod instance;
mod platform;
mod settings;
mod shortcuts;
mod tray;
mod updates;
mod util;

use std::path::PathBuf;
use std::time::Duration;

use tracing_subscriber::fmt::writer::MakeWriterExt;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    init_logging();
    log_panics();
    let args = parse_args();

    // Launching Visor while it runs shows the running one instead.
    let request = match &args {
        Args::Edit(dir) => instance::Request::Edit(dir.clone()),
        _ => instance::Request::ShowPanel,
    };
    let instance = instance::claim(&request);
    if matches!(instance, instance::Instance::HandedOver) {
        tracing::info!("Visor is already running; handed this launch to it");
        return Ok(());
    }

    visor_media::init()?;

    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut backend_selector = slint::BackendSelector::new().backend_name("winit".into());
    #[cfg(target_os = "macos")]
    {
        use slint::winit_030::winit;
        use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
        // Menu bar app: no Dock icon until a regular window opens.
        let mut builder = winit::event_loop::EventLoop::with_user_event();
        builder.with_activation_policy(ActivationPolicy::Accessory);
        backend_selector = backend_selector.with_winit_event_loop_builder(builder);
    }
    backend_selector.select()?;

    let app = app::App::install(visor_capture::backend()?);
    if let instance::Instance::Primary(primary) = instance {
        primary.serve(|request| {
            app::post(move |app| match request {
                instance::Request::ShowPanel => app.open_panel(),
                instance::Request::Edit(dir) => app.open_editor(dir),
            })
        });
    }

    // The tray needs a running event loop on macOS.
    slint::Timer::single_shot(Duration::ZERO, move || {
        app.recover_interrupted();
        app.create_tray();
        app.create_shortcuts();
        match args {
            Args::Edit(dir) => app.open_editor(dir),
            Args::Record { stop_after } => app.record_when_ready(stop_after),
            Args::Panel => app.open_panel(),
        }
    });
    slint::run_event_loop_until_quit()?;
    Ok(())
}

enum Args {
    Panel,
    Edit(PathBuf),
    Record { stop_after: Option<u64> },
}

fn parse_args() -> Args {
    let mut parsed = Args::Panel;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--edit" => {
                if let Some(dir) = args.next() {
                    parsed = Args::Edit(PathBuf::from(dir));
                }
            }
            "--record" => {
                if !matches!(parsed, Args::Record { .. }) {
                    parsed = Args::Record { stop_after: None };
                }
            }
            "--stop-after" => {
                let secs = args.next().and_then(|s| s.parse().ok());
                parsed = Args::Record { stop_after: secs };
            }
            "--version" => {
                println!("visor {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!(
                    "usage: visor [--edit <recording folder> | --record [--stop-after SECONDS]]"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    parsed
}

/// Writes panics (any thread) to the log with a backtrace, so a crash in the
/// app bundle, where stderr goes nowhere, leaves a trace.
fn log_panics() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(no message)".into());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_default();
        tracing::error!(
            thread = thread.name().unwrap_or("unnamed"),
            %location,
            "panic: {message}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
        default_hook(info);
    }));
}

/// Logs to stderr and to a file (`~/Library/Logs/Visor/visor.log` on macOS,
/// the data dir elsewhere), since a bundled app has no visible stderr.
fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,wgpu=warn,naga=warn".into());
    let log_dir = if cfg!(target_os = "macos") {
        dirs::home_dir().map(|h| h.join("Library/Logs/Visor"))
    } else {
        dirs::data_local_dir().map(|d| d.join("Visor"))
    };
    let file = log_dir.and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("visor.log"))
            .ok()
    });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false);
    match file {
        Some(file) => builder
            .with_writer(std::io::stderr.and(std::sync::Mutex::new(file)))
            .init(),
        None => builder.with_writer(std::io::stderr).init(),
    }
}
