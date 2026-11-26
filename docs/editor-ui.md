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

**Timeline.** It shows the edit as it plays: all positions are fractions of
the output duration (`timeline_pos`, `source_at`). Each clip is a `Clip` row:
where it plays, and where its part of the recording starts. The thumbnails and
waveform are made once for the whole recording; each clip draws them `zoom`
(recording ÷ edit length) times wider, shifted so its part shows, and clips
the rest. Clips always play back to back: nothing leaves blank space.

- The ruler (a strip above the clips): click to seek, drag to select. The
  selection (also **I / O**) is in output time.
- A clip: drag it to move it, or click it to seek there and select it. While
  one moves it's drawn under the pointer (`moving-x`), `clip-slot` asks Rust
  which slot its middle is over, and `shown-start` slides the others aside to
  open that slot (`landing`); letting go calls `move-clip`.
- Deleted stretches (`Gone`, from `EditList::removed`): a ▼ on the ruler where
  each would come back. Hovering opens a card on the same `TouchArea`, so it
  stays open as the pointer moves onto it, with the length, the thumbnail
  covering its first frame and *Bring back* (`bring-back` →
  `EditList::bring_back`, one undo step).
- clip edges (`CutEdge` with `inside: true`): drag to show less of the
  recording or more of what was deleted next to it, previewing the clip's first
  or last frame; click one and **← →** nudges it a frame (held nudges are one
  undo step, via `nudged`);
- blur bands (`BlurBand`, one per part of a blur that plays) and the picked
  blur's edges, marker flags and the thumbnail mark, placed by output time and
  hidden when cut.

**Shortcuts.** Space play/pause, ← → step (into the next or previous clip at
a clip's end), I / O selection, ⌫ delete the selection, T trim, C or ⌘B cut
at the playhead (as Premiere's razor and Final Cut's blade), M marker, B blur,
⌘Z / ⇧⌘Z undo / redo, Esc lets go of what's picked.

**Words.** *Cut* only means cutting a clip in two. Taking video out is
*Delete* in the editor (leaving a ▼ to bring it back) and *Remove* for pauses
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
- Later siblings in Slint get pointer events first. The ▼ notches come after
  the ruler's `TouchArea`, and the selection handles and clip edges after the
  clips, so they win over them.
- A `TouchArea` loses `has-hover` when a sibling on top of it takes the
  pointer. The ▼ card is one element that grows when open, with a single
  `TouchArea` for both, so hovering the card keeps it open.

## Tests

The editor itself has no UI tests: its logic is tested where it lives
(`visor-core` for edits, `visor-media` for player and export), plus a few
pure helpers in `editor.rs`. Changes to the timeline are checked live by
opening a recording in a test home (`HOME=<temp> visor --edit <folder>`) and
looking at a screenshot.
