//! The editor window: preview playback, trim/cut editing with undo, camera
//! bubble layout and export.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel};
use visor_core::project::BubbleShape;
use visor_core::{EditList, Micros, Project};
use visor_media::player::Player;
use visor_media::{ExportFormat, ExportOptions};

use crate::app::app;
use crate::platform;
use crate::util;
use crate::{Editor, Region, Thumb};

const THUMBNAILS: usize = 24;
const WAVE_BUCKETS: usize = 600;

pub struct EditorWindow {
    ui: Editor,
    dir: PathBuf,
    project: RefCell<Project>,
    player: RefCell<Player>,
    undo: RefCell<Vec<EditList>>,
    redo: RefCell<Vec<EditList>>,
    export: RefCell<Option<ExportJob>>,
    last_export: RefCell<Option<PathBuf>>,
    _timer: Timer,
}

struct ExportJob {
    out: PathBuf,
    /// Format name for the summary, e.g. "MP4".
    kind: &'static str,
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
    result: Arc<Mutex<Option<anyhow::Result<()>>>>,
}

impl EditorWindow {
    pub fn open(dir: PathBuf, default_format: i32) -> anyhow::Result<Rc<Self>> {
        let project = visor_media::recover::load_or_recover(&dir)?;
        let player = Player::open(&dir, &project, (1920, 1200), 360)?;
        let ui = Editor::new()?;

        ui.set_project_title(project.title.clone().into());
        if let Some(t) = &project.tracks.screen
            && let (Some(w), Some(h)) = (t.width, t.height)
        {
            ui.set_screen_aspect(w as f32 / h.max(1) as f32);
        }
        ui.set_has_camera(project.tracks.camera.is_some());
        let o = &project.camera_overlay;
        ui.set_bubble_visible(o.visible);
        ui.set_bubble_shape(if o.shape == BubbleShape::Circle { 0 } else { 1 });
        ui.set_bubble_x(o.x as f32);
        ui.set_bubble_y(o.y as f32);
        ui.set_bubble_size(o.size as f32);
        ui.set_bubble_mirror(o.mirror);
        ui.set_speed(project.speed as f32);
        {
            let folded = app().settings.borrow().folded_sections.clone();
            let open = |name: &str| !folded.iter().any(|s| s == name);
            ui.set_edit_open(open("edit"));
            ui.set_camera_open(open("camera"));
            ui.set_speed_open(open("speed"));
        }
        ui.set_export_format(default_format.clamp(0, 3));
        ui.set_file_manager(platform::FILE_MANAGER.into());
        ui.set_can_copy(platform::CAN_COPY_FILE);

        let this = Rc::new_cyclic(|weak: &Weak<EditorWindow>| {
            let timer = Timer::default();
            let w = weak.clone();
            timer.start(TimerMode::Repeated, Duration::from_millis(16), move || {
                if let Some(e) = w.upgrade() {
                    e.tick();
                }
            });
            EditorWindow {
                ui: ui.clone_strong(),
                dir: dir.clone(),
                project: RefCell::new(project),
                player: RefCell::new(player),
                undo: RefCell::default(),
                redo: RefCell::default(),
                export: RefCell::default(),
                last_export: RefCell::default(),
                _timer: timer,
            }
        });
        this.wire(Rc::downgrade(&this));
        this.refresh_edit_ui();
        this.load_previews();
        ui.show()?;
        Ok(this)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn show(&self) {
        let _ = self.ui.show();
    }

    fn wire(&self, weak: Weak<EditorWindow>) {
        let ui = &self.ui;
        macro_rules! on {
            ($setter:ident, |$e:ident $(, $arg:ident)*| $body:expr) => {{
                let w = weak.clone();
                ui.$setter(move |$($arg),*| {
                    if let Some($e) = w.upgrade() {
                        $body
                    }
                });
            }};
        }
        on!(on_toggle_play, |e| e.player.borrow_mut().toggle());
        on!(on_seek, |e, f| {
            let d = e.duration();
            e.player
                .borrow_mut()
                .seek((f64::from(f) * d as f64) as Micros);
        });
        on!(on_selection_changed, |e, _a, _b| e
            .ui
            .set_kept_label(e.kept_label().into()));
        on!(on_set_in, |e| {
            let p = e.position_fraction();
            e.ui.set_sel_start(p);
            if e.ui.get_sel_end() <= p {
                e.ui.set_sel_end((p + 0.05).min(1.0));
            }
            e.ui.set_kept_label(e.kept_label().into());
        });
        on!(on_set_out, |e| {
            let p = e.position_fraction();
            if e.ui.get_sel_start() < 0.0 || e.ui.get_sel_start() >= p {
                e.ui.set_sel_start((p - 0.05).max(0.0));
            }
            e.ui.set_sel_end(p);
            e.ui.set_kept_label(e.kept_label().into());
        });
        on!(on_cut_selection, |e| {
            if let Some((a, b)) = e.selection() {
                e.apply(|edit| edit.cut(a, b));
            }
        });
        on!(on_trim_to_selection, |e| {
            if let Some((a, b)) = e.selection() {
                e.apply(|edit| edit.trim(a, b));
            }
        });
        on!(on_restore, |e| {
            let d = e.duration();
            e.apply(|edit| *edit = EditList::full(d));
        });
        on!(on_undo, |e| e.undo_redo(true));
        on!(on_redo, |e| e.undo_redo(false));
        on!(on_step, |e, frames| {
            let pos = e.player.borrow().position() as i64;
            let to = (pos + i64::from(frames) * 33_333).max(0) as Micros;
            e.player.borrow_mut().seek(to);
        });
        on!(on_bubble_changed, |e| {
            {
                let mut p = e.project.borrow_mut();
                let o = &mut p.camera_overlay;
                o.visible = e.ui.get_bubble_visible();
                o.shape = if e.ui.get_bubble_shape() == 0 {
                    BubbleShape::Circle
                } else {
                    BubbleShape::RoundedRect
                };
                o.x = f64::from(e.ui.get_bubble_x());
                o.y = f64::from(e.ui.get_bubble_y());
                o.size = f64::from(e.ui.get_bubble_size());
                o.mirror = e.ui.get_bubble_mirror();
            }
            e.save();
        });
        on!(on_sections_changed, |e| {
            let folded = [
                ("edit", e.ui.get_edit_open()),
                ("camera", e.ui.get_camera_open()),
                ("speed", e.ui.get_speed_open()),
            ]
            .into_iter()
            .filter(|(_, open)| !open)
            .map(|(name, _)| name.to_string())
            .collect();
            let app = app();
            let mut settings = app.settings.borrow_mut();
            settings.folded_sections = folded;
            settings.save();
        });
        on!(on_speed_changed, |e| {
            let speed = f64::from(e.ui.get_speed()).clamp(0.5, 2.0);
            e.project.borrow_mut().speed = speed;
            e.player.borrow_mut().set_speed(speed);
            e.save();
            e.refresh_edit_ui();
        });
        on!(on_title_edited, |e, title| {
            let title = title.trim().to_string();
            if !title.is_empty() {
                e.project.borrow_mut().title = title;
                e.save();
            }
        });
        on!(on_export, |e| e.start_export());
        on!(on_cancel_export, |e| {
            if let Some(job) = &*e.export.borrow() {
                job.cancel.store(true, Ordering::Relaxed);
            }
        });
        on!(on_reveal_export, |e| {
            if let Some(path) = &*e.last_export.borrow() {
                platform::reveal(path);
            }
        });
        on!(on_reveal_project, |e| platform::reveal(
            &e.dir.join(visor_core::project::PROJECT_FILE)
        ));
        on!(on_open_export, |e| {
            if let Some(path) = &*e.last_export.borrow() {
                platform::open_file(path);
            }
        });
        on!(on_copy_export, |e| {
            if let Some(path) = &*e.last_export.borrow() {
                e.ui.set_copied(platform::copy_file(path));
            }
        });
        on!(on_done, |e| {
            e.closing();
            let _ = e.ui.hide();
        });

        let w = weak.clone();
        ui.window().on_close_requested(move || {
            if let Some(e) = w.upgrade() {
                e.closing();
            }
            slint::CloseRequestResponse::HideWindow
        });
    }

    /// Stops work for a window that is being hidden, then lets the app drop it.
    fn closing(&self) {
        if let Some(job) = &*self.export.borrow() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.player.borrow_mut().pause();
        let dir = self.dir.clone();
        // Drop the window (and its player threads) after this callback.
        slint::Timer::single_shot(Duration::ZERO, move || app().editor_closed(&dir));
    }

    fn duration(&self) -> Micros {
        self.project.borrow().duration_us.max(1)
    }

    fn position_fraction(&self) -> f32 {
        (self.player.borrow().position() as f64 / self.duration() as f64) as f32
    }

    /// The selection in source time, if any.
    fn selection(&self) -> Option<(Micros, Micros)> {
        let (a, b) = (self.ui.get_sel_start(), self.ui.get_sel_end());
        (a >= 0.0 && b > a).then(|| {
            let d = self.duration() as f64;
            ((f64::from(a) * d) as Micros, (f64::from(b) * d) as Micros)
        })
    }

    fn apply(&self, change: impl FnOnce(&mut EditList)) {
        let before = self.project.borrow().edit.clone();
        let mut after = before.clone();
        change(&mut after);
        if after == before {
            return;
        }
        if after.duration() == 0 {
            self.ui
                .set_kept_label("That would remove everything; nothing changed.".into());
            return;
        }
        self.undo.borrow_mut().push(before);
        self.redo.borrow_mut().clear();
        self.set_edit(after);
        self.ui.set_sel_start(-1.0);
        self.ui.set_sel_end(-1.0);
    }

    fn undo_redo(&self, undo: bool) {
        let (from, to) = if undo {
            (&self.undo, &self.redo)
        } else {
            (&self.redo, &self.undo)
        };
        let Some(edit) = from.borrow_mut().pop() else {
            return;
        };
        to.borrow_mut().push(self.project.borrow().edit.clone());
        self.set_edit(edit);
    }

    fn set_edit(&self, edit: EditList) {
        self.project.borrow_mut().edit = edit.clone();
        self.player.borrow_mut().set_edit(edit);
        self.save();
        self.refresh_edit_ui();
    }

    fn save(&self) {
        if let Err(e) = self.project.borrow().save(&self.dir) {
            tracing::error!("saving project.json failed: {e}");
        }
    }

    fn refresh_edit_ui(&self) {
        let project = self.project.borrow();
        let d = project.duration_us.max(1) as f64;
        let mut cuts = Vec::new();
        let mut last = 0;
        for r in project.edit.ranges() {
            if r.0 > last {
                cuts.push(Region {
                    start: (last as f64 / d) as f32,
                    end: (r.0 as f64 / d) as f32,
                });
            }
            last = r.1;
        }
        if last < project.duration_us {
            cuts.push(Region {
                start: (last as f64 / d) as f32,
                end: 1.0,
            });
        }
        self.ui.set_cuts(ModelRc::new(VecModel::from(cuts)));
        self.ui.set_can_undo(!self.undo.borrow().is_empty());
        self.ui.set_can_redo(!self.redo.borrow().is_empty());
        self.ui
            .set_edited(project.edit != EditList::full(project.duration_us));
        let kept = project.edit.duration();
        let speed_label = if (project.speed - 1.0).abs() < 1e-6 {
            format!("Normal speed, {}", util::clock(kept))
        } else {
            format!(
                "Exports at {} instead of {}",
                util::clock(project.output_duration()),
                util::clock(kept)
            )
        };
        self.ui.set_speed_label(speed_label.into());
        drop(project);
        self.ui.set_kept_label(self.kept_label().into());
    }

    fn kept_label(&self) -> String {
        let p = self.project.borrow();
        let kept = p.edit.duration();
        let mut label = if kept == p.duration_us {
            format!("Full recording, {}", util::clock(kept))
        } else {
            format!(
                "Keeping {} of {}",
                util::clock(kept),
                util::clock(p.duration_us)
            )
        };
        if let Some((a, b)) = self.selection() {
            label.push_str(&format!(" · Selected {}", util::clock_tenths(b - a)));
        }
        label
    }

    fn tick(&self) {
        let frames = self.player.borrow_mut().take_frames();
        if let Some((screen, camera)) = frames {
            if let Some(f) = screen {
                self.ui.set_screen(util::preview_image(&f));
            }
            if let Some(f) = camera {
                self.ui.set_camera(util::preview_image(&f));
            }
        }
        let (pos, playing) = {
            let p = self.player.borrow();
            (p.position(), p.is_playing())
        };
        self.ui.set_playing(playing);
        self.ui
            .set_position((pos as f64 / self.duration() as f64) as f32);
        let (edit, speed) = {
            let p = self.project.borrow();
            (p.edit.clone(), p.speed)
        };
        // Times as they will be in the export, after cuts and speed.
        let out = edit.to_output(pos).unwrap_or_else(|| edit.duration());
        let sped = |t: Micros| (t as f64 / speed) as Micros;
        self.ui.set_time_label(
            format!(
                "{} / {}",
                util::clock_tenths(sped(out)),
                util::clock_tenths(sped(edit.duration()))
            )
            .into(),
        );
        self.poll_export();
    }

    fn load_previews(&self) {
        let project = self.project.borrow();
        let duration = project.duration_us;
        let screen = project
            .tracks
            .screen
            .as_ref()
            .map(|t| self.dir.join(&t.file));
        let audio: Vec<PathBuf> = [&project.tracks.mic, &project.tracks.system]
            .into_iter()
            .flatten()
            .map(|t| self.dir.join(&t.file))
            .collect();
        let weak = self.ui.as_weak();
        std::thread::spawn(move || {
            if !audio.is_empty() {
                let paths: Vec<&Path> = audio.iter().map(PathBuf::as_path).collect();
                match visor_media::preview::waveform(&paths, duration, WAVE_BUCKETS) {
                    Ok(peaks) => {
                        let path = util::wave_path(&peaks);
                        let _ = weak.upgrade_in_event_loop(move |ui| ui.set_wave_path(path.into()));
                    }
                    Err(e) => tracing::warn!("waveform failed: {e:#}"),
                }
            }
            if let Some(screen) = screen {
                match visor_media::preview::thumbnails(&screen, duration, THUMBNAILS, 64) {
                    Ok(thumbs) => {
                        let thumbs: Vec<_> = thumbs
                            .into_iter()
                            .map(|(_, f)| {
                                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                    &f.rgba, f.width, f.height,
                                )
                            })
                            .collect();
                        let _ = weak.upgrade_in_event_loop(move |ui| {
                            let n = thumbs.len().max(1) as f32;
                            let model: Vec<Thumb> = thumbs
                                .into_iter()
                                .enumerate()
                                .map(|(i, buf)| Thumb {
                                    image: slint::Image::from_rgba8(buf),
                                    start: i as f32 / n,
                                    end: (i + 1) as f32 / n,
                                })
                                .collect();
                            ui.set_thumbs(ModelRc::new(VecModel::from(model)));
                        });
                    }
                    Err(e) => tracing::warn!("thumbnails failed: {e:#}"),
                }
            }
        });
    }

