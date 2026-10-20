//! Menu bar / system tray icon and its menu.

use std::path::PathBuf;

use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// What a menu click asks the app to do.
#[derive(Debug, Clone)]
pub enum Action {
    NewRecording,
    TogglePause,
    StopRecording,
    OpenRecent(PathBuf),
    OpenFolder,
    Preferences,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    pause: MenuItem,
    stop: MenuItem,
    new: MenuItem,
    recent: Submenu,
    recent_items: Vec<(MenuItem, PathBuf)>,
    idle_icon: Icon,
    recording_icon: Icon,
}

const ID_NEW: &str = "new";
const ID_PAUSE: &str = "pause";
const ID_STOP: &str = "stop";
const ID_FOLDER: &str = "folder";
const ID_PREFS: &str = "prefs";
const ID_QUIT: &str = "quit";

impl Tray {
    pub fn new(recent: &[(String, PathBuf)]) -> anyhow::Result<Self> {
        let new = MenuItem::with_id(ID_NEW, "New Recording…", true, None);
        let pause = MenuItem::with_id(ID_PAUSE, "Pause Recording", false, None);
        let stop = MenuItem::with_id(ID_STOP, "Stop Recording", false, None);
        let recent_menu = Submenu::new("Recent Recordings", true);
        let menu = Menu::new();
        menu.append_items(&[
            &new,
            &pause,
            &stop,
            &PredefinedMenuItem::separator(),
            &recent_menu,
            &MenuItem::with_id(ID_FOLDER, "Open Recordings Folder", true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id(ID_PREFS, "Preferences…", true, None),
            &MenuItem::with_id(ID_QUIT, "Quit Visor", true, None),
        ])?;

        let idle_icon = icon(false)?;
        let recording_icon = icon(true)?;
        let builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Visor");
        // macOS tints template icons to match the menu bar.
        #[cfg(target_os = "macos")]
        let builder = builder.with_icon_templated(idle_icon.clone());
        #[cfg(not(target_os = "macos"))]
        let builder = builder.with_icon(idle_icon.clone());
        let tray = builder.build()?;
        let mut tray = Self {
            icon: tray,
            pause,
            stop,
            new,
            recent: recent_menu,
            recent_items: Vec::new(),
            idle_icon,
            recording_icon,
        };
        tray.set_recent(recent);
        Ok(tray)
    }

    /// Maps a menu event to an action.
    pub fn action(&self, event: &MenuEvent) -> Option<Action> {
        Some(match event.id().as_ref() {
            ID_NEW => Action::NewRecording,
            ID_PAUSE => Action::TogglePause,
            ID_STOP => Action::StopRecording,
            ID_FOLDER => Action::OpenFolder,
            ID_PREFS => Action::Preferences,
            ID_QUIT => Action::Quit,
            id => {
                let (_, path) = self
                    .recent_items
                    .iter()
                    .find(|(item, _)| item.id().as_ref() == id)?;
                Action::OpenRecent(path.clone())
            }
        })
    }

    pub fn set_recording(&self, recording: bool, paused: bool) {
        self.new.set_enabled(!recording);
        self.pause.set_enabled(recording);
        self.stop.set_enabled(recording);
        self.pause.set_text(if paused {
            "Resume Recording"
        } else {
            "Pause Recording"
        });
        let _ = if recording {
            self.icon.set_icon(Some(self.recording_icon.clone()))
        } else {
            #[cfg(target_os = "macos")]
            let set = self.icon.set_icon_templated(Some(self.idle_icon.clone()));
            #[cfg(not(target_os = "macos"))]
            let set = self.icon.set_icon(Some(self.idle_icon.clone()));
            set
        };
    }

    pub fn set_recent(&mut self, recent: &[(String, PathBuf)]) {
        for (item, _) in self.recent_items.drain(..) {
            let _ = self.recent.remove(&item);
        }
        for (i, (title, path)) in recent.iter().enumerate() {
            let item = MenuItem::with_id(format!("recent-{i}"), title, true, None);
            if self.recent.append(&item).is_ok() {
                self.recent_items.push((item, path.clone()));
            }
        }
        self.recent.set_enabled(!self.recent_items.is_empty());
    }
}

/// A 44x44 icon: the app's V with a dot above it (a black template on macOS,
/// which adapts to the menu bar; red elsewhere, visible on light and dark
/// taskbars) or a solid red dot while recording.
fn icon(recording: bool) -> anyhow::Result<Icon> {
    const S: usize = 44;
    // V arms from the top corners to the bottom point, and the dot, in pixels.
    const LEFT: (f32, f32) = (9.0, 15.0);
    const POINT: (f32, f32) = (22.0, 36.0);
    const RIGHT: (f32, f32) = (35.0, 15.0);
    const STROKE: f32 = 3.4;
    const DOT: ((f32, f32), f32) = ((22.0, 12.0), 4.6);

    let mut rgba = vec![0u8; S * S * 4];
    for y in 0..S {
        for x in 0..S {
            let p = (x as f32 + 0.5, y as f32 + 0.5);
            // Signed distance to each shape's edge (negative inside), turned
            // into anti-aliased coverage.
            let cover = |dist: f32| (0.5 - dist).clamp(0.0, 1.0);
            let (alpha, color) = if recording {
                (cover(distance(p, (22.0, 22.0)) - 15.0), [255, 77, 94])
            } else {
                let v = segment_distance(p, LEFT, POINT).min(segment_distance(p, POINT, RIGHT))
                    - STROKE;
                let dot = distance(p, DOT.0) - DOT.1;
                (
                    cover(v.min(dot)),
                    if cfg!(target_os = "macos") {
                        [0, 0, 0]
                    } else {
                        [255, 77, 94]
                    },
                )
            };
            let i = (y * S + x) * 4;
            rgba[i..i + 3].copy_from_slice(&color);
            rgba[i + 3] = (alpha * 255.0) as u8;
        }
    }
    Ok(Icon::from_rgba(rgba, S as u32, S as u32)?)
}

fn distance(a: (f32, f32), b: (f32, f32)) -> f32 {
    ((a.0 - b.0).powi(2) + (a.1 - b.1).powi(2)).sqrt()
}

/// Distance from `p` to the segment `a`–`b`.
fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    distance(p, (a.0 + t * dx, a.1 + t * dy))
}
