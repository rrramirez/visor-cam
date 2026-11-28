//! `visor <command>`: controls the running Visor from a terminal or an
//! agent, and `visor mcp` offers the same commands to agents over MCP (see
//! `crate::mcp`). Commands go to the running app through
//! `crate::instance`; Visor is started in the background if it isn't
//! running. The app answers only while agent control is on in Preferences.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::instance::{self, ControlError};

const USAGE: &str = "\
usage: visor [--edit <recording folder> | --library | --record [--stop-after SECONDS]]
       visor <command>

Control the running Visor (turn on agent control in Preferences first):
  status                      what Visor is doing
  sources                     displays, windows, apps, cameras and microphones
  list                        recordings in the save folder, newest first
  start [options]             start recording; returns once it's recording
      --name NAME             name the recording (and its folder)
      --screen [DISPLAY]      the whole screen (the main one by default)
      --window WINDOW         one window: its id, or part of \"App — Title\"
      --app APP               one app's windows: its pid or name
      --area X,Y,W,H          an area, in points from the display's top-left
      --display DISPLAY       the display for --area
      --camera CAMERA|none    a camera by name or id
      --mic MIC|none          a microphone by name or id
      --system-audio on|off   record what the computer plays
      --system-audio-app APP  record only this app's sound
      --folder FOLDER         record more onto that stopped recording, at its
                              end, from what it was recorded from
  pause | resume
  mark [TEXT]                 note this moment, shown on the editor's timeline
  stop [--open-editor]        stop and save; prints the recording's folder
  discard                     stop and delete the recording
  mcp                         serve these commands to agents over MCP (stdio)

