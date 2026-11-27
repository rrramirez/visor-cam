//! The editor window: preview playback, trim/cut editing with undo, camera
//! bubble layout and export.

use std::cell::{Cell, RefCell, RefMut};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, Timer, TimerMode, VecModel};
use visor_core::project::BubbleShape;
use visor_core::{Addition, Blur, EditList, Layout, Marker, Micros, Project, TrackKind};
use visor_media::dead_time;
use visor_media::player::Player;
use visor_media::{ExportFormat, ExportOptions};

use crate::app::app;
use crate::platform;
use crate::util;
use crate::{BlurBand, BlurRow, Clip, Editor, Gone, MarkerMark, Thumb, Tick};

const THUMBNAILS: usize = 24;
const WAVE_BUCKETS: usize = 600;
/// Ticks with nothing to update before the UI timer stops.
const IDLE_TICKS: u32 = 2;

pub struct EditorWindow {
    ui: Editor,
    dir: PathBuf,
    project: RefCell<Project>,
    player: RefCell<Player>,
    undo: RefCell<Vec<Step>>,
    redo: RefCell<Vec<Step>>,
    export: RefCell<Option<ExportJob>>,
    last_export: RefCell<Option<PathBuf>>,
    /// Polls the player and export while something is happening; stopped
    /// when idle so an open editor doesn't keep waking the CPU.
    timer: Timer,
    idle_ticks: Cell<u32>,
    /// Why the last save failed, for an agent's edit to report.
    save_error: RefCell<Option<String>>,
    /// The edit after the last nudge of a clip's edge, and that edge, so
    /// the nudges that follow join its step to undo.
    nudged: RefCell<Option<(EditList, bool, Micros)>>,
    /// The blurs after the last nudge of a blur's edge, likewise.
    blur_nudged: RefCell<Option<Vec<Blur>>>,
    /// Background work writing into the recording's folder (exports,
    /// thumbnail images); the folder isn't renamed while any runs.
    jobs: Jobs,
    /// Set to stop looking for pauses, when the window closes.
    pauses_cancel: RefCell<Option<Arc<AtomicBool>>>,
    /// Recordings or videos being copied in to add.
    adding: RefCell<Option<AddJob>>,
    /// Files copied in for additions, to delete when the editor closes if
    /// the recording no longer has them (undone).
    added_files: RefCell<Vec<String>>,
    /// The recordings the *Add video…* menu lists, in its order.
    recent: RefCell<Vec<PathBuf>>,
    /// Files dropped on the window, gathered until the drop is over.
    dropped: Rc<RefCell<Vec<PathBuf>>>,
}

/// Hears where each added recording or video starts, in source time, with
/// the recording as it now is; or why nothing was added.
pub type AddReply = Box<dyn FnOnce(Result<(Project, Vec<Micros>), String>)>;

struct AddJob {
    cancel: Arc<AtomicBool>,
    progress: Arc<AtomicU32>,
    result: Arc<Mutex<Option<anyhow::Result<visor_media::import::Prepared>>>>,
    reply: Option<AddReply>,
}

/// A step to undo or redo: the edit, the markers or the blurs, as they
/// were.
enum Step {
    Edit(EditList),
    Markers(Vec<Marker>),
    Blurs(Vec<Blur>),
    /// Recordings or videos added from this source time on: undoing takes
    /// them out (to redo).
    Added(Micros),
    /// What was added from that time on, taken out (to undo), to put back.
    TakenOut(Micros, Addition),
}

/// Puts a take just recorded (`clip`, and the `markers` made in it) into
/// every step to undo or redo, so going back never takes it out again.
fn add_take_to_steps(steps: &mut [Step], clip: visor_core::Range, markers: &[Marker]) {
    for step in steps {
        match step {
            Step::Edit(edit) => edit.append(clip),
            Step::Markers(m) => {
                m.extend(markers.iter().cloned());
                m.sort_by_key(|m| m.at_us);
            }
            Step::Blurs(b) => visor_core::project::stretch_blurs(b, clip),
            Step::Added(_) | Step::TakenOut(..) => {}
        }
    }
}

/// Puts what was added at source time `at` (`part`, as [`take_addition`]
/// gives it) into every step, so undoing them doesn't take it out: only
/// undoing the addition does.
///
/// [`take_addition`]: Project::take_addition
fn add_to_steps(steps: &mut [Step], at: Micros, part: &Addition) {
    for step in steps {
        match step {
            Step::Edit(edit) => edit.append_edit(&part.edit, at),
            Step::Markers(m) => {
                m.extend(part.markers.iter().map(|x| Marker {
                    at_us: at + x.at_us,
                    ..x.clone()
                }));
                m.sort_by_key(|m| m.at_us);
            }
            Step::Blurs(b) => b.extend(part.blurs.iter().map(|x| Blur {
                start_us: at + x.start_us,
                end_us: at + x.end_us,
                ..x.clone()
            })),
            Step::Added(_) | Step::TakenOut(..) => {}
        }
    }
}

/// Takes what was added from source time `at` on out of every step again,
/// as undoing the addition takes it out of the recording.
fn strip_from_steps(steps: &mut [Step], at: Micros) {
    for step in steps {
        match step {
            Step::Edit(edit) => {
                edit.split_off(at);
            }
            Step::Markers(m) => m.retain(|m| m.at_us < at),
            Step::Blurs(b) => b.retain(|b| b.start_us < at),
            Step::Added(_) | Step::TakenOut(..) => {}
        }
    }
}

/// Counts running background jobs; each holds a [`JobGuard`].
#[derive(Clone, Default)]
pub struct Jobs(Arc<AtomicU32>);

pub struct JobGuard(Arc<AtomicU32>);

impl Jobs {
    fn start(&self) -> JobGuard {
        self.0.fetch_add(1, Ordering::SeqCst);
        JobGuard(self.0.clone())
    }

