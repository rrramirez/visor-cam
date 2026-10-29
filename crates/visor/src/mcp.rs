//! `visor mcp`: a Model Context Protocol server on stdin/stdout, so agents
//! (Claude Code, Claude Desktop, Cursor…) can record the screen for the
//! user. Each tool is an agent command (see `app::agent`), sent to the
//! running Visor like the command line's (see `crate::cli`).

use std::io::{BufRead, Write};

use serde_json::{Value, json};

/// The protocol versions this server speaks (it only uses tools, which
/// they all share), newest first. A client asking for another gets the
/// newest, as the protocol says.
const PROTOCOL_VERSIONS: [&str; 4] = ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "\
Visor records the user's screen, camera and microphone, offline, on their computer. \
A typical demo: list_sources to see what can be recorded, start_recording with a \
name for the video, then do the demo, calling add_marker at each step, and finally \
stop_recording. Keep the video free of dead time: call pause_recording before \
setting up something that shouldn't be seen, and as soon as a step fails or you \
need to retry or think, then resume_recording once the next step is ready to show. \
After stopping, cut_recording removes stretches that are still dull or went wrong \
(marker times help find them; view_frame shows what was on screen at a moment, \
to check before cutting), and open_recording hands the result to the user. \
Visor's own windows are never recorded, but your terminal or chat window is if it's \
on the recorded screen: record a single window or app to keep it out. \
The user must turn on agent control in Visor's Preferences first; if a tool says it \
is off, ask them to.";

/// The tools, built once.
fn tools() -> &'static Value {
    static TOOLS: std::sync::LazyLock<Value> = std::sync::LazyLock::new(tool_list);
    &TOOLS
}

fn tool_list() -> Value {
    let device = |what: &str| {
        json!({
            "type": ["string", "integer", "boolean"],
            "description": format!(
                "The {what}'s name (or part of it) or id from list_sources; \"none\" or false for no {what}; true for one (the user's last choice, else the first). Left out: the user's last choice, which may be none."
            ),
        })
    };
    json!([
        {
            "name": "status",
            "description": "What Visor is doing: idle, starting, recording, paused or saving (or not_running), with the recorded time so far.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "list_sources",
            "description": "Lists the displays, windows, apps, cameras and microphones that start_recording can use.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "start_recording",
            "description": "Starts recording and returns once it is recording, after the user's countdown (3 seconds unless they changed it). Arguments left out keep what Visor's recorder panel has chosen.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "The recording's name, e.g. \"Creating a GitHub repository\". Its folder and exports are named after it. Left out: the date and time, and the user is asked for a name when it opens in the editor.",
                    },
                    "target": {
                        "type": "string",
                        "enum": ["screen", "window", "app", "area"],
                        "description": "What to record: a whole screen, one window, all windows of one app, or an area of a screen.",
                    },
                    "display": {
                        "type": ["string", "integer"],
                        "description": "For screen and area: the display's name or id. Left out: the one chosen in the panel (the main display at first).",
                    },
                    "window": {
                        "type": ["string", "integer"],
                        "description": "For target window: the window's id, or part of \"App — Title\".",
                    },
                    "app": {
                        "type": ["string", "integer"],
                        "description": "For target app: the app's name or pid.",
                    },
                    "area": {
                        "type": "object",
                        "description": "For target area: in points from the display's top-left corner.",
                        "properties": {
                            "x": {"type": "number"},
                            "y": {"type": "number"},
                            "width": {"type": "number"},
                            "height": {"type": "number"},
                        },
                        "required": ["x", "y", "width", "height"],
                    },
                    "camera": device("camera"),
                    "mic": device("microphone"),
                    "system_audio": {
                        "type": "boolean",
                        "description": "Also record what the computer plays.",
                    },
                    "system_audio_app": {
                        "type": ["string", "integer"],
                        "description": "Record only this app's sound (name or pid); turns system_audio on.",
                    },
                },
            },
        },
        {
            "name": "pause_recording",
            "description": "Pauses the recording; nothing is recorded until resume_recording. Pause whenever nothing worth watching is happening: while setting up, retrying a failed step, or waiting.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "resume_recording",
            "description": "Resumes a paused recording.",
            "inputSchema": {"type": "object", "properties": {}},
        },
        {
            "name": "add_marker",
            "description": "Notes this moment of the recording, e.g. \"Step 2: open settings\". Markers show on the editor's timeline.",
            "inputSchema": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
            },
        },
        {
            "name": "stop_recording",
            "description": "Stops and saves the recording. Returns its name, folder, duration and markers.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "open_editor": {
                        "type": "boolean",
                        "description": "Open Visor's editor so the user can trim and export it. Default false.",
                    },
                },
            },
        },
        {
            "name": "cut_recording",
            "description": "Removes part of a recording. Times are in recorded seconds, as add_marker and status report them, and earlier cuts don't shift them. Edits the recording last stopped unless folder is given. If it's open in Visor's editor the cut shows there and the user can undo it. Returns what is kept.",
            "inputSchema": edit_schema(),
        },
        {
            "name": "trim_recording",
            "description": "Keeps only the part of a recording between start_seconds and end_seconds (recorded seconds, like cut_recording), dropping the rest.",
            "inputSchema": edit_schema(),
        },
        {
            "name": "restore_recording",
            "description": "Undoes all cuts and trims of a recording.",
            "inputSchema": {"type": "object", "properties": {"folder": folder_schema()}},
        },
        {
            "name": "view_frame",
            "description": "Shows the frame of a recording at a moment, as the exported video would (camera bubble included), even if that moment was cut. Use it to see what happened around a marker, or to check a stretch before cutting it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "seconds": {
                        "type": "number",
                        "description": "When, in recorded seconds, as add_marker and cut_recording use them.",
                    },
                    "folder": folder_schema(),
                },
                "required": ["seconds"],
            },
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "open_recording",
            "description": "Opens a recording in Visor's editor, for the user to review, trim and export it.",
            "inputSchema": {"type": "object", "properties": {"folder": folder_schema()}},
        },
        {
            "name": "discard_recording",
            "description": "Stops the recording and deletes it.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"destructiveHint": true},
        },
    ])
}