Edit the recording last stopped, or the one given with --folder FOLDER.
Times are recorded seconds (or M:SS), as marks and status show them:
  remove START END [S E ...]  remove START to END (or several), closing up the space
  trim START END              keep only START to END
  restore                     bring everything back, in recorded order
  open                        open it in the editor
  info                        everything about it: clips, markers, blurs, exports
  split TIME                  cut the clip at TIME in two
  move TIME [--before TIME]   play the clip at TIME before another (or last)
  bring-back TIME             bring back the removed stretch at TIME
  marker add TIME [TEXT] | rename TIME TEXT | delete TIME
  blur add X Y W H [--from S] [--to E]    blur an area (fractions of the frame)
  blur change TIME|--number N [--x .. --y .. --width .. --height .. --from S --to E]
  blur delete TIME|--number N   (N: its place in info's blurs)
  set [--name N] [--speed X] [--thumbnail TIME|first] [--bubble on|off]
      [--bubble-shape circle|rounded] [--bubble-x X] [--bubble-y Y]
      [--bubble-size S] [--mirror on|off]
  export [--format mp4|webm|webm_av1|gif] [--size original|1080p|720p]
         [--reduce-noise on|off] [--even-volume on|off]   waits until it's written
  add PATH...                 add video files (MP4, MOV, M4V, MKV, WebM) or other
                              Visor recordings (their folders) at its end
  frame SECONDS [--out FILE]  save the frame at SECONDS, removed or not, as a JPEG
                              (frame-SECONDS.jpg here by default)
  dead-time [--min SECONDS]   list stretches where the screen is still and
                              nothing is heard (2 seconds or longer)

Replies are JSON. Options left out keep what the recorder panel has.";

/// Runs a command-line command, if `args` is one. Returns the exit code.
pub fn run(args: &[String]) -> Option<i32> {
    let (first, rest) = args.split_first()?;
    let command = match first.as_str() {
        "mcp" => return Some(crate::mcp::serve()),
        "help" => {
            attach_console();
            println!("{USAGE}");
            return Some(0);
        }
        "status" | "sources" | "list" => match command_for(first, rest) {
            Ok(command) => command,
            Err(e) => {
                attach_console();
                eprintln!("visor {first}: {e}\n\n{USAGE}");
                return Some(2);
            }
        },
        "start" => match start_args(rest) {
            Ok(command) => command,
            Err(e) => {
                attach_console();
                eprintln!("visor start: {e}\n\n{USAGE}");
                return Some(2);
            }
        },
        "pause" => json!({"command": "pause_recording"}),
        "resume" => json!({"command": "resume_recording"}),
        "mark" => json!({"command": "add_marker", "text": rest.join(" ")}),
        "stop" => json!({
            "command": "stop_recording",
            "open_editor": rest.iter().any(|a| a == "--open-editor"),
        }),
        "discard" => json!({"command": "discard_recording"}),
        "frame" => {
            attach_console();
            return Some(match frame_args(rest) {
                Ok((command, out)) => save_frame(&command, out),
                Err(e) => {
                    eprintln!("visor frame: {e}\n\n{USAGE}");
                    2
                }
            });
        }
        "add" => match add_args(rest) {
            Ok(command) => command,
            Err(e) => {
                attach_console();
                eprintln!("visor add: {e}\n\n{USAGE}");
                return Some(2);
            }
        },
        "info" | "split" | "move" | "bring-back" | "marker" | "blur" | "set" | "export" => {
            match finish_args(first, rest) {
                Ok(command) => command,
                Err(e) => {
                    attach_console();
                    eprintln!("visor {first}: {e}\n\n{USAGE}");
                    return Some(2);
                }
            }
        }
        "remove" | "trim" | "restore" | "open" | "dead-time" => match edit_args(first, rest) {
            Ok(command) => command,
            Err(e) => {
                attach_console();
                eprintln!("visor {first}: {e}\n\n{USAGE}");
                return Some(2);
            }
        },
        _ => return None,
    };
    attach_console();
    match reply_text(call(&command)) {
        Ok(text) => {
            println!("{text}");
            Some(0)
        }
        Err(error) => {
            eprintln!("visor: {error}");
            Some(1)
        }
    }
}

/// A reply as text: its fields as JSON, or why it failed.
pub fn reply_text(reply: Value) -> Result<String, String> {
    reply_fields(reply).map(|fields| serde_json::to_string_pretty(&fields).unwrap_or_default())
}

/// A reply's fields, or why it failed.
fn reply_fields(mut reply: Value) -> Result<Value, String> {
    if reply["ok"].as_bool() != Some(true) {
        return Err(reply["error"].as_str().unwrap_or("Failed.").to_string());
    }
    if let Some(fields) = reply.as_object_mut() {
        fields.remove("ok");
    }
    Ok(reply)
}

/// Where a reply carries a frame, as a base64 JPEG: the command line saves
/// it, and MCP passes it on as an image.
pub const IMAGE: &str = "image_jpeg";

/// Takes the frame out of a reply, if it has one.
pub fn take_image(reply: &mut Value) -> Option<Value> {
    reply.as_object_mut()?.remove(IMAGE)
}

/// The command for a plain command-line command, one that takes nothing.
fn command_for(name: &str, rest: &[String]) -> Result<Value, String> {
    if !rest.is_empty() {
        return Err("takes no arguments".into());
    }
    Ok(json!({"command": match name {
        "sources" => "list_sources",
        "list" => "list_recordings",
        _ => "status",
    }}))
}

fn start_args(args: &[String]) -> Result<Value, String> {
    let mut command = json!({"command": "start_recording"});
    let mut args = args.iter().peekable();
    while let Some(flag) = args.next() {
        macro_rules! value {
            () => {
                args.next()
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
        }
        match flag.as_str() {
            "--screen" => {
                command["target"] = "screen".into();
                if let Some(display) = args.next_if(|a| !a.starts_with("--")) {
                    command["display"] = display.as_str().into();
                }
            }
            "--window" => {
                command["target"] = "window".into();
                command["window"] = value!()?.into();
            }
            "--app" => {
                command["target"] = "app".into();
                command["app"] = value!()?.into();
            }
            "--area" => {
                let v = value!()?;
                let n: Vec<f64> = v
                    .split(',')
                    .map(|n| n.trim().parse::<f64>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| format!("--area takes X,Y,W,H, not \"{v}\""))?;
                let [x, y, width, height] = n[..] else {
                    return Err(format!("--area takes X,Y,W,H, not \"{v}\""));
                };
                command["target"] = "area".into();
                command["area"] = json!({"x": x, "y": y, "width": width, "height": height});
            }
            "--name" => command["name"] = value!()?.into(),
            "--folder" => command["folder"] = value!()?.into(),
            "--display" => command["display"] = value!()?.into(),
            "--camera" => command["camera"] = value!()?.into(),
            "--mic" => command["mic"] = value!()?.into(),
            "--system-audio" => {
                command["system_audio"] = match value!()?.as_str() {
                    "on" | "true" | "yes" => true,
                    "off" | "false" | "no" => false,
                    other => return Err(format!("--system-audio is on or off, not \"{other}\"")),
                }
                .into();
            }
            "--system-audio-app" => command["system_audio_app"] = value!()?.into(),
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(command)
}

fn edit_args(name: &str, args: &[String]) -> Result<Value, String> {
    let command_name = match name {
        "frame" => "view_frame",
        "dead-time" => "find_dead_time",
        "remove" => "remove_from_recording",
        "trim" => "trim_recording",
        "restore" => "restore_recording",
        _ => "open_recording",
    };
    let mut command = json!({"command": command_name});
    let mut times = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--folder" {
            command["folder"] = args.next().ok_or("--folder needs a value")?.as_str().into();
        } else if arg == "--min" && name == "dead-time" {
            command["min_seconds"] = seconds_arg(args.next().ok_or("--min needs a value")?)?.into();
        } else {
            times.push(seconds_arg(arg)?);
        }
    }
    match (name, times.as_slice()) {
        ("remove" | "trim", [start, end]) => {
            command["start_seconds"] = (*start).into();
            command["end_seconds"] = (*end).into();
        }
        // Several stretches, removed as one step.
        ("remove", pairs) if pairs.len() >= 4 && pairs.len() % 2 == 0 => {
            command["ranges"] = pairs
                .chunks(2)
                .map(|p| json!([p[0], p[1]]))
                .collect::<Vec<_>>()
                .into();
        }
        ("remove", _) => return Err("give a START and an END time, or several pairs".into()),
        ("trim", _) => return Err("give a START and an END time".into()),
        ("frame", [at]) => command["seconds"] = (*at).into(),
        ("frame", _) => return Err("give one time".into()),
        (_, []) => {}
        _ => return Err("takes no times".into()),
    }
    Ok(command)
}

fn add_args(args: &[String]) -> Result<Value, String> {
    let mut command = json!({"command": "add_video"});
    let mut paths = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--folder" {
            command["folder"] = args.next().ok_or("--folder needs a value")?.as_str().into();
        } else {
            paths.push(arg.clone());
        }
    }
    if paths.is_empty() {
        return Err("give the video files or recordings to add".into());
    }
    command["paths"] = paths.into();
    Ok(command)
}

