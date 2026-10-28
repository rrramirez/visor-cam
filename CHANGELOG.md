# Changelog

What's new in each version of Visor. The release script publishes the
section for the version being released as its GitHub release notes, which
is also what the update window shows.

## Unreleased

### New
- **Adjust cuts:** drag either edge of a cut on the timeline to make it longer or shorter, watching the frame under it, or click an edge and move it a frame at a time with the arrow keys. Works for your cuts and an agent's alike.
- **Agents can cut their recordings:** after stopping, an agent can remove stretches that went wrong or trim the ends, then open the result in the editor for you. If the recording is already open, its cuts show there and you can undo them.

## 0.3.0

### New
- **Let AI agents record for you:** agents like Claude Code, Claude Desktop and Cursor can start, pause, mark and stop recordings through Visor's MCP server or the `visor` command. It's off until you turn it on in Preferences, and recordings an agent starts say *Agent* on the control bar. Agents can name the recording when they start it, and pause whenever nothing worth watching is happening.
- **Markers:** notes an agent adds while recording ("Step 2: open settings") show on the editor's timeline; click one to jump there.

### Improved
- With several displays on a Mac, the one with the menu bar is now always the main one: the recorder panel opens there and records it by default.

## 0.2.0

### New
- **Keyboard shortcuts that work from any app:** ⌥⇧R starts a recording with your last choices, or stops one; ⌥⇧P pauses or resumes; ⌥⇧⌫ discards (Alt+Shift+R, P and Backspace on Windows and Linux). You can turn them off in Preferences, which also tells you when another app already uses one.
- **Name your recordings:** when a recording stops, the editor opens ready for you to type a name. Exports are named after it, and the recording's folder is renamed to match when you close the editor.
- **Thumbnail:** pick the frame that MP4 exports show as their cover in Finder, Explorer and video players, or save it as a full-size image.
- **Cleaner audio:** *Reduce background noise* and *Even out volume* in the export options. They apply to the exported file, not the preview, and Visor remembers your choice.

### Improved
- The Recent recordings menu lists recordings in the order they were made, including recordings recovered after a crash.
