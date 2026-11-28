//! The Recordings window's side of the app: what's listed, what each
//! recording is in use for, and renaming and trashing them.

use std::path::Path;

use crate::library::{Busy, LibraryWindow, trash_question, trash_refusal};

use super::*;

impl App {
    /// Opens the Recordings window, or brings it forward.
    pub fn open_library(&self) {
        if self.library.borrow().is_none() {
            match LibraryWindow::new() {
                Ok(w) => *self.library.borrow_mut() = Some(w),
                Err(e) => return tracing::error!("could not open the Recordings window: {e:#}"),
            }
        }
        if let Some(w) = self.library.borrow().clone() {
            w.set_note("");
            w.show();
            platform::focus(w.window());
        }
        self.library_changed();
        self.update_dock();
        platform::activate();
    }

    /// Starts a recording from video files the user picks, and opens it
    /// in the editor once they're copied in.
    pub fn new_from_video(&self) {
        platform::activate();
        let Some(paths) = rfd::FileDialog::new()
            .set_title("Start from videos")
            .add_filter("Videos", &visor_media::import::picker_extensions())
            .pick_files()
        else {
            return;
        };
        let root = self.settings.borrow().save_dir.clone();
        std::thread::spawn(move || {
            let made = visor_media::import::new_recording(
                &root,
                &paths,
                None,
                &mut |_| {},
                &std::sync::atomic::AtomicBool::new(false),
            );
            post(move |app| match made {
                Ok(dir) => {
                    app.refresh_tray();
                    app.open_editor(dir);
                }
                Err(e) => {
                    tracing::warn!("couldn't start from videos: {e:#}");
                    let _ = rfd::MessageDialog::new()
                        .set_title("Visor")
                        .set_description(format!("Couldn't start from those videos: {e:#}"))
                        .show();
                }
            });
        });
    }

    /// Lists the recordings again, if the Recordings window is showing.
    pub fn library_changed(&self) {
        let Some(w) = self.library.borrow().clone().filter(|w| w.is_visible()) else {
            return;
        };
        let root = self.settings.borrow().save_dir.clone();
        let entries = visor_core::library::entries(&root).unwrap_or_default();
        let busy = entries.iter().map(|e| self.busy_for(&e.dir)).collect();
        // Thumbnails of recordings gone (trashed, renamed, moved away).
        let dirs: Vec<PathBuf> = entries.iter().map(|e| e.dir.clone()).collect();
        visor_media::library::prune_thumbnails(
            &crate::library::cache_dir(),
            &dirs,
            crate::library::THUMB_HEIGHT,
        );
        w.set_entries(entries, busy);
    }

    pub(crate) fn library_thumbnail(
        &self,
        dir: &Path,
        rendered: Option<(PathBuf, Option<std::time::SystemTime>)>,
    ) {
        if let Some(w) = self.library.borrow().clone() {
            w.thumbnail(dir, rendered);
        }
    }

    pub(super) fn library_visible(&self) -> bool {
        self.library
            .borrow()
            .as_ref()
            .is_some_and(|w| w.is_visible())
    }

    /// What the recording in `dir` is in use for, if anything.
    fn busy_for(&self, dir: &Path) -> Option<Busy> {
        let dir = visor_core::project::real_path(dir).unwrap_or_else(|_| dir.to_path_buf());
        let same =
            |d: &Path| visor_core::project::real_path(d).ok().as_deref() == Some(dir.as_path());
        let recording = match &*self.session.borrow() {
            Session::Recording { recorder, .. } => same(recorder.dir()),
            Session::Starting {
                recorder: Some(r), ..
            } => same(r.dir()),
            _ => false,
        };
        let more = self.more.borrow().as_deref().is_some_and(same)
            && !matches!(*self.session.borrow(), Session::Idle);
        if recording || more || self.more_cancelled.borrow().as_deref().is_some_and(same) {
            Some(Busy::Recording)
        } else if self.editor_for(&dir).is_some() {
            Some(Busy::Open)
        } else if self.copying(&dir) {
            Some(Busy::Adding)
        } else {
            None
        }
    }

