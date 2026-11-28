# Visor knowledge base

How Visor works, for contributors and coding agents about to change it. Each
page covers one area: how it works, the invariants that must hold, and the
traps already hit. [`DESIGN.md`](../DESIGN.md) is the one-page overview; start
there.

- [edit-model.md](edit-model.md): `EditList`, clips, source and output time,
  cuts and trims, speed, chapters.
- [recording.md](recording.md): recording to disk, takes (*Record more*),
  added recordings and videos, reading across them, crash recovery.
- [voiceover.md](voiceover.md): voice and camera recorded over a stretch of
  the video, how they follow clips, ducking the video's sound, the editor's
  recording flow.
- [agent-tools.md](agent-tools.md): agents' MCP tools and the command line,
  how they find recordings, address things by time, and edit through the editor.
- [library.md](library.md): the Recordings window, listing, thumbnails,
  renaming and trashing recordings.
- [editor-ui.md](editor-ui.md): the Slint editor window, its timeline, undo
  steps and shortcuts.