/// The commands that finish a recording off: what `get_recording`,
/// `edit_clips`, `edit_markers`, `edit_blurs`, `set_recording` and
/// `export_recording` take, from the command line.
fn finish_args(name: &str, args: &[String]) -> Result<Value, String> {
    // Words, and --flags with their values.
    let mut words: Vec<&str> = Vec::new();
    let mut flags: Vec<(&str, &str)> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.strip_prefix("--") {
            Some(flag) => {
                let value = it.next().ok_or_else(|| format!("--{flag} needs a value"))?;
                flags.push((flag, value));
            }
            None => words.push(a),
        }
    }
    let flag = |k: &str| flags.iter().rev().find(|(f, _)| *f == k).map(|(_, v)| *v);
    let number = |v: &str, what: &str| {
        v.parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .ok_or_else(|| format!("{what} is a number, not \"{v}\""))
    };
    let on_off = |v: &str, what: &str| match v {
        "on" | "true" | "yes" => Ok(true),
        "off" | "false" | "no" => Ok(false),
        _ => Err(format!("{what} is on or off, not \"{v}\"")),
    };
    let time = |w: Option<&&str>| -> Result<f64, String> { seconds_arg(w.ok_or("give a time")?) };
    let known = |allowed: &[&str]| -> Result<(), String> {
        match flags
            .iter()
            .find(|(f, _)| *f != "folder" && !allowed.contains(f))
        {
            Some((f, _)) => Err(format!("unknown option --{f}")),
            None => Ok(()),
        }
    };
    let mut c = match name {
        "info" => {
            known(&[])?;
            json!({"command": "get_recording"})
        }
        "split" | "bring-back" => {
            known(&[])?;
            let action = if name == "split" {
                "split"
            } else {
                "bring_back"
            };
            json!({"command": "edit_clips", "action": action, "at_seconds": time(words.first())?})
        }
        "move" => {
            known(&["before"])?;
            let mut c = json!({"command": "edit_clips", "action": "move", "at_seconds": time(words.first())?});
            if let Some(b) = flag("before") {
                c["before_seconds"] = seconds_arg(b)?.into();
            }
            c
        }
        "marker" => {
            known(&[])?;
            let action = *words
                .first()
                .ok_or("marker add|rename|delete TIME [TEXT]")?;
            let mut c = json!({"command": "edit_markers", "action": action, "at_seconds": time(words.get(1))?});
            match action {
                "add" | "rename" => {
                    let text = words[2..].join(" ");
                    if action == "rename" && text.is_empty() {
                        return Err("give the marker's new text".into());
                    }
                    c["text"] = text.into();
                }
                "delete" => {}
                _ => return Err("marker add|rename|delete TIME [TEXT]".into()),
            }
            c
        }
        "blur" => {
            known(&["from", "to", "number", "x", "y", "width", "height"])?;
            let action = *words
                .first()
                .ok_or("blur add X Y W H | change TIME | delete TIME")?;
            let mut c = json!({"command": "edit_blurs", "action": action});
            match action {
                "add" => {
                    let [x, y, w, h] = words
                        .get(1..5)
                        .and_then(|a| <[&str; 4]>::try_from(a).ok())
                        .ok_or("blur add X Y WIDTH HEIGHT (fractions of the frame)")?;
                    for (k, v) in [("x", x), ("y", y), ("width", w), ("height", h)] {
                        c[k] = number(v, k)?.into();
                    }
                }
                // A time it covers, or --number alone.
                "change" | "delete" => {
                    if words.get(1).is_some() || flag("number").is_none() {
                        c["at_seconds"] = time(words.get(1))?.into();
                    }
                }
                _ => return Err("blur add X Y W H | change TIME | delete TIME".into()),
            }
            for k in ["x", "y", "width", "height"] {
                if let Some(v) = flag(k) {
                    c[k] = number(v, k)?.into();
                }
            }
            if let Some(n) = flag("number") {
                c["number"] = n.parse::<u64>().map_err(|_| "--number is 1, 2, …")?.into();
            }
            if let Some(v) = flag("from") {
                c["start_seconds"] = seconds_arg(v)?.into();
            }
            if let Some(v) = flag("to") {
                c["end_seconds"] = seconds_arg(v)?.into();
            }
            c
        }
        "set" => {
            known(&[
                "name",
                "speed",
                "thumbnail",
                "bubble",
                "bubble-shape",
                "bubble-x",
                "bubble-y",
                "bubble-size",
                "mirror",
            ])?;
            let mut c = json!({"command": "set_recording"});
            if let Some(n) = flag("name") {
                c["name"] = n.into();
            }
            if let Some(v) = flag("speed") {
                c["speed"] = number(v, "--speed")?.into();
            }
            if let Some(v) = flag("thumbnail") {
                c["thumbnail_seconds"] = if v == "first" {
                    Value::Null
                } else {
                    seconds_arg(v)?.into()
                };
            }
            let mut bubble = serde_json::Map::new();
            if let Some(v) = flag("bubble") {
                bubble.insert("visible".into(), on_off(v, "--bubble")?.into());
            }
            if let Some(v) = flag("bubble-shape") {
                bubble.insert("shape".into(), v.into());
            }
            for (f, k) in [
                ("bubble-x", "x"),
                ("bubble-y", "y"),
                ("bubble-size", "size"),
            ] {
                if let Some(v) = flag(f) {
                    bubble.insert(k.into(), number(v, f)?.into());
                }
            }
            if let Some(v) = flag("mirror") {
                bubble.insert("mirror".into(), on_off(v, "--mirror")?.into());
            }
            if !bubble.is_empty() {
                c["bubble"] = bubble.into();
            }
            if c.as_object().is_some_and(|o| o.len() == 1) {
                return Err("say what to change".into());
            }
            c
        }
        _ => {
            known(&["format", "size", "reduce-noise", "even-volume"])?;
            let mut c = json!({"command": "export_recording"});
            if let Some(v) = flag("format") {
                c["format"] = v.into();
            }
            if let Some(v) = flag("size") {
                c["size"] = v.into();
            }
            if let Some(v) = flag("reduce-noise") {
                c["reduce_noise"] = on_off(v, "--reduce-noise")?.into();
            }
            if let Some(v) = flag("even-volume") {
                c["even_volume"] = on_off(v, "--even-volume")?.into();
            }
            c
        }
    };
    if let Some(f) = flag("folder") {
        c["folder"] = f.into();
    }
    Ok(c)
}

