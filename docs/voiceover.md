# Voiceovers

A voiceover is a voice and/or camera recorded while a stretch of the video plays, laid over
it: the voice is mixed in, the video's own sound lowered under it, and its camera shown in
the bubble there. Model in `visor-core` `project.rs`, recording in `visor-recorder`
(`VoiceRecorder`), mixing in `visor-media` `timeline.rs` (`AudioMix`), the editor's flow in
`crates/visor/src/editor.rs` (`open_voiceover` … `stop_voiceover`), agents in
`app/agent_edits.rs`.

## How it works

**Model.** `Project::voiceovers` is a list of `Voiceover { pieces, voice, camera,
clip_sound }`. Each `VoicePiece { start_us, end_us, file_us }` is a stretch of *source* time
and the time in the voiceover's files that plays there. A voiceover recorded across a clip
boundary has several pieces, in the order they played (`EditList::play_span(from, len)`
works them out: the kept ranges that play from `from` for `len`, with each one's offset).
Source time means a voiceover belongs to the moments it was recorded over: moving a clip
moves it, deleting part of a clip silences that part of it, bringing it back brings the
voice back. Older projects have no `voiceovers` key, and none is written while it's empty.

**Reading.** `Project::parts(TrackKind::Voice)` turns the pieces into `Part`s with
`offset = file_us`, so the ordinary readers play them; where voiceovers overlap, the later
one wins. `parts(TrackKind::Camera)` carves voiceover cameras into the recorded camera parts
(`carve`): a voiceover's camera replaces whatever camera was there, and shows over a clip
that had none (an imported phone video). `has_track` counts voiceovers, so a recording with
no recorded camera still gets a bubble. The readers (`VideoReader`, `AudioReader` in
`decode.rs`) seek a part's file to its `offset` when they open it, and the audio reader drops
samples from before the offset that the seek landed on.

**Mixing.** `AudioMix::for_project` mixes mic, system audio and voice parts. Mic and system
are *ducked*: multiplied by `Project::clip_sound_gain(t)`, which is the voiceover's
`clip_sound` (default 0.25) inside a voice piece, ramping over `DUCK_FADE` (150 ms) at each
edge, and 1 elsewhere. The voice itself isn't ducked. The gain is looked up once per 480
frames (10 ms). Export and the player both use `for_project`, so they sound the same.

**Recording.** `VoiceRecorder::start(dir, n, mic, camera)` writes `voice-N.m4a` and/or
`vcam-N.mp4` from the mic's and camera's broadcast streams, on one clock that starts
paused; `resume` starts it when playback starts, after the countdown. `stop` returns the
duration and the files; `discard` deletes them. `N` is the lowest number no `voice-N` or
`vcam-N` file has.

**Editor.** *Record voiceover…* opens a panel (`vo-panel`): voice and camera switches with
pickers, a level meter and a camera preview, and what it will cover
(`voiceover_stretch`: the selection's kept part, else from the playhead to the end). Record
starts the recorder paused and counts down 3-2-1, then plays from the start at 1×, muted
(`Player::set_muted`), and resumes the recorder. `vo_tick` stops it once it has run as long
as the stretch or playback ended; Stop or Space stops it sooner, Esc throws it away. Stop
turns the recording into a voiceover over what played (`voiceover_over`) and adds it as a
`Step::Voiceovers` (the whole list as it was), as do deleting one and the *Clip sound*
slider. Afterwards the player gets back its speed and sound (`end_voiceover_playback`).
The Voiceovers section lists them (`voiceover_label`: "0:00–0:04 · Voice and camera").

**Agents.** `get_recording` lists `voiceovers` (number, pieces in recorded seconds, voice,
camera, clip_sound); `edit_voiceovers {action: delete, number}` (`delete_voiceover`) and
`visor voiceover delete N` delete one, through the editor's undo when it's open. Agents
can't record voiceovers.

## Invariants

- Pieces are in source time and in play order as recorded; `file_us` grows along them, so
  each piece plays the right moment of the files whatever the clip order later becomes.
- A voiceover's files stay in the folder while any undo or redo step could bring it back;
  `remove_unused_added_files` (on close) counts voiceover files as used.
- While recording, the editor takes no clicks (a layer over it) and only Esc and Space
  as keys: moving the playhead would put the voice out of step with the picture.
- Recording is always at 1×, whatever the recording's speed; an export at another speed
  plays the voice at that speed too (pitch kept).

## Gotchas

- The panel's state is in a `RefCell`, and asking for camera or mic permission or opening
  a device can run the event loop, where the preview timer borrows it too. Don't hold a
  borrow across those calls; the camera is opened on a thread and handed back through a
  channel the preview timer polls (`camera_rx`). Holding it panicked with "already mutably
  borrowed".
- A device the recorder panel already has open is shared (`App::shared_mic`,
  `shared_camera`) rather than opened twice, which may fail.
- A part starting partway into its file needs the reader to seek there; before voiceovers
  every file was read from its start, and a voiceover's `file_us` was silently ignored.

## Tests

`visor-core` `project` (`voiceovers_lay_voice_and_camera_over_what_they_cover`, the ducking
envelope), `edit` (`a_stretch_plays_on_across_clips`), `visor-recorder`
(`a_voiceover_records_voice_and_camera_without_a_screen`), `visor-media` `tests/voiceover.rs`
(an export: the video's sound at a quarter under the voice, the voiceover camera in the
bubble), `visor` `editor.rs` (`a_voiceover_covers_what_played_while_it_recorded`),
`agent_edits.rs` and `cli.rs` (listing and deleting). The panel, countdown and recording with
real devices are checked live (the `ui-test` skill).
