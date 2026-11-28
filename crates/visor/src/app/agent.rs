//! Agent control: commands from the `visor` command line and its MCP server
//! (see `crate::cli`), which reach the running app through
//! `crate::instance`. Off unless the user turns it on in Preferences.
//!
//! A command is a JSON object whose `command` names what to do; the other
//! fields are its arguments. Replies are JSON objects with `ok` and either
//! the result's fields or an `error` for the agent to show or act on.

use std::path::Path;

use serde_json::{Value, json};
use visor_core::{EditList, Micros};

use super::*;

pub type Reply = crossbeam_channel::Sender<Value>;

/// What agents asked for and are waiting for.
#[derive(Default)]
pub(super) struct AgentState {
    /// A start an agent asked for, waiting for the devices; taken by
    /// `start_recording` into the session.
    pub start_request: Option<AgentRun>,
    start: Option<(Reply, Timer)>,
    /// The panel's choices before the waiting start's, put back if it fails.
    undo: Option<PanelChoices>,
    /// Whether to open the editor once it's saved.
    stop: Option<(Reply, bool)>,
    /// The recording an agent last stopped: what edits apply to by default.
    last_folder: Option<PathBuf>,
    discard: Option<Reply>,
}

/// A recording an agent started, kept with the session: the control bar
/// says so, and the editor doesn't ask for a name the agent gave.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum EditKind {
    Remove,
    Trim,
    Restore,
}

/// What follows a recording being saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterStop {
    /// The editor, ready for the user to name the recording.
    Name,
    /// The editor, the recording already named.
    Edit,
    Nothing,
}

const OFF: &str = "Agent control is off. The user can turn it on in Visor's Preferences \
                   (Agents › Let AI agents control recording).";

/// How long a start may wait for the camera, the countdown and capture.
const START_TIMEOUT: Duration = Duration::from_secs(60);

fn send(reply: Reply, result: Result<Value, String>) {
    let value = match result {
        Ok(mut v) => {
            if let Some(fields) = v.as_object_mut() {
                fields.insert("ok".into(), true.into());
            }
            v
        }
        Err(error) => json!({"ok": false, "error": error}),
    };
    let _ = reply.send(value);
}

/// Kept pieces shorter than this, left between an agent's cut and another
/// or an end, are cut too: they would only flash by in the video.
const SLIVER: Micros = 250_000;

/// How tall frames shown to agents are at most: enough to read text.
const FRAME_HEIGHT: u32 = 720;

type Job = Box<dyn FnOnce() + Send>;

/// Renders frames for agents one at a time, on one thread, so an agent
/// asking for many at once can't swamp the computer, or a recording going
/// on.
fn render(job: Job) {
    static QUEUE: std::sync::OnceLock<crossbeam_channel::Sender<Job>> = std::sync::OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = crossbeam_channel::unbounded::<Job>();
        let spawned = std::thread::Builder::new()
            .name("visor-frames".into())
            .spawn(move || {
                // A crash in one frame (logged by the panic hook) mustn't
                // stop the rest from rendering.
                for job in rx {
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                        tracing::warn!("rendering a frame crashed");
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::warn!("could not start rendering frames: {e}");
        }
        tx
    });
    // No thread to take it (it couldn't start): render it here.
    if let Err(crossbeam_channel::SendError(job)) = queue.send(job) {
        job();
    }
}

/// A time argument, in seconds.
fn time_arg(args: &Value, name: &str) -> Result<Micros, String> {
    arg(args, name)
        .and_then(Value::as_f64)
        .filter(|t| t.is_finite() && *t >= 0.0)
        .map(|t| (t * 1e6).round() as Micros)
        .ok_or_else(|| format!("{name} is a number of seconds."))
}

/// Refuses an edit while the user records a voiceover in the recording's
/// editor: what it covers mustn't change, nor the player be replaced.
fn not_during_voiceover(editor: &Option<Rc<EditorWindow>>) -> Result<(), String> {
    if editor.as_ref().is_some_and(|e| e.voiceover_active()) {
        return Err(
            "The user is recording a voiceover in that recording; try again once they're done."
                .into(),
        );
    }
    Ok(())
}

fn past_the_end(what: &str, duration: Micros) -> String {
    format!(
        "{what} is past the end: the recording is {} seconds long.",
        seconds(duration)
    )
}

/// Exact to the millisecond, so an agent can cut right at a marker.
fn seconds(us: visor_core::Micros) -> f64 {
    (us as f64 / 1000.0).round() / 1000.0
}

/// An agent's change to a recording, before it's made (see `agent_change`).
enum Change {
    Edit(EditList),
    Markers(Vec<visor_core::Marker>),
    Blurs(Vec<visor_core::Blur>),
    Looks(Box<visor_core::Project>),
    Voiceovers(Vec<visor_core::Voiceover>),
}