    /// Renames a recording from the Recordings window: through its editor
    /// when it's open (the folder follows when that closes), else here,
    /// folder and all.
    pub fn library_rename(&self, dir: &Path, title: &str) {
        if title.is_empty() {
            return;
        }
        // Editors and agents know folders by their real paths.
        let dir = &visor_core::project::real_path(dir).unwrap_or_else(|_| dir.to_path_buf());
        let note = |text: &str| {
            if let Some(w) = self.library.borrow().clone() {
                w.set_note(text);
            }
        };
        let busy = self.busy_for(dir);
        // Being recorded (more onto, its editor hidden): the take's save
        // would put the old name back.
        if busy == Some(Busy::Recording) {
            return note("It's being recorded; rename it once it's stopped.");
        }
        if let Some(editor) = self.editor_for(dir) {
            editor.rename(title);
            return self.library_changed();
        }
        match busy {
            Some(Busy::Recording) => {}
            Some(Busy::Adding) => {
                return note(
                    "Something is being written into it (videos added, or an export); rename it once that's done.",
                );
            }
            // Open, but its editor wasn't found above: never rename under it.
            Some(Busy::Open) => return note("Rename it in its editor."),
            None => {}
        }
        let renamed = visor_core::Project::load(dir)
            .map_err(anyhow::Error::from)
            .and_then(|mut p| {
                p.title = title.to_string();
                p.save(dir)?;
                let root = self.settings.borrow().save_dir.clone();
                Ok(visor_core::project::rename_to_title(dir, &root)?)
            });
        match renamed {
            Ok(Some(to)) => {
                self.agent_folder_renamed(dir, &to);
                self.refresh_tray();
                if let Some(w) = self.library.borrow().clone() {
                    w.select(&to);
                }
            }
            Ok(None) => self.library_changed(),
            Err(e) => note(&format!("Couldn't rename it: {e:#}")),
        }
    }

    /// Moves the recordings selected in the Recordings window to the
    /// Trash, after asking; refused for any in use.
    pub fn library_trash(&self) {
        let Some(w) = self.library.borrow().clone() else {
            return;
        };
        // As things are now, not as they were when last listed.
        let selected: Vec<_> = w
            .selected()
            .into_iter()
            .map(|(dir, title, _)| {
                let busy = self.busy_for(&dir);
                (dir, title, busy)
            })
            .collect();
        if selected.is_empty() {
            return;
        }
        let named: Vec<(&str, Option<Busy>)> =
            selected.iter().map(|(_, t, b)| (t.as_str(), *b)).collect();
        if let Some(why) = trash_refusal(&named) {
            return w.set_note(&why);
        }
        let names: Vec<&str> = named.iter().map(|(t, _)| *t).collect();
        let answer = rfd::MessageDialog::new()
            .set_title("Move to Trash")
            .set_description(trash_question(&names))
            .set_level(rfd::MessageLevel::Warning)
            .set_buttons(rfd::MessageButtons::OkCancelCustom(
                "Move to Trash".into(),
                "Cancel".into(),
            ))
            .show();
        if !matches!(answer, rfd::MessageDialogResult::Custom(ref b) if b == "Move to Trash")
            && !matches!(answer, rfd::MessageDialogResult::Ok)
        {
            return;
        }
        let dirs: Vec<PathBuf> = selected.into_iter().map(|(d, _, _)| d).collect();
        #[allow(unused_mut)]
        let mut trash = trash::TrashContext::default();
        // Through the file manager's own call, not by asking Finder to do it
        // (an Apple Event the app isn't allowed to send).
        #[cfg(target_os = "macos")]
        {
            use trash::macos::{DeleteMethod, TrashContextExtMacos};
            trash.set_delete_method(DeleteMethod::NsFileManager);
        }
        match trash.delete_all(&dirs) {
            Ok(()) => w.set_note(""),
            Err(e) => w.set_note(&format!("Couldn't move it to the Trash: {e}")),
        }
        self.refresh_tray();
    }
}
