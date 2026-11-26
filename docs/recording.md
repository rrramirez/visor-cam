# Recording and takes

How a recording gets onto disk, and how *Record more* adds to one later
(`crates/visor-recorder`, `visor-core` `project.rs`, `visor-media`
`decode.rs` and `recover.rs`, the app's `record_more`).

## How it works

**A recording.** `Recorder::start` opens one writer per track (`screen.mp4`,
`camera.mp4`, `mic.m4a`, `system.m4a`), all timed by one pause-aware
`MediaClock`, and writes `project.json` with no duration so a crash can be
recovered. It also saves what it recorded from (`Project::sources`, the
`RecordingConfig`), so the same sources can be used again. `stop` fills in
the duration and an edit list keeping everything.

**Takes.** *Record more* (editor **R**, agents' `start_recording` with
`folder`, `visor start --folder`) records another piece onto a stopped
recording instead of a new one. It is a take: `Project::takes` holds a
`Take { start_us, duration_us, tracks }` whose files are numbered
(`take_file`: `screen-2.mp4`, `mic-2.m4a`, … for the first take). A take goes
on from source time `start_us`, the recording's length before it, so source
time stays one line: the first piece from 0, then each take after it.
`duration_us` is the whole length. The recorder (`RecorderOptions::append`)
loads the project, `begin_take`s a take with duration 0 and saves it, so a
crash can be recovered; markers made during it are in the whole recording's
time. `stop` calls `finish_take`: the take gets its length, the recording
grows by it and it is appended as the last clip (`EditList::append`);
blurs that lasted to the end go on over it (`stretch_blurs`), like the
whole-video blur they usually are, so recovered and agents' takes get it too.
`discard` deletes only the take's files and saves the project as it was.

**Reading across takes.** Everything that reads tracks goes through
`Project::parts(kind)`: each piece's file over source time, or none for a
piece recorded without that track. `VideoReader::open_parts` and
`AudioReader::open_parts` read the parts as one track: timestamps offset by
the part's start, the next file opened at a part's end, a seek picking the
part. A take of another size (a resized window, another display) is scaled to
fit inside the target size with black bars (`fit_inside`, `letterbox`) for
RGBA, YUV420P and GRAY8. A part with no file has no frames
(`VideoReader::shows`) or no samples (the audio mix fills gaps with silence).
`FrameCursor` returns no frame there, so the camera bubble isn't drawn
(`Player::camera_shown`), and never carries a frame from one take into
another. The player, export, thumbnails, waveform and *Remove pauses* all use
parts; the size of a track is its first file's.

**The app.** `App::record_more(dir)` keeps the folder in `App::more`, opens
the panel and sets it from the saved sources (`choose_saved_sources`, using
the pure `saved_choices`). If all of them are there it starts the countdown;
otherwise the panel stays open saying what's gone, and recording from it
still adds to that recording. The editor hides while it records. When the
take is saved, `EditorWindow::take_added` reloads the project, reopens the
player and previews, appends the clip (and its markers) to every undo and
redo step (`add_take_to_steps`), selects it and seeks to its start. A
discard, a cancelled countdown or closing the panel shows the editor again
as it was, only once the take's recorder has put the project back, so its
save can't overwrite an edit. *Record more* waits while the editor exports
or looks for pauses, which would act on the recording without the take.
Agents can't edit a recording while it is being recorded more onto.

**The countdown** (`show_countdown`) is cancelled by Esc, as the stop
shortcut does (`stop_recording(false)` while starting). It takes the
keyboard only if Visor is already the active app
(`platform::focus_if_active`, macOS only), so it never pulls the user out
of another app (opening the panel to start does activate Visor). Its "esc to
cancel" hint (`can-cancel`) shows only when it took the keyboard: Slint's
`has-focus` is true for a window that never became active, so it can't say.
While a cancelled take is being put back (`more_cancelled`), its recording
can't be opened, recorded more onto, or edited by agents.

**Recovery.** `is_interrupted` is also true for a project whose last take has
duration 0. `recover` then finishes it from what its files hold (the longest
readable track), dropping unreadable tracks, or drops the take if its screen
track is unreadable.

## Invariants

- Takes are in order and inside the recording: `start_us` at or after the
  previous take's end, `start_us + duration_us <= duration_us` (checked on
  load). Only the last take may be recording (duration 0).
- Track files of takes are plain names in the folder, like the first piece's.
- Recording more never changes the first piece or the edit's earlier clips;
  undo can't remove the take, because every undo step has it.
- Recording more keeps the recording's camera bubble layout (the live bubble
  position isn't saved over it).

## Gotchas

- Versions before takes read only the first piece's files: a recording with
  takes opened there shows the take's time as the last frame held, silent,
  and saving an edit there drops `takes` and `sources` (their files stay).
- The last part of a track has no end, so a frame a little past the recorded
  length still shows; earlier parts cut off frames at their end.
- A recording in progress (a take included) looks interrupted, so recovery
  only runs at launch, before anything can be recording.

## Tests

`visor-core` `project` (parts, bookkeeping of takes, old JSON),
`visor-media` `tests/decode.rs` (reading parts, letterbox, gaps),
`tests/export.rs` and `tests/player.rs` (a take exported and played),
`tests/recover.rs` (a take cut short), `visor-recorder` (recording more with a
fake screen, discard keeps the original), `visor` `agent.rs`
(`saved_choices`) and `editor.rs` (`add_take_to_steps`). The hide / countdown /
come back flow is checked live.
