//! Agent control: commands from the `visor` command line and its MCP server
//! (see `crate::cli`), which reach the running app through
//! `crate::instance`. Off unless the user turns it on in Preferences.
//!
//! A command is a JSON object whose `command` names what to do; the other
//! fields are its arguments. Replies are JSON objects with `ok` and either
//! the result's fields or an `error` for the agent to show or act on.

use std::time::Instant;

use serde_json::{Value, json};

use super::*;

pub type Reply = crossbeam_channel::Sender<Value>;

/// What agents are waiting for.
#[derive(Default)]
pub(super) struct AgentState {
    /// The current recording was started by an agent; the control bar says so.
    pub recording: bool,
    start: Option<(Reply, Timer)>,
    /// Whether to open the editor once it's saved.
    stop: Option<(Reply, bool)>,
    discard: Option<Reply>,
    /// Name for the recording being started; taken by `start_recording`.
    pub name: Option<String>,
    /// The current recording was named by the agent.
    pub named: bool,
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

fn window_label(w: &visor_capture::WindowInfo) -> String {
    format!("{} — {}", w.app, w.title)
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
        {
            let mut agent = self.agent.borrow_mut();
            agent.recording = true;
            agent.named = name.is_some();
            agent.name = name;
        }
        self.record_when_ready(None);
        let started = Instant::now();
        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
            app().agent_watch_start(started)
        });
        self.agent.borrow_mut().start = Some((reply, timer));
    }

    fn agent_watch_start(&self, started: Instant) {
        let outcome = match &*self.session.borrow() {
            Session::Recording { .. } => Ok(()),
            Session::Idle if self.pending_start.borrow().is_none() => {
                let error = self
                    .panel
                    .borrow()
                    .as_ref()
                    .map(|p| p.ui.get_error().to_string())
                    .filter(|e| !e.is_empty());
                Err(error.unwrap_or_else(|| "The recording was cancelled.".into()))
            }
            Session::Stopping => Err("The recording was stopped as it began.".into()),
            _ if started.elapsed() > START_TIMEOUT => {
                Err("The recording didn't start within a minute.".into())
            }
            _ => return,
        };
        let Some((reply, timer)) = self.agent.borrow_mut().start.take() else {
            return;
        };
        timer.stop();
        // Not dropped here: this runs inside the timer's own callback.
        slint::Timer::single_shot(Duration::ZERO, move || drop(timer));
        match outcome {
            Ok(()) => send(reply, Ok(self.agent_status())),
            Err(e) => {
                let mut agent = self.agent.borrow_mut();
                agent.recording = false;
                agent.name = None;
                drop(agent);
                send(reply, Err(e));
            }
        }
    }

    /// Sets the recorder panel's choices from a start command's arguments.
    /// Missing arguments keep what the panel has.
    fn apply_agent_choices(&self, args: &Value) -> Result<(), String> {
        let mut area = None;
        {
            let panel = self.panel.borrow();
            let p = panel
                .as_ref()
                .ok_or("The recorder panel didn't open.".to_string())?;
            let (ui, sources) = (&p.ui, &p.sources);
            if let Some(query) = arg(args, "display") {
                let i = find(
                    "display",
                    &sources.displays,
                    query,
                    |d| d.id.to_string(),
                    |d| d.name.clone(),
                )?;
                ui.set_display_index(i as i32);
            }
            match arg(args, "target").map(|t| t.as_str().unwrap_or_default()) {
                None => {}
                Some("screen") => ui.set_mode(0),
                Some("window") => {
                    let query = arg(args, "window")
                        .ok_or("Say which window to record (list_sources shows them).")?;
                    let i = find(
                        "window",
                        &sources.windows,
                        query,
                        |w| w.id.to_string(),
                        window_label,
                    )?;
                    ui.set_window_index(i as i32);
                    ui.set_mode(1);
                }
                Some("app") => {
                    let query = arg(args, "app")
                        .ok_or("Say which app to record (list_sources shows them).")?;
                    let i = find(
                        "app",
                        &sources.apps,
                        query,
                        |a| a.pid.to_string(),
                        |a| a.name.clone(),
                    )?;
                    ui.set_app_index(i as i32);
                    ui.set_mode(2);
                }
                Some("area") => {
                    let display = usize::try_from(ui.get_display_index())
                        .ok()
                        .and_then(|i| sources.displays.get(i))
                        .or(sources.displays.first())
                        .ok_or("No display available to record.")?;
                    let rect = area_arg(args, display)?;
                    area = Some((display.id, rect));
                    ui.set_mode(3);
                }
                Some(other) => {
                    return Err(format!(
                        "Unknown target \"{other}\": use screen, window, app or area."
                    ));
                }
            }
            if let Some(on) = arg(args, "system_audio") {
                ui.set_system_audio(on.as_bool().ok_or("system_audio is true or false.")?);
            }
            if let Some(query) = arg(args, "system_audio_app") {
                let i = find(
                    "app",
                    &sources.apps,
                    query,
                    |a| a.pid.to_string(),
                    |a| a.name.clone(),
                )?;
                ui.set_system_audio(true);
                // Index 0 is "All apps".
                ui.set_audio_app_index(i as i32 + 1);
            }
        }
        if let Some((display, rect)) = area
            && let Some(p) = &mut *self.panel.borrow_mut()
        {
            p.area = Some((display, rect));
            p.ui.set_area_label(area_label(rect).into());
        }
        if let Some(query) = arg(args, "camera") {
            let index = self.device_index("camera", query, |p| {
                p.cameras
                    .iter()
                    .map(|c| (c.id.clone(), c.name.clone()))
                    .collect()
            })?;
            if let Some(p) = &*self.panel.borrow() {
                p.ui.set_camera_index(index as i32);
            }
            self.choose_camera(index);
            let d = self.devices.borrow();
            if index > 0 && d.camera.is_none() && d.camera_pending.is_none() {
                return Err(self.device_error("camera"));
            }
        }
        if let Some(query) = arg(args, "mic") {
            let index = self.device_index("microphone", query, |p| {
                p.mics
                    .iter()
                    .map(|m| (m.id.clone(), m.name.clone()))
                    .collect()
            })?;
            if let Some(p) = &*self.panel.borrow() {
                p.ui.set_mic_index(index as i32);
            }
            self.choose_mic(index);
            if index > 0 && self.devices.borrow().mic.is_none() {
                return Err(self.device_error("microphone"));
            }
        }
        self.update_hint();
        self.update_screen_highlight();
        Ok(())
    }

    /// The panel's index for a camera or mic argument: 0 is none.
    fn device_index(
        &self,
        what: &str,
        query: &Value,
        devices: impl Fn(&Panel) -> Vec<(String, String)>,
    ) -> Result<usize, String> {
        if query == &Value::Bool(false) || query.as_str().is_some_and(|s| s == "none") {
            return Ok(0);
        }
        let panel = self.panel.borrow();
        let p = panel
            .as_ref()
            .ok_or("The recorder panel didn't open.".to_string())?;
        let list = devices(p);
        find(what, &list, query, |d| d.0.clone(), |d| d.1.clone()).map(|i| i + 1)
    }

    fn device_error(&self, what: &str) -> String {
        let shown = self
            .panel
            .borrow()
            .as_ref()
            .map(|p| p.ui.get_error().to_string())
            .filter(|e| !e.is_empty());
        shown.unwrap_or_else(|| {
            format!(
                "Visor is asking the user for access to the {what}. Once they allow it, try again."
            )
        })
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
        if self.cancel_pending_start() {
            self.agent.borrow_mut().recording = false;
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
    pub(super) fn agent_stopped(&self, result: &anyhow::Result<PathBuf>) -> AfterStop {
        let mut agent = self.agent.borrow_mut();
        agent.recording = false;
        let named = std::mem::take(&mut agent.named);
        let editor = if named {
            AfterStop::Edit
        } else {
            AfterStop::Name
        };
        let Some((reply, open_editor)) = agent.stop.take() else {
            // Stopped by the user.
            return editor;
        };
        drop(agent);
        send(
            reply,
            match result {
                Ok(dir) => {
                    let project = visor_core::Project::load(dir).ok();
                    Ok(json!({
                        "name": project.as_ref().map(|p| p.title.clone()),
                        "folder": dir.display().to_string(),
                        "duration_seconds": project.as_ref().map(|p| seconds(p.duration_us)),
                        "markers": project.map(|p| p.markers.iter().map(|m| json!({
                            "at_seconds": seconds(m.at_us),
                            "text": m.text,
                        })).collect::<Vec<_>>()),
                        "editor_opened": open_editor,
                    }))
                }
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
        let mut agent = self.agent.borrow_mut();
        agent.recording = false;
        agent.named = false;
        let Some(reply) = agent.discard.take() else {
            return true;
        };
        drop(agent);
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
}
