//! The Recordings window: every recording in the save folder, to open,
//! rename, show or move to the Trash.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, SystemTime};

use slint::{ComponentHandle, Image, ModelRc, VecModel};
use visor_core::library::Entry;

use crate::app::{app, post};
use crate::{Library, RecordingCard, platform, util};

/// How tall cached thumbnails are, in pixels: sharp on a 2x display.
const THUMB_HEIGHT: u32 = 360;

/// What a recording is in use for, which limits what can be done to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Recording,
    Open,
    Adding,
}

/// How a click picks cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Only,
    /// ⌘-click: in or out of the selection.
    Toggle,
    /// ⇧-click: everything from the last card picked.
    Range,
}

/// The cards picked, by index, in order, and the last one clicked (where
/// a ⇧-click range starts).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    cards: Vec<usize>,
    anchor: Option<usize>,
}

impl Selection {
    pub fn cards(&self) -> &[usize] {
        &self.cards
    }

    pub fn contains(&self, card: usize) -> bool {
        self.cards.contains(&card)
    }

    pub fn click(&mut self, card: usize, pick: Pick) {
        match (pick, self.anchor) {
            (Pick::Toggle, _) => {
                match self.cards.iter().position(|&c| c == card) {
                    Some(i) => {
                        self.cards.remove(i);
                    }
                    None => self.cards.push(card),
                }
                self.cards.sort_unstable();
            }
            (Pick::Range, Some(from)) => {
                self.cards = (from.min(card)..=from.max(card)).collect();
                return;
            }
            _ => self.cards = vec![card],
        }
        self.anchor = Some(card);
    }

    /// Moves to the card `by` away from the last one picked (a row is
    /// `by` = ±columns), staying among `count` cards.
    pub fn step(&mut self, by: isize, count: usize) {
        if count == 0 {
            return;
        }
        let card = match self.anchor.or(self.cards.last().copied()) {
            None => 0,
            Some(from) => {
                let to = from as isize + by;
                if to < 0 || to >= count as isize {
                    from.min(count - 1)
                } else {
                    to as usize
                }
            }
        };
        self.click(card, Pick::Only);
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Why the recordings `selected` (name, and what it's in use for) can't
/// go to the Trash, if one can't.
pub fn trash_refusal(selected: &[(&str, Option<Busy>)]) -> Option<String> {
    selected.iter().find_map(|(name, busy)| {
        busy.map(|b| match b {
            Busy::Open => format!("Close “{name}” in the editor first."),
            Busy::Recording => format!("“{name}” is being recorded; stop it first."),
            Busy::Adding => {
                format!("Videos are being added to “{name}”; wait for that to finish.")
            }
        })
    })
}

/// What to ask before moving `names` to the Trash.
pub fn trash_question(names: &[&str]) -> String {
    match names {
        [one] => format!("Move “{one}” to the Trash?"),
        _ => {
            let list: Vec<String> = names.iter().map(|n| format!("“{n}”")).collect();
            format!(
                "Move {} recordings to the Trash? ({})",
                names.len(),
                list.join(", ")
            )
        }
    }
}

pub struct LibraryWindow {
    ui: Library,
    entries: RefCell<Vec<Entry>>,
    busy: RefCell<Vec<Option<Busy>>>,
    selection: RefCell<Selection>,
    /// Each recording's thumbnail, and the `project.json` change it shows.
    thumbs: RefCell<HashMap<PathBuf, (Option<SystemTime>, Image)>>,
    /// Asked for and not back yet.
    pending: RefCell<HashSet<PathBuf>>,
    renderer: crossbeam_channel::Sender<PathBuf>,
}

/// Where thumbnails are cached: Visor's folder in the user's cache folder.
fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Visor")
        .join("thumbnails")
}

impl LibraryWindow {
    pub fn new() -> anyhow::Result<Rc<Self>> {
        let ui = Library::new()?;
        ui.set_file_manager(platform::FILE_MANAGER.into());
        // One thread renders thumbnails, one at a time, so a big library
        // doesn't take over the computer.
        let (tx, rx) = crossbeam_channel::unbounded::<PathBuf>();
        std::thread::Builder::new()
            .name("visor-thumbnails".into())
            .spawn(move || {
                let cache = cache_dir();
                for dir in rx {
                    let result = visor_media::library::cached_thumbnail(&dir, &cache, THUMB_HEIGHT);
                    if let Err(e) = &result {
                        tracing::info!("no thumbnail for {}: {e:#}", dir.display());
                    }
                    post(move |app| app.library_thumbnail(&dir, result.ok()));
                }
            })?;
        let this = Rc::new_cyclic(|weak: &Weak<LibraryWindow>| {
            wire(&ui, weak.clone());
            LibraryWindow {
                ui: ui.clone_strong(),
                entries: RefCell::default(),
                busy: RefCell::default(),
                selection: RefCell::default(),
                thumbs: RefCell::default(),
                pending: RefCell::default(),
                renderer: tx,
            }
        });
        Ok(this)
    }

    pub fn window(&self) -> &slint::Window {
        self.ui.window()
    }

    pub fn show(&self) {
        let _ = self.ui.show();
    }

    pub fn is_visible(&self) -> bool {
        self.ui.window().is_visible()
    }

    /// Shows `entries` (newest first), what each is in use for, and
    /// starts rendering thumbnails that are missing or out of date.
    pub fn set_entries(&self, entries: Vec<Entry>, busy: Vec<Option<Busy>>) {
        // The selection follows recordings, not places in the grid.
        let picked: Vec<PathBuf> = {
            let old = self.entries.borrow();
            self.selection
                .borrow()
                .cards()
                .iter()
                .filter_map(|&i| old.get(i).map(|e| e.dir.clone()))
                .collect()
        };
        let mut selection = Selection::default();
        for (i, e) in entries.iter().enumerate() {
            if picked.contains(&e.dir) {
                selection.click(i, Pick::Toggle);
            }
        }
        for e in entries.iter().filter(|e| e.info.is_some()) {
            let current = self
                .thumbs
                .borrow()
                .get(&e.dir)
                .is_some_and(|(changed, _)| *changed == e.changed_at);
            if !current && self.pending.borrow_mut().insert(e.dir.clone()) {
                let _ = self.renderer.send(e.dir.clone());
            }
        }
        let dirs: Vec<PathBuf> = entries.iter().map(|e| e.dir.clone()).collect();
        self.thumbs.borrow_mut().retain(|d, _| dirs.contains(d));
        *self.entries.borrow_mut() = entries;
        *self.busy.borrow_mut() = busy;
        *self.selection.borrow_mut() = selection;
        if self.ui.get_renaming() >= self.entries.borrow().len() as i32 {
            self.ui.set_renaming(-1);
        }
        self.show_cards();
    }

    /// A thumbnail rendered (or not) for `dir`.
    pub fn thumbnail(&self, dir: &Path, path: Option<PathBuf>) {
        self.pending.borrow_mut().remove(dir);
        let changed = self
            .entries
            .borrow()
            .iter()
            .find(|e| e.dir == dir)
            .map(|e| e.changed_at);
        let (Some(changed), Some(path)) = (changed, path) else {
            return;
        };
        match Image::load_from_path(&path) {
            Ok(image) => {
                self.thumbs
                    .borrow_mut()
                    .insert(dir.to_path_buf(), (changed, image));
                self.show_cards();
            }
            Err(e) => tracing::info!("can't load {}: {e:?}", path.display()),
        }
    }

    fn show_cards(&self) {
        let entries = self.entries.borrow();
        let busy = self.busy.borrow();
        let selection = self.selection.borrow();
        let thumbs = self.thumbs.borrow();
        let cards: Vec<RecordingCard> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let thumb = thumbs.get(&e.dir).map(|(_, image)| image.clone());
                RecordingCard {
                    title: e.title.as_str().into(),
                    detail: detail(e).into(),
                    has_thumb: thumb.is_some(),
                    thumb: thumb.unwrap_or_default(),
                    readable: e.info.is_some(),
                    badge: match busy.get(i).copied().flatten() {
                        Some(Busy::Recording) => "Recording",
                        Some(Busy::Open | Busy::Adding) => "Open",
                        None => "",
                    }
                    .into(),
                    selected: selection.contains(i),
                }
            })
            .collect();
        self.ui.set_cards(ModelRc::new(VecModel::from(cards)));
    }

