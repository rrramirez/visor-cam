# Recording, takes and added videos

How a recording gets onto disk, how *Record more* adds to one later, and how
other recordings and video files are added to it (`crates/visor-recorder`,
`visor-core` `project.rs`, `visor-media` `decode.rs`, `import.rs` and
`recover.rs`, the app's `record_more` and the editor's `add_videos`).

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

**Added videos.** *Add video…* (the editor's menu, a drop on the editor,
agents' `add_video`, `visor add`) adds other Visor recordings and video
files (MP4, MOV, M4V, MKV, WebM) at the end, as takes too. `import::prepare`
reads each one without changing anything: a recording (a folder with
`project.json`) becomes one take per piece of it, with its edit, markers and
blurs (`Addition`); a file is probed (`probe_video`: a video stream Visor
can decode, the size as shown, the time it starts, whether its sound can be
played) and becomes one take whose screen and mic tracks name the same file.
Files are numbered with `Project::free_take_number` and keep their
extension (`screen-3.mov`). `Prepared::copy` copies them in (written as
`.partial`, then renamed; removed again on failure or cancel), then
`Project::add` puts each at the recording's end: its takes after
`duration_us`, its clips in its own play order (`EditList::append_edit`,
which keeps what it had removed, and its trimmed start, coming back where
they did), its markers, and its blurs, placed over the same content when its
frame is letterboxed (`fit_rect`). This recording's blurs don't stretch over
added footage, unlike over a take recorded onto it. Its title, speed,
thumbnail and bubble layout aren't taken; its camera shows in this
recording's bubble. `Project::take_addition(at)` takes everything from source
time `at` on out again, for undo (see [editor-ui.md](editor-ui.md)).

**Files from elsewhere.** `Track::start_us` is the file time where the track
starts (another app's video may start past 0); `Part::offset` carries it to
the readers, which subtract it. A video stream's display matrix (a phone held
upright) is read as a clockwise turn (`rotation`), noted on each decoded
frame as side data, and the frame is turned after scaling (`rotate`), for
the formats with bars; `size()` is the size as shown.

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
- A take's number is never one an existing file in the folder has
  (`free_take_number`): an addition undone keeps its files until the editor
  closes, for redo, and a take recorded meanwhile mustn't write over them.
- What's added is copied in; its original is never written to. A recording
  can't be added to itself, nor one still being recorded (`is_interrupted`).

## Gotchas

- Versions before takes read only the first piece's files: a recording with
  takes opened there shows the take's time as the last frame held, silent,
  and saving an edit there drops `takes` and `sources` (their files stay).
- The last part of a track has no end, so a frame a little past the recorded
  length still shows; earlier parts cut off frames at their end.
- A recording in progress (a take included) looks interrupted, so recovery
  only runs at launch, before anything can be recording.

- `av_display_rotation_set` takes a clockwise angle, but
  `av_display_rotation_get` returns a counterclockwise one: a matrix set to
  90 reads back as -90.
- Packaged builds decode only what `scripts/build-ffmpeg-macos.sh` (and the
  Windows script) enable; a video in another codec is refused by
  `probe_video`, which checks for a decoder, rather than failing to play.

## Tests

`visor-core` `project` (parts, bookkeeping of takes, old JSON),
`visor-media` `tests/decode.rs` (reading parts, letterbox, gaps),
`tests/export.rs` and `tests/player.rs` (a take exported and played),
`tests/recover.rs` (a take cut short), `tests/import.rs` (probing, copying,
a recording and a phone's video added and exported), `tests/decode.rs` (a
file starting late and turned), `visor-recorder` (recording more with a
fake screen, discard keeps the original), `visor` `agent.rs`
(`saved_choices`) and `editor.rs` (`add_take_to_steps`, `add_to_steps`),
`cli.rs` (`visor add`). The hide / countdown /
come back flow is checked live.
