# Editor UI

The editor window (`crates/visor/src/editor.rs`, `crates/visor/ui/editor.slint`)
plays a recording, edits it and exports it. Rust owns the project and the
player; the Slint side holds view state and reports gestures through
callbacks.

## How it works

**State.** `EditorWindow` holds the `Project`, the `Player`, and undo / redo
stacks of `Step`s: a whole `EditList`, marker list or blur list as it was.
Every change goes through `apply` (edits), `change_markers` or
`change_blurs`, which push one step, save `project.json` and call
`refresh_edit_ui` to rebuild the Slint models.

**Timeline.** It shows the whole recording laid out by `visor_core::Layout`:
each clip in the order it plays, with what was deleted next to it (dimmed)
where it would come back. Every source time has one place on it, so a
position (a fraction of the recording's length) maps one to one to a source
time (`timeline_pos`, `source_at`, `display_at`), and nothing shifts when a
part is deleted or brought back. Each clip is a `Clip` row: where it plays,
its block (with what was deleted next to it, which moves with it), and where
its part of the recording starts. The thumbnails and waveform are made once
for the whole recording; each clip and dimmed part draws them shifted so its
part shows, and clips the rest.

- The ruler (a strip above the clips): click to seek, drag to select. The
  selection (also **I / O**) is in timeline time; deleting or trimming uses
  only what plays in it (`Layout::kept_pieces`).
- A clip: drag it to move it, or click it to seek there and select it. While
  one moves its block is drawn under the pointer (`moving-x`), `clip-slot`
  asks Rust which slot its middle is over, and `shift` slides the other
  blocks aside to open that slot (`landing`); letting go calls `move-clip`.
- Dimmed parts (`Gone`, the layout's segments that don't play): hover one for
  a card with its length and *Bring back* (`bring-back` sends its index in
  timeline order, which `bring_back_shown` looks up in the same `Layout`;
  `EditList::removed` lists them in recording order instead; one undo step). The part and the card each have a
  `TouchArea`, and the card stays open while either is hovered. Clicking the
  part seeks to what plays after it on the timeline.
- clip edges (`CutEdge` with `inside: true`): drag to show less of the
  recording or more of what was deleted next to it (over its dimmed part), previewing the clip's first
  or last frame; click one and **← →** nudges it a frame (held nudges are one
  undo step, via `nudged`);
- blur bands (`BlurBand`, one per part of a blur that plays) and the picked
  blur's edges, marker flags and the thumbnail mark, placed by
  `Layout::display`; ruler ticks are times in the video, placed where they
  play.

**Shortcuts.** Space play/pause, ← → step (into the next or previous clip at
a clip's end), I / O selection, ⌫ delete the selection, T trim, C or ⌘B cut
at the playhead (as Premiere's razor and Final Cut's blade), M marker, B blur,
⌘Z / ⇧⌘Z undo / redo, R record more (see [recording.md](recording.md)), Esc
lets go of what's picked.

**Words.** *Cut* only means cutting a clip in two. Taking video out is
*Delete* in the editor (leaving it dimmed, to bring back) and *Remove* for pauses
and agents (`remove_from_recording`, `visor remove`); all of them close up the
space.

## Invariants

- The player's position is always source time. Convert at the boundary:
  `timeline_pos` to show it, `source_at` for a click.
- An edit change clears the selection (`set_edit`): its fractions refer to the
  old timeline.
- A step to undo holds a whole list, so undo restores the order of clips
  along with their ranges.

## Gotchas

- Slint's `Path` keeps its proportions unless told otherwise; the waveform
  needs `fit: fill` or it shrinks into the middle of the timeline.
- Later siblings in Slint get pointer events first. The selection handles and
  clip edges come after the clips and dimmed parts, so they win over them; an
  open card raises its part with `z`.
- A `TouchArea` loses `has-hover` when another on top of it takes the
  pointer, so a hover card needs its own `TouchArea`, and stays open while
  either is hovered.

## Tests

The editor itself has no UI tests: its logic is tested where it lives
(`visor-core` for edits, `visor-media` for player and export), plus a few
pure helpers in `editor.rs`. Changes to the timeline are checked live by
opening a recording in a test home (`HOME=<temp> visor --edit <folder>`) and
looking at a screenshot.