    /// The recordings selected: folder, name and what they're in use for.
    pub fn selected(&self) -> Vec<(PathBuf, String, Option<Busy>)> {
        let entries = self.entries.borrow();
        let busy = self.busy.borrow();
        self.selection
            .borrow()
            .cards()
            .iter()
            .filter_map(|&i| {
                let e = entries.get(i)?;
                Some((
                    e.dir.clone(),
                    e.title.clone(),
                    busy.get(i).copied().flatten(),
                ))
            })
            .collect()
    }

    fn entry(&self, card: i32) -> Option<Entry> {
        let i = usize::try_from(card).ok()?;
        self.entries.borrow().get(i).cloned()
    }

    pub fn set_note(&self, note: &str) {
        self.ui.set_note(note.into());
    }
}

/// "Nov 27, 2026 · 1:23", or why it can't be opened.
fn detail(e: &Entry) -> String {
    let date = e.recorded_at.map(|t| {
        chrono::DateTime::<chrono::Local>::from(t)
            .format("%b %-d, %Y")
            .to_string()
    });
    let length = e.info.as_ref().map(|i| util::clock(i.video_us));
    match (date, length) {
        (Some(d), Some(l)) => format!("{d} · {l}"),
        (None, Some(l)) => l,
        (Some(d), None) => format!("{d} · can't be read"),
        (None, None) => "Can't be read".into(),
    }
}