/// The recording's state as `get_recording` gives it, with its exports.
fn recording_value(
    dir: &Path,
    project: &visor_core::Project,
    editor_open: bool,
    writing: &[PathBuf],
) -> Value {
    let mut exports: Vec<(std::time::SystemTime, String, u64)> =
        std::fs::read_dir(dir.join("exports"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| {
                        let meta = e.metadata().ok().filter(|m| m.is_file())?;
                        let name = e.file_name().to_string_lossy().into_owned();
                        (!name.starts_with('.')).then(|| {
                            (
                                meta.modified().unwrap_or(std::time::UNIX_EPOCH),
                                name,
                                meta.len(),
                            )
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
    exports.sort();
    let mut exports: Vec<(String, u64, bool)> = exports
        .into_iter()
        .map(|(_, n, s)| {
            let busy = writing.iter().any(|w| w == &dir.join("exports").join(&n));
            (n, s, busy)
        })
        .collect();
    // Exports being set up, whose file isn't written yet.
    for w in writing
        .iter()
        .filter(|w| w.parent() == Some(dir.join("exports").as_path()))
    {
        let name = w
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !exports.iter().any(|(n, _, _)| *n == name) {
            exports.push((name, 0, true));
        }
    }
    agent_edits::recording_json(project, &dir.display().to_string(), &exports, editor_open)
}

/// An optional time argument, in seconds.
fn opt_time(args: &Value, name: &str) -> Result<Option<Micros>, String> {
    match arg(args, name) {
        None => Ok(None),
        Some(_) => time_arg(args, name).map(Some),
    }
}

/// The `action` argument, one of `allowed`.
fn action_arg(args: &Value, allowed: &[&str]) -> Result<String, String> {
    let action = arg(args, "action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if allowed.contains(&action) {
        Ok(action.to_string())
    } else {
        Err(format!("action is one of: {}.", allowed.join(", ")))
    }
}

/// `ranges`: [[start, end], …] in seconds.
fn ranges_arg(v: &Value) -> Result<Vec<(Micros, Micros)>, String> {
    let bad = || "ranges is a list of [start_seconds, end_seconds] pairs.".to_string();
    let list = v.as_array().ok_or_else(bad)?;
    list.iter()
        .map(|pair| {
            let p = pair.as_array().filter(|p| p.len() == 2).ok_or_else(bad)?;
            let t = |v: &Value| {
                v.as_f64()
                    .filter(|t| t.is_finite() && *t >= 0.0)
                    .map(|t| (t * 1e6).round() as Micros)
                    .ok_or_else(bad)
            };
            Ok((t(&p[0])?, t(&p[1])?))
        })
        .collect()
}

/// `set_recording`'s arguments.
fn looks_arg(args: &Value) -> Result<agent_edits::Looks, String> {
    let mut looks = agent_edits::Looks::default();
    if let Some(name) = arg(args, "name") {
        looks.name = Some(name.as_str().ok_or("name is text.")?.to_string());
    }
    if let Some(speed) = arg(args, "speed") {
        looks.speed = Some(speed.as_f64().ok_or("speed is a number, 0.5 to 2.")?);
    }
    // null: back to the first kept frame.
    match args.get("thumbnail_seconds") {
        None => {}
        Some(Value::Null) => looks.thumbnail = Some(None),
        Some(_) => looks.thumbnail = Some(Some(time_arg(args, "thumbnail_seconds")?)),
    }
    if let Some(bubble) = arg(args, "bubble") {
        let b = bubble.as_object().ok_or("bubble is an object.")?;
        let num = |k: &str| -> Result<Option<f64>, String> {
            match b.get(k).filter(|v| !v.is_null()) {
                None => Ok(None),
                Some(v) => v
                    .as_f64()
                    .map(Some)
                    .ok_or_else(|| format!("bubble.{k} is a number.")),
            }
        };
        let flag = |k: &str| -> Result<Option<bool>, String> {
            match b.get(k).filter(|v| !v.is_null()) {
                None => Ok(None),
                Some(v) => v
                    .as_bool()
                    .map(Some)
                    .ok_or_else(|| format!("bubble.{k} is true or false.")),
            }
        };
        looks.bubble_visible = flag("visible")?;
        looks.bubble_mirror = flag("mirror")?;
        looks.bubble_x = num("x")?;
        looks.bubble_y = num("y")?;
        looks.bubble_size = num("size")?;
        looks.bubble_shape = match b.get("shape").and_then(Value::as_str) {
            None => None,
            Some("circle") => Some(visor_core::project::BubbleShape::Circle),
            Some("rounded") => Some(visor_core::project::BubbleShape::RoundedRect),
            Some(_) => return Err("bubble.shape is circle or rounded.".into()),
        };
    }
    Ok(looks)
}

/// A field that is present and not null.
fn arg<'a>(args: &'a Value, name: &str) -> Option<&'a Value> {
    args.get(name).filter(|v| !v.is_null())
}

/// Finds the item `query` names: a number matches the id, text the label
/// (whole, then in part, ignoring case).
fn find<T>(
    what: &str,
    items: &[T],
    query: &Value,
    id: impl Fn(&T) -> String,
    label: impl Fn(&T) -> String,
) -> Result<usize, String> {
    let given = match query {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return Err(format!("Give the {what} as a name or an id.")),
    };
    let wanted = given.to_lowercase();
    if let Some(i) = items
        .iter()
        .position(|item| id(item).to_lowercase() == wanted)
    {
        return Ok(i);
    }
    if let Some(i) = items
        .iter()
        .position(|item| label(item).to_lowercase() == wanted)
    {
        return Ok(i);
    }
    let matches: Vec<usize> = (0..items.len())
        .filter(|&i| label(&items[i]).to_lowercase().contains(&wanted))
        .collect();
    match matches.as_slice() {
        [i] => Ok(*i),
        [] => Err(format!(
            "No {what} matches \"{given}\". list_sources shows what is available."
        )),
        several => Err(format!(
            "Several {what}s match \"{given}\": {}. Use a longer name or the id.",
            several
                .iter()
                .take(5)
                .map(|&i| format!("\"{}\"", label(&items[i])))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn sources_json(sources: &Sources) -> Value {
    let s = &sources.shareable;
    json!({
        "displays": s.displays.iter().enumerate().map(|(i, d)| json!({
            "id": d.id,
            "name": d.name,
            "width": d.frame.width,
            "height": d.frame.height,
            "main": i == 0,
        })).collect::<Vec<_>>(),
        "windows": s.windows.iter().map(|w| json!({
            "id": w.id,
            "app": w.app,
            "title": w.title,
        })).collect::<Vec<_>>(),
        "apps": s.apps.iter().map(|a| json!({"pid": a.pid, "name": a.name})).collect::<Vec<_>>(),
        "cameras": sources.cameras.iter().map(|c| json!({"id": c.id, "name": c.name})).collect::<Vec<_>>(),
        "microphones": sources.mics.iter().map(|m| json!({
            "id": m.id,
            "name": m.name,
            "default": m.is_default,
        })).collect::<Vec<_>>(),
    })
}

impl App {
    /// Answers an agent command, now or once what it waits for happens.
    pub fn agent_command(&self, command: Value, reply: Reply) {
        let name = command["command"].as_str().unwrap_or_default().to_string();
        tracing::info!(command = name, "agent command");
        if !self.settings.borrow().agent_control {
            return send(reply, Err(OFF.into()));
        }
        let result = match name.as_str() {
            "status" => Ok(self.agent_status()),
            "list_sources" => return self.agent_list_sources(reply),
            "start_recording" => return self.agent_start(&command, reply),
            "pause_recording" => self.agent_pause(true),
            "resume_recording" => self.agent_pause(false),
            "add_marker" => self.agent_mark(&command),
            "stop_recording" => return self.agent_stop(&command, reply, false),
            "discard_recording" => return self.agent_stop(&command, reply, true),
            "remove_from_recording" => self.agent_edit(&command, EditKind::Remove),
            "trim_recording" => self.agent_edit(&command, EditKind::Trim),
            "restore_recording" => self.agent_edit(&command, EditKind::Restore),
            "open_recording" => self.agent_open(&command),
            "add_video" => return self.agent_add(&command, reply),
            "list_recordings" => Ok(self.agent_list_recordings()),
            "get_recording" => self.agent_get_recording(&command),
            "create_recording" => return self.agent_create(&command, reply),
            "edit_clips" => self.agent_edit_clips(&command),
            "edit_markers" => self.agent_edit_markers(&command),
            "edit_blurs" => self.agent_edit_blurs(&command),
            "set_recording" => self.agent_set_recording(&command),
            "edit_voiceovers" => self.agent_edit_voiceovers(&command),
            "export_recording" => return self.agent_export(&command, reply),
            "view_frame" => return self.agent_frame(&command, reply),
            "find_dead_time" => return self.agent_dead_time(&command, reply),
            other => Err(format!("Unknown command \"{other}\".")),
        };
        send(reply, result);
    }

    fn agent_status(&self) -> Value {
        let session = self.session.borrow();
        let (state, recorded, folder) = match &*session {
            Session::Idle if self.pending_start.borrow().is_some() => ("starting", None, None),
            Session::Idle => ("idle", None, None),
            Session::Starting { .. } => ("starting", None, None),
            Session::Recording { recorder, .. } => (
                if recorder.is_paused() {
                    "paused"
                } else {
                    "recording"
                },
                Some(seconds(recorder.elapsed())),
                Some(recorder.dir().display().to_string()),
            ),
            Session::Stopping => ("saving", None, None),
        };
        json!({"state": state, "recorded_seconds": recorded, "folder": folder})
    }

    fn agent_list_sources(&self, reply: Reply) {
        let backend = self.backend.clone();
        std::thread::spawn(move || {
            let mut sources = list_sources(backend.as_ref());
            // Display names come from AppKit, on the UI thread.
            post(move |_| {
                visor_capture::name_displays(&mut sources.shareable);
                send(reply, Ok(sources_json(&sources)));
            });
        });
    }

    /// Starts recording with the given choices, through the recorder panel
    /// as if the user had made them, and answers once recording has begun
    /// (after the countdown).
    fn agent_start(&self, args: &Value, reply: Reply) {
        if !matches!(*self.session.borrow(), Session::Idle) || self.pending_start.borrow().is_some()
        {
            return send(
                reply,
                Err("Visor is already recording. Stop or discard that recording first.".into()),
            );
        }
        if self.quit_after_start.get().is_some() {
            return send(reply, Err("Visor is quitting.".into()));
        }
        // Recording more onto a stopped recording.
        let more = match arg(args, "folder") {
            Some(_) => match self.agent_recording(args) {
                Ok(dir) => Some(dir),
                Err(e) => return send(reply, Err(e)),
            },
            None => None,
        };
        if let Some(dir) = &more
            && self.editor_for(dir).is_some_and(|e| e.is_busy())
        {
            return send(
                reply,
                Err(
                    "That recording is busy in the editor (exporting, finding pauses, adding \
                     videos or recording a voiceover); try again once that's done."
                        .into(),
                ),
            );
        }
        // A new recording, not more of one the user was about to record
        // more onto from the panel.
        let pending = self.more.borrow_mut().take();
        if let Some(dir) = pending
            && more.as_ref() != Some(&dir)
        {
            self.show_hidden_editor(&dir);
        }
        // Cleared first: opening the panel reopens the user's devices and
        // says if one of those fails.
        self.set_panel_error("");
        self.open_panel();
        // The user's own choices, put back if this start fails.
        let users = self.panel_choices();
        if let Some(dir) = &more {
            // Its own sources, unless the arguments say otherwise.
            if let Err((e, missing)) = self.choose_saved_sources(dir)
                && !replaces_missing(&missing, args)
            {
                if let Some(users) = users {
                    self.restore_choices(users);
                }
                return send(
                    reply,
                    Err(format!(
                        "{e} Say what to record instead (list_sources shows what there is)."
                    )),
                );
            }
        }
        if let Err(e) = self.apply_agent_choices(args) {
            if more.is_some()
                && let Some(users) = users
            {
                self.restore_choices(users);
            }
            return send(reply, Err(e));
        }
        if more.is_some() {
            self.agent.borrow_mut().undo = users;
        }
        if let Some(dir) = more {
            if let Some(editor) = self.editor_for(&dir) {
                editor.hide_for_take();
            }
            *self.more.borrow_mut() = Some(dir);
        }
        let name: Option<String> = arg(args, "name")
            .and_then(Value::as_str)
            .map(|n| n.trim().chars().take(200).collect())
            .filter(|n: &String| !n.is_empty());
        // Answered by `agent_start_finished` once it records or fails;
        // the timer only guards against a start that never gets that far.
        let guard = Timer::default();
        guard.start(TimerMode::SingleShot, START_TIMEOUT, || {
            app().agent_start_timed_out()
        });
        {
            let mut agent = self.agent.borrow_mut();
            agent.start_request = Some(AgentRun { name });
            agent.start = Some((reply, guard));
        }
        self.record_when_ready(None);
    }

    /// Answers an agent waiting for a start: it is recording, or why not.
    /// Called wherever a start ends; does nothing if no agent is waiting.
    pub(super) fn agent_start_finished(&self, result: Result<(), String>) {
        let (waiting, undo) = {
            let mut agent = self.agent.borrow_mut();
            // Nothing may start for an agent once it was told it didn't.
            agent.start_request = None;
            (agent.start.take(), agent.undo.take())
        };
        let Some((reply, guard)) = waiting else {
            return;
        };
        // Not recording more after all: its editor comes back.
        if result.is_err()
            && matches!(*self.session.borrow(), Session::Idle)
            && let Some(dir) = self.take_more()
        {
            self.show_hidden_editor(&dir);
        }
        // A start that failed leaves the user's choices as they were.
        if result.is_err()
            && let Some(before) = undo
        {
            self.restore_choices(before);
        }
        // Not dropped here: this may run inside the timer's own callback.
        guard.stop();
        slint::Timer::single_shot(Duration::ZERO, move || drop(guard));
        send(reply, result.map(|()| self.agent_status()));
    }

    fn agent_start_timed_out(&self) {
        self.agent_start_finished(Err(
            "The recording didn't start within a minute, so it was cancelled.".into(),
        ));
        self.cancel_pending_start();
        if matches!(*self.session.borrow(), Session::Starting { .. }) {
            self.stop_recording(true);
        }
    }

    /// Sets the recorder panel's choices from a start command's arguments,
    /// as if the user had made them. Missing arguments keep what the panel
    /// has. All are checked before any is applied, and if a camera or mic
    /// then fails to open, the panel's earlier choices are put back.
    fn apply_agent_choices(&self, args: &Value) -> Result<(), String> {
        let choices = self.resolve_choices(args)?;
        let before = self.panel_choices();
        self.set_choices(&choices);
        if let Err(e) = self.open_chosen_devices(&choices) {
            if let Some(before) = before {
                self.restore_choices(before);
            }
            return Err(e);
        }
        // A camera still opening may yet fail, failing the start.
        self.agent.borrow_mut().undo = before;
        self.update_hint();
        self.update_screen_highlight();
        Ok(())
    }

    fn resolve_choices(&self, args: &Value) -> Result<Choices, String> {
        let panel = self.panel.borrow();
        let p = panel
            .as_ref()
            .ok_or("The recorder panel didn't open.".to_string())?;
        let (ui, sources) = (&p.ui, &p.sources);
        let mut c = Choices::default();
        if let Some(query) = arg(args, "display") {
            c.display = Some(find(
                "display",
                &sources.displays,
                query,
                |d| d.id.to_string(),
                |d| d.name.clone(),
            )?);
        }
        // A window, app or area says what to record by itself.
        const KINDS: [&str; 3] = ["window", "app", "area"];
        let target = match arg(args, "target") {
            Some(t) => Some(
                t.as_str()
                    .ok_or("target is screen, window, app or area.")?
                    .to_string(),
            ),
            None => match KINDS
                .iter()
                .filter(|k| arg(args, k).is_some())
                .collect::<Vec<_>>()
                .as_slice()
            {
                // A display alone means its whole screen.
                [] if c.display.is_some() => Some("screen".into()),
                [] => None,
                [kind] => Some(kind.to_string()),
                _ => return Err("Give only one of window, app or area.".into()),
            },
        };
        if let Some(target) = &target
            && let Some(other) = KINDS
                .iter()
                .find(|k| *k != target && arg(args, k).is_some())
        {
            return Err(format!("\"{other}\" doesn't go with target {target}."));
        }
        match target.as_deref() {
            None => {}
            Some("screen") => c.mode = Some(0),
            Some("window") => {
                let query = arg(args, "window")
                    .ok_or("Say which window to record (list_sources shows them).")?;
                c.window = Some(find(
                    "window",
                    &sources.windows,
                    query,
                    |w| w.id.to_string(),
                    window_label,
                )?);
                c.mode = Some(1);
            }
            Some("app") => {
                let query =
                    arg(args, "app").ok_or("Say which app to record (list_sources shows them).")?;
                c.app = Some(find(
                    "app",
                    &sources.apps,
                    query,
                    |a| a.pid.to_string(),
                    |a| a.name.clone(),
                )?);
                c.mode = Some(2);
            }
            Some("area") => {
                let display = c
                    .display
                    .or(usize::try_from(ui.get_display_index()).ok())
                    .and_then(|i| sources.displays.get(i))
                    .or(sources.displays.first())
                    .ok_or("No display available to record.")?;
                c.area = Some((display.id, area_arg(args, display)?));
                c.mode = Some(3);
            }
            Some(other) => {
                return Err(format!(
                    "Unknown target \"{other}\": use screen, window, app or area."
                ));
            }
        }
        if let Some(on) = arg(args, "system_audio") {
            c.system_audio = Some(on.as_bool().ok_or("system_audio is true or false.")?);
        }
        if let Some(query) = arg(args, "system_audio_app") {
            c.audio_app = Some(find(
                "app",
                &sources.apps,
                query,
                |a| a.pid.to_string(),
                |a| a.name.clone(),
            )?);
        }
        if let Some(query) = arg(args, "camera") {
            let cameras: Vec<_> = p
                .cameras
                .iter()
                .map(|c| (c.id.clone(), c.name.clone()))
                .collect();
            let current = usize::try_from(ui.get_camera_index()).unwrap_or(0);
            c.camera = Some(device_index("camera", &cameras, current, query)?);
        }
        if let Some(query) = arg(args, "mic") {
            let mics: Vec<_> = p
                .mics
                .iter()
                .map(|m| (m.id.clone(), m.name.clone()))
                .collect();
            let current = usize::try_from(ui.get_mic_index()).unwrap_or(0);
            c.mic = Some(device_index("microphone", &mics, current, query)?);
        }
        Ok(c)
    }

    fn set_choices(&self, c: &Choices) {
        let mut panel = self.panel.borrow_mut();
        let Some(p) = panel.as_mut() else { return };
        let ui = &p.ui;
        if let Some(i) = c.display {
            ui.set_display_index(i as i32);
        }
        if let Some(i) = c.window {
            ui.set_window_index(i as i32);
        }
        if let Some(i) = c.app {
            ui.set_app_index(i as i32);
        }
        if let Some((display, rect)) = c.area {
            ui.set_area_label(area_label(rect).into());
            p.area = Some((display, rect));
        }
        if let Some(mode) = c.mode {
            p.ui.set_mode(mode);
        }
        if let Some(on) = c.system_audio {
            p.ui.set_system_audio(on);
        }
        if let Some(i) = c.audio_app {
            p.ui.set_system_audio(true);
            // Index 0 is "All apps".
            p.ui.set_audio_app_index(i as i32 + 1);
        }
    }

    fn open_chosen_devices(&self, c: &Choices) -> Result<(), String> {
        if let Some(index) = c.camera {
            if let Some(p) = &*self.panel.borrow() {
                p.ui.set_camera_index(index as i32);
            }
            self.choose_camera(index);
            let d = self.devices.borrow();
            if index > 0 && d.camera.is_none() && d.camera_pending.is_none() {
                return Err(self.device_error(Permission::Camera));
            }
        }
        if let Some(index) = c.mic {
            if let Some(p) = &*self.panel.borrow() {
                p.ui.set_mic_index(index as i32);
            }
            self.choose_mic(index);
            if index > 0 && self.devices.borrow().mic.is_none() {
                return Err(self.device_error(Permission::Microphone));
            }
        }
        Ok(())
    }

    fn panel_choices(&self) -> Option<PanelChoices> {
        let panel = self.panel.borrow();
        let p = panel.as_ref()?;
        let ui = &p.ui;
        let settings = self.settings.borrow();
        Some(PanelChoices {
            mode: ui.get_mode(),
            display: ui.get_display_index(),
            window: ui.get_window_index(),
            app: ui.get_app_index(),
            area: p.area,
            area_label: ui.get_area_label(),
            system_audio: ui.get_system_audio(),
            audio_app: ui.get_audio_app_index(),
            saved_system_audio: settings.system_audio,
            last_camera: settings.last_camera.clone(),
            last_mic: settings.last_mic.clone(),
        })
    }

    fn restore_choices(&self, before: PanelChoices) {
        {
            let mut panel = self.panel.borrow_mut();
            let Some(p) = panel.as_mut() else { return };
            let ui = &p.ui;
            ui.set_mode(before.mode);
            ui.set_display_index(before.display);
            ui.set_window_index(before.window);
            ui.set_app_index(before.app);
            ui.set_area_label(before.area_label);
            p.area = before.area;
        }
        {
            let mut settings = self.settings.borrow_mut();
            settings.last_camera = before.last_camera;
            settings.last_mic = before.last_mic;
            settings.system_audio = before.saved_system_audio;
            settings.save();
        }
        // Reopens the camera and mic the user had chosen.
        let error = self.panel.borrow().as_ref().map(|p| p.ui.get_error());
        self.restore_device_choices();
        if let Some(p) = &*self.panel.borrow() {
            p.ui.set_system_audio(before.system_audio);
            p.ui.set_audio_app_index(before.audio_app);
            // Keep saying why the agent's choice failed.
            if let Some(error) = error {
                p.ui.set_error(error);
            }
        }
    }

    /// Why the camera or mic chosen for an agent's recording isn't open:
    /// what the panel says (it failed or access is off), or else that
    /// Visor is asking the user for access.
    pub(super) fn device_error(&self, which: Permission) -> String {
        let (what, arg) = match which {
            Permission::Camera => ("camera", "camera"),
            Permission::Microphone => ("microphone", "mic"),
        };
        let shown = self
            .panel
            .borrow()
            .as_ref()
            .map(|p| p.ui.get_error().to_string())
            .filter(|e| !e.is_empty());
        let why = shown.unwrap_or_else(|| match access(which) {
            Access::Asking => format!(
                "Visor is asking the user for access to the {what}; once they allow it, try again."
            ),
            Access::Off => format!(
                "The user has turned off {what} access for Visor (System Settings › Privacy & Security)."
            ),
            Access::Granted => format!("The {what} couldn't be opened."),
        });
        format!("{why} To record without it, pass {arg} \"none\".")
    }

    /// Sets the panel to record more onto the recording in `dir` with what
    /// it was recorded from. Err says what is gone (the rest is chosen), or
    /// that it never saved what it was recorded from.
    pub(super) fn choose_saved_sources(
        &self,
        dir: &Path,
    ) -> Result<(), (String, Vec<&'static str>)> {
        // What a recording that saved nothing has gone missing.
        const ALL: [&str; 3] = ["the display", "the camera", "the microphone"];
        let project = visor_core::Project::load(dir).map_err(|e| (e.to_string(), ALL.to_vec()))?;
        let Some(saved) = &project.sources else {
            return Err((
                "This recording was made before Visor kept what it recorded from: \
                 choose what to record more from."
                    .into(),
                ALL.to_vec(),
            ));
        };
        let (choices, missing) = {
            let panel = self.panel.borrow();
            let p = panel
                .as_ref()
                .ok_or(("The recorder panel didn't open.".to_string(), ALL.to_vec()))?;
            let cameras: Vec<&str> = p.cameras.iter().map(|c| c.id.as_str()).collect();
            let mics: Vec<&str> = p.mics.iter().map(|m| m.id.as_str()).collect();
            saved_choices(saved, &p.sources, &cameras, &mics)
        };
        self.set_choices(&choices);
        let opened = self.open_chosen_devices(&choices);
        self.update_hint();
        self.update_screen_highlight();
        if !missing.is_empty() {
            let gone = missing.join(" and ");
            let mut gone = gone.chars();
            let first = gone.next().map(|c| c.to_uppercase().to_string());
            let e = format!(
                "{}{} it was recorded from isn't there any more: choose what to record more from.",
                first.unwrap_or_default(),
                gone.as_str()
            );
            return Err((e, missing));
        }
        opened.map_err(|e| (e, Vec::new()))
    }

    /// The recording an edit or open command is about: its `folder`, or
    /// the one the agent last stopped. Only recordings directly in the
    /// recordings folder, and not one being recorded.
    fn agent_recording(&self, args: &Value) -> Result<PathBuf, String> {
        // Its final save could overwrite the edit, and it isn't the last
        // recording yet.
        if matches!(*self.session.borrow(), Session::Stopping) {
            return Err("A recording is still being saved; try again in a moment.".into());
        }
        let dir =
            match arg(args, "folder") {
                Some(folder) => PathBuf::from(folder.as_str().ok_or("folder is a path.")?),
                None => self.agent.borrow().last_folder.clone().ok_or(
                    "Say which recording (its folder): no recording has been stopped yet.",
                )?,
            };
        let missing = || {
            format!(
                "There's no recording at {}. The user may have renamed or deleted it.",
                dir.display()
            )
        };
        let dir = visor_core::project::real_path(&dir).map_err(|_| missing())?;
        if !dir.join(visor_core::project::PROJECT_FILE).is_file() {
            return Err(missing());
        }
        let root = visor_core::project::real_path(&self.settings.borrow().save_dir).ok();
        if dir.parent() != root.as_deref() {
            return Err("Only recordings in Visor's recordings folder can be used.".into());
        }
        match &*self.session.borrow() {
            Session::Recording { recorder, .. }
                if visor_core::project::real_path(recorder.dir()).ok().as_ref() == Some(&dir) =>
            {
                return Err("That recording is still going; stop it first.".into());
            }
            _ => {}
        }
        // Being recorded more onto (from the countdown on): the take's save
        // would overwrite an edit made now. Only waiting in the panel, the
        // recorder hasn't read it yet.
        if self.more_cancelled.borrow().as_ref() == Some(&dir) {
            return Err(
                "That recording is being put back after a cancelled take; try again in a moment."
                    .into(),
            );
        }
        if self.more.borrow().as_ref() == Some(&dir)
            && !matches!(*self.session.borrow(), Session::Idle)
        {
            return Err(
                "Visor is recording more onto that recording; stop or discard that first.".into(),
            );
        }
        Ok(dir)
    }

    /// Keeps the agent's last recording when its folder is renamed.
    pub(super) fn agent_folder_renamed(&self, from: &Path, to: &Path) {
        let mut agent = self.agent.borrow_mut();
        // Editors' folders are canonical, and so is last_folder.
        if agent.last_folder.as_deref() == Some(from) {
            agent.last_folder = Some(to.to_path_buf());
        }
    }

    /// The editor showing the recording in `dir`, if one is open.
    pub(crate) fn editor_for(&self, dir: &Path) -> Option<Rc<EditorWindow>> {
        self.editors
            .borrow()
            .iter()
            .find(|e| e.dir() == dir)
            .cloned()
    }

    /// Cuts or trims a recording (in recorded time, as markers are), or
    /// undoes all its cuts. Goes through its editor when open, so the user
    /// sees the change and can undo it.
    fn agent_edit(&self, args: &Value, kind: EditKind) -> Result<Value, String> {
        if let (EditKind::Remove, Some(ranges)) = (kind, arg(args, "ranges")) {
            let ranges = ranges_arg(ranges)?;
            // With what a single removal answers too.
            let mut v = self.agent_change(args, |p| {
                agent_edits::remove_ranges(p, &ranges, SLIVER).map(Change::Edit)
            })?;
            v["kept"] = v["clips"].clone();
            return Ok(v);
        }
        let dir = self.agent_recording(args)?;
        let range = match kind {
            EditKind::Restore => None,
            EditKind::Remove | EditKind::Trim => {
                let (start, end) = (
                    time_arg(args, "start_seconds")?,
                    time_arg(args, "end_seconds")?,
                );
                if end <= start {
                    return Err("end_seconds must be after start_seconds.".into());
                }
                Some((start, end))
            }
        };
        let (editor, current) = self.agent_project(&dir)?;
        not_during_voiceover(&editor)?;
        let duration = current.duration_us;
        if let Some((start, _)) = range
            && start >= duration
        {
            return Err(past_the_end("start_seconds", duration));
        }
        let mut edit = current.edit.clone();
        match (kind, range) {
            (EditKind::Remove, Some((a, b))) => edit.cut_joining(a, b, SLIVER),
            (EditKind::Trim, Some((a, b))) => edit.trim_joining(a, b, SLIVER),
            _ => edit = EditList::full(duration),
        }
        if edit.duration() == 0 {
            return Err("That would remove the whole recording.".into());
        }
        let project = match &editor {
            Some(editor) => editor.apply_agent_edit(edit)?,
            None => {
                let mut project = current;
                project.edit = edit;
                project.save(&dir).map_err(|e| e.to_string())?;
                project
            }
        };
        Ok(json!({
            "folder": dir.display().to_string(),
            "recorded_seconds": seconds(project.duration_us),
            "kept_seconds": seconds(project.edit.duration()),
            "video_seconds": seconds(project.output_duration()),
            "kept": project.edit.ranges().iter().map(|r| [seconds(r.0), seconds(r.1)]).collect::<Vec<_>>(),
            "editor_open": editor.is_some(),
        }))
    }

    /// Makes an agent's change to a recording: through its editor when
    /// open (shown there, and undoable as the user's own would be), else
    /// saved to its folder. Replies with the recording as it now is.
    fn agent_change(
        &self,
        args: &Value,
        make: impl FnOnce(&visor_core::Project) -> Result<Change, String>,
    ) -> Result<Value, String> {
        let mut dir = self.agent_recording(args)?;
        let (editor, current) = self.agent_project(&dir)?;
        not_during_voiceover(&editor)?;
        let change = make(&current)?;
        let project = match (&editor, change) {
            (Some(e), Change::Edit(edit)) => e.apply_agent_edit(edit)?,
            (Some(e), Change::Markers(m)) => e.apply_agent_markers(m)?,
            (Some(e), Change::Blurs(b)) => e.apply_agent_blurs(b)?,
            (Some(e), Change::Looks(after)) => e.apply_agent_looks(&after)?,
            (Some(e), Change::Voiceovers(v)) => e.apply_agent_voiceovers(v)?,
            (None, change) => {
                let mut p = current;
                let mut deleted_voiceovers = false;
                let renamed = match change {
                    Change::Edit(edit) => {
                        p.edit = edit;
                        false
                    }
                    Change::Markers(m) => {
                        p.markers = m;
                        false
                    }
                    Change::Blurs(b) => {
                        p.blurs = b;
                        false
                    }
                    Change::Voiceovers(v) => {
                        p.voiceovers = v;
                        deleted_voiceovers = true;
                        false
                    }
                    Change::Looks(after) => {
                        let renamed = after.title != p.title;
                        // Not while a copy or export writes into the folder.
                        if renamed && self.copying(&dir) {
                            return Err(
                                "Something is being written into it (videos added, or an export); \
                                        rename it once that's done."
                                    .into(),
                            );
                        }
                        p = *after;
                        renamed
                    }
                };
                p.save(&dir).map_err(|e| e.to_string())?;
                // Nothing can bring a deleted voiceover back without an
                // editor's undo: its files go, once nothing reads them.
                if deleted_voiceovers {
                    self.tidy_voiceovers(&dir);
                }
                // Its folder takes the new name, as the editor's does.
                if renamed {
                    let root = self.settings.borrow().save_dir.clone();
                    if let Ok(Some(to)) = visor_core::project::rename_to_title(&dir, &root) {
                        self.agent_folder_renamed(&dir, &to);
                        dir = to;
                    }
                }
                self.refresh_tray();
                p
            }
        };
        Ok(self.recording_value(&dir, &project, editor.is_some()))
    }

    /// Exports a recording and answers once the file is written: in its
    /// editor when open (the user sees it), else in the background, its
    /// folder not renamed meanwhile.
    fn agent_export(&self, args: &Value, reply: Reply) {
        let request = self.agent_recording(args).and_then(|dir| {
            let choice = agent_edits::export_choice(args, crate::editor::ExportChoice::last())?;
            let (editor, project) = self.agent_project(&dir)?;
            Ok((dir, choice, editor, project))
        });
        let (dir, choice, editor, project) = match request {
            Ok(r) => r,
            Err(e) => return send(reply, Err(e)),
        };
        let answer = {
            let (dir, video) = (dir.clone(), project.output_duration());
            move |result: Result<PathBuf, String>| -> Result<Value, String> {
                let file = result?;
                Ok(json!({
                    "folder": dir.display().to_string(),
                    "file": file.display().to_string(),
                    "bytes": std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0),
                    "video_seconds": seconds(video),
                }))
            }
        };
        if let Some(editor) = editor {
            let r = reply.clone();
            let done: crate::editor::ExportReply = Box::new(move |result| send(r, answer(result)));
            if let Err(e) = editor.agent_export(choice, done) {
                send(reply, Err(e));
            }
            return;
        }
        // One at a time: two could pick the same file name.
        let exports = dir.join("exports");
        if self
            .agent_exports
            .borrow()
            .iter()
            .any(|f| f.parent() == Some(exports.as_path()))
        {
            return send(
                reply,
                Err("It's already being exported; try again once that's done.".into()),
            );
        }
        let (options, _) = choice.options();
        let out = match crate::editor::export_path(&dir, &project, options.format.extension()) {
            Ok(out) => out,
            Err(e) => return send(reply, Err(format!("Couldn't export it: {e:#}"))),
        };
        self.copy_started(&dir);
        self.agent_exports.borrow_mut().push(out.clone());
        std::thread::spawn(move || {
            let cancel = std::sync::atomic::AtomicBool::new(false);
            // Written under a hidden name and named once done: one cut short
            // (Visor quit) never looks like a finished export.
            let name = out
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let partial = out.with_file_name(format!(".{name}"));
            let result =
                visor_media::export(&dir, &project, &partial, &options, &mut |_| {}, &cancel)
                    .and_then(|()| Ok(std::fs::rename(&partial, &out)?))
                    .map(|()| out.clone())
                    .map_err(|e| {
                        let _ = std::fs::remove_file(&partial);
                        format!("The export failed: {e:#}")
                    });
            post(move |app| {
                app.agent_exports.borrow_mut().retain(|f| *f != out);
                let mut answer = answer(result);
                // A rename held while it exported may move the folder now.
                if let (Ok(v), Some(to)) = (&mut answer, app.copy_finished(&dir)) {
                    agent_edits::moved(v, &dir, &to);
                }
                send(reply, answer);
            });
        });
    }

    /// Starts a recording from video files, off the UI thread; it becomes
    /// the recording edits apply to by default.
    fn agent_create(&self, args: &Value, reply: Reply) {
        let paths: Vec<PathBuf> = match arg(args, "paths") {
            Some(Value::Array(p)) => p
                .iter()
                .filter_map(|p| p.as_str().map(PathBuf::from))
                .collect(),
            _ => Vec::new(),
        };
        if paths.is_empty() {
            return send(reply, Err("Say which video files: paths.".into()));
        }
        if let Some(p) = paths.iter().find(|p| !p.is_absolute() || !p.exists()) {
            return send(reply, Err(format!("There's no file at {}.", p.display())));
        }
        let name = arg(args, "name").and_then(Value::as_str).map(String::from);
        let root = self.settings.borrow().save_dir.clone();
        std::thread::spawn(move || {
            let made = visor_media::import::new_recording(
                &root,
                &paths,
                name.as_deref(),
                &mut |_| {},
                &std::sync::atomic::AtomicBool::new(false),
            );
            post(move |app| {
                let result = made
                    .map_err(|e| format!("Couldn't start it: {e:#}"))
                    .and_then(|dir| {
                        let dir = canonical(&dir);
                        app.agent.borrow_mut().last_folder = Some(dir.clone());
                        app.refresh_tray();
                        let project = visor_core::Project::load(&dir).map_err(|e| e.to_string())?;
                        Ok(app.recording_value(&dir, &project, false))
                    });
                send(reply, result);
            });
        });
    }

    /// A recording's whole state, for an agent to find its way around it.
    fn agent_get_recording(&self, args: &Value) -> Result<Value, String> {
        let dir = self.agent_recording(args)?;
        let (editor, project) = self.agent_project(&dir)?;
        Ok(self.recording_value(&dir, &project, editor.is_some()))
    }

    /// The recording's state as `get_recording` gives it, with its exports
    /// (those still being written marked so).
    fn recording_value(
        &self,
        dir: &Path,
        project: &visor_core::Project,
        editor_open: bool,
    ) -> Value {
        recording_value(dir, project, editor_open, &self.exports_writing())
    }

    fn agent_edit_clips(&self, args: &Value) -> Result<Value, String> {
        let at = time_arg(args, "at_seconds")?;
        let before = opt_time(args, "before_seconds")?;
        let action = action_arg(args, &["split", "move", "bring_back"])?;
        self.agent_change(args, |p| {
            match action.as_str() {
                "split" => agent_edits::split(p, at),
                "move" => agent_edits::move_clip(p, at, before),
                _ => agent_edits::bring_back_at(p, at),
            }
            .map(Change::Edit)
        })
    }

    fn agent_edit_markers(&self, args: &Value) -> Result<Value, String> {
        let at = time_arg(args, "at_seconds")?;
        let text = arg(args, "text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let action = action_arg(args, &["add", "rename", "delete"])?;
        self.agent_change(args, |p| {
            match action.as_str() {
                "add" => agent_edits::add_marker(p, at, &text),
                "rename" => agent_edits::rename_marker(p, at, &text),
                _ => agent_edits::delete_marker(p, at),
            }
            .map(Change::Markers)
        })
    }

    fn agent_edit_blurs(&self, args: &Value) -> Result<Value, String> {
        let action = action_arg(args, &["add", "change", "delete"])?;
        let number = match arg(args, "number") {
            None => None,
            Some(n) => Some(n.as_u64().ok_or("number is 1, 2, …")? as usize),
        };
        let fraction = |name: &str| -> Result<Option<f64>, String> {
            match arg(args, name) {
                None => Ok(None),
                Some(v) => v
                    .as_f64()
                    .map(Some)
                    .ok_or_else(|| format!("{name} is a fraction of the frame, 0 to 1.")),
            }
        };
        let area = agent_edits::Area {
            x: fraction("x")?,
            y: fraction("y")?,
            width: fraction("width")?,
            height: fraction("height")?,
        };
        let (start, end) = (
            opt_time(args, "start_seconds")?,
            opt_time(args, "end_seconds")?,
        );
        let at = opt_time(args, "at_seconds")?;
        self.agent_change(args, |p| {
            match action.as_str() {
                "add" => agent_edits::add_blur(p, area, start, end),
                "change" => agent_edits::change_blur(p, at, number, area, start, end),
                _ => agent_edits::delete_blur(p, at, number),
            }
            .map(Change::Blurs)
        })
    }

    fn agent_edit_voiceovers(&self, args: &Value) -> Result<Value, String> {
        action_arg(args, &["delete"])?;
        let number = arg(args, "number")
            .and_then(Value::as_u64)
            .ok_or("number is 1, 2, … as get_recording lists the voiceovers.")?
            as usize;
        self.agent_change(args, |p| {
            agent_edits::delete_voiceover(p, number).map(Change::Voiceovers)
        })
    }

    fn agent_set_recording(&self, args: &Value) -> Result<Value, String> {
        let looks = looks_arg(args)?;
        if looks == agent_edits::Looks::default() {
            return Err("Say what to change: name, speed, thumbnail_seconds or bubble.".into());
        }
        self.agent_change(args, |p| {
            agent_edits::set_looks(p, &looks).map(|p| Change::Looks(Box::new(p)))
        })
    }

    /// The recording in `dir` as its open editor has it, else as saved, and
    /// that editor.
    fn agent_project(
        &self,
        dir: &Path,
    ) -> Result<(Option<Rc<EditorWindow>>, visor_core::Project), String> {
        let editor = self.editor_for(dir);
        let project = match &editor {
            Some(editor) => editor.project(),
            None => visor_core::Project::load(dir).map_err(|e| e.to_string())?,
        };
        if project.duration_us == 0 {
            return Err("That recording wasn't saved properly, so it can't be used.".into());
        }
        Ok((editor, project))
    }

    /// Shows an agent the frame recorded at `seconds`, cut or not, as the
    /// export would show it. Rendered off the UI thread.
    fn agent_frame(&self, args: &Value, reply: Reply) {
        let request = self.agent_recording(args).and_then(|dir| {
            let t = time_arg(args, "seconds")?;
            let (_, project) = self.agent_project(&dir)?;
            if t >= project.duration_us {
                return Err(past_the_end("seconds", project.duration_us));
            }
            Ok((dir, project, t))
        });
        let (dir, project, t) = match request {
            Ok(request) => request,
            Err(e) => return send(reply, Err(e)),
        };
        render(Box::new(move || {
            use base64::prelude::*;
            let result = visor_media::export::frame_jpeg(&dir, &project, t, FRAME_HEIGHT)
                .map(|jpeg| {
                    let mut reply = json!({
                        "folder": dir.display().to_string(),
                        "seconds": seconds(t),
                        "removed": project.edit.to_output(t).is_none(),
                    });
                    reply[crate::cli::IMAGE] = BASE64_STANDARD.encode(jpeg).into();
                    reply
                })
                .map_err(|e| format!("Couldn't show that frame: {e:#}"));
            send(reply, result);
        }));
    }

    /// Lists a recording's dead time, still and silent, for an agent to cut.
    /// Found off the UI thread: the whole recording is decoded.
    fn agent_dead_time(&self, args: &Value, reply: Reply) {
        let request = self.agent_recording(args).and_then(|dir| {
            let min = match arg(args, "min_seconds") {
                None => visor_media::dead_time::MIN_LEN,
                Some(min) => {
                    let min = min
                        .as_f64()
                        .filter(|m| m.is_finite() && *m >= 0.5)
                        .ok_or("min_seconds is a number of seconds, 0.5 or more.")?;
                    (min * 1e6) as Micros
                }
            };
            let (editor, project) = self.agent_project(&dir)?;
            // It reads the folder's files as it goes: held as a job of its
            // editor, else as a copy, so nothing renames or tidies them away.
            if editor.is_none() {
                self.copy_started(&dir);
            }
            let guard = editor.map(|e| e.start_job());
            Ok((dir, project, min, guard))
        });
        let (dir, project, min, guard) = match request {
            Ok(request) => request,
            Err(e) => return send(reply, Err(e)),
        };
        std::thread::spawn(move || {
            let cancel = Default::default();
            let result = visor_media::dead_time::find(&dir, &project, min, &cancel)
                .map(|stretches| {
                    let total: Micros = stretches.iter().map(|r| r.len()).sum();
                    // Rounded inwards, so cutting them never cuts into
                    // the margin left around what happens.
                    let inside = |r: &visor_core::Range| {
                        [
                            (r.0 as f64 / 1e3).ceil() / 1e3,
                            (r.1 as f64 / 1e3).floor() / 1e3,
                        ]
                    };
                    json!({
                        "folder": dir.display().to_string(),
                        "stretches": stretches.iter().map(inside).collect::<Vec<_>>(),
                        "total_seconds": seconds(total),
                    })
                })
                .map_err(|e| format!("Couldn't look for dead time: {e:#}"));
            let held = guard.is_none();
            drop(guard);
            send(reply, result);
            if held {
                post(move |app| {
                    app.copy_finished(&dir);
                });
            }
        });
    }

    /// The recordings in the save folder, newest first.
    fn agent_list_recordings(&self) -> Value {
        let root = self.settings.borrow().save_dir.clone();
        let entries = visor_core::library::entries(&root).unwrap_or_default();
        let recordings: Vec<Value> = entries
            .iter()
            .map(|e| {
                let mut r = json!({
                    "name": e.title,
                    "folder": e.dir.display().to_string(),
                    "recorded_at": e.recorded_at.map(|t| {
                        chrono::DateTime::<chrono::Local>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
                    }),
                });
                match &e.info {
                    Some(i) => {
                        r["recorded_seconds"] = seconds(i.recorded_us).into();
                        r["video_seconds"] = seconds(i.video_us).into();
                        r["markers"] = i.markers.into();
                    }
                    None => r["error"] = "project.json can't be read".into(),
                }
                r
            })
            .collect();
        let root = visor_core::project::real_path(&root).unwrap_or(root);
        json!({"folder": root.display().to_string(), "recordings": recordings})
    }

    /// Adds video files or other recordings at the end of a recording:
    /// through its editor when open (the user can undo it), else copied in
    /// off the UI thread and saved.
    fn agent_add(&self, args: &Value, reply: Reply) {
        let request = self.agent_recording(args).and_then(|dir| {
            let paths: Vec<PathBuf> = match arg(args, "paths") {
                Some(Value::Array(paths)) => paths
                    .iter()
                    .map(|p| p.as_str().map(PathBuf::from).ok_or("paths are paths."))
                    .collect::<Result<_, _>>()?,
                Some(Value::String(path)) => vec![path.into()],
                _ => Vec::new(),
            };
            if paths.is_empty() {
                return Err("Say what to add: paths, the video files or recording folders.".into());
            }
            for path in &paths {
                if !path.is_absolute() {
                    return Err(format!("{} isn't an absolute path.", path.display()));
                }
                if !path.exists() {
                    return Err(format!("There's nothing at {}.", path.display()));
                }
            }
            // Waiting in the panel to be recorded more onto: the take's
            // save would drop what's added now.
            if self.more.borrow().as_ref() == Some(&dir) {
                return Err("The user is about to record more onto that recording; \
                            add to it once they're done."
                    .into());
            }
            let (editor, project) = self.agent_project(&dir)?;
            Ok((dir, paths, editor, project))
        });
        let (dir, paths, editor, project) = match request {
            Ok(request) => request,
            Err(e) => return send(reply, Err(e)),
        };
        if let Some(editor) = editor {
            let (r, d) = (reply.clone(), dir.clone());
            let answer: crate::editor::AddReply = Box::new(move |result| {
                send(
                    r,
                    result.map(|(project, starts)| added_json(&d, &project, &starts, true)),
                );
            });
            if let Err(e) = editor.add_videos(paths, Some(answer)) {
                send(reply, Err(e));
            }
            return;
        }
        self.copy_started(&dir);
        std::thread::spawn(move || {
            let prepared = visor_media::import::prepare(&dir, &project, &paths).and_then(|p| {
                p.copy(
                    &dir,
                    &mut |_| {},
                    &std::sync::atomic::AtomicBool::new(false),
                )?;
                Ok(p)
            });
            post(move |app| app.agent_added(dir, prepared, reply));
        });
    }

    /// Deletes the files of voiceovers deleted from the recording in `dir`,
    /// unless something may still read them: its editor (whose undo can
    /// bring them back), or a copy, export or search in it. Runs again when
    /// those are done (`copy_finished`, `rename_closed`).
    pub(crate) fn tidy_voiceovers(&self, dir: &Path) {
        if self.copying(dir) || self.editor_for(dir).is_some() {
            return;
        }
        if let Ok(project) = visor_core::Project::load(dir) {
            visor_core::project::remove_unused_voiceover_files(dir, &project);
        }
    }

    /// Whether videos are being copied into `dir` (see `App::copies`).
    pub(crate) fn copying(&self, dir: &Path) -> bool {
        self.copies.borrow().contains(&canonical(dir))
    }

    /// A copy into `dir` starts; [`copy_finished`](Self::copy_finished)
    /// once it's over, whatever came of it.
    pub(crate) fn copy_started(&self, dir: &Path) {
        self.copies.borrow_mut().push(canonical(dir));
    }

    /// A copy into `dir` is over. Returns where the folder moved, if a
    /// rename held meanwhile happened now.
    pub(crate) fn copy_finished(&self, dir: &Path) -> Option<PathBuf> {
        let dir = canonical(dir);
        {
            let mut copies = self.copies.borrow_mut();
            if let Some(i) = copies.iter().position(|d| *d == dir) {
                copies.remove(i);
            }
        }
        // A rename held while it was written to (its editor closed, and that
        // editor's own jobs wound down, meanwhile) happens now, if nothing
        // else has the folder.
        self.tidy_voiceovers(&dir);
        let held = self.held_renames.borrow().contains(&dir);
        if held && !self.copying(&dir) && self.editor_for(&dir).is_none() {
            self.held_renames.borrow_mut().retain(|d| *d != dir);
            let root = self.settings.borrow().save_dir.clone();
            if let Ok(Some(to)) = visor_core::project::rename_to_title(&dir, &root) {
                tracing::info!("renamed {} to {}", dir.display(), to.display());
                self.agent_folder_renamed(&dir, &to);
                self.refresh_tray();
                return Some(to);
            }
        }
        None
    }

    /// Exports being written, by agents in the background or in editors:
    /// their files may not exist yet.
    pub(crate) fn exports_writing(&self) -> Vec<PathBuf> {
        let mut writing = self.agent_exports.borrow().clone();
        writing.extend(
            self.editors
                .borrow()
                .iter()
                .filter_map(|e| e.exporting_to()),
        );
        writing
    }

    /// Copied in for an agent with no editor open: adds it, if the
    /// recording can still be changed.
    fn agent_added(
        &self,
        dir: PathBuf,
        prepared: anyhow::Result<visor_media::import::Prepared>,
        reply: Reply,
    ) {
        let mut result = self.add_copied(&dir, prepared);
        // Only then: a rename held while it copied may move the folder.
        if let (Ok(v), Some(to)) = (&mut result, self.copy_finished(&dir)) {
            agent_edits::moved(v, &dir, &to);
        }
        send(reply, result);
    }

    fn add_copied(
        &self,
        dir: &Path,
        prepared: anyhow::Result<visor_media::import::Prepared>,
    ) -> Result<Value, String> {
        let dir = dir.to_path_buf();
        let prepared = prepared.map_err(|e| format!("Couldn't add it: {e:#}"))?;
        let remove_copies = |prepared: &visor_media::import::Prepared| {
            for name in prepared.files() {
                let _ = std::fs::remove_file(dir.join(name));
            }
        };
        let still = self
            .agent_recording(&json!({"folder": dir.display().to_string()}))
            .and_then(|_| match self.more.borrow().as_ref() == Some(&dir) {
                true => Err("The user started recording more onto that recording \
                             meanwhile; nothing was added."
                    .to_string()),
                false => Ok(()),
            });
        if let Err(e) = still {
            remove_copies(&prepared);
            return Err(e);
        }
        // Opened in the meantime: added there, to undo, unless it's
        // exporting or the like, which would leave it out.
        if let Some(editor) = self.editor_for(&dir) {
            if editor.is_busy() {
                remove_copies(&prepared);
                return Err(
                    "That recording was opened and is busy (exporting, finding pauses, \
                     adding videos or recording a voiceover); nothing was added. Try again \
                     once that's done."
                        .into(),
                );
            }
            let starts = editor.add_prepared(prepared);
            return Ok(added_json(&dir, &editor.project(), &starts, true));
        }
        let mut project = match visor_core::Project::load(&dir) {
            Ok(p) => p,
            Err(e) => {
                remove_copies(&prepared);
                return Err(e.to_string());
            }
        };
        let starts: Vec<Micros> = prepared
            .additions
            .iter()
            .cloned()
            .map(|a| project.add(a))
            .collect();
        if let Err(e) = project.save(&dir) {
            remove_copies(&prepared);
            return Err(format!("Couldn't save the recording: {e}"));
        }
        Ok(added_json(&dir, &project, &starts, false))
    }

    /// Opens a recording in the editor for the user.
    fn agent_open(&self, args: &Value) -> Result<Value, String> {
        let dir = self.agent_recording(args)?;
        // Its editor is hidden while the user picks what to record more
        // onto it from the recorder panel.
        if self.more.borrow().as_ref() == Some(&dir) {
            return Err(
                "The user is about to record more onto that recording from the \
                 recorder panel; it opens again once they record or close the panel."
                    .into(),
            );
        }
        self.open_editor(dir.clone());
        Ok(json!({"folder": dir.display().to_string()}))
    }

    fn agent_pause(&self, pause: bool) -> Result<Value, String> {
        let paused = match &*self.session.borrow() {
            Session::Recording { recorder, .. } => recorder.is_paused(),
            _ => return Err("Visor isn't recording.".into()),
        };
        if paused != pause {
            self.toggle_pause();
        }
        Ok(self.agent_status())
    }

    fn agent_mark(&self, args: &Value) -> Result<Value, String> {
        let text: String = args["text"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .chars()
            .take(200)
            .collect();
        let at = match &mut *self.session.borrow_mut() {
            Session::Recording { recorder, .. } => recorder.mark(text),
            _ => return Err("Visor isn't recording.".into()),
        };
        Ok(json!({"at_seconds": seconds(at)}))
    }

    /// Stops or discards the recording and answers once that's done.
    fn agent_stop(&self, args: &Value, reply: Reply, discard: bool) {
        if self.abandon_pending_start() {
            return send(reply, Ok(json!({"cancelled": true})));
        }
        let recording = match &*self.session.borrow() {
            Session::Recording { .. } => true,
            Session::Starting { .. } => false,
            _ => return send(reply, Err("Visor isn't recording.".into())),
        };
        if !recording {
            // Still counting down: nothing was recorded yet.
            self.stop_recording(true);
            return send(reply, Ok(json!({"cancelled": true})));
        }
        if discard {
            self.agent.borrow_mut().discard = Some(reply);
        } else {
            let open_editor = args["open_editor"].as_bool().unwrap_or(false);
            self.agent.borrow_mut().stop = Some((reply, open_editor));
        }
        self.stop_recording(discard);
    }

    /// A recording finished saving (or failed to). Answers an agent that
    /// stopped it; returns what to open.
    pub(super) fn agent_stopped(
        &self,
        result: &anyhow::Result<(PathBuf, visor_core::Project)>,
        run: Option<AgentRun>,
    ) -> AfterStop {
        let editor = if run.as_ref().is_some_and(|r| r.name.is_some()) {
            AfterStop::Edit
        } else {
            AfterStop::Name
        };
        let stop = self.agent.borrow_mut().stop.take();
        // Edits default to a recording the agent started or stopped, never
        // one the user made by hand.
        if let Ok((dir, _)) = result
            && (run.is_some() || stop.is_some())
        {
            // Canonical, like the editors' folders that renames report.
            self.agent.borrow_mut().last_folder =
                Some(visor_core::project::real_path(dir).unwrap_or_else(|_| dir.clone()));
        }
        let Some((reply, open_editor)) = stop else {
            // Stopped by the user.
            return editor;
        };
        send(
            reply,
            match result {
                Ok((dir, project)) => Ok(json!({
                    "name": project.title,
                    "folder": dir.display().to_string(),
                    "duration_seconds": seconds(project.duration_us),
                    "markers": project.markers.iter().map(|m| json!({
                        "at_seconds": seconds(m.at_us),
                        "text": m.text,
                    })).collect::<Vec<_>>(),
                    "editor_opened": open_editor,
                })),
                Err(e) => Err(format!("Saving the recording failed: {e:#}")),
            },
        );
        if open_editor {
            editor
        } else {
            AfterStop::Nothing
        }
    }

    /// A recording was discarded. Answers an agent that discarded it;
    /// returns whether to show the recorder panel as usual.
    pub(super) fn agent_discarded(&self, result: &anyhow::Result<()>) -> bool {
        let Some(reply) = self.agent.borrow_mut().discard.take() else {
            return true;
        };
        send(
            reply,
            match result {
                Ok(()) => Ok(json!({"discarded": true})),
                Err(e) => Err(format!("Discarding the recording failed: {e:#}")),
            },
        );
        false
    }
}

fn canonical(dir: &Path) -> PathBuf {
    visor_core::project::real_path(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// The reply to `add_video`: where each addition landed in recorded time,
/// and what's kept now.
fn added_json(
    dir: &Path,
    project: &visor_core::Project,
    starts: &[Micros],
    editor_open: bool,
) -> Value {
    let ends = starts
        .iter()
        .skip(1)
        .copied()
        .chain(std::iter::once(project.duration_us));
    let added: Vec<Value> = starts
        .iter()
        .zip(ends)
        .map(|(&a, b)| json!({"start_seconds": seconds(a), "end_seconds": seconds(b)}))
        .collect();
    json!({
        "folder": dir.display().to_string(),
        "added": added,
        "recorded_seconds": seconds(project.duration_us),
        "kept_seconds": seconds(project.edit.duration()),
        "video_seconds": seconds(project.output_duration()),
        "kept": project.edit.ranges().iter().map(|r| [seconds(r.0), seconds(r.1)]).collect::<Vec<_>>(),
        "editor_open": editor_open,
    })
}

/// Whether a start command's `args` pick something else for everything in
/// `missing` (as `saved_choices` lists it): only then may recording more go
/// on without what's gone.
fn replaces_missing(missing: &[&str], args: &Value) -> bool {
    let has = |keys: &[&str]| keys.iter().any(|k| arg(args, k).is_some());
    missing.iter().all(|m| match *m {
        "the window" | "the app" | "the display" => {
            has(&["target", "display", "window", "app", "area"])
        }
        "the camera" => has(&["camera"]),
        "the microphone" => has(&["mic"]),
        _ => has(&["system_audio", "system_audio_app"]),
    })
}

/// The panel choices that record from `saved` again (a recording's
/// sources, to record more), given what can be recorded now and the ids of
/// the cameras and mics listed in the panel; and what of it is gone, as
/// words for the user ("the window").
fn saved_choices(
    saved: &RecordingConfig,
    sources: &Shareable,
    cameras: &[&str],
    mics: &[&str],
) -> (Choices, Vec<&'static str>) {
    let mut c = Choices::default();
    let mut missing = Vec::new();
    let display = |id: u32| sources.displays.iter().position(|d| d.id == id);
    match &saved.target {
        CaptureTarget::Display { id } => match display(*id) {
            Some(i) => (c.mode, c.display) = (Some(0), Some(i)),
            None => missing.push("the display"),
        },
        CaptureTarget::Window { id } => match sources.windows.iter().position(|w| w.id == *id) {
            Some(i) => (c.mode, c.window) = (Some(1), Some(i)),
            None => missing.push("the window"),
        },
        CaptureTarget::App { pid } => match sources.apps.iter().position(|a| a.pid == *pid) {
            Some(i) => (c.mode, c.app) = (Some(2), Some(i)),
            None => missing.push("the app"),
        },
        CaptureTarget::Area { display_id, rect } => match display(*display_id) {
            Some(i) => {
                (c.mode, c.display) = (Some(3), Some(i));
                c.area = Some((*display_id, *rect));
            }
            None => missing.push("the display"),
        },
    }
    c.system_audio = Some(saved.audio.system || saved.audio.app_pid.is_some());
    if let Some(pid) = saved.audio.app_pid {
        match sources.apps.iter().position(|a| a.pid == pid) {
            Some(i) => c.audio_app = Some(i),
            None => missing.push("the app whose sound was recorded"),
        }
    }
    // Panel indexes, where 0 is none.
    let device = |id: &Option<String>, ids: &[&str], what, missing: &mut Vec<_>| match id {
        None => Some(0),
        Some(id) => match ids.iter().position(|d| d == id) {
            Some(i) => Some(i + 1),
            None => {
                missing.push(what);
                None
            }
        },
    };
    c.camera = device(&saved.camera, cameras, "the camera", &mut missing);
    c.mic = device(&saved.audio.mic, mics, "the microphone", &mut missing);
    (c, missing)
}

/// A start command's choices, checked against the panel's lists before
/// any is applied: indexes into them, and the panel mode to record with.
#[derive(Debug, Default, PartialEq)]
struct Choices {
    display: Option<usize>,
    mode: Option<i32>,
    window: Option<usize>,
    app: Option<usize>,
    area: Option<(u32, Rect)>,
    system_audio: Option<bool>,
    audio_app: Option<usize>,
    /// Panel indexes, where 0 is none.
    camera: Option<usize>,
    mic: Option<usize>,
}

// Only macOS asks for access; elsewhere it is always granted.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum Access {
    Granted,
    Asking,
    Off,
}

/// Whether Visor may use the camera or mic (always, outside macOS).
fn access(which: Permission) -> Access {
    #[cfg(target_os = "macos")]
    {
        use visor_capture::macos_permissions::{Kind, Status, status};
        let kind = match which {
            Permission::Camera => Kind::Camera,
            Permission::Microphone => Kind::Microphone,
        };
        match status(kind) {
            Status::Granted => Access::Granted,
            Status::NotDetermined => Access::Asking,
            Status::Denied => Access::Off,
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = which;
        Access::Granted
    }
}

/// The panel's choices before an agent's, to put back if those fail.
struct PanelChoices {
    mode: i32,
    display: i32,
    window: i32,
    app: i32,
    area: Option<(u32, Rect)>,
    area_label: SharedString,
    system_audio: bool,
    audio_app: i32,
    /// The saved setting, which a start that got far enough changes.
    saved_system_audio: bool,
    last_camera: Option<String>,
    last_mic: Option<String>,
}

/// The panel's index for a camera or mic argument among `devices` (id,
/// name), where 0 is none: a name or id, `"none"` or false for none, or
/// true for the one chosen now (`current`), else the first.
fn device_index(
    what: &str,
    devices: &[(String, String)],
    current: usize,
    query: &Value,
) -> Result<usize, String> {
    match query {
        Value::Bool(false) => Ok(0),
        Value::String(s) if s.trim().eq_ignore_ascii_case("none") => Ok(0),
        Value::Bool(true) if current > 0 => Ok(current),
        Value::Bool(true) if !devices.is_empty() => Ok(1),
        Value::Bool(true) => Err(format!("No {what} is connected.")),
        _ => find(what, devices, query, |d| d.0.clone(), |d| d.1.clone()).map(|i| i + 1),
    }
}

/// An `area` argument (`x`, `y`, `width`, `height` in points from the
/// display's top-left corner) as a global rect on `display`.
fn area_arg(args: &Value, display: &DisplayInfo) -> Result<Rect, String> {
    let area = arg(args, "area").ok_or("Give the area as {x, y, width, height}.")?;
    let get = |k: &str| {
        area[k]
            .as_f64()
            .ok_or_else(|| format!("The area needs a number for \"{k}\"."))
    };
    let (x, y, width, height) = (get("x")?, get("y")?, get("width")?, get("height")?);
    let frame = display.frame;
    if x < 0.0
        || y < 0.0
        || width < 16.0
        || height < 16.0
        || x + width > frame.width
        || y + height > frame.height
    {
        return Err(format!(
            "The area must be at least 16×16 and fit the {}×{} display.",
            frame.width, frame.height
        ));
    }
    Ok(Rect {
        x: frame.x + x,
        y: frame.y + y,
        width,
        height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_arguments_that_replace_what_is_gone_do() {
        let window_gone = ["the window"];
        assert!(!replaces_missing(&window_gone, &json!({"camera": "none"})));
        assert!(replaces_missing(&window_gone, &json!({"target": "screen"})));
        assert!(replaces_missing(&window_gone, &json!({"display": 2})));
        let both = ["the window", "the camera"];
        assert!(!replaces_missing(&both, &json!({"window": 3})));
        assert!(replaces_missing(
            &both,
            &json!({"window": 3, "camera": "none"})
        ));
        assert!(!replaces_missing(
            &["the microphone"],
            &json!({"camera": true})
        ));
        assert!(replaces_missing(
            &["the microphone"],
            &json!({"mic": "none"})
        ));
        let sound = ["the app whose sound was recorded"];
        assert!(replaces_missing(&sound, &json!({"system_audio": false})));
        assert!(replaces_missing(&[], &json!({})));
    }

    #[test]
    fn saved_sources_come_back_as_panel_choices() {
        use visor_capture::{AppInfo, DisplayInfo, WindowInfo};
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 50.0,
        };
        let sources = Shareable {
            displays: [7, 9]
                .map(|id| DisplayInfo {
                    id,
                    name: format!("Display {id}"),
                    frame: rect,
                    scale: 2.0,
                })
                .into(),
            windows: vec![WindowInfo {
                id: 42,
                title: "Notes".into(),
                app: "Notes".into(),
                pid: 5,
            }],
            apps: vec![AppInfo {
                pid: 5,
                name: "Notes".into(),
            }],
        };
        let (cameras, mics) = (["cam-a", "cam-b"], ["mic-a"]);
        let mut saved = RecordingConfig::display(9);
        saved.camera = Some("cam-b".into());
        saved.audio.mic = Some("mic-a".into());
        let (c, missing) = saved_choices(&saved, &sources, &cameras, &mics);
        assert!(missing.is_empty());
        assert_eq!((c.mode, c.display), (Some(0), Some(1)));
        // Panel device indexes count "None" as 0.
        assert_eq!((c.camera, c.mic), (Some(2), Some(1)));
        assert_eq!((c.system_audio, c.audio_app), (Some(false), None));

        saved.target = CaptureTarget::Window { id: 42 };
        saved.camera = None;
        saved.audio.app_pid = Some(5);
        let (c, _) = saved_choices(&saved, &sources, &cameras, &mics);
        assert_eq!((c.mode, c.window, c.camera), (Some(1), Some(0), Some(0)));
        assert_eq!(c.audio_app, Some(0));

        saved.target = CaptureTarget::Area {
            display_id: 7,
            rect,
        };
        let (c, _) = saved_choices(&saved, &sources, &cameras, &mics);
        assert_eq!(
            (c.mode, c.display, c.area),
            (Some(3), Some(0), Some((7, rect)))
        );

        // What's gone is said, and the rest still chosen.
        saved.target = CaptureTarget::Window { id: 43 };
        saved.camera = Some("cam-c".into());
        let (c, missing) = saved_choices(&saved, &sources, &cameras, &mics);
        assert_eq!(missing, ["the window", "the camera"]);
        assert_eq!(
            (c.mode, c.window, c.camera, c.mic),
            (None, None, None, Some(1))
        );
    }

    #[test]
    fn finds_by_id_name_or_part_of_a_name() {
        let items = [
            ("1", "Safari"),
            ("2", "Safari Technology Preview"),
            ("3", "Notes"),
        ];
        let find = |q: Value| find("app", &items, &q, |i| i.0.into(), |i| i.1.into());
        assert_eq!(find(json!(3)), Ok(2));
        assert_eq!(find(json!("safari")), Ok(0));
        assert_eq!(find(json!("tech")), Ok(1));
        assert!(find(json!("saf")).unwrap_err().contains("Several"));
        assert!(find(json!("Mail")).unwrap_err().contains("No app"));
    }

    #[test]
    fn devices_take_none_true_or_a_name() {
        let cams = [
            ("a".to_string(), "FaceTime HD".to_string()),
            ("b".into(), "Studio".into()),
        ];
        assert_eq!(device_index("camera", &cams, 2, &json!("None")), Ok(0));
        assert_eq!(device_index("camera", &cams, 2, &json!(false)), Ok(0));
        assert_eq!(device_index("camera", &cams, 2, &json!(true)), Ok(2));
        assert_eq!(device_index("camera", &cams, 0, &json!(true)), Ok(1));
        assert_eq!(device_index("camera", &cams, 0, &json!("studio")), Ok(2));
        assert!(device_index("camera", &[], 0, &json!(true)).is_err());
    }
}