/// `view_frame`'s command, and where to save the frame if not the default.
fn frame_args(args: &[String]) -> Result<(Value, Option<String>), String> {
    let mut rest = Vec::new();
    let mut out = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        if arg == "--out" {
            out = Some(args.next().ok_or("--out needs a value")?.clone());
        } else {
            rest.push(arg.clone());
        }
    }
    Ok((edit_args("frame", &rest)?, out))
}

/// Asks for a frame and saves it; prints the reply with the file in place
/// of the image. Returns the exit code.
fn save_frame(command: &Value, out: Option<String>) -> i32 {
    use base64::prelude::*;
    let mut reply = call(command);
    let image = take_image(&mut reply);
    let saved = reply_fields(reply).and_then(|mut reply| {
        let jpeg = match image {
            Some(Value::String(image)) => BASE64_STANDARD
                .decode(image)
                .map_err(|e| format!("Visor sent an image that can't be read: {e}"))?,
            Some(_) => return Err("Visor sent an image that can't be read.".into()),
            None => return Err("Visor sent no image.".into()),
        };
        let seconds = command["seconds"].as_f64().unwrap_or_default();
        let path = write_frame(out, Path::new("."), seconds, &jpeg)?;
        reply["image"] = path.display().to_string().into();
        Ok(reply)
    });
    match saved {
        Ok(reply) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&reply).unwrap_or_default()
            );
            0
        }
        Err(error) => {
            eprintln!("visor: {error}");
            1
        }
    }
}

