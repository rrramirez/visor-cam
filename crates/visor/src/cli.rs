//! `visor <command>`: controls the running Visor from a terminal or an
//! agent, and `visor mcp` offers the same commands to agents over MCP (see
//! `crate::mcp`). Commands go to the running app through
//! `crate::instance`; Visor is started in the background if it isn't
//! running. The app answers only while agent control is on in Preferences.

use std::io::Write;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::instance::{self, ControlError};

const USAGE: &str = "\
usage: visor [--edit <recording folder> | --record [--stop-after SECONDS]]
       visor <command>

Control the running Visor (turn on agent control in Preferences first):
  status                      what Visor is doing
  sources                     displays, windows, apps, cameras and microphones
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
  pause | resume
  mark [TEXT]                 note this moment, shown on the editor's timeline
  stop [--open-editor]        stop and save; prints the recording's folder
  discard                     stop and delete the recording
  mcp                         serve these commands to agents over MCP (stdio)

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
        "status" => json!({"command": "status"}),
        "sources" => json!({"command": "list_sources"}),
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
        _ => return None,
    };
    attach_console();
    let mut reply = call(&command);
    let ok = reply["ok"].as_bool().unwrap_or(false);
    if ok {
        if let Some(fields) = reply.as_object_mut() {
            fields.remove("ok");
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&reply).unwrap_or_default()
        );
        Some(0)
    } else {
        eprintln!("visor: {}", reply["error"].as_str().unwrap_or("failed"));
        Some(1)
    }
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

/// Sends a command to the running Visor, starting Visor first if needed
/// (except to ask its status). Always returns a reply object with `ok`.
pub fn call(command: &Value) -> Value {
    match instance::control(command) {
        Ok(reply) => reply,
        Err(ControlError::NotRunning) if command["command"] == "status" => {
            json!({"ok": true, "state": "not_running"})
        }
        Err(ControlError::NotRunning) => {
            if let Err(e) = launch() {
                return json!({"ok": false, "error": format!("Couldn't start Visor: {e}")});
            }
            // Wait for it to listen.
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                std::thread::sleep(Duration::from_millis(250));
                match instance::control(command) {
                    Ok(reply) => return reply,
                    Err(e) if Instant::now() > deadline => {
                        return json!({"ok": false, "error": format!("Visor didn't start: {e}")});
                    }
                    Err(_) => {}
                }
            }
        }
        Err(e) => json!({"ok": false, "error": e.to_string()}),
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
        let status = std::process::Command::new("open")
            .args(["-g", "-a"])
            .arg(bundle)
            .args(["--args", "--background"])
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
    }
}
