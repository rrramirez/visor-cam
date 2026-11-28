# Changelog

What's new in each version of Visor. The release script publishes the
section for the version being released as its GitHub release notes, which
is also what the update window shows.

## 0.9.0

### New
- **Record more:** forgot a step? In the editor, *Record more* (**R**) records another piece with the same screen, window or area, camera and microphone, right after a countdown, and brings you back with it as the last clip, ready to drag where it belongs. Undo still works on what you did before. A piece of another size fits in the frame with black bars, discarding it leaves the recording as it was, and a crash keeps what was recorded. If a source is gone (a closed window, an unplugged camera), the recorder opens so you can pick another. Agents can do it too: `start_recording` with `folder`, or `visor start --folder`.
- **Add video:** bring another Visor recording (an intro you recorded separately) or a video file (MP4, MOV, M4V, MKV or VP8/VP9 WebM, say from your phone) into the one you're editing. *Add video…* in the editor offers your recent recordings and file pickers, or drop files on the editor. They land as clips at the end, ready to drag where they belong; a Visor recording comes as you edited it, with its markers and blurs, a phone video shows upright, and a different size fits with black bars. Files are copied in, so the originals stay as they are, and **⌘Z** takes the whole addition out again. Agents can do it too: `add_video`, or `visor add`.
- **Clips:** cut the video into clips with **C** or **⌘B** (*Cut at playhead*) and drag a clip to play it somewhere else, while the others make room; blurs and markers go with it, and chapters follow the new order. Above the clips, a ruler is where you click to seek and drag to select. What you delete stays on the timeline, dimmed, next to the clip it came from, and moves with it; playing and exporting skip it.
- **Bring back what you deleted:** hover a dimmed part and click *Bring back* to put just that part back, without undoing everything you did since. You can still drag a clip's edge to bring back some of it, and *Restore the whole recording* puts everything back as it was recorded.

### Improved
- **Esc cancels the countdown** on macOS, when the countdown says so: it doesn't take the keyboard from another app you're in, and there **⌥⇧R** cancels it, as before.

### Changed
- The editor's words: *Cut selection* is now *Delete selection*, and *Cut pauses* is now *Remove pauses*. For agents, `cut_recording` is now `remove_from_recording`, and on the command line `visor cut` is now `visor remove`.

### Fixed
- The sound wave under the timeline was squeezed into its middle; it spans the timeline again.

## 0.8.0

### New
- **Blur an area:** hide a password, an email address or a notification. Press **B** or *Blur an area* in the editor, drag the box over what to hide, and resize it from its corners. To blur only part of the video, select that part on the timeline first, then drag the blur's edges on the timeline or nudge them with **← →** to start and end it on the exact frame. Exports, the thumbnail and what agents see are all blurred, and the text underneath can't be read.

## 0.7.0

### Fixed
- **Crash recovery:** if Visor crashed or was force-quit while recording, little or nothing could be recovered when the screen barely changed, because most of the recording was still in memory. Visor now saves it at least every half second, even while the screen is still or recording is paused, and a recovered recording keeps all the narration. A power cut or a crash of the Mac itself can still lose the last few seconds.

### Improved
- Agents see times to the millisecond, so a cut at a marker lands on it, well within a frame. They used to be rounded to a tenth of a second, which could leave a moment of what should have been cut.
- When an agent's cut leaves less than a quarter second between it and another cut, or the start or end, that bit is cut too, rather than flashing by in the video.

## 0.6.0

### New
- **Markers:** mark each step while you record, with **⌥⇧M** or the flag button on the control bar, and name them afterwards in the editor's new Markers section. Add markers there too (**M** or *Add marker*), jump to them, or delete them. Exports get a chapter at each one.
- **Chapters:** MP4 and WebM exports have a chapter at each marker, named after it, which QuickTime, VLC and other players list so viewers can jump straight to a step. Chapters follow your cuts and the speed you chose.

### Fixed
- The times shown for markers in the editor now match the clock under the video (time in the export, after cuts and speed), and update when you cut. They used to show the time in the recording.

## 0.5.0

### New
- **Agents can see what they recorded:** an agent can look at the frame at any moment of a recording, as the export would show it, to check a stretch before cutting it. On the command line, `visor frame SECONDS` saves it as a JPEG.
- **Cut pauses:** one button in the editor finds every stretch of 2 seconds or more where the screen stays still and nothing is heard, and cuts them all in one step you can undo. It leaves a moment around what happens, and isn't fooled by a spinner turning or a clock ticking seconds on screen. Agents can find these stretches too, and decide what to cut.

## 0.4.0

### New
- **Adjust cuts:** drag either edge of a cut on the timeline to make it longer or shorter, watching the frame under it, or click an edge and move it a frame at a time with the arrow keys. Works for your cuts and an agent's alike.
- **Agents can cut their recordings:** after stopping, an agent can remove stretches that went wrong or trim the ends, then open the result in the editor for you. If the recording is already open, its cuts show there and you can undo them.

### Fixed
- On a Mac, agent commands Visor answers at once, like `status`, sometimes failed with "Invalid argument".

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