/// Saves a frame at `out`, or as frame-SECONDS.jpg in `dir`, numbered
/// rather than replacing one already there. Returns where.
fn write_frame(
    out: Option<String>,
    dir: &Path,
    seconds: f64,
    jpeg: &[u8],
) -> Result<PathBuf, String> {
    let saved = match out {
        Some(out) => std::fs::write(&out, jpeg).map(|()| PathBuf::from(out)),
        None => (1..)
            .map(|n| match n {
                1 => dir.join(format!("frame-{seconds}.jpg")),
                n => dir.join(format!("frame-{seconds} ({n}).jpg")),
            })
            .find_map(|name| {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&name);
                match file {
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => None,
                    file => Some(file.and_then(|mut f| f.write_all(jpeg)).map(|()| name)),
                }
            })
            .expect("names never run out"),
    };
    saved
        .and_then(std::path::absolute)
        .map_err(|e| format!("couldn't save the frame: {e}"))
}

/// Seconds, or minutes and seconds as M:SS (seconds below 60).
fn seconds_arg(s: &str) -> Result<f64, String> {
    let number = |n: &str| {
        n.parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && *n >= 0.0 && !n.is_sign_negative())
    };
    let parsed = match s.split_once(':') {
        Some((m, sec)) => m
            .parse::<u32>()
            .ok()
            .zip(number(sec).filter(|sec| *sec < 60.0))
            .map(|(m, sec)| f64::from(m) * 60.0 + sec),
        None => number(s),
    };
    parsed.ok_or_else(|| format!("\"{s}\" isn't a time (seconds, or M:SS)"))
}

