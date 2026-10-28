//! Agent control: commands from the `visor` command line and its MCP server
//! (see `crate::cli`), which reach the running app through
//! `crate::instance`. Off unless the user turns it on in Preferences.
//!
//! A command is a JSON object whose `command` names what to do; the other
//! fields are its arguments. Replies are JSON objects with `ok` and either
//! the result's fields or an `error` for the agent to show or act on.

use serde_json::{Value, json};

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
    discard: Option<Reply>,
}

/// A recording an agent started, kept with the session: the control bar
/// says so, and the editor doesn't ask for a name the agent gave.
#[derive(Debug, Clone)]
pub struct AgentRun {
    pub name: Option<String>,
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

fn seconds(us: visor_core::Micros) -> f64 {
    (us as f64 / 100_000.0).round() / 10.0
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
        self.open_panel();
        self.set_panel_error("");
        if let Err(e) = self.apply_agent_choices(args) {
            return send(reply, Err(e));
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
                return Err(self.device_error("camera"));
            }
        }
        if let Some(index) = c.mic {
            if let Some(p) = &*self.panel.borrow() {
                p.ui.set_mic_index(index as i32);
            }
            self.choose_mic(index);
            if index > 0 && self.devices.borrow().mic.is_none() {
                return Err(self.device_error("microphone"));
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
    pub(super) fn device_error(&self, what: &str) -> String {
        let shown = self
            .panel
            .borrow()
            .as_ref()
            .map(|p| p.ui.get_error().to_string())
            .filter(|e| !e.is_empty());
        let why = shown.unwrap_or_else(|| {
            format!(
                "Visor is asking the user for access to the {what}; once they allow it, try again."
            )
        });
        format!("{why} To record without it, pass {what} \"none\".")
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
        let editor = if run.is_some_and(|r| r.name.is_some()) {
            AfterStop::Edit
        } else {
            AfterStop::Name
        };
        let Some((reply, open_editor)) = self.agent.borrow_mut().stop.take() else {
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

/// A start command's choices, checked against the panel's lists before
/// any is applied: indexes into them, and the panel mode to record with.
#[derive(Default)]
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