fn folder_schema() -> Value {
    json!({
        "type": "string",
        "description": "The recording's folder, as stop_recording returned it: an absolute path (or starting with ~/). Left out: the recording last stopped.",
    })
}

fn edit_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "start_seconds": {"type": "number"},
            "end_seconds": {"type": "number"},
            "folder": folder_schema(),
        },
        "required": ["start_seconds", "end_seconds"],
    })
}

/// Serves requests from stdin until it closes. Returns the exit code.
///
/// Each message is answered on its own thread: starting and stopping wait
/// for the recording, and meanwhile the client's pings must still be
/// answered or it may think the server hung.
pub fn serve() -> i32 {
    let out = std::sync::Arc::new(std::sync::Mutex::new(std::io::stdout()));
    let mut running = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let out = out.clone();
        running.retain(|t: &std::thread::JoinHandle<()>| !t.is_finished());
        running.push(std::thread::spawn(move || {
            if let Some(response) = respond(&line) {
                let mut out = out.lock().unwrap_or_else(|e| e.into_inner());
                let _ = writeln!(out, "{response}").and_then(|()| out.flush());
            }
        }));
    }
    // The client stopped sending, but may still read: finish what it asked
    // for (a stop saving the recording) and answer.
    for t in running {
        let _ = t.join();
    }
    0
}

/// The response to one JSON-RPC message; none for notifications.
fn respond(line: &str) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(m) => m,
        Err(_) => {
            return Some(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32700, "message": "Parse error"},
            }));
        }
    };
    let id = message.get("id").filter(|id| !id.is_null())?.clone();
    let params = &message["params"];
    let result = match message["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"]
                .as_str()
                .and_then(|v| PROTOCOL_VERSIONS.into_iter().find(|known| *known == v))
                .unwrap_or(PROTOCOL_VERSIONS[0]),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "visor", "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call_tool(params),
        _ => Err((-32601, "Method not found")),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": code, "message": message},
        }),
    })
}

fn call_tool(params: &Value) -> Result<Value, (i64, &'static str)> {
    let name = params["name"].as_str().unwrap_or_default();
    let known = tools()
        .as_array()
        .is_some_and(|tools| tools.iter().any(|t| t["name"] == name));
    if !known {
        return Err((-32602, "Unknown tool"));
    }
    let mut command = match &params["arguments"] {
        Value::Object(args) => Value::Object(args.clone()),
        _ => json!({}),
    };
    command["command"] = name.into();
    let mut reply = crate::cli::call(&command);
    // A frame goes to the agent as an image, with the rest as text.
    let image = reply.as_object_mut().and_then(|r| r.remove("image_jpeg"));
    let (text, is_error) = match crate::cli::reply_text(reply) {
        Ok(text) => (text, false),
        Err(error) => (error, true),
    };
    let mut content = vec![json!({"type": "text", "text": text})];
    if let Some(data) = image.filter(|_| !is_error) {
        content.push(json!({"type": "image", "data": data, "mimeType": "image/jpeg"}));
    }
    Ok(json!({"content": content, "isError": is_error}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_the_handshake_and_lists_tools() {
        let init = respond(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#,
        )
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        let future = respond(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2099-01-01"}}"#,
        )
        .unwrap();
        assert_eq!(future["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert_eq!(init["result"]["serverInfo"]["name"], "visor");
        assert!(respond(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        let list = respond(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#).unwrap();
        let names: Vec<_> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"start_recording") && names.contains(&"add_marker"));
        let unknown = respond(r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
        let bad_tool =
            respond(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"rm"}}"#)
                .unwrap();
        assert_eq!(bad_tool["error"]["code"], -32602);
    }
}
