# Agent tools

Agents control Visor through MCP tools (`visor mcp`, `crates/visor/src/mcp.rs`) or the same
commands on the command line (`crates/visor/src/cli.rs`). Both send a JSON command to the
running app (`crate::instance`), which answers in `crates/visor/src/app/agent.rs`; what edits
do is worked out in `app/agent_edits.rs`. Off until the user turns on agent control.

## How it works

**Commands.** Every tool is a JSON object whose `command` names it; the CLI builds the same
objects (`edit_args`, `finish_args`, `add_args`, `start_args`) and resolves paths and `~`
before sending (`with_absolute_folder`). Replies are JSON with `ok` and the fields, or `ok:
false` and an `error` written for the agent to act on. A command the app answers later
(starting, stopping, rendering a frame, finding dead time, adding videos, exporting) keeps
its `Reply` channel until then; the client waits up to 10 minutes.

**Which recording.** Edits take `folder`, else the recording the agent last stopped.
`agent_recording` resolves it to a real path (`project::real_path`), keeps it inside the save
folder, and refuses one being recorded or recorded more onto (whose take would overwrite the
edit), or one whose cancelled take is being put back.

**Times.** All times are recorded (source) seconds, as markers and `status` report them, so
they don't shift when something is removed or clips move. Things are addressed by time: a clip
by a moment it plays, a removed stretch by a moment in it, a marker by its time (nearest within
0.5 s, `MARKER_REACH`), a blur by a moment it covers (and `number` among several).

**Reading.** `get_recording` (`recording_json`) gives the whole state: clips in play order,
removed stretches and the clip each comes back next to (`EditList::removed`), markers, blurs
(fractions of the frame), the camera bubble, thumbnail, speed, exports and whether the editor
is open. Edit tools answer with the same, so an agent sees the result of each step.

**Changing.** `agent_change` makes a change through the recording's editor when it's open, so
it shows at once and the user can undo it like their own: the edit, markers and blurs are one
undo step each (`apply_agent_edit`, `apply_agent_markers`, `apply_agent_blurs`); name, speed,
thumbnail and bubble are set as the editor's controls set them, not undoable
(`apply_agent_looks`). Without an editor the change is saved to `project.json`, and a new name
renames the folder at once (`rename_to_title`). `remove_from_recording` with `ranges` removes
several stretches as one step.

**Exporting.** `export_recording` takes the user's last export choices for anything not given
(`ExportChoice::last`, `export_choice`). With the editor open it runs there
(`EditorWindow::agent_export`), shown and cancellable, and refused while the editor exports or
adds videos; the reply comes from `poll_export`, or an error if the editor closes first.
Otherwise it runs on a thread into `exports/`, holding folder renames (`App::copies`) until
written.

## Invariants

- An agent's edit through the editor is undoable exactly when the same edit by the user is.
- Edits never apply while a recording is being recorded onto: its final save would overwrite
  them.
- Agents can't delete recordings, and nothing they do leaves the computer except the frames
  they ask for with `view_frame`, which go to the agent.

## Gotchas

- A tool's reply shape is part of its contract: `remove_from_recording` with `ranges` answers
  with the same `kept` field as with `start_seconds`/`end_seconds`.
- `thumbnail_seconds: null` means "first kept frame", unlike other arguments, where null means
  left out; `looks_arg` reads it before `arg` drops nulls.

## Tests

`app/agent_edits.rs` (each edit and its refusals, export choices, the state JSON), `cli.rs`
(every command's arguments), `instance.rs` (commands reach the app). The tools end to end are
checked live with `ui.sh cli` against a test instance (the `ui-test` skill).
