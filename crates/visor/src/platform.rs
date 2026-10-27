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
pub fn configure_overlay<C: slint::ComponentHandle + 'static>(ui: &C, kind: Overlay) {
    apply_overlay(ui.window(), kind);
    // Slint applies its own window properties (such as always-on-top) just
    // after showing a window, which resets the level. Apply it again after.
    let weak = ui.as_weak();
    slint::Timer::single_shot(OVERLAY_SETTLE, move || {
        if let Some(ui) = weak.upgrade() {
            apply_overlay(ui.window(), kind);
        }
    });
}

/// How long after showing an overlay its window level is applied again.
pub const OVERLAY_SETTLE: std::time::Duration = std::time::Duration::from_millis(50);

fn apply_overlay(window: &slint::Window, kind: Overlay) {
    window.with_winit_window(|w: &winit::window::Window| {
        // Hidden from screen capture (NSWindowSharingNone / WDA_EXCLUDEFROMCAPTURE).
        // VISOR_DEBUG_UNPROTECTED=1 keeps overlays visible to screenshots.
        if std::env::var_os("VISOR_DEBUG_UNPROTECTED").is_none() {
            w.set_content_protected(true);
        }
        // On macOS `raise` sets the level directly; going through winit's
        // floating level first would let AppKit pull a full-screen overlay
        // below the menu bar.
        #[cfg(target_os = "macos")]
        macos::raise(w, kind);
        #[cfg(not(target_os = "macos"))]
        {
            w.set_window_level(winit::window::WindowLevel::AlwaysOnTop);
            let _ = kind;
        }
    });
}

/// Moves and sizes a shown window to `frame` (global logical coordinates,
/// top-left origin), even over the menu bar and Dock on macOS, where plain
/// window positioning keeps windows clear of them.
pub fn set_frame(window: &slint::Window, frame: visor_core::Rect) {
    // Slint re-applies the size it knows, so tell it too.
    window.set_size(slint::LogicalSize::new(
        frame.width as f32,
        frame.height as f32,
    ));
    #[cfg(target_os = "macos")]
    window.with_winit_window(|w: &winit::window::Window| macos::set_frame(w, frame));
    #[cfg(not(target_os = "macos"))]
    {
        window.set_position(slint::LogicalPosition::new(frame.x as f32, frame.y as f32));
        window.set_size(slint::LogicalSize::new(
            frame.width as f32,
            frame.height as f32,
        ));
    }
}

/// Lets clicks pass through a shown window to whatever is underneath.
pub fn set_click_through(window: &slint::Window) {
    window.with_winit_window(|w: &winit::window::Window| {
        if let Err(e) = w.set_cursor_hittest(false) {
            tracing::debug!("click-through not supported: {e}");
        }
    });
}

/// Gives a shown window keyboard focus.
pub fn focus(window: &slint::Window) {
    window.with_winit_window(|w: &winit::window::Window| w.focus_window());
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
        // Not `cmd /C start`: cmd would interpret `&`, `^` and `%` in the
        // file name, which comes from the project title.
        std::process::Command::new("explorer").arg(path).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(path).spawn()
    };
    if let Err(e) = result {
        tracing::warn!("could not open {}: {e}", path.display());
    }
}

/// Opens a web page in the default browser.
pub fn open_url(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    if let Err(e) = std::process::Command::new(program).arg(url).spawn() {
        tracing::warn!("could not open {url}: {e}");
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
        NSApplication, NSApplicationActivationPolicy, NSPasteboard, NSPasteboardWriting, NSScreen,
        NSView, NSWindow, NSWindowAnimationBehavior, NSWindowCollectionBehavior,
    };
    use objc2_foundation::{NSArray, NSPoint, NSRect, NSSize, NSString, NSURL};
    use slint::winit_030::winit;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    use super::Overlay;

    /// NSStatusWindowLevel and NSScreenSaverWindowLevel.
    const STATUS_LEVEL: isize = 25;
    const SHIELD_LEVEL: isize = 1000;

    fn ns_window(w: &winit::window::Window) -> Option<Retained<NSWindow>> {
        let handle = w.window_handle().ok()?;
        let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return None;
        };
        // SAFETY: winit guarantees ns_view is a live NSView on the main thread.
        let view: &NSView = unsafe { appkit.ns_view.cast().as_ref() };
        view.window()
    }

    pub fn set_frame(w: &winit::window::Window, frame: visor_core::Rect) {
        let Some(window) = ns_window(w) else { return };
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        // AppKit's global coordinates start at the bottom-left of the
        // primary display.
        let screens = NSScreen::screens(mtm);
        let Some(primary) = screens.firstObject() else {
            return;
        };
        let primary_height = primary.frame().size.height;
        let rect = NSRect::new(
            NSPoint::new(frame.x, primary_height - frame.y - frame.height),
            NSSize::new(frame.width, frame.height),
        );
        if window.frame() != rect {
            window.setFrame_display(rect, true);
        }
    }

    pub fn raise(w: &winit::window::Window, kind: Overlay) {
        let Some(window) = ns_window(w) else { return };
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
        // Overlays appear and go instantly, without AppKit's zoom.
        window.setAnimationBehavior(NSWindowAnimationBehavior::None);
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
