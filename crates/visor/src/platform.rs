//! Platform-specific window and app behaviour.

use std::path::Path;

use slint::winit_030::{WinitWindowAccessor, winit};

/// How an overlay window sits relative to other windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// Control bar and camera bubble: above normal windows, on every Space.
    Floating,
    /// Area selector and countdown: above everything, including the menu bar.
    Shield,
}

/// Configures a shown Slint window as an overlay that Visor never records.
pub fn configure_overlay(window: &slint::Window, kind: Overlay) {
    window.with_winit_window(|w: &winit::window::Window| {
        // Hidden from screen capture (NSWindowSharingNone / WDA_EXCLUDEFROMCAPTURE).
        // VISOR_DEBUG_UNPROTECTED=1 keeps overlays visible to screenshots.
        if std::env::var_os("VISOR_DEBUG_UNPROTECTED").is_none() {
            w.set_content_protected(true);
        }
        w.set_window_level(winit::window::WindowLevel::AlwaysOnTop);
        #[cfg(target_os = "macos")]
        macos::raise(w, kind);
        #[cfg(not(target_os = "macos"))]
        let _ = kind;
    });
}

/// Shows or hides the Dock icon (macOS). Visor is menu-bar only except while
/// a regular window (editor, preferences) is open.
pub fn set_dock_visible(visible: bool) {
    #[cfg(target_os = "macos")]
    macos::set_dock_visible(visible);
    #[cfg(not(target_os = "macos"))]
    let _ = visible;
}

/// Brings Visor to the front so newly shown windows get focus.
pub fn activate() {
    #[cfg(target_os = "macos")]
    macos::activate();
}

/// Opens the file manager with `path` selected.
pub fn reveal(path: &Path) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .spawn()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn()
    } else {
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        std::process::Command::new("xdg-open").arg(dir).spawn()
    };
    if let Err(e) = result {
        tracing::warn!("could not reveal {}: {e}", path.display());
    }
}

/// Opens a file with its default app.
pub fn open_file(path: &Path) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(path).spawn()
    } else if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(path)
            .spawn()
    } else {
        std::process::Command::new("xdg-open").arg(path).spawn()
    };
    if let Err(e) = result {
        tracing::warn!("could not open {}: {e}", path.display());
    }
}

/// What the file manager is called, for "Show in …" buttons.
pub const FILE_MANAGER: &str = if cfg!(target_os = "macos") {
    "Finder"
} else if cfg!(target_os = "windows") {
    "Explorer"
} else {
    "folder"
};

/// Whether [`copy_file`] is supported here.
pub const CAN_COPY_FILE: bool = cfg!(target_os = "macos");

/// Puts a file on the clipboard, so it can be pasted into another app (as
/// if copied in the file manager). Returns false if that failed.
pub fn copy_file(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    return macos::copy_file(path);
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        false
    }
}

/// Opens a folder in the file manager.
pub fn open_folder(path: &Path) {
    let _ = std::fs::create_dir_all(path);
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(cmd).arg(path).spawn() {
        tracing::warn!("could not open {}: {e}", path.display());
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::path::Path;

    use objc2::MainThreadMarker;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSPasteboard, NSPasteboardWriting, NSView,
        NSWindowCollectionBehavior,
    };
    use objc2_foundation::{NSArray, NSString, NSURL};
    use slint::winit_030::winit;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use super::Overlay;

    /// NSStatusWindowLevel and NSScreenSaverWindowLevel.
    const STATUS_LEVEL: isize = 25;
    const SHIELD_LEVEL: isize = 1000;

    pub fn raise(w: &winit::window::Window, kind: Overlay) {
        let Ok(handle) = w.window_handle() else {
            return;
        };
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return;
        };
        // SAFETY: winit guarantees ns_view is a live NSView on the main thread.
        let view: &NSView = unsafe { appkit.ns_view.cast().as_ref() };
        let Some(window) = view.window() else { return };
        let level = match kind {
            Overlay::Floating => STATUS_LEVEL,
            Overlay::Shield => SHIELD_LEVEL,
        };
        window.setLevel(level);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary,
        );
        window.setHasShadow(false);
    }

    fn app() -> Option<Retained<NSApplication>> {
        MainThreadMarker::new().map(NSApplication::sharedApplication)
    }

    pub fn set_dock_visible(visible: bool) {
        if let Some(app) = app() {
            let policy = if visible {
                NSApplicationActivationPolicy::Regular
            } else {
                NSApplicationActivationPolicy::Accessory
            };
            app.setActivationPolicy(policy);
        }
    }

    pub fn copy_file(path: &Path) -> bool {
        let Some(path) = path.to_str() else {
            return false;
        };
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        let item: Retained<ProtocolObject<dyn NSPasteboardWriting>> =
            ProtocolObject::from_retained(url);
        let pasteboard = NSPasteboard::generalPasteboard();
        pasteboard.clearContents();
        pasteboard.writeObjects(&NSArray::from_retained_slice(&[item]))
    }

    pub fn activate() {
        if let Some(app) = app() {
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
        }
    }
}
