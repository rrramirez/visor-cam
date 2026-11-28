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
After stopping, find_dead_time lists stretches where nothing happens, and \
remove_from_recording removes those and others that went wrong (marker times help find \
them; view_frame shows what was on screen at a moment, to check before removing it); \
then open_recording hands the result to the user. \
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
                    "folder": {
                        "type": "string",
                        "description": "Record more onto this stopped recording (its folder, as stop_recording returns it) instead of starting a new one: what's recorded is added at its end as another clip, from what it was recorded from unless other arguments say otherwise. stop_recording then returns the whole recording.",
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
            "name": "remove_from_recording",
            "description": "Removes part of a recording, closing up the space it leaves. Times are in recorded seconds, as add_marker and status report them, and earlier removals don't shift them. Edits the recording last stopped unless folder is given. If it's open in Visor's editor the change shows there and the user can undo it or bring the part back. Anything under 0.25 s it leaves kept between it and another removed part, or an end, is removed too, unless that is all that's left. Returns what is kept: the clips in the order they play, which the user may have changed in the editor.",
            "inputSchema": edit_schema(),
        },
        {
            "name": "trim_recording",
            "description": "Keeps only the part of a recording between start_seconds and end_seconds (recorded seconds, like remove_from_recording), removing the rest. Like remove_from_recording, it also removes anything under 0.25 s it leaves kept next to another removed part.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "start_seconds": {"type": "number"},
                    "end_seconds": {"type": "number"},
                    "folder": folder_schema(),
                },
                "required": ["start_seconds", "end_seconds"],
            },
        },
        {
            "name": "restore_recording",
            "description": "Brings back everything removed or trimmed from a recording, and plays it in the order recorded again, as one clip.",
            "inputSchema": {"type": "object", "properties": {"folder": folder_schema()}},
        },
        {
            "name": "view_frame",
            "description": "Shows the frame of a recording at a moment, as the exported video would (camera bubble included), even if that moment was removed. Use it to see what happened around a marker, or to check a stretch before removing it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "seconds": {
                        "type": "number",
                        "description": "When, in recorded seconds, as add_marker and remove_from_recording use them.",
                    },
                    "folder": folder_schema(),
                },
                "required": ["seconds"],
            },
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "find_dead_time",
            "description": "Lists the stretches of a recording where the screen stays still and nothing is heard, in recorded seconds, ready for remove_from_recording. Each leaves a moment on either side of what happens. Takes about a second per minute of recording.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "min_seconds": {
                        "type": "number",
                        "description": "The shortest stretch to list. Default 2.",
                    },
                    "folder": folder_schema(),
                },
            },
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "list_recordings",
            "description": "Lists the recordings in Visor's recordings folder, newest first: each one's name, folder (for the other tools' folder argument), when it was recorded, its recorded and video seconds, and how many markers it has.",
            "inputSchema": {"type": "object", "properties": {}},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "add_video",
            "description": "Adds video files (MP4, MOV, M4V, MKV or WebM) or other Visor recordings (their folders) at the end of a stopped recording, in the order given, as clips the user can move. A Visor recording comes as it was edited, with its markers and blurs. The files are copied into the recording. If it's open in Visor's editor the change shows there and the user can undo it. Returns where each landed, in recorded seconds, and the clips kept.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Absolute paths (or starting with ~/) of the video files or recording folders to add.",
                    },
                    "folder": folder_schema(),
                },
                "required": ["paths"],
            },
        },
        {
            "name": "create_recording",
            "description": "Starts a new recording from video files (MP4, MOV, M4V, MKV or WebM, e.g. a screen recording made on a phone), without recording: they're copied in as its clips, in the order given, and it's named name (else after the first file). Then edit it like any recording, or open it for the user. Returns it as get_recording does; edits default to it from then on.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Absolute paths (or starting with ~/) of the video files.",
                    },
                    "name": {"type": "string"},
                },
                "required": ["paths"],
            },
        },
        {
            "name": "get_recording",
            "description": "Everything about a recording, to find your way around it before changing it: its name, lengths, speed and frame size; its clips in the order they play (recorded start and end seconds); what's removed and which clip it comes back next to; markers; blurs (x, y, width, height as fractions of the frame); the camera bubble; the thumbnail frame; its exports; and whether it's open in the editor. Times are recorded seconds, as all tools use them.",
            "inputSchema": {"type": "object", "properties": {"folder": folder_schema()}},
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "edit_clips",
            "description": "Changes a recording's clips, by recorded time: split cuts the clip playing at at_seconds in two there; move plays the clip at at_seconds just before the clip at before_seconds (or last, without before_seconds); bring_back puts back the removed stretch at at_seconds, next to the clip it was removed from. Returns the recording as get_recording does. Shows in the editor if it's open, where the user can undo it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["split", "move", "bring_back"]},
                    "at_seconds": {"type": "number"},
                    "before_seconds": {"type": "number", "description": "move only: the clip to play before. Left out: last."},
                    "folder": folder_schema(),
                },
                "required": ["action", "at_seconds"],
            },
        },
        {
            "name": "edit_markers",
            "description": "Adds, renames or deletes a marker of a stopped recording (add_marker is for while recording). add puts one at at_seconds with text; rename and delete act on the marker nearest at_seconds within 0.5 s. Markers become chapters in MP4 and WebM exports. Returns the recording as get_recording does.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "rename", "delete"]},
                    "at_seconds": {"type": "number"},
                    "text": {"type": "string"},
                    "folder": folder_schema(),
                },
                "required": ["action", "at_seconds"],
            },
        },
        {
            "name": "edit_blurs",
            "description": "Blurs an area of the screen so it can't be read (a password, an email address), or changes or deletes a blur. The area is x, y, width, height as fractions of the frame (0 to 1, from the top left), as view_frame's image shows it; the camera bubble stays on top. add needs the area, and lasts start_seconds to end_seconds (default the whole recording). change and delete act on blur number (1, 2, … its place in get_recording's blurs), or on the one blur covering at_seconds; given both, the blur must cover that time. change sets only what's given. Returns the recording as get_recording does.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "change", "delete"]},
                    "x": {"type": "number"},
                    "y": {"type": "number"},
                    "width": {"type": "number"},
                    "height": {"type": "number"},
                    "start_seconds": {"type": "number"},
                    "end_seconds": {"type": "number"},
                    "at_seconds": {"type": "number", "description": "change and delete: a moment the blur covers."},
                    "number": {"type": "integer", "minimum": 1},
                    "folder": folder_schema(),
                },
                "required": ["action"],
            },
        },
        {
            "name": "edit_voiceovers",
            "description": "Deletes a voiceover: voice and/or camera the user recorded over the video while it played. number is its place in get_recording's voiceovers (1, 2, …). Agents can't record voiceovers. Returns the recording as get_recording does.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["delete"]},
                    "number": {"type": "integer", "minimum": 1},
                    "folder": folder_schema(),
                },
                "required": ["action", "number"],
            },
        },
        {
            "name": "set_recording",
            "description": "Sets a recording's name (its folder follows), speed (0.5 to 2; speech keeps its pitch), thumbnail frame (thumbnail_seconds, a kept moment; null for the first kept frame) and camera bubble (visible; shape circle or rounded; x and y of its top-left corner as fractions of the frame's width and height; size as a fraction of the frame's width; mirror). Only what's given changes. Returns the recording as get_recording does.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "speed": {"type": "number", "minimum": 0.5, "maximum": 2},
                    "thumbnail_seconds": {"type": ["number", "null"]},
                    "bubble": {
                        "type": "object",
                        "properties": {
                            "visible": {"type": "boolean"},
                            "shape": {"type": "string", "enum": ["circle", "rounded"]},
                            "x": {"type": "number"},
                            "y": {"type": "number"},
                            "size": {"type": "number"},
                            "mirror": {"type": "boolean"},
                        },
                    },
                    "folder": folder_schema(),
                },
            },
        },
        {
            "name": "export_recording",
            "description": "Exports a recording, as edited, to its exports folder, and returns the file once it's written. Options left out are the user's last export choices. MP4 and WebM exports get a chapter per marker, and MP4s carry the thumbnail as their cover. If the recording is open in the editor the export runs there, where the user sees it and can cancel it. A long export may outlast this call; it carries on, and get_recording lists the file with in_progress until it's written.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "format": {"type": "string", "enum": ["mp4", "webm", "webm_av1", "gif"]},
                    "size": {"type": "string", "enum": ["original", "1080p", "720p"]},
                    "reduce_noise": {"type": "boolean"},
                    "even_volume": {"type": "boolean"},
                    "folder": folder_schema(),
                },
            },
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
            "ranges": {
                "type": "array",
                "items": {"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 2},
                "description": "remove_from_recording only: several [start_seconds, end_seconds] stretches, removed in one step (one undo in the editor), in place of start_seconds and end_seconds.",
            },
            "folder": folder_schema(),
        },
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
    let image = crate::cli::take_image(&mut reply);
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