/// Sends a command to the running Visor, starting Visor first for the
/// commands that need it. Always returns a reply object with `ok`.
pub fn call(command: &Value) -> Value {
    let command = &with_absolute_folder(command);
    match instance::control(command) {
        Ok(reply) => reply,
        Err(ControlError::NotRunning) => match command["command"].as_str() {
            Some("start_recording" | "list_sources" | "list_recordings") => {
                launch_and_call(command)
            }
            // A fresh Visor knows no last recording: only a named one helps.
            Some(
                "remove_from_recording"
                | "trim_recording"
                | "restore_recording"
                | "open_recording"
                | "view_frame"
                | "find_dead_time"
                | "add_video"
                | "get_recording"
                | "edit_clips"
                | "edit_markers"
                | "edit_blurs"
                | "set_recording"
                | "export_recording",
            ) if command.get("folder").is_some() => launch_and_call(command),
            Some(
                "remove_from_recording"
                | "trim_recording"
                | "restore_recording"
                | "open_recording"
                | "view_frame"
                | "find_dead_time"
                | "add_video"
                | "get_recording"
                | "edit_clips"
                | "edit_markers"
                | "edit_blurs"
                | "set_recording"
                | "export_recording",
            ) => {
                json!({
                    "ok": false,
                    "error": "Visor isn't running, so it has no last recording; say which with a folder.",
                })
            }
            Some("status") => json!({"ok": true, "state": "not_running"}),
            _ => json!({"ok": false, "error": "Visor isn't running, so it isn't recording."}),
        },
        Err(e) => json!({"ok": false, "error": e.to_string()}),
    }
}

/// `command` with its folder resolved here, since Visor runs in its own
/// directory, a leading `~` expanded, as an agent may write one with no
/// shell to expand it, and a null folder left out, as Visor reads it.
fn with_absolute_folder(command: &Value) -> Value {
    let mut command = command.clone();
    if let Some(args) = command.as_object_mut() {
        match args.get("folder") {
            Some(Value::Null) => {
                args.remove("folder");
            }
            Some(Value::String(folder)) => {
                let folder = absolute(folder);
                args.insert("folder".into(), folder.into());
            }
            _ => {}
        }
        // Videos and recordings to add, likewise.
        if let Some(Value::Array(paths)) = args.get_mut("paths") {
            for p in paths {
                if let Value::String(path) = p {
                    *p = absolute(path).into();
                }
            }
        }
    }
    command
}

/// `path` resolved here, a leading `~` expanded.
fn absolute(path: &str) -> String {
    let home = |rest: &str| dirs::home_dir().map(|h| h.join(rest));
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => home(rest),
        None if path == "~" => home(""),
        None => None,
    }
    .unwrap_or_else(|| path.into());
    std::path::absolute(&expanded)
        .unwrap_or(expanded)
        .display()
        .to_string()
}

fn launch_and_call(command: &Value) -> Value {
    if let Err(e) = launch() {
        return json!({"ok": false, "error": format!("Couldn't start Visor: {e}")});
    }
    // Wait for it to listen. Only a command no Visor took is sent again:
    // once one has it, sending it twice could start two recordings.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        std::thread::sleep(Duration::from_millis(250));
        match instance::control(command) {
            Ok(reply) => return reply,
            Err(ControlError::NotRunning) if Instant::now() > deadline => {
                return json!({
                    "ok": false,
                    "error": "Visor didn't start. If an older Visor without agent control is \
                              running, the user needs to quit it first.",
                });
            }
            Err(ControlError::NotRunning) => {}
            Err(e) => return json!({"ok": false, "error": e.to_string()}),
        }
    }
}

/// Starts Visor in the background (no recorder panel).
fn launch() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    // In the app bundle, through Launch Services, so it runs as the app.
    #[cfg(target_os = "macos")]
    if let Some(bundle) = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
    {
        // Its output would land in the MCP server's JSON stream.
        let status = std::process::Command::new("open")
            .args(["-g", "-a"])
            .arg(bundle)
            .args(["--args", "--background"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        return if status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("open failed ({status})")))
        };
    }
    let mut command = std::process::Command::new(exe);
    command
        .arg("--background")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Not tied to this terminal or agent session.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x8;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().map(drop)
}

/// The command that starts Visor's MCP server, for agents' settings.
pub fn mcp_command() -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "visor".into());
    if exe.contains(' ') {
        format!("\"{exe}\" mcp")
    } else {
        format!("{exe} mcp")
    }
}