fn wire(ui: &Library, weak: Weak<LibraryWindow>) {
    macro_rules! on {
        ($setter:ident, |$w:ident $(, $arg:ident)*| $body:expr) => {{
            let weak = weak.clone();
            ui.$setter(move |$($arg),*| {
                if let Some($w) = weak.upgrade() {
                    $body
                }
            });
        }};
    }
    on!(on_card_clicked, |w, card, pick| {
        w.set_note("");
        match usize::try_from(card) {
            Ok(i) => w.selection.borrow_mut().click(
                i,
                match pick {
                    1 => Pick::Toggle,
                    2 => Pick::Range,
                    _ => Pick::Only,
                },
            ),
            Err(_) => w.selection.borrow_mut().clear(),
        }
        w.show_cards();
    });
    on!(on_menu_for, |w, card| {
        if let Ok(i) = usize::try_from(card)
            && !w.selection.borrow().contains(i)
        {
            w.selection.borrow_mut().click(i, Pick::Only);
            w.show_cards();
        }
    });
    on!(on_step, |w, by| {
        let count = w.entries.borrow().len();
        w.selection.borrow_mut().step(by as isize, count);
        w.show_cards();
    });
    on!(on_open, |w, card| {
        // -1: the one selected (Return).
        let entry = if card >= 0 {
            w.entry(card)
        } else {
            match w.selection.borrow().cards() {
                [one] => w.entries.borrow().get(*one).cloned(),
                _ => None,
            }
        };
        if let Some(e) = entry.filter(|e| e.info.is_some()) {
            app().open_editor(e.dir);
        }
    });
    on!(on_rename, |w, card, title| {
        if let Some(e) = w.entry(card) {
            app().library_rename(&e.dir, title.trim());
        }
    });
    on!(on_reveal, |w, card| {
        if let Some(e) = w.entry(card) {
            platform::reveal(&e.dir);
        }
    });
    on!(on_reveal_exports, |w, card| {
        if let Some(e) = w.entry(card) {
            platform::reveal(&last_export(&e.dir));
        }
    });
    on!(on_reveal_all, |_w| {
        platform::open_folder(&app().settings.borrow().save_dir);
    });
    on!(on_new_recording, |_w| app().open_panel());
    on!(on_trash, |_w| app().library_trash());
    {
        // Back from Finder or the editor: what changed there shows.
        use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
        ui.window().on_winit_window_event(|_, event| {
            if let winit::event::WindowEvent::Focused(true) = event {
                // After this event, not inside it.
                slint::Timer::single_shot(Duration::ZERO, || app().library_changed());
            }
            EventResult::Propagate
        });
    }
    ui.window().on_close_requested(|| {
        post(|app| app.update_dock());
        slint::CloseRequestResponse::HideWindow
    });
}

/// The newest file in the recording's exports, else the exports folder (or
/// the recording, if it has none).
fn last_export(dir: &Path) -> PathBuf {
    let exports = dir.join("exports");
    std::fs::read_dir(&exports)
        .ok()
        .and_then(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
                .map(|e| e.path())
        })
        .unwrap_or(if exports.is_dir() {
            exports
        } else {
            dir.to_path_buf()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_pick_one_toggle_or_a_range() {
        let mut s = Selection::default();
        s.click(2, Pick::Only);
        assert_eq!(s.cards(), [2]);
        s.click(5, Pick::Toggle);
        assert_eq!(s.cards(), [2, 5]);
        s.click(2, Pick::Toggle);
        assert_eq!(s.cards(), [5]);
        // From the last one clicked, as Finder does.
        s.click(4, Pick::Range);
        assert_eq!(s.cards(), [2, 3, 4]);
        s.click(5, Pick::Toggle);
        s.click(3, Pick::Range);
        assert_eq!(s.cards(), [3, 4, 5]);
        s.click(0, Pick::Only);
        assert_eq!(s.cards(), [0]);
        // Arrows: one along, a row (4 cards) up or down, kept inside.
        s.step(1, 10);
        assert_eq!(s.cards(), [1]);
        s.step(4, 10);
        assert_eq!(s.cards(), [5]);
        s.step(4, 10);
        assert_eq!(s.cards(), [9]);
        s.step(4, 10);
        assert_eq!(s.cards(), [9]);
        s.step(-1, 10);
        assert_eq!(s.cards(), [8]);
        let mut empty = Selection::default();
        empty.step(1, 3);
        assert_eq!(empty.cards(), [0]);
    }

    #[test]
    fn busy_recordings_are_not_trashed() {
        assert_eq!(trash_refusal(&[("Intro", None), ("Demo", None)]), None);
        assert_eq!(
            trash_refusal(&[("Intro", None), ("Demo", Some(Busy::Open))]),
            Some("Close “Demo” in the editor first.".to_string())
        );
        assert_eq!(
            trash_refusal(&[("Demo", Some(Busy::Recording))]),
            Some("“Demo” is being recorded; stop it first.".to_string())
        );
        assert_eq!(
            trash_refusal(&[("Demo", Some(Busy::Adding))]),
            Some("Videos are being added to “Demo”; wait for that to finish.".to_string())
        );
        assert_eq!(trash_question(&["Intro"]), "Move “Intro” to the Trash?");
        assert_eq!(
            trash_question(&["Intro", "Demo", "Outro"]),
            "Move 3 recordings to the Trash? (“Intro”, “Demo”, “Outro”)"
        );
    }
}
