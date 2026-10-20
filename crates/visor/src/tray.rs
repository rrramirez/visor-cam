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

/// A 44x44 icon: a ring with a dot (a black template on macOS, which adapts
/// to the menu bar; red elsewhere, visible on light and dark taskbars) or a
/// solid red dot while recording.
fn icon(recording: bool) -> anyhow::Result<Icon> {
    const S: usize = 44;
    let mut rgba = vec![0u8; S * S * 4];
    let c = S as f32 / 2.0;
    for y in 0..S {
        for x in 0..S {
            let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
            let coverage = |inner: f32, outer: f32| {
                ((outer - d + 0.5).clamp(0.0, 1.0)) * ((d - inner + 0.5).clamp(0.0, 1.0))
            };
            let (alpha, color) = if recording {
                (coverage(-1.0, 15.0), [255, 77, 94])
            } else {
                (
                    (coverage(13.0, 17.0) + coverage(-1.0, 8.0)).min(1.0),
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