    pub fn running(&self) -> bool {
        self.0.load(Ordering::SeqCst) > 0
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
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
    /// `name_first`: start with the keyboard in the name field (for a
    /// recording that was just made).
    pub fn open(dir: PathBuf, name_first: bool) -> anyhow::Result<Rc<Self>> {
        let project = visor_media::recover::load_or_recover(&dir)?;
        let player = Player::open(&dir, &project, (1920, 1200), 360)?;
        let ui = Editor::new()?;

        ui.set_project_title(project.title.clone().into());
        if let Some(t) = &project.tracks.screen
            && let (Some(w), Some(h)) = (t.width, t.height)
        {
            ui.set_screen_aspect(w as f32 / h.max(1) as f32);
        }
        ui.set_has_camera(project.has_track(TrackKind::Camera));
        let o = &project.camera_overlay;
        ui.set_bubble_visible(o.visible);
        ui.set_bubble_shape(if o.shape == BubbleShape::Circle { 0 } else { 1 });
        ui.set_bubble_x(o.x as f32);
        ui.set_bubble_y(o.y as f32);
        ui.set_bubble_size(o.size as f32);
        ui.set_bubble_mirror(o.mirror);
        ui.set_speed(project.speed() as f32);
        {
            let folded = app().settings.borrow().folded_sections.clone();
            let open = |name: &str| !folded.iter().any(|s| s == name);
            ui.set_edit_open(open("edit"));
            ui.set_camera_open(open("camera"));
            ui.set_speed_open(open("speed"));
            ui.set_thumbnail_open(open("thumbnail"));
            ui.set_markers_open(open("markers"));
            ui.set_blur_open(open("blur"));
        }
        {
            let app = app();
            let settings = app.settings.borrow();
            ui.set_export_format(settings.export_format.clamp(0, 3));
            ui.set_export_resolution(settings.export_resolution.clamp(0, 2));
            ui.set_export_denoise(settings.export_denoise);
            ui.set_export_normalize(settings.export_normalize);
        }
        ui.set_has_audio(project.has_track(TrackKind::Mic) || project.has_track(TrackKind::System));
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
                timer,
                idle_ticks: Cell::new(0),
                save_error: RefCell::default(),
                nudged: RefCell::default(),
                blur_nudged: RefCell::default(),
                jobs: Jobs::default(),
                pauses_cancel: RefCell::default(),
                adding: RefCell::default(),
                added_files: RefCell::default(),
                recent: RefCell::default(),
                dropped: Rc::default(),
            }
        });
        this.wire(Rc::downgrade(&this));
        this.refresh_edit_ui();
        this.load_previews();
        ui.show()?;
        if name_first {
            // Focus belongs to the window, so it is in place whenever the
            // window becomes active.
            ui.invoke_focus_name();
        }
        Ok(this)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn show(&self) {
        let _ = self.ui.show();
    }

    /// Whether an export or a search for pauses is running: recording
    /// more waits, as either acts on the recording without the take.
    pub fn is_busy(&self) -> bool {
        self.export.borrow().is_some()
            || self.ui.get_finding_pauses()
            || self.adding.borrow().is_some()
    }

    /// Hides the editor to record more onto its recording.
    fn record_more(&self) {
        if self.is_busy() {
            return;
        }
        self.control().pause();
        let _ = self.ui.hide();
        app().record_more(self.dir.clone());
    }

    /// Hides the editor while an agent records more onto its recording.
    pub fn hide_for_take(&self) {
        self.control().pause();
        let _ = self.ui.hide();
    }

    /// Back from recording more: the recording, as saved with the take,
    /// replaces the one shown, the take is the last clip, selected, with
    /// the playhead at its start; what there was to undo still is.
    pub fn take_added(&self) {
        let project = match Project::load(&self.dir) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("could not reload {}: {e}", self.dir.display());
                return self.show();
            }
        };
        let old_duration = self.project.borrow().duration_us;
        let clip = project
            .edit
            .ranges()
            .last()
            .copied()
            .filter(|r| r.0 >= old_duration);
        if let Some(clip) = clip {
            let markers: Vec<Marker> = {
                let old = self.project.borrow();
                project
                    .markers
                    .iter()
                    .filter(|m| m.at_us >= old_duration && !old.markers.contains(m))
                    .cloned()
                    .collect()
            };
            // What was added before stays: it can't be taken out from
            // under the take, nor put back where the take now is.
            self.undo
                .borrow_mut()
                .retain(|s| !matches!(s, Step::Added(_)));
            if self
                .redo
                .borrow()
                .iter()
                .any(|s| matches!(s, Step::TakenOut(..)))
            {
                tracing::info!("recorded more: an addition undone can't be redone");
                self.redo.borrow_mut().clear();
            }
            add_take_to_steps(&mut self.undo.borrow_mut(), clip, &markers);
            add_take_to_steps(&mut self.redo.borrow_mut(), clip, &markers);
        }
        *self.project.borrow_mut() = project;
        self.reload_media();
        if let Some(clip) = clip {
            self.select_from(clip.0);
        }
        self.show();
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
        on!(on_toggle_play, |e| e.control().toggle());
        on!(on_seek, |e, f| {
            let t = e.source_at(f);
            e.control().seek(t);
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
        on!(on_delete_selection, |e| {
            if let Some((a, b)) = e.selection() {
                let pieces = e.layout().kept_pieces(a, b);
                e.apply(|edit| edit.delete(&pieces));
            }
        });
        on!(on_remove_pauses, |e| e.remove_pauses());
        on!(on_record_more, |e| e.record_more());
        on!(on_open_add_menu, |e| e.open_add_menu());
        on!(on_add_recent, |e, i| {
            let dir = usize::try_from(i)
                .ok()
                .and_then(|i| e.recent.borrow().get(i).cloned());
            if let Some(dir) = dir {
                let _ = e.add_videos(vec![dir], None);
            }
        });
        on!(on_add_files, |e| e.choose_files(false));
        on!(on_add_recording, |e| e.choose_files(true));
        {
            // A drop of several files comes as one event each: add them
            // together once they're in.
            use slint::winit_030::{EventResult, WinitWindowAccessor, winit};
            let (w, dropped) = (weak.clone(), self.dropped.clone());
            ui.window().on_winit_window_event(move |_, event| {
                if let winit::event::WindowEvent::DroppedFile(path) = event {
                    let first = dropped.borrow().is_empty();
                    dropped.borrow_mut().push(path.clone());
                    if first {
                        let (w, dropped) = (w.clone(), dropped.clone());
                        Timer::single_shot(Duration::from_millis(100), move || {
                            let paths = std::mem::take(&mut *dropped.borrow_mut());
                            if let Some(e) = w.upgrade() {
                                let _ = e.add_videos(paths, None);
                            }
                        });
                    }
                    return EventResult::PreventDefault;
                }
                EventResult::Propagate
            });
        }
        on!(on_trim_to_selection, |e| {
            if let Some((a, b)) = e.selection() {
                let pieces = e.layout().kept_pieces(a, b);
                e.apply(|edit| edit.keep_only(pieces));
            }
        });
        on!(on_restore, |e| {
            let d = e.duration();
            e.apply(|edit| *edit = EditList::full(d));
        });
        on!(on_undo, |e| e.undo_redo(true));
        on!(on_redo, |e| e.undo_redo(false));
        on!(on_step, |e, frames| {
            let to = e.stepped(frames);
            e.control().seek(to);
        });
        on!(on_preview, |e, index, f, end| {
            // The frame it will show once let go.
            let index = index as usize;
            let Some(t) = e.clip_edge_time(index, end, f) else {
                return;
            };
            if let Some((_, _, shown)) = e.clip_edge_moved(index, end, t) {
                e.control().preview(shown);
            }
        });
        on!(on_cut_at_playhead, |e| e.cut_at_playhead());
        on!(on_bring_back, |e, index| {
            let d = e.duration();
            if let Ok(i) = usize::try_from(index) {
                e.apply(|edit| {
                    bring_back_shown(edit, d, i);
                });
            }
        });
        on!(on_move_clip, |e, from, to| {
            if let (Ok(from), Ok(to)) = (usize::try_from(from), usize::try_from(to)) {
                e.ui.set_active_edge(-1);
                e.apply(|edit| edit.move_clip(from, to));
            }
        });
        on!(on_select_clip, |e, index| {
            let span = usize::try_from(index)
                .ok()
                .and_then(|i| e.layout().clip_span(i));
            if let Some((a, b)) = span {
                let d = e.duration().max(1) as f64;
                e.ui.set_sel_start((a as f64 / d) as f32);
                e.ui.set_sel_end((b as f64 / d) as f32);
                e.ui.set_kept_label(e.kept_label().into());
            }
        });
        let w = weak.clone();
        ui.on_clip_slot(move |f| {
            w.upgrade().map_or(0, |e| {
                let layout = e.layout();
                let at = f64::from(f) * layout.duration() as f64;
                // Before the first clip whose middle is past the pointer,
                // counting what was removed next to it, which moves with it.
                let n = e.project.borrow().edit.ranges().len();
                (0..n)
                    .find(|&i| {
                        layout
                            .block_span(i)
                            .is_some_and(|(a, b)| (a + b) as f64 / 2.0 > at)
                    })
                    .unwrap_or(n) as i32
            })
        });
        // Back from a preview, which may be inside a cut, to a kept frame.
        on!(on_drag_ended, |e| {
            let pos = e.player.borrow().position();
            e.control().seek(pos);
        });
        on!(on_clip_edge_moved, |e, index, end, f| {
            let index = index as usize;
            if let Some(t) = e.clip_edge_time(index, end, f) {
                e.move_clip_edge(index, end, t, false);
            }
        });
        on!(on_nudge_edge, |e, frames, repeat| {
            let Some((index, end, at)) = e.picked_edge() else {
                return;
            };
            let Some(&clip) = e.project.borrow().edit.ranges().get(index) else {
                return;
            };
            let to = e.frames_from(at, frames);
            // Nudging shrinks a clip to a frame at least, never away.
            if (end && to <= clip.0) || (!end && to >= clip.1) {
                return;
            }
            // Held down, the nudges of one edge are one step to undo.
            let edit = e.project.borrow().edit.clone();
            let again = repeat && e.nudged.borrow().as_ref() == Some(&(edit, end, at));
            e.move_clip_edge(index, end, to, again);
            let nudged = e
                .picked_edge()
                .map(|(_, end, t)| (e.project.borrow().edit.clone(), end, t));
            *e.nudged.borrow_mut() = nudged;
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
                ("thumbnail", e.ui.get_thumbnail_open()),
                ("markers", e.ui.get_markers_open()),
                ("blur", e.ui.get_blur_open()),
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
            let speed = f64::from(e.ui.get_speed()).clamp(
                visor_core::project::MIN_SPEED,
                visor_core::project::MAX_SPEED,
            );
            e.project.borrow_mut().speed = speed;
            e.control().set_speed(speed);
            e.save();
            e.refresh_edit_ui();
        });
        on!(on_add_marker, |e| e.add_marker());
        on!(on_rename_marker, |e, i, text| e
            .rename_marker(i as usize, &text));
        on!(on_delete_marker, |e, i| e.delete_marker(i as usize));
        on!(on_add_blur, |e| e.add_blur());
        on!(on_pick_blur, |e, i| e.pick_blur(i));
        on!(on_delete_blur, |e, i| e.delete_blur(i as usize));
        on!(on_blur_to_selection, |e, i| e.blur_to_selection(i as usize));
        on!(on_blur_moved, |e, done| e.blur_moved(done));
        on!(on_blur_edge_dragged, |e, end, f| {
            if let Some((blurs, shown)) = e.blur_edge_moved(end, e.blur_edge_at(end, f)) {
                e.control().set_blurs(blurs);
                e.control().preview(shown);
            }
        });
        on!(on_blur_edge_moved, |e, end, f| {
            e.move_blur_edge(end, e.blur_edge_at(end, f), false);
        });
        on!(on_show_blur_edge, |e, end| e.show_blur_edge(end));
        on!(on_nudge_blur_edge, |e, frames, repeat| {
            let Some(at) = e.picked_blur_edge(e.ui.get_blur_edge() == 1) else {
                return;
            };
            let end = e.ui.get_blur_edge() == 1;
            // Held down, the nudges of one edge are one step to undo.
            let again =
                repeat && e.blur_nudged.borrow().as_ref() == Some(&e.project.borrow().blurs);
            e.move_blur_edge(end, e.frames_from(at, frames), again);
            *e.blur_nudged.borrow_mut() = Some(e.project.borrow().blurs.clone());
        });
        on!(on_title_edited, |e, title| {
            let title = title.trim().to_string();
            if !title.is_empty() {
                e.ui.set_project_title(title.clone().into());
                e.project.borrow_mut().title = title;
                e.save();
            }
        });
        on!(on_set_thumbnail, |e| {
            let position = e.player.borrow().position();
            let frame = kept_frame(&e.project.borrow().edit, position);
            e.project.borrow_mut().thumbnail_us = Some(frame);
            e.save();
            e.refresh_edit_ui();
        });
        on!(on_reset_thumbnail, |e| {
            e.project.borrow_mut().thumbnail_us = None;
            e.save();
            e.refresh_edit_ui();
        });
        on!(on_save_thumbnail, |e| e.save_thumbnail());
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
        self.stop_work();
        self.remove_unused_added_files();
        let (dir, jobs) = (self.dir.clone(), self.jobs.clone());
        // Drop the window (and its player threads) after this callback.
        slint::Timer::single_shot(Duration::ZERO, move || app().editor_closed(&dir, jobs));
    }

    /// Marks work reading the recording's folder from elsewhere (an agent),
    /// so the folder isn't renamed under it.
    pub fn start_job(&self) -> JobGuard {
        self.jobs.start()
    }

    /// Cancels an export and stops playback. Returns the background jobs,
    /// which may take a moment to wind down.
    pub fn stop_work(&self) -> Jobs {
        if let Some(job) = &*self.export.borrow() {
            job.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(cancel) = self.pauses_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(job) = self.adding.take() {
            job.cancel.store(true, Ordering::Relaxed);
            self.ui.set_adding(false);
            if let Some(reply) = job.reply {
                reply(Err("The editor was closed before it was added.".into()));
            }
        }
        self.control().pause();
        self.jobs.clone()
    }

    /// The player, for changing it: restarts the UI timer so the change
    /// shows up.
    fn control(&self) -> RefMut<'_, Player> {
        self.idle_ticks.set(0);
        if !self.timer.running() {
            self.timer.restart();
        }
        self.player.borrow_mut()
    }

    pub fn duration(&self) -> Micros {
        self.project.borrow().duration_us.max(1)
    }

    /// Adds a marker at the playhead, in time order, ready to be named.
    /// Its index is `new-marker`, for the UI to put the keyboard in its name.
    fn add_marker(&self) {
        let at = self.player.borrow().position();
        let index = self
            .project
            .borrow()
            .markers
            .partition_point(|m| m.at_us <= at);
        self.change_markers(|markers| {
            let text = String::new();
            markers.insert(index, Marker { at_us: at, text });
        });
        self.ui.set_markers_open(true);
        self.ui.set_new_marker(index as i32);
    }

    fn delete_marker(&self, index: usize) {
        if index < self.project.borrow().markers.len() {
            self.change_markers(|markers| {
                markers.remove(index);
            });
        }
    }

    /// Changes the markers as one step to undo.
    fn change_markers(&self, change: impl FnOnce(&mut Vec<Marker>)) {
        let before = self.project.borrow().markers.clone();
        change(&mut self.project.borrow_mut().markers);
        self.undo.borrow_mut().push(Step::Markers(before));
        self.redo.borrow_mut().clear();
        self.save();
        self.refresh_edit_ui();
    }

    /// Names a marker as it's typed (not a step to undo), updating its row
    /// in place: rebuilding the list would take the keyboard away.
    fn rename_marker(&self, index: usize, typed: &str) {
        let text = typed.trim();
        match self.project.borrow_mut().markers.get_mut(index) {
            Some(m) => m.text = text.to_string(),
            None => return,
        }
        self.save();
        let rows = self.ui.get_markers();
        if let Some(mut row) = rows.row_data(index) {
            row.label = marker_label(&row.time, text).into();
            row.text = typed.into();
            rows.set_row_data(index, row);
        }
    }

    /// Blurs an area during the selection, or the whole video without one,
    /// and shows its box to place it.
    fn add_blur(&self) {
        let duration = self.project.borrow().duration_us;
        // One for each part of the recording the selection plays, as moved
        // clips can make it several.
        let pieces = self.selection_pieces();
        let pieces = if pieces.is_empty() {
            vec![visor_core::Range(0, duration)]
        } else {
            pieces
        };
        let index = self.project.borrow().blurs.len();
        self.change_blurs(|blurs| {
            for p in pieces {
                blurs.push(Blur {
                    start_us: p.0,
                    end_us: p.1,
                    x: 0.35,
                    y: 0.4,
                    width: 0.3,
                    height: 0.2,
                });
            }
        });
        self.ui.set_blur_open(true);
        self.pick_blur(index as i32);
    }

    /// Shows a blur's box on the video, at a moment it covers, to move and
    /// resize it; -1 hides it.
    fn pick_blur(&self, index: i32) {
        let blur = usize::try_from(index)
            .ok()
            .and_then(|i| self.project.borrow().blurs.get(i).cloned());
        self.ui.set_blur_edge(-1);
        let Some(blur) = blur else {
            self.ui.set_picked_blur(-1);
            return;
        };
        self.ui.set_picked_blur(index);
        self.show_blur_box(&blur);
        if !blur.covers(self.player.borrow().position()) {
            self.control().seek(blur.start_us);
        }
    }

    fn show_blur_box(&self, blur: &Blur) {
        self.ui.set_blur_x(blur.x as f32);
        self.ui.set_blur_y(blur.y as f32);
        self.ui.set_blur_w(blur.width as f32);
        self.ui.set_blur_h(blur.height as f32);
    }

    fn delete_blur(&self, index: usize) {
        if index >= self.project.borrow().blurs.len() {
            return;
        }
        let picked = self.ui.get_picked_blur();
        self.ui.set_picked_blur(match usize::try_from(picked) {
            Ok(p) if p == index => -1,
            Ok(p) if p > index => picked - 1,
            _ => picked,
        });
        self.change_blurs(|blurs| {
            blurs.remove(index);
        });
    }

    /// Makes a blur cover the selection instead: one blur for each part of
    /// the recording it plays, as moved clips can make it several.
    fn blur_to_selection(&self, index: usize) {
        let pieces = self.selection_pieces();
        let Some(blur) = self.project.borrow().blurs.get(index).cloned() else {
            return;
        };
        if pieces.is_empty() {
            return;
        }
        self.change_blurs(|blurs| {
            blurs.splice(index..=index, blur_over(&blur, &pieces));
        });
    }

    /// The picked blur's box moved: the preview follows as it's dragged,
    /// and letting go makes it a step to undo.
    fn blur_moved(&self, done: bool) {
        let Ok(index) = usize::try_from(self.ui.get_picked_blur()) else {
            return;
        };
        let mut blurs = self.project.borrow().blurs.clone();
        let Some(blur) = blurs.get_mut(index) else {
            return;
        };
        // Only what moved: the box comes back from the UI less precise, and
        // a click that moves nothing shouldn't be a step to undo.
        let set = |v: &mut f64, ui: f32| {
            if *v as f32 != ui {
                *v = f64::from(ui);
            }
        };
        set(&mut blur.x, self.ui.get_blur_x());
        set(&mut blur.y, self.ui.get_blur_y());
        set(&mut blur.width, self.ui.get_blur_w());
        set(&mut blur.height, self.ui.get_blur_h());
        if done {
            self.change_blurs(|all| *all = blurs);
        } else {
            self.control().set_blurs(blurs);
        }
    }

    /// Where the picked blur's start (or `end`) is.
    fn picked_blur_edge(&self, end: bool) -> Option<Micros> {
        let index = usize::try_from(self.ui.get_picked_blur()).ok()?;
        let project = self.project.borrow();
        let blur = project.blurs.get(index)?;
        Some(if end { blur.end_us } else { blur.start_us })
    }

    /// The blurs with the picked one's start (or `end`) moved to `t`,
    /// keeping it a frame long at least, and the frame to show for that
    /// edge: the first or the last it blurs (the one up just before it
    /// ends; see `Blur::covers_frame`).
    fn blur_edge_moved(&self, end: bool, t: Micros) -> Option<(Vec<Blur>, Micros)> {
        let index = usize::try_from(self.ui.get_picked_blur()).ok()?;
        let mut blurs = self.project.borrow().blurs.clone();
        let d = self.duration();
        let blur = blurs.get_mut(index)?;
        let shown = if end {
            blur.end_us = t.max(self.frames_from(blur.start_us, 1)).min(d);
            blur.end_us.saturating_sub(1).max(blur.start_us)
        } else {
            blur.start_us = t.min(self.frames_from(blur.end_us, -1));
            blur.start_us
        };
        Some((blurs, shown))
    }

    /// Moves the picked blur's start (or `end`) to `t` and shows the frame
    /// by it (see `blur_edge_moved`). `merge` makes it part of the last
    /// step to undo.
    fn move_blur_edge(&self, end: bool, t: Micros, merge: bool) {
        let Some((blurs, shown)) = self.blur_edge_moved(end, t) else {
            return;
        };
        let steps = self.undo.borrow().len();
        // Unchanged, after a drag's preview of another place: as it was.
        self.control().set_blurs(blurs.clone());
        self.change_blurs(|all| *all = blurs);
        if merge && self.undo.borrow().len() > steps {
            self.undo.borrow_mut().pop();
        }
        self.control().preview(shown);
    }

    /// Shows the frame by the picked blur's start (or `end`) as it is.
    fn show_blur_edge(&self, end: bool) {
        let Some(at) = self.picked_blur_edge(end) else {
            return;
        };
        if let Some((_, shown)) = self.blur_edge_moved(end, at) {
            self.control()
                .set_blurs(self.project.borrow().blurs.clone());
            self.control().preview(shown);
        }
    }

    /// Changes the blurs as one step to undo.
    fn change_blurs(&self, change: impl FnOnce(&mut Vec<Blur>)) {
        let before = self.project.borrow().blurs.clone();
        let mut after = before.clone();
        change(&mut after);
        if after == before {
            return;
        }
        self.undo.borrow_mut().push(Step::Blurs(before));
        self.redo.borrow_mut().clear();
        self.set_blurs(after);
    }

    fn set_blurs(&self, blurs: Vec<Blur>) {
        self.project.borrow_mut().blurs = blurs.clone();
        self.control().set_blurs(blurs);
        self.save();
        self.refresh_edit_ui();
    }

    /// The timeline: the whole recording, clip by clip, with what was
    /// removed next to the clip it would come back at.
    fn layout(&self) -> Layout {
        let p = self.project.borrow();
        Layout::new(&p.edit, p.duration_us)
    }

    /// Where on the timeline (0..1) source time `t` is.
    fn timeline_pos(&self, t: Micros) -> f32 {
        let layout = self.layout();
        (layout.display(t) as f64 / layout.duration().max(1) as f64) as f32
    }

    fn position_fraction(&self) -> f32 {
        self.timeline_pos(self.player.borrow().position())
    }

    /// Timeline position `f` (0..1) in timeline time.
    fn display_at(&self, f: f32) -> Micros {
        (f64::from(f.clamp(0.0, 1.0)) * self.duration() as f64) as Micros
    }

    /// The source time that plays at timeline position `f` (0..1), or
    /// next after it if that was removed.
    fn source_at(&self, f: f32) -> Micros {
        self.layout().playing_at(self.display_at(f))
    }

    /// Where a blur's start (or `end`) dragged to timeline position `f`
    /// goes in the recording, at the nearest frame. An end goes after the
    /// frame just before it on the timeline, which may be the last of a
    /// clip that some other part of the recording follows.
    fn blur_edge_at(&self, end: bool, f: f32) -> Micros {
        let layout = self.layout();
        let d = self.display_at(f);
        let t = if end {
            layout.source(d.saturating_sub(1)).0 + 1
        } else {
            layout.source(d).0
        };
        self.frame_time(self.frames_at(t as f64).round())
    }

    /// The selection in timeline time, if any.
    /// Within half a frame of a clip's edge, it's that edge: no flash of
    /// the next clip is left by deleting, nor kept by a trim.
    fn selection(&self) -> Option<(Micros, Micros)> {
        let (a, b) = (self.ui.get_sel_start(), self.ui.get_sel_end());
        (a >= 0.0 && b > a).then(|| {
            let half_frame = (500_000.0 / self.fps()) as Micros;
            let layout = self.layout();
            let at = |f: f32| snap_to_clip_edge(&layout, self.display_at(f), half_frame);
            (at(a), at(b))
        })
    }

    /// The parts of the recording the selection plays, in order, those of
    /// clips split but not moved apart joined.
    fn selection_pieces(&self) -> Vec<visor_core::Range> {
        self.selection()
            .map(|(a, b)| EditList::joined(self.layout().kept_pieces(a, b)))
            .unwrap_or_default()
    }

    /// Where ← → (`frames` frames) takes the playhead: frame by frame in
    /// its clip, and on into the clip that plays before or after it.
    fn stepped(&self, frames: i32) -> Micros {
        let pos = self.player.borrow().position();
        let to = self.frames_from(pos, frames);
        let edit = self.project.borrow().edit.clone();
        let clip = edit.clip_at(pos);
        if clip.is_some() && edit.clip_at(to) == clip {
            return to;
        }
        let out = edit.kept_before(pos) as i64 + to as i64 - pos as i64;
        let out = out.clamp(0, edit.duration().saturating_sub(1) as i64) as Micros;
        edit.to_source(out).unwrap_or(to)
    }

    /// Splits the clip under the playhead there, at the nearest frame.
    fn cut_at_playhead(&self) {
        let pos = self.player.borrow().position();
        let t = self.frame_time(self.frames_at(pos as f64).round());
        self.apply(|edit| {
            edit.split(t);
        });
    }

    /// The screen recording's frame rate.
    fn fps(&self) -> f64 {
        let fps = self
            .project
            .borrow()
            .tracks
            .screen
            .as_ref()
            .and_then(|t| t.fps);
        f64::from(fps.unwrap_or(30).max(1))
    }

    /// How many frames into the recording `t` is.
    fn frames_at(&self, t: f64) -> f64 {
        t * self.fps() / 1_000_000.0
    }

    /// When frame `n` starts, on the exact grid so it doesn't drift.
    fn frame_time(&self, n: f64) -> Micros {
        (n.max(0.0) * 1_000_000.0 / self.fps()).round() as Micros
    }

    /// `frames` frames on from `t`, from frame to frame even when `t` is
    /// between frames.
    fn frames_from(&self, t: Micros, frames: i32) -> Micros {
        let at = self.frames_at(t as f64);
        let from = if frames > 0 {
            (at + 0.01).floor()
        } else {
            (at - 0.01).ceil()
        };
        self.frame_time(from + f64::from(frames))
    }

    /// The source time clip `index`'s start (or `end`) goes to, dragged to
    /// timeline position `f`: as far along the recording as it moved on
    /// the timeline, at the nearest frame.
    fn clip_edge_time(&self, index: usize, end: bool, f: f32) -> Option<Micros> {
        let layout = self.layout();
        let at = (f64::from(f) * layout.duration() as f64) as i64;
        let t = clip_edge_source(&self.project.borrow().edit, &layout, index, end, at)? as f64;
        Some(self.frame_time(self.frames_at(t).round()))
    }

    /// The edit with one edge of a clip moved to `t` (see
    /// `EditList::with_clip_edge_moved`), where the edge then is, and the
    /// frame to show for it: the clip's first, or its last.
    fn clip_edge_moved(
        &self,
        index: usize,
        end: bool,
        t: Micros,
    ) -> Option<(EditList, Micros, Micros)> {
        let d = self.duration();
        let current = self.project.borrow().edit.clone();
        let r = *current.ranges().get(index)?;
        // Less than a frame from another clip's part of the recording (or
        // either end of it), or from its own other edge, would leave a
        // flash frame: take it all, or none of it.
        let others = current
            .ranges()
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != index);
        let mut t = t.min(d);
        let near = |a: Micros, b: Micros| self.frames_at(a.abs_diff(b) as f64) < 1.0;
        if end {
            let next = others.map(|(_, o)| o.0).filter(|&s| s >= r.1).min();
            let next = next.unwrap_or(d);
            if t < next && near(t, next) {
                t = next;
            } else if near(t, r.0) {
                t = r.0;
            }
        } else {
            let prev = others.map(|(_, o)| o.1).filter(|&e| e <= r.0).max();
            let prev = prev.unwrap_or(0);
            if t > prev && near(t, prev) {
                t = prev;
            } else if near(t, r.1) {
                t = r.1;
            }
        }
        let edit = current.with_clip_edge_moved(d, index, end, t)?;
        // Where the edge ended up, and the frame by it, in its clip.
        let shown = match edit.ranges().get(index) {
            Some(c) if edit.ranges().len() == current.ranges().len() => {
                t = if end { c.1 } else { c.0 };
                if end {
                    self.frames_from(c.1, -1).max(c.0)
                } else {
                    c.0
                }
            }
            // Gone: where the video goes on.
            _ => edit
                .ranges()
                .get(index)
                .or(edit.ranges().last())
                .map_or(0, |c| c.0),
        };
        Some((edit, t, shown))
    }

    /// Moves one edge of a clip to `t` and shows the frame by it (see
    /// `clip_edge_moved`). `merge` makes it part of the last step to undo.
    fn move_clip_edge(&self, index: usize, end: bool, t: Micros, merge: bool) {
        let steps = self.undo.borrow().len();
        let moved = self.clip_edge_moved(index, end, t);
        if let Some((edit, _, _)) = &moved {
            self.apply(|e| *e = edit.clone());
        }
        // Not moved: back from the drag's preview to where it was.
        let Some((_, t, shown)) = moved.filter(|(edit, ..)| self.project.borrow().edit == *edit)
        else {
            let pos = self.player.borrow().position();
            self.control().seek(pos);
            return;
        };
        if merge && self.undo.borrow().len() > steps {
            self.undo.borrow_mut().pop();
        }
        // Still picked where it now is, unless its clip went away.
        self.pick_edge(Some((end, t)));
        self.control().seek(shown);
    }

    /// The clip edge picked for nudging: its clip's index, whether it's
    /// the end, and where it is in the recording.
    fn picked_edge(&self) -> Option<(usize, bool, Micros)> {
        let edge = usize::try_from(self.ui.get_active_edge()).ok()?;
        let (index, end) = (edge / 2, edge % 2 == 1);
        let clip = *self.project.borrow().edit.ranges().get(index)?;
        Some((index, end, if end { clip.1 } else { clip.0 }))
    }

    /// Picks the clip edge at source time `t` (an end, or a start), if a
    /// clip has one there.
    fn pick_edge(&self, edge: Option<(bool, Micros)>) {
        let index = edge.and_then(|(end, t)| {
            let i = self
                .project
                .borrow()
                .edit
                .ranges()
                .iter()
                .position(|c| if end { c.1 == t } else { c.0 == t })?;
            i32::try_from(i * 2 + usize::from(end)).ok()
        });
        self.ui.set_active_edge(index.unwrap_or(-1));
    }

    /// The project as edited here.
    pub fn project(&self) -> Project {
        self.project.borrow().clone()
    }

    /// Sets an agent's edit as if it were made here, so it shows at once
    /// and the user can undo it. Fails if it couldn't be saved.
    pub fn apply_agent_edit(&self, edit: EditList) -> Result<Project, String> {
        self.save_error.borrow_mut().take();
        self.apply(|e| *e = edit);
        match self.save_error.borrow_mut().take() {
            Some(e) => Err(format!(
                "The change shows in the editor but couldn't be saved: {e}"
            )),
            None => Ok(self.project()),
        }
    }

    /// Changes the edit as one step to undo. Returns whether it changed.
    fn apply(&self, change: impl FnOnce(&mut EditList)) -> bool {
        let before = self.project.borrow().edit.clone();
        let mut after = before.clone();
        change(&mut after);
        if after == before {
            return false;
        }
        if after.duration() == 0 {
            self.ui
                .set_kept_label("That would remove everything; nothing changed.".into());
            return false;
        }
        self.undo.borrow_mut().push(Step::Edit(before));
        self.redo.borrow_mut().clear();
        self.set_edit(after);
        self.ui.set_sel_start(-1.0);
        self.ui.set_sel_end(-1.0);
        true
    }

    fn undo_redo(&self, undo: bool) {
        let (from, to) = if undo {
            (&self.undo, &self.redo)
        } else {
            (&self.redo, &self.undo)
        };
        let Some(step) = from.borrow_mut().pop() else {
            return;
        };
        match step {
            Step::Edit(edit) => {
                to.borrow_mut()
                    .push(Step::Edit(self.project.borrow().edit.clone()));
                self.set_edit(edit);
            }
            Step::Markers(markers) => {
                let now = std::mem::replace(&mut self.project.borrow_mut().markers, markers);
                to.borrow_mut().push(Step::Markers(now));
                self.save();
                self.refresh_edit_ui();
            }
            Step::Blurs(blurs) => {
                let now = self.project.borrow().blurs.clone();
                // A blur added or deleted renumbers the others: keep the
                // picked one picked, if it's still there as it was.
                if blurs.len() != now.len() {
                    let picked = usize::try_from(self.ui.get_picked_blur())
                        .ok()
                        .and_then(|i| now.get(i))
                        .and_then(|p| blurs.iter().position(|b| b == p));
                    self.ui.set_picked_blur(picked.map_or(-1, |i| i as i32));
                    self.ui.set_blur_edge(-1);
                }
                to.borrow_mut().push(Step::Blurs(now));
                self.set_blurs(blurs);
            }
            Step::Added(at) => {
                let part = self.project.borrow_mut().take_addition(at);
                strip_from_steps(&mut self.undo.borrow_mut(), at);
                self.redo.borrow_mut().push(Step::TakenOut(at, part));
                self.addition_changed();
            }
            Step::TakenOut(at, part) => {
                {
                    let mut p = self.project.borrow_mut();
                    if p.duration_us != at {
                        tracing::warn!("putting back an addition at {at}, not {}", p.duration_us);
                    }
                    p.add(part.clone());
                }
                add_to_steps(&mut self.undo.borrow_mut(), at, &part);
                self.undo.borrow_mut().push(Step::Added(at));
                self.addition_changed();
                self.select_from(at);
            }
        }
    }

    /// After adding or taking out recordings or videos: the recording is
    /// saved, and played and shown as it now is.
    fn addition_changed(&self) {
        self.save();
        self.ui.set_sel_start(-1.0);
        self.ui.set_sel_end(-1.0);
        self.pick_edge(None);
        self.ui.set_picked_blur(-1);
        self.ui.set_blur_edge(-1);
        self.reload_media();
    }

    /// Reopens the player and previews over the recording as it now is
    /// (its length or files changed).
    fn reload_media(&self) {
        let project = self.project.borrow().clone();
        match Player::open(&self.dir, &project, (1920, 1200), 360) {
            Ok(player) => *self.player.borrow_mut() = player,
            Err(e) => tracing::error!("could not reopen the player: {e:#}"),
        }
        self.ui.set_has_camera(project.has_track(TrackKind::Camera));
        self.ui.set_has_audio(
            project.has_track(TrackKind::Mic) || project.has_track(TrackKind::System),
        );
        *self.nudged.borrow_mut() = None;
        self.refresh_edit_ui();
        self.load_previews();
        // The new player shows nothing until told where.
        let at = kept_frame(&project.edit, 0);
        self.control().seek(at);
    }

    /// Selects the first clip (as they play) from source time `at` on, and
    /// puts the playhead at its start.
    fn select_from(&self, at: Micros) {
        let (index, start) = {
            let p = self.project.borrow();
            match p.edit.ranges().iter().position(|r| r.0 >= at) {
                Some(i) => (i, p.edit.ranges()[i].0),
                None => return,
            }
        };
        if let Some((a, b)) = self.layout().clip_span(index) {
            let d = self.duration() as f64;
            self.ui.set_sel_start((a as f64 / d) as f32);
            self.ui.set_sel_end((b as f64 / d) as f32);
            self.ui.set_kept_label(self.kept_label().into());
        }
        self.control().seek(start);
    }

    /// Adds Visor recordings (folders) and video files at the end: they're
    /// copied in the background, then added as one step to undo. `reply`
    /// hears where each landed in source time, or why none was added.
    /// Refused at once (with why) while exporting, finding pauses or adding.
    pub fn add_videos(&self, paths: Vec<PathBuf>, reply: Option<AddReply>) -> Result<(), String> {
        let refused = if paths.is_empty() {
            Some("Nothing to add.")
        } else if self.export.borrow().is_some() {
            Some("Wait for the export to finish, then add it.")
        } else if self.ui.get_finding_pauses() {
            Some("Wait for Remove pauses to finish, then add it.")
        } else if self.adding.borrow().is_some() {
            Some("Still adding the last one.")
        } else {
            None
        };
        if let Some(why) = refused {
            self.ui.set_add_error(why.into());
            return Err(why.to_string());
        }
        let job = AddJob {
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(AtomicU32::new(0)),
            result: Arc::new(Mutex::new(None)),
            reply,
        };
        let (cancel, progress, result) =
            (job.cancel.clone(), job.progress.clone(), job.result.clone());
        let dir = self.dir.clone();
        let project = self.project();
        let guard = self.jobs.start();
        std::thread::spawn(move || {
            let _guard = guard;
            let r = visor_media::import::prepare(&dir, &project, &paths).and_then(|prepared| {
                prepared.copy(
                    &dir,
                    &mut |p| progress.store((p as f32).to_bits(), Ordering::Relaxed),
                    &cancel,
                )?;
                if cancel.load(Ordering::Relaxed) {
                    for name in prepared.files() {
                        let _ = std::fs::remove_file(dir.join(name));
                    }
                    anyhow::bail!("cancelled");
                }
                Ok(prepared)
            });
            *result.lock().unwrap() = Some(r);
        });
        self.ui.set_add_error("".into());
        self.ui.set_adding(true);
        self.ui.set_adding_label("Adding…".into());
        *self.adding.borrow_mut() = Some(job);
        // The UI timer polls the copy.
        drop(self.control());
        Ok(())
    }

    fn poll_adding(&self) {
        let finished = {
            let job = self.adding.borrow();
            let Some(job) = job.as_ref() else { return };
            let p = f32::from_bits(job.progress.load(Ordering::Relaxed));
            self.ui
                .set_adding_label(format!("Adding… {:.0}%", p * 100.0).into());
            job.result.lock().unwrap().take()
        };
        let Some(result) = finished else { return };
        let job = self.adding.borrow_mut().take().expect("job present");
        self.ui.set_adding(false);
        let outcome = match result {
            Ok(prepared) => Ok(self.apply_addition(prepared)),
            Err(e) => {
                let why = format!("{e:#}");
                tracing::warn!("adding failed: {why}");
                if !job.cancel.load(Ordering::Relaxed) {
                    self.ui
                        .set_add_error(format!("Couldn't add it: {why}").into());
                }
                Err(why)
            }
        };
        if let Some(reply) = job.reply {
            reply(outcome.map(|starts| (self.project(), starts)));
        }
    }

    /// Adds what was copied in (for an agent, while the editor wasn't
    /// open), as one step to undo; returns where each starts.
    pub fn add_prepared(&self, prepared: visor_media::import::Prepared) -> Vec<Micros> {
        self.apply_addition(prepared)
    }

    /// Adds what was copied in, as one step to undo; returns where each
    /// starts in source time.
    fn apply_addition(&self, prepared: visor_media::import::Prepared) -> Vec<Micros> {
        let at = self.project.borrow().duration_us;
        self.added_files
            .borrow_mut()
            .extend(prepared.files().map(String::from));
        let starts = {
            let mut p = self.project.borrow_mut();
            prepared.additions.into_iter().map(|a| p.add(a)).collect()
        };
        let part = self.project.borrow().clone().take_addition(at);
        add_to_steps(&mut self.undo.borrow_mut(), at, &part);
        self.undo.borrow_mut().push(Step::Added(at));
        self.redo.borrow_mut().clear();
        self.addition_changed();
        self.select_from(at);
        starts
    }

    /// Deletes the files copied in for additions that the recording no
    /// longer has (undone, or never finished).
    fn remove_unused_added_files(&self) {
        let project = self.project.borrow();
        let used: std::collections::HashSet<&str> = std::iter::once(&project.tracks)
            .chain(project.takes.iter().map(|t| &t.tracks))
            .flat_map(|t| [&t.screen, &t.camera, &t.mic, &t.system])
            .flatten()
            .map(|t| t.file.as_str())
            .collect();
        for name in self.added_files.borrow_mut().drain(..) {
            if !used.contains(name.as_str()) {
                let path = self.dir.join(&name);
                if let Err(e) = std::fs::remove_file(&path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!("could not remove {}: {e}", path.display());
                }
            }
        }
    }

    /// Opens the *Add video…* menu: fills in the recent recordings.
    fn open_add_menu(&self) {
        let here = std::fs::canonicalize(&self.dir).ok();
        let recent: Vec<(String, PathBuf)> = app()
            .recent()
            .into_iter()
            .filter(|(_, d)| std::fs::canonicalize(d).ok() != here)
            .filter(|(_, d)| !visor_media::recover::is_interrupted(d))
            .collect();
        let names: Vec<slint::SharedString> =
            recent.iter().map(|(t, _)| t.as_str().into()).collect();
        self.ui
            .set_recent_recordings(ModelRc::new(VecModel::from(names)));
        *self.recent.borrow_mut() = recent.into_iter().map(|(_, d)| d).collect();
    }

    fn choose_files(&self, recordings: bool) {
        self.control().pause();
        let dialog = rfd::FileDialog::new();
        let picked = if recordings {
            dialog
                .set_title("Choose a Visor recording")
                .set_directory(app().settings.borrow().save_dir.clone())
                .pick_folders()
        } else {
            dialog
                .set_title("Choose video files")
                .add_filter("Videos", &visor_media::import::VIDEO_EXTENSIONS)
                .pick_files()
        };
        if let Some(paths) = picked {
            let _ = self.add_videos(paths, None);
        }
    }

    fn set_edit(&self, edit: EditList) {
        // The clips may be renumbered: keep the picked edge by where it is,
        // and end a drag, whose clip may now be another. The timeline
        // changes under the selection: it goes.
        let picked = self.picked_edge().map(|(_, end, t)| (end, t));
        self.project.borrow_mut().edit = edit.clone();
        self.pick_edge(picked);
        self.ui.set_drag_clip(-1);
        self.ui.set_moving_clip(-1);
        self.ui.set_sel_start(-1.0);
        self.ui.set_sel_end(-1.0);
        self.control().set_edit(edit);
        self.save();
        self.refresh_edit_ui();
    }

    fn save(&self) {
        if let Err(e) = self.project.borrow().save(&self.dir) {
            tracing::error!("saving project.json failed: {e}");
            *self.save_error.borrow_mut() = Some(e.to_string());
        }
    }

    fn refresh_edit_ui(&self) {
        let project = self.project.borrow();
        let d = project.duration_us.max(1) as f64;
        let edit = &project.edit;
        // The timeline shows the whole recording, clip after clip, each
        // with what was removed next to it.
        let layout = Layout::new(edit, project.duration_us);
        let pos = |t: Micros| (layout.display(t) as f64 / d) as f32;
        let frac = |t: Micros| (t as f64 / d) as f32;
        let clips: Vec<Clip> = (0..edit.ranges().len())
            .filter_map(|i| {
                let (a, b) = layout.clip_span(i)?;
                let (ba, bb) = layout.block_span(i)?;
                Some(Clip {
                    start: frac(a),
                    end: frac(b),
                    block_start: frac(ba),
                    block_end: frac(bb),
                    src_start: frac(edit.ranges()[i].0),
                    // As long as it lasts in the video, after speed.
                    label: util::clock_tenths(((b - a) as f64 / project.speed()).round() as Micros)
                        .into(),
                })
            })
            .collect();
        self.ui.set_clips(ModelRc::new(VecModel::from(clips)));
        let removed: Vec<Gone> = layout
            .segments()
            .iter()
            .filter(|s| !s.kept)
            .map(|s| {
                let len = (s.range.len() as f64 / project.speed()).round() as Micros;
                Gone {
                    start: frac(s.at),
                    end: frac(s.at + s.range.len()),
                    src_start: frac(s.range.0),
                    clip: s.clip as i32,
                    label: format!("{} deleted", util::clock_tenths(len)).into(),
                }
            })
            .collect();
        self.ui.set_removed(ModelRc::new(VecModel::from(removed)));
        // Times in the video, where they play.
        let ticks: Vec<Tick> = ruler_ticks(project.output_duration())
            .into_iter()
            .filter_map(|t| {
                let src = edit.to_source((t as f64 * project.speed()).round() as Micros)?;
                Some(Tick {
                    pos: pos(src),
                    label: util::clock(t).into(),
                })
            })
            .collect();
        self.ui.set_ticks(ModelRc::new(VecModel::from(ticks)));
        // Marked where they were recorded, at the time the clock shows there.
        let markers: Vec<MarkerMark> = project
            .markers
            .iter()
            .map(|m| {
                let time = util::clock(project.output_time(m.at_us));
                MarkerMark {
                    pos: pos(m.at_us),
                    label: marker_label(&time, &m.text).into(),
                    time: time.into(),
                    text: m.text.clone().into(),
                }
            })
            .collect();
        // Changed in place where the rows stay the same, so a name being
        // typed keeps the keyboard.
        let rows = self.ui.get_markers();
        match rows.as_any().downcast_ref::<VecModel<MarkerMark>>() {
            Some(model) if model.row_count() == markers.len() => {
                for (i, row) in markers.into_iter().enumerate() {
                    if model
                        .row_data(i)
                        .is_none_or(|r| r.label != row.label || r.pos != row.pos)
                    {
                        model.set_row_data(i, row);
                    }
                }
            }
            _ => self.ui.set_markers(ModelRc::new(VecModel::from(markers))),
        }
        // Blurs: where each starts and ends on the timeline, if it does
        // there, and the parts of it that play; and the picked one's box
        // where it now is.
        let blurs: Vec<BlurRow> = project
            .blurs
            .iter()
            .map(|b| {
                let last = b.end_us.saturating_sub(1).max(b.start_us);
                BlurRow {
                    start: pos(b.start_us),
                    end: frac(layout.display(last) + 1),
                    time: blur_label(&project, b).into(),
                }
            })
            .collect();
        self.ui.set_blurs(ModelRc::new(VecModel::from(blurs)));
        let mut bands = Vec::new();
        for (i, b) in project.blurs.iter().enumerate() {
            for seg in layout.segments().iter().filter(|s| s.kept) {
                let r = seg.range;
                let (s, e) = (b.start_us.max(r.0), b.end_us.min(r.1));
                if e > s {
                    bands.push(BlurBand {
                        blur: i as i32,
                        start: frac(seg.at + s - r.0),
                        end: frac(seg.at + e - r.0),
                    });
                }
            }
        }
        self.ui.set_blur_bands(ModelRc::new(VecModel::from(bands)));
        match usize::try_from(self.ui.get_picked_blur()) {
            Ok(i) if i < project.blurs.len() => self.show_blur_box(&project.blurs[i]),
            _ => {
                self.ui.set_picked_blur(-1);
                self.ui.set_blur_edge(-1);
            }
        }
        self.ui.set_can_undo(!self.undo.borrow().is_empty());
        self.ui.set_can_redo(!self.redo.borrow().is_empty());
        self.ui
            .set_edited(project.edit != EditList::full(project.duration_us));
        let kept = project.edit.duration();
        let speed_label = if (project.speed() - 1.0).abs() < 1e-6 {
            format!("Normal speed, {}", util::clock(kept))
        } else {
            format!(
                "Exports at {} instead of {}",
                util::clock(project.output_duration()),
                util::clock(kept)
            )
        };
        self.ui.set_speed_label(speed_label.into());
        let thumbnail = visor_media::export::thumbnail_time(&project);
        let chosen = project.thumbnail_us == Some(thumbnail);
        self.ui
            .set_thumbnail_pos(if chosen { pos(thumbnail) } else { -1.0 });
        self.ui.set_thumbnail_label(
            if chosen {
                format!(
                    "Frame at {}",
                    util::clock_tenths(project.output_time(thumbnail))
                )
            } else {
                "First frame".to_string()
            }
            .into(),
        );
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
        let clips = p.edit.ranges().len();
        if clips > 1 {
            label.push_str(&format!(" in {clips} clips"));
        }
        if self.selection().is_some() {
            let kept: Micros = self.selection_pieces().iter().map(|p| p.len()).sum();
            label.push_str(&format!(" · Selected {}", util::clock_tenths(kept)));
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
            self.ui
                .set_camera_shown(self.player.borrow().camera_shown());
        }
        let (pos, playing) = {
            let p = self.player.borrow();
            (p.position(), p.is_playing())
        };
        self.ui.set_playing(playing);
        self.ui.set_position(self.timeline_pos(pos));
        // Times as they will be in the export, after cuts and speed.
        let label = {
            let p = self.project.borrow();
            format!(
                "{} / {}",
                util::clock_tenths(p.output_time(pos)),
                util::clock_tenths(p.output_duration())
            )
        };
        self.ui.set_time_label(label.into());
        self.poll_export();
        self.poll_adding();

        let idle = self.player.borrow().is_idle()
            && self.export.borrow().is_none()
            && self.adding.borrow().is_none();
        self.idle_ticks
            .set(if idle { self.idle_ticks.get() + 1 } else { 0 });
        if self.idle_ticks.get() >= IDLE_TICKS {
            self.timer.stop();
        }
    }

    fn load_previews(&self) {
        let project = self.project.borrow();
        let duration = project.duration_us;
        let screen = project
            .has_track(TrackKind::Screen)
            .then(|| project.parts(TrackKind::Screen));
        let audio: Vec<_> = [TrackKind::Mic, TrackKind::System]
            .into_iter()
            .filter(|&k| project.has_track(k))
            .map(|k| project.parts(k))
            .collect();
        let dir = self.dir.clone();
        // Separate threads: thumbnails come from the screen track alone and
        // shouldn't wait for every audio track to be decoded.
        let weak = self.ui.as_weak();
        if !audio.is_empty() {
            let (weak, dir) = (weak.clone(), dir.clone());
            std::thread::spawn(move || {
                match visor_media::preview::waveform(&dir, &audio, duration, WAVE_BUCKETS) {
                    Ok(peaks) => {
                        let path = util::wave_path(&peaks);
                        let _ = weak.upgrade_in_event_loop(move |ui| ui.set_wave_path(path.into()));
                    }
                    Err(e) => tracing::warn!("waveform failed: {e:#}"),
                }
            });
        }
        if let Some(screen) = screen {
            std::thread::spawn(move || {
                match visor_media::preview::thumbnails(&dir, &screen, duration, THUMBNAILS, 64) {
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
            });
        }
    }

    fn start_export(&self) {
        if self.export.borrow().is_some() {
            return;
        }
        self.control().pause();
        let (format, kind) = match self.ui.get_export_format() {
            1 => (ExportFormat::WebM { av1: false }, "WebM"),
            2 => (ExportFormat::WebM { av1: true }, "WebM (AV1)"),
            3 => (ExportFormat::Gif, "GIF"),
            _ => (ExportFormat::Mp4, "MP4"),
        };
        let mut options = ExportOptions::new(format);
        // The next editor starts with these choices.
        {
            let app = app();
            let mut settings = app.settings.borrow_mut();
            settings.export_format = self.ui.get_export_format();
            settings.export_resolution = self.ui.get_export_resolution();
            settings.export_denoise = self.ui.get_export_denoise();
            settings.export_normalize = self.ui.get_export_normalize();
            settings.save();
        }
        options.audio = visor_media::export::AudioCleanup {
            denoise: self.ui.get_export_denoise(),
            normalize: self.ui.get_export_normalize(),
        };
        options.max_height = match self.ui.get_export_resolution() {
            1 => Some(1080),
            2 => Some(720),
            _ => None,
        };
        let project = self.project.borrow().clone();
        let exports = match self.exports_dir() {
            Ok(dir) => dir,
            Err(e) => {
                self.ui
                    .set_export_status(format!("Export failed: {e}").into());
                return;
            }
        };
        let out = unique_path(&exports, &sanitize(&project.title), format.extension());
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
        let guard = self.jobs.start();
        std::thread::spawn(move || {
            let _guard = guard;
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

    /// The recording's `exports` folder, created if needed.
    fn exports_dir(&self) -> anyhow::Result<PathBuf> {
        let exports = self.dir.join("exports");
        // A symlinked exports folder would let a shared project make the
        // export write outside it.
        if exports.is_symlink() {
            anyhow::bail!("the exports folder is a link");
        }
        std::fs::create_dir_all(&exports)?;
        Ok(exports)
    }

    /// Writes the thumbnail to the exports folder as a JPEG and shows it.
    fn save_thumbnail(&self) {
        if self.ui.get_saving_thumbnail() {
            return;
        }
        let project = self.project.borrow().clone();
        let out = match self.exports_dir() {
            Ok(dir) => unique_path(
                &dir,
                &format!("{} thumbnail", sanitize(&project.title)),
                "jpg",
            ),
            Err(e) => {
                self.ui
                    .set_thumbnail_error(format!("Saving the image failed: {e}").into());
                return;
            }
        };
        self.ui.set_thumbnail_error("".into());
        self.ui.set_saving_thumbnail(true);
        let dir = self.dir.clone();
        let weak = self.ui.as_weak();
        let guard = self.jobs.start();
        std::thread::spawn(move || {
            let result = visor_media::export::save_thumbnail(&dir, &project, &out);
            drop(guard);
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_saving_thumbnail(false);
                match result {
                    Ok(()) => platform::reveal(&out),
                    Err(e) => {
                        tracing::error!("saving the thumbnail failed: {e:#}");
                        ui.set_thumbnail_error(format!("Saving the image failed: {e:#}").into());
                    }
                }
            });
        });
    }

    /// Looks for dead time (see `visor_media::dead_time`) off the UI
    /// thread, then cuts it all as one step to undo.
    fn remove_pauses(&self) {
        if self.ui.get_finding_pauses() {
            return;
        }
        self.ui.set_finding_pauses(true);
        let project = self.project.borrow().clone();
        let dir = self.dir.clone();
        let guard = self.jobs.start();
        let cancel = Arc::new(AtomicBool::new(false));
        *self.pauses_cancel.borrow_mut() = Some(cancel.clone());
        std::thread::spawn(move || {
            let found = dead_time::find(&dir, &project, dead_time::MIN_LEN, &cancel)
                .map_err(|e| format!("{e:#}"));
            drop(guard);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(editor) = app().editor_for(&dir) {
                    editor.pauses_found(found);
                }
            });
        });
    }

    fn pauses_found(&self, found: Result<Vec<visor_core::Range>, String>) {
        // Not this window's: it was closed, and opened again since.
        if !self.ui.get_finding_pauses() {
            return;
        }
        self.ui.set_finding_pauses(false);
        self.pauses_cancel.take();
        let message = match found {
            Err(e) => {
                tracing::error!("looking for pauses failed: {e}");
                format!("Couldn't look for pauses: {e}")
            }
            Ok(pauses) if pauses.is_empty() => format!(
                "No pauses of {} seconds or more to cut.",
                dead_time::MIN_LEN / 1_000_000
            ),
            Ok(pauses) => {
                let mut edit = self.project.borrow().edit.clone();
                for p in &pauses {
                    edit.cut(p.0, p.1);
                }
                if !self.apply(|e| *e = edit) {
                    "Nothing happens in what's kept, so nothing was removed.".into()
                } else {
                    let total: Micros = pauses.iter().map(|p| p.len()).sum();
                    format!(
                        "Removed {} pause{}, {} in all. ⌘Z undoes it.",
                        pauses.len(),
                        if pauses.len() == 1 { "" } else { "s" },
                        util::clock_tenths(total)
                    )
                }
            }
        };
        self.ui.set_kept_label(message.into());
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

/// `t` if the edit keeps it, else the closest kept frame before it (the
/// playhead rests just past the last kept frame after playing to the end),
/// else the first kept frame.
fn kept_frame(edit: &EditList, t: Micros) -> Micros {
    if edit.to_output(t).is_some() {
        return t;
    }
    let ranges = edit.ranges();
    // The latest part recorded before it, wherever it plays.
    ranges
        .iter()
        .map(|r| r.1)
        .filter(|&e| e <= t)
        .max()
        .map(|e| e - 1)
        .or_else(|| ranges.first().map(|r| r.0))
        .unwrap_or(0)
}

/// A file name stem for exports of a recording titled `title`, made safe
/// the same way as its folder name.
fn sanitize(title: &str) -> String {
    visor_core::project::folder_name(title).unwrap_or_else(|| "Recording".into())
}

/// `dir/name.ext`, or `dir/name 2.ext`, … if taken. Any entry counts as
/// taken, including a dangling symlink, which FFmpeg would write through.
fn unique_path(dir: &Path, name: &str, ext: &str) -> PathBuf {
    let mut path = dir.join(format!("{name}.{ext}"));
    let mut n = 2;
    while path.symlink_metadata().is_ok() {
        path = dir.join(format!("{name} {n}.{ext}"));
        n += 1;
    }
    path
}

/// When a blur is in the video: the whole of it, a stretch in the video's
/// time (after cuts and speed), or nothing if that was cut.
fn blur_label(project: &Project, blur: &Blur) -> String {
    if blur.start_us == 0 && blur.end_us >= project.duration_us {
        return "Whole video".to_string();
    }
    // The stretches of the video it plays in, after moves, cuts and speed;
    // parts of it that play one after the other are one.
    let mut parts: Vec<(Micros, Micros)> = Vec::new();
    let mut at = 0;
    for r in project.edit.ranges() {
        let (s, e) = (blur.start_us.max(r.0), blur.end_us.min(r.1));
        if e > s {
            let part = (at + s - r.0, at + e - r.0);
            match parts.last_mut() {
                Some(last) if last.1 == part.0 => last.1 = part.1,
                _ => parts.push(part),
            }
        }
        at += r.len();
    }
    let Some(&(start, end)) = parts.first() else {
        return "Deleted from the video".to_string();
    };
    let time = |t: Micros| util::clock_tenths((t as f64 / project.speed()).round() as Micros);
    let range = format!("{} – {}", time(start), time(end));
    match parts.len() {
        1 => range,
        2 => format!("{range} and 1 more part"),
        n => format!("{range} and {} more parts", n - 1),
    }
}

/// Brings back the `index`th dimmed part on the timeline, in the order the
/// timeline shows them (`refresh_edit_ui` lists them that way).
fn bring_back_shown(edit: &mut EditList, duration: Micros, index: usize) -> bool {
    let range = Layout::new(edit, duration)
        .segments()
        .iter()
        .filter(|s| !s.kept)
        .nth(index)
        .map(|s| s.range);
    range.is_some_and(|r| edit.bring_back(duration, r))
}

/// Times to mark on the ruler of a video `duration` long: every second, or
/// every 2, 5, 10… so there are a dozen at most, from 0.
fn ruler_ticks(duration: Micros) -> Vec<Micros> {
    const STEPS: [Micros; 10] = [1, 2, 5, 10, 15, 30, 60, 120, 300, 600];
    let secs = duration / 1_000_000;
    let step = STEPS.into_iter().find(|s| secs / s <= 12).unwrap_or(1_200) * 1_000_000;
    (0..duration).step_by(step as usize).collect()
}

/// Output time `t` moved onto the nearest clip edge (or either end of the
/// video) if it's within `tolerance` of one, so a selection made from a
/// clip's span, which comes back from the UI a few µs off, covers it
/// exactly.
fn snap_to_clip_edge(layout: &Layout, t: Micros, tolerance: Micros) -> Micros {
    layout
        .segments()
        .iter()
        .flat_map(|s| [s.at, s.at + s.range.len()])
        .chain([0])
        .min_by_key(|e| e.abs_diff(t))
        .filter(|e| e.abs_diff(t) <= tolerance)
        .unwrap_or(t)
}

/// The source time clip `index`'s start (or `end`) goes to when dragged to
/// timeline time `at`: as far along the recording as it moved on the
/// timeline, over what was removed next to it.
fn clip_edge_source(
    edit: &EditList,
    layout: &Layout,
    index: usize,
    end: bool,
    at: i64,
) -> Option<Micros> {
    let r = *edit.ranges().get(index)?;
    let (a, b) = layout.clip_span(index)?;
    let (edge, shown) = if end { (r.1, b) } else { (r.0, a) };
    Some((edge as i64 + at - shown as i64).max(0) as Micros)
}

/// `blur` as one blur for each of `pieces`, the same area in each.
fn blur_over(blur: &Blur, pieces: &[visor_core::Range]) -> Vec<Blur> {
    pieces
        .iter()
        .map(|p| Blur {
            start_us: p.0,
            end_us: p.1,
            ..blur.clone()
        })
        .collect()
}

/// A marker as the timeline shows it: its time, and its text if any.
fn marker_label(time: &str, text: &str) -> String {
    match text.trim() {
        "" => time.to_string(),
        text => format!("{time}  {text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use visor_core::Range;

    #[test]
    fn a_take_is_in_every_step_to_undo_or_redo() {
        let marker = |at_us| Marker {
            at_us,
            text: String::new(),
        };
        let mut cut = EditList::full(20);
        cut.cut(5, 8);
        let mut steps = vec![
            Step::Edit(EditList::full(20)),
            Step::Markers(vec![marker(3)]),
            Step::Blurs(Vec::new()),
            Step::Edit(cut),
        ];
        let blur = |start_us, end_us| Blur {
            start_us,
            end_us,
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 0.5,
        };
        steps[2] = Step::Blurs(vec![blur(0, 20), blur(2, 6)]);
        add_take_to_steps(&mut steps, Range(20, 25), &[marker(22)]);
        // A blur lasting to the end goes on over the take.
        let Step::Blurs(b) = &steps[2] else { panic!() };
        assert_eq!(
            b.iter().map(|b| (b.start_us, b.end_us)).collect::<Vec<_>>(),
            [(0, 25), (2, 6)]
        );
        let Step::Edit(e) = &steps[0] else { panic!() };
        assert_eq!(e.ranges(), [Range(0, 20), Range(20, 25)]);
        let Step::Markers(m) = &steps[1] else {
            panic!()
        };
        assert_eq!(m.iter().map(|m| m.at_us).collect::<Vec<_>>(), [3, 22]);
        let Step::Edit(e) = &steps[3] else { panic!() };
        assert_eq!(e.ranges(), [Range(0, 5), Range(8, 20), Range(20, 25)]);
        assert!(e.is_valid(25));
    }

    #[test]
    fn an_addition_is_in_every_step_below_it_until_undone() {
        let marker = |at_us| Marker {
            at_us,
            text: String::new(),
        };
        let blur = |start_us, end_us| Blur {
            start_us,
            end_us,
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 0.5,
        };
        let mut cut = EditList::full(20);
        cut.cut(5, 8);
        let before = || {
            vec![
                Step::Edit(EditList::full(20)),
                Step::Markers(vec![marker(3)]),
                Step::Blurs(vec![blur(0, 20)]),
                Step::Added(10),
                Step::Edit(cut.clone()),
            ]
        };
        let mut edit = EditList::full(10);
        edit.cut(0, 2);
        let part = visor_core::Addition {
            duration_us: 10,
            pieces: Vec::new(),
            edit,
            markers: vec![marker(4)],
            blurs: vec![blur(1, 3)],
            size: None,
        };
        let mut steps = before();
        add_to_steps(&mut steps, 20, &part);
        let Step::Edit(e) = &steps[0] else { panic!() };
        assert_eq!(e.ranges(), [Range(0, 20), Range(22, 30)]);
        let Step::Markers(m) = &steps[1] else {
            panic!()
        };
        assert_eq!(m.iter().map(|m| m.at_us).collect::<Vec<_>>(), [3, 24]);
        // This recording's blurs don't go on over it.
        let Step::Blurs(b) = &steps[2] else { panic!() };
        assert_eq!(
            b.iter().map(|b| (b.start_us, b.end_us)).collect::<Vec<_>>(),
            [(0, 20), (21, 23)]
        );
        let Step::Edit(e) = &steps[4] else { panic!() };
        assert_eq!(e.ranges(), [Range(0, 5), Range(8, 20), Range(22, 30)]);

        strip_from_steps(&mut steps, 20);
        for (a, b) in steps.iter().zip(before()) {
            match (a, b) {
                (Step::Edit(x), Step::Edit(y)) => assert_eq!(*x, y),
                (Step::Markers(x), Step::Markers(y)) => assert_eq!(*x, y),
                (Step::Blurs(x), Step::Blurs(y)) => assert_eq!(*x, y),
                (Step::Added(x), Step::Added(y)) => assert_eq!(*x, y),
                _ => panic!("steps changed kind"),
            }
        }
    }

    #[test]
    fn bring_back_restores_the_dimmed_part_shown_there() {
        let mut edit = EditList::full(100);
        edit.cut(0, 10);
        edit.split(50);
        edit.cut(60, 70);
        edit.cut(90, 100);
        edit.move_clip(0, 3);
        // On the timeline: [50, 60) dimmed [60, 70), [70, 90) dimmed
        // [90, 100), dimmed [0, 10) [10, 50).
        assert!(bring_back_shown(&mut edit, 100, 0));
        assert_eq!(edit.ranges(), [Range(50, 70), Range(70, 90), Range(10, 50)]);
        assert!(bring_back_shown(&mut edit, 100, 1));
        // Now dimmed: [90, 100), then [0, 10).
        assert_eq!(edit.ranges(), [Range(50, 70), Range(70, 90), Range(0, 50)]);
        assert!(!bring_back_shown(&mut edit, 100, 1));
    }

    #[test]
    fn the_ruler_marks_a_dozen_times_at_most() {
        const S: Micros = 1_000_000;
        assert_eq!(ruler_ticks(4 * S + S / 2), [0, S, 2 * S, 3 * S, 4 * S]);
        assert_eq!(ruler_ticks(16 * S).len(), 8, "every 2 s");
        let hour = ruler_ticks(3_600 * S);
        assert_eq!(hour[1], 300 * S);
        assert!(hour.len() <= 13);
        assert!(ruler_ticks(0).is_empty());
    }

    #[test]
    fn blurs_are_labelled_with_their_time_in_the_video() {
        let mut project = Project::new("Demo", "2026-10-30T00:00:00Z", 20_000_000);
        let blur = |start_us, end_us| Blur {
            start_us,
            end_us,
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 0.5,
        };
        assert_eq!(blur_label(&project, &blur(0, 20_000_000)), "Whole video");
        // After cuts and speed, as the clock under the video shows it.
        project.edit.cut(0, 2_000_000);
        project.speed = 2.0;
        assert_eq!(
            blur_label(&project, &blur(4_000_000, 8_000_000)),
            "0:01.0 – 0:03.0"
        );
        assert_eq!(
            blur_label(&project, &blur(500_000, 1_500_000)),
            "Deleted from the video"
        );
    }

    #[test]
    fn blurs_over_moved_clips_are_labelled_by_when_they_play() {
        let mut project = Project::new("Demo", "2026-10-30T00:00:00Z", 100_000_000);
        project.edit.split(50_000_000);
        project.edit.move_clip(1, 0);
        // [50, 100) plays first, then [0, 50): source 10–60 s plays at
        // 0–10 s and 60–100 s.
        let blur = Blur {
            start_us: 10_000_000,
            end_us: 60_000_000,
            x: 0.0,
            y: 0.0,
            width: 0.5,
            height: 0.5,
        };
        assert_eq!(
            blur_label(&project, &blur),
            "0:00.0 – 0:10.0 and 1 more part"
        );
        let inside = Blur {
            start_us: 60_000_000,
            end_us: 70_000_000,
            ..blur.clone()
        };
        assert_eq!(blur_label(&project, &inside), "0:10.0 – 0:20.0");
    }

    #[test]
    fn selected_clips_snap_to_their_edges() {
        let mut edit = EditList::full(123_456_789);
        edit.split(61_000_001);
        edit.cut(70_000_000, 80_000_000);
        let l = Layout::new(&edit, 123_456_789);
        // A few µs off, as a clip's span is after a trip through f32.
        assert_eq!(snap_to_clip_edge(&l, 61_000_004, 1_000), 61_000_001);
        assert_eq!(snap_to_clip_edge(&l, 123_456_780, 1_000), 123_456_789);
        assert_eq!(snap_to_clip_edge(&l, 9, 1_000), 0);
        assert_eq!(snap_to_clip_edge(&l, 61_500_000, 1_000), 61_500_000);
        // And to what was removed.
        assert_eq!(snap_to_clip_edge(&l, 79_999_500, 1_000), 80_000_000);
    }

    #[test]
    fn clip_edges_drag_over_what_was_removed_next_to_them() {
        let mut edit = EditList::full(100);
        edit.trim(10, 90);
        edit.split(50);
        edit.move_clip(1, 0);
        // [50, 90) then [10, 50): what was trimmed off either end shows
        // by the clip it would come back at.
        let l = Layout::new(&edit, 100);
        assert_eq!(l.clip_span(0), Some((0, 40)));
        assert_eq!(l.clip_span(1), Some((60, 100)));
        // The first clip's end over the 10 trimmed after it.
        assert_eq!(clip_edge_source(&edit, &l, 0, true, 46), Some(96));
        // The second's start over the 10 trimmed before it.
        assert_eq!(clip_edge_source(&edit, &l, 1, false, 55), Some(5));
        assert_eq!(clip_edge_source(&edit, &l, 1, false, -50), Some(0));
        assert_eq!(clip_edge_source(&edit, &l, 2, false, 0), None);
    }

    #[test]
    fn a_blur_limited_to_a_selection_over_moved_clips_covers_only_it() {
        let blur = Blur {
            start_us: 0,
            end_us: 100,
            x: 0.1,
            y: 0.2,
            width: 0.3,
            height: 0.4,
        };
        let parts = blur_over(
            &blur,
            &[visor_core::Range(90, 100), visor_core::Range(0, 10)],
        );
        assert_eq!(parts.len(), 2);
        assert_eq!((parts[0].start_us, parts[0].end_us), (90, 100));
        assert_eq!((parts[1].start_us, parts[1].end_us), (0, 10));
        assert!(parts.iter().all(|p| p.x == 0.1 && p.height == 0.4));
    }

    #[test]
    fn thumbnails_snap_to_kept_frames_of_moved_clips() {
        let mut edit = EditList::full(10_000_000);
        edit.cut(4_000_000, 6_000_000);
        edit.trim(0, 8_000_000);
        edit.move_clip(1, 0);
        // Past everything: the last frame of the latest part recorded.
        assert_eq!(kept_frame(&edit, 9_000_000), 7_999_999);
    }

    #[test]
    fn thumbnails_snap_to_kept_frames() {
        let mut edit = EditList::full(10_000_000);
        edit.cut(4_000_000, 6_000_000);
        edit.trim(1_000_000, 9_000_000);
        assert_eq!(kept_frame(&edit, 2_000_000), 2_000_000);
        // At the end of the last kept range: its last frame.
        assert_eq!(kept_frame(&edit, 9_000_000), 8_999_999);
        // In a cut: the frame before it.
        assert_eq!(kept_frame(&edit, 5_000_000), 3_999_999);
        // Before everything kept: the first kept frame.
        assert_eq!(kept_frame(&edit, 0), 1_000_000);
    }

    #[test]
    fn export_names_are_safe_and_unique() {
        assert_eq!(sanitize(" Demo: a/b? "), "Demo- a-b-");
        assert_eq!(sanitize("  "), "Recording");
        let dir = std::env::temp_dir().join(format!("visor-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.mp4"), b"").unwrap();
        assert_eq!(unique_path(&dir, "x", "mp4"), dir.join("x 2.mp4"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("missing"), dir.join("y.mp4")).unwrap();
            assert_eq!(unique_path(&dir, "y", "mp4"), dir.join("y 2.mp4"));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
