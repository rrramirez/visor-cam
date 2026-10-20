//! Visor: an offline screen recorder.
//!
//! Usage: `visor` starts in the menu bar and opens the recorder panel;
//! `visor --edit <project-folder>` opens a recording in the editor.

mod app;
mod editor;
mod platform;
mod settings;
mod tray;
mod util;

use std::path::PathBuf;
use std::time::Duration;

use tracing_subscriber::fmt::writer::MakeWriterExt;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    init_logging();
    visor_media::init()?;

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
    let edit_dir = parse_args();

    // The tray needs a running event loop on macOS.
    slint::Timer::single_shot(Duration::ZERO, move || {
        app.create_tray();
        match edit_dir {
            Some(dir) => app.open_editor(dir),
            None => app.open_panel(),
        }
    });
    slint::run_event_loop_until_quit()?;
    Ok(())
}

fn parse_args() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--edit" => return args.next().map(PathBuf::from),
            "--version" => {
                println!("visor {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("usage: visor [--edit <recording folder>]");
                std::process::exit(0);
            }
            _ => {}
        }
    }
    None
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