/// Release builds on Windows have no console of their own; print to the
/// terminal that ran the command, unless output already goes to a pipe or
/// file (as when an agent runs it).
fn attach_console() {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        if std::io::stdout().as_raw_handle().is_null() {
            use windows::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
            // SAFETY: plain Win32 call; failing just means nowhere to print.
            let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
        }
    }
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split(' ').map(String::from).collect()
    }

    #[test]
    fn frames_are_named_after_their_time_and_never_replaced() {
        let dir = std::env::temp_dir().join(format!("visor-frames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = |seconds| {
            let path = write_frame(None, &dir, seconds, b"jpeg").unwrap();
            path.file_name().unwrap().to_string_lossy().into_owned()
        };
        assert_eq!(name(15.0), "frame-15.jpg");
        assert_eq!(name(15.0), "frame-15 (2).jpg");
        assert_eq!(name(62.5), "frame-62.5.jpg");
        let out = dir.join("chosen.jpg");
        std::fs::write(&out, "old").unwrap();
        let out_name = Some(out.display().to_string());
        assert_eq!(write_frame(out_name, &dir, 1.0, b"new").unwrap(), out);
        assert_eq!(std::fs::read(&out).unwrap(), b"new");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edit_commands_take_times_and_a_folder() {
        assert_eq!(
            edit_args("remove", &args("3 1:10.5 --folder /tmp/Demo")).unwrap(),
            json!({
                "command": "remove_from_recording",
                "start_seconds": 3.0,
                "end_seconds": 70.5,
                "folder": "/tmp/Demo",
            })
        );
        assert_eq!(
            edit_args("open", &[]).unwrap(),
            json!({"command": "open_recording"})
        );
        assert!(edit_args("trim", &args("5")).is_err());
        for bad in ["-0:30", "1:-5", "1:75", "-3", "1:2:3", "x:10"] {
            assert!(seconds_arg(bad).is_err(), "{bad}");
        }
        assert_eq!(seconds_arg("0:59.5"), Ok(59.5));
        let relative = with_absolute_folder(&edit_args("open", &args("--folder Demo")).unwrap());
        assert!(std::path::Path::new(relative["folder"].as_str().unwrap()).is_absolute());
        let null = with_absolute_folder(&json!({"command": "open_recording", "folder": null}));
        assert_eq!(null, json!({"command": "open_recording"}));
        let home = with_absolute_folder(&json!({"folder": "~/Movies/Visor/Demo"}));
        assert_eq!(
            home["folder"].as_str().unwrap(),
            dirs::home_dir()
                .unwrap()
                .join("Movies")
                .join("Visor")
                .join("Demo")
                .display()
                .to_string()
        );
        let (frame, out) = frame_args(&args("1:02.5 --out f.jpg --folder /tmp/Demo")).unwrap();
        assert_eq!(
            frame,
            json!({"command": "view_frame", "seconds": 62.5, "folder": "/tmp/Demo"})
        );
        assert_eq!(out.as_deref(), Some("f.jpg"));
        assert!(frame_args(&args("1 2")).is_err());
        assert!(frame_args(&[]).is_err());
        assert_eq!(
            edit_args("dead-time", &args("--min 1.5")).unwrap(),
            json!({"command": "find_dead_time", "min_seconds": 1.5})
        );
        assert!(edit_args("remove", &args("--min 1 2 3")).is_err());
        assert!(edit_args("remove", &args("a b")).is_err());
        assert!(edit_args("restore", &args("5")).is_err());
    }

    #[test]
    fn finishing_commands_become_agent_tools() {
        let c = |line: &str| {
            let words = args(line);
            finish_args(&words[0], &words[1..])
        };
        assert_eq!(
            c("info --folder /tmp/D").unwrap(),
            json!({"command": "get_recording", "folder": "/tmp/D"})
        );
        assert_eq!(
            c("split 1:02").unwrap(),
            json!({"command": "edit_clips", "action": "split", "at_seconds": 62.0})
        );
        assert_eq!(
            c("move 30 --before 4").unwrap(),
            json!({"command": "edit_clips", "action": "move", "at_seconds": 30.0, "before_seconds": 4.0})
        );
        assert_eq!(
            c("bring-back 12").unwrap(),
            json!({"command": "edit_clips", "action": "bring_back", "at_seconds": 12.0})
        );
        assert_eq!(
            c("marker add 5 Open the settings").unwrap(),
            json!({"command": "edit_markers", "action": "add", "at_seconds": 5.0, "text": "Open the settings"})
        );
        assert_eq!(
            c("marker delete 5").unwrap(),
            json!({"command": "edit_markers", "action": "delete", "at_seconds": 5.0})
        );
        assert_eq!(
            c("blur add 0.1 0.2 0.3 0.4 --from 2 --to 9").unwrap(),
            json!({"command": "edit_blurs", "action": "add", "x": 0.1, "y": 0.2, "width": 0.3,
                   "height": 0.4, "start_seconds": 2.0, "end_seconds": 9.0})
        );
        assert_eq!(
            c("blur change 3 --number 2 --x 0.5 --to 8").unwrap(),
            json!({"command": "edit_blurs", "action": "change", "at_seconds": 3.0, "number": 2,
                   "x": 0.5, "end_seconds": 8.0})
        );
        assert_eq!(
            c("set --name Tour --speed 1.5 --thumbnail first --bubble off --bubble-shape rounded --bubble-size 0.25").unwrap(),
            json!({"command": "set_recording", "name": "Tour", "speed": 1.5, "thumbnail_seconds": null,
                   "bubble": {"visible": false, "shape": "rounded", "size": 0.25}})
        );
        assert_eq!(
            c("export --format webm --size 720p --reduce-noise on").unwrap(),
            json!({"command": "export_recording", "format": "webm", "size": "720p", "reduce_noise": true})
        );
        assert!(c("split").is_err());
        assert!(c("marker rename 5").is_err());
        assert!(c("blur add 0.1 0.2").is_err());
        assert!(c("set").is_err());
        // Several stretches to remove at once.
        assert_eq!(
            edit_args("remove", &args("2 3 5 6.5")).unwrap(),
            json!({"command": "remove_from_recording", "ranges": [[2.0, 3.0], [5.0, 6.5]]})
        );
        assert!(edit_args("remove", &args("2 3 5")).is_err());
    }

    #[test]
    fn list_asks_for_the_recordings() {
        assert_eq!(
            command_for("list", &[]).unwrap(),
            json!({"command": "list_recordings"})
        );
    }

    #[test]
    fn add_takes_videos_and_recordings_to_add() {
        assert_eq!(
            add_args(&args("intro.mp4 /tmp/Other --folder /tmp/Demo")).unwrap(),
            json!({
                "command": "add_video",
                "paths": ["intro.mp4", "/tmp/Other"],
                "folder": "/tmp/Demo",
            })
        );
        assert!(add_args(&args("--folder /tmp/Demo")).is_err());
        // Resolved here, as Visor runs in its own directory.
        let command = with_absolute_folder(&add_args(&args("intro.mp4 ~/Movies/a.mov")).unwrap());
        let paths: Vec<&str> = command["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect();
        assert!(paths.iter().all(|p| std::path::Path::new(p).is_absolute()));
        assert!(paths[1].ends_with("Movies/a.mov") && !paths[1].contains('~'));
    }

    #[test]
    fn start_options_become_command_arguments() {
        let c = start_args(&args("--window Safari --camera none --system-audio on")).unwrap();
        assert_eq!(
            c,
            json!({
                "command": "start_recording",
                "target": "window",
                "window": "Safari",
                "camera": "none",
                "system_audio": true,
            })
        );
        let c = start_args(&args("--screen --mic MacBook --name Demo")).unwrap();
        assert_eq!(c["name"], "Demo");
        assert_eq!(c["target"], "screen");
        assert!(c.get("display").is_none());
        assert_eq!(c["mic"], "MacBook");
        let c = start_args(&args("--area 10,20,640,480 --display 2")).unwrap();
        assert_eq!(
            c["area"],
            json!({"x": 10.0, "y": 20.0, "width": 640.0, "height": 480.0})
        );
        assert_eq!(c["display"], "2");
        assert!(start_args(&args("--area 1,2,3")).is_err());
        assert!(start_args(&args("--window")).is_err());
        assert!(start_args(&args("--bogus")).is_err());
        // Recording more onto a stopped recording.
        let c = start_args(&args("--folder Demo --camera none")).unwrap();
        assert_eq!(c["folder"], "Demo");
        assert!(start_args(&args("--folder")).is_err());
    }
}