    fn start_export(&self) {
        if self.export.borrow().is_some() {
            return;
        }
        self.player.borrow_mut().pause();
        let (format, kind) = match self.ui.get_export_format() {
            1 => (ExportFormat::WebM { av1: false }, "WebM"),
            2 => (ExportFormat::WebM { av1: true }, "WebM (AV1)"),
            3 => (ExportFormat::Gif, "GIF"),
            _ => (ExportFormat::Mp4, "MP4"),
        };
        let mut options = ExportOptions::new(format);
        options.max_height = match self.ui.get_export_resolution() {
            1 => Some(1080),
            2 => Some(720),
            _ => None,
        };
        let project = self.project.borrow().clone();
        let out = unique_path(
            &self.dir.join("exports"),
            &sanitize(&project.title),
            format.extension(),
        );
        if let Some(parent) = out.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            self.ui
                .set_export_status(format!("Export failed: {e}").into());
            return;
        }
        let job = ExportJob {
            out: out.clone(),
            kind,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(AtomicU32::new(0)),
            result: Arc::new(Mutex::new(None)),
        };
        let (cancel, progress, result) =
            (job.cancel.clone(), job.progress.clone(), job.result.clone());
        let dir = self.dir.clone();
        std::thread::spawn(move || {
            let r = visor_media::export(
                &dir,
                &project,
                &out,
                &options,
                &mut |p| progress.store(p.to_bits(), Ordering::Relaxed),
                &cancel,
            );
            *result.lock().unwrap() = Some(r);
        });
        self.ui.set_exporting(true);
        self.ui.set_export_done(false);
        self.ui.set_export_card(false);
        self.ui.set_copied(false);
        self.ui.set_export_progress(0.0);
        self.ui.set_export_status("Exporting… 0%".into());
        *self.export.borrow_mut() = Some(job);
    }

    fn poll_export(&self) {
        let finished = {
            let job = self.export.borrow();
            let Some(job) = job.as_ref() else { return };
            let p = f32::from_bits(job.progress.load(Ordering::Relaxed));
            self.ui.set_export_progress(p);
            self.ui
                .set_export_status(format!("Exporting… {:.0}%", p * 100.0).into());
            job.result.lock().unwrap().take()
        };
        let Some(result) = finished else { return };
        let job = self.export.borrow_mut().take().expect("job present");
        self.ui.set_exporting(false);
        match result {
            Ok(()) => {
                let name = job
                    .out
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let mut detail = vec![job.kind.to_string()];
                if let Ok(meta) = std::fs::metadata(&job.out) {
                    detail.push(util::file_size(meta.len()));
                }
                detail.push(util::clock(self.project.borrow().output_duration()));
                self.ui.set_export_done(true);
                self.ui.set_export_status(format!("Saved {name}").into());
                self.ui.set_export_name(name.into());
                self.ui.set_export_detail(detail.join(" · ").into());
                self.ui.set_copied(false);
                self.ui.set_export_card(true);
                *self.last_export.borrow_mut() = Some(job.out);
            }
            Err(e) if job.cancel.load(Ordering::Relaxed) => {
                tracing::info!("export cancelled: {e:#}");
                self.ui.set_export_done(false);
                self.ui.set_export_status("Export cancelled".into());
            }
            Err(e) => {
                tracing::error!("export failed: {e:#}");
                self.ui.set_export_done(false);
                self.ui
                    .set_export_status(format!("Export failed: {e:#}").into());
            }
        }
    }
}

fn sanitize(title: &str) -> String {
    let cleaned: String = title
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '-'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        "Recording".into()
    } else {
        trimmed.into()
    }
}

/// `dir/name.ext`, or `dir/name 2.ext`, … if taken.
fn unique_path(dir: &Path, name: &str, ext: &str) -> PathBuf {
    let mut path = dir.join(format!("{name}.{ext}"));
    let mut n = 2;
    while path.exists() {
        path = dir.join(format!("{name} {n}.{ext}"));
        n += 1;
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_names_are_safe_and_unique() {
        assert_eq!(sanitize(" Demo: a/b? "), "Demo- a-b-");
        assert_eq!(sanitize("  "), "Recording");
        let dir = std::env::temp_dir().join(format!("visor-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.mp4"), b"").unwrap();
        assert_eq!(unique_path(&dir, "x", "mp4"), dir.join("x 2.mp4"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
