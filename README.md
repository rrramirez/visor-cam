# Visor

An offline, open source screen recorder. Record your screen, a window, an app
or an area, with your camera and microphone and system audio; pause and
resume; trim and cut; export MP4, WebM or GIF. No accounts, no uploads, no
cloud — recordings are plain folders on your disk.

Built in Rust with [Slint](https://slint.dev) (native UI, no webview, no
Electron) and FFmpeg. See [DESIGN.md](DESIGN.md) for the architecture.

## Status

| Platform | Status |
|----------|--------|
| macOS 14+ | Works: menu bar app, recording, editor, export, `.dmg` packaging |
| Windows 10 2004+ / 11 | Capture backend written and cross-compiled; untested on Windows |
| Linux (Wayland/X11 with xdg-desktop-portal + PipeWire) | Capture backend written; builds and tests in Docker; untested on a desktop |

## Using Visor (macOS)

1. Open `Visor.app` (from `dist/Visor-<version>.dmg`, or `cargo run -p visor`).
   It lives in the menu bar; the recorder panel opens at launch.
2. Grant **Screen Recording** permission when asked (System Settings › Privacy
   & Security), then restart Visor. Camera and microphone access are requested
   when you pick a device.
3. Pick what to record (Screen / Window / App / Area), a camera, a microphone
   and optionally system audio (all apps or one app), then **Start recording**.
   With several displays, the chosen one is outlined in red. Click the
   countdown to skip it; on macOS, **Esc** cancels it when it says so (it
   doesn't take the keyboard from another app you're in; **⌥⇧R** cancels it
   there).
4. Use the floating control bar (or the menu bar menu) to pause, resume, stop
   or discard. The control bar and camera bubble never appear in the recording.
   From any app, **⌥⇧R** starts a recording with your last choices (or stops
   one), **⌥⇧P** pauses or resumes, **⌥⇧M** adds a marker and **⌥⇧⌫**
   discards (Alt+Shift+R, P, M and Backspace on Windows and Linux/X11; they
   can be turned off in Preferences). The flag button on the control bar
   adds a marker too: mark each step as you go, and name them later.
5. Stopping opens the editor, ready for you to name the recording: type a
   name and press Enter to get back to editing. Exports are named after it,
   and you can rename it any time in the sidebar's **Name** field. When the
   editor closes, the recording's folder is renamed to match.
   In the editor:
   - **Space** play/pause, **← →** step a frame (**⇧** for 10), click the
     ruler above the clips to seek, drag along it to select. The timeline
     shows the clips in the order they play, each with what was deleted next
     to it, dimmed; playing and exporting skip what's dimmed.
   - **I / O** set the selection from the playhead, **⌫** *Delete selection*,
     **T** trim to it, **⌘Z / ⇧⌘Z** undo/redo, *Restore the whole recording*
     to start over.
   - **Clips**: **C** or **⌘B** (*Cut at playhead*) cuts the video in two
     clips there; deleting also leaves one either side. Drag a clip to play
     it somewhere else (the others make room), or click it to select it.
   - **Bring back what you deleted**: hover a dimmed part and click *Bring
     back* to put just that part back, without undoing what you did since. A
     dimmed part moves with the clip it was deleted from.
   - **Markers**: *Add marker* (**M**) marks the playhead. Name each one in
     the sidebar, click its time to jump there, or delete it.
   - **Blur**: *Blur an area* (**B**) hides part of the screen, like a
     password or an email address. To blur only part of the video, select
     that part on the timeline first; otherwise it covers the whole video.
     Drag its box on the video to place it and its corners to resize it.
     Click a blur in the sidebar to show its box again, and its edges on
     the timeline: drag one to change when it starts or ends, with the
     preview showing the first or last frame it blurs, or click one and
     press **← →** to move it a frame at a time (**⇧** for 10). With part
     of the timeline selected, *Blur only during the selection* sets both
     at once. It's heavy enough that text under it can't be read, fades out
     at its edges, and the camera bubble stays on top.
   - Drag either edge of a clip to show less of the recording, or more of
     what was deleted next to it; the preview shows the clip's first or last
     frame. Click an edge, then **← →** move it a frame at a time (**⇧** for
     10). Dragging an edge onto the clip's other edge removes the clip.
   - **Record more** (**R**): forgot something? Record another piece with the
     same screen, camera and mic; the editor comes back with it as the last
     clip, ready to drag where it belongs.
   - **Add video**: *Add video…* adds another Visor recording (from the
     recent ones, or any folder) or video files (MP4, MOV, M4V, MKV, WebM)
     at the end, as clips; or drop them on the editor. A recording comes as
     you edited it, with its markers and blurs. They're copied in, and
     **⌘Z** takes them out again. To start a recording from video files
     instead, use *New from Video…* in the menu bar or "or start from a
     video…" in the recorder.
   - **Voiceover**: *Record voiceover…* records your voice and/or camera
     while the selection (or from the playhead to the end) plays without
     its sound, after a countdown; Stop when done, Esc to throw it away.
     The voice plays over the video, whose own sound drops to the
     voiceover's *Clip sound* level (25% to start) under it, and your camera
     shows in the bubble there. A voiceover moves with the clips it covers;
     the Voiceovers section lists them to jump to or delete.
   - *Remove pauses* finds every stretch of 2 seconds or more where the screen
     stays still and nothing is heard, and removes them all in one step. It
     leaves a moment around what happens, and ignores a spinner or a clock
     ticking on screen.
   - **Speed** (0.5×–2×) speeds up or slows down the whole video; speech
     keeps its pitch. The preview plays at that speed.
   - Drag, resize, reshape, mirror or hide the camera bubble. It starts where
     it was on screen while recording, mirrored as you saw it.
   - **Thumbnail**: *Use frame* picks the frame MP4 exports carry as
     their cover (shown by Finder, Explorer and players); *Save image* writes
     it as a full-size JPEG.
   - **Export** to MP4, WebM (VP9 or AV1) or GIF, at original size, 1080p or
     720p, optionally with *Reduce background noise* and *Even out volume*
     (to -16 LUFS; both apply to the export, not the preview). Files go to the
     recording's `exports/` folder. MP4 and WebM exports have a chapter
     for each marker, named after it, so players can jump between steps.

Recordings are saved to `~/Movies/Visor/<date time> Recording/` (configurable
in Preferences) as separate tracks plus `project.json`, so edits are
non-destructive and you can always re-export. Once you name a recording, its
folder takes that name.

**Recordings window:** the recorder panel's folder button, or *Recordings…* in
the menu bar, shows all your recordings with a thumbnail, date and length,
newest first. Double-click one (or press Return) to open it in the editor;
click the name of the selected one to rename it; select several (⌘-click,
⇧-click) and press ⌫ to move them to the Trash. A card's menu also shows it in
Finder or shows its exports. Recordings being recorded or open in an editor
say so, and can't be moved to the Trash.

Command line: `visor --edit <recording folder>` opens a recording;
`visor --library` opens the Recordings window;
`visor --record [--stop-after SECONDS]` starts recording with the saved
preferences as soon as the saved camera is ready, optionally stopping after
that many seconds of recorded time.

## Letting AI agents record

Agents such as Claude Code, Claude Desktop or Cursor can record a demo for
you: name the recording, start it, mark each step, pause while they set
something up or retry a step, stop, look at what was recorded, and remove what
went wrong. They can also finish it off as you would in the editor: rearrange
clips, name the chapters, blur what's private, set the speed, the camera
bubble and the thumbnail, and export it, before handing it to you. Turn on **Let AI agents control recording** in Preferences (it's off
until you do). Recordings an agent starts say *Agent* on the control bar,
and your own controls and shortcuts keep working.

Add Visor to the agent as an MCP server. The command is shown in
Preferences; for the installed app on macOS, with Claude Code:

```sh
claude mcp add visor -- /Applications/Visor.app/Contents/MacOS/visor mcp
```

Other agents take the same command and the `mcp` argument in their MCP
settings. Agents that run shell commands can use the command line instead:

```sh
visor sources                                # what can be recorded
visor list                                   # recordings, newest first
visor start --window "Safari" --name "Safari tour"  # returns once recording
visor mark "Step 2: open settings"           # shown on the editor's timeline
visor pause; visor resume
visor stop                                   # prints the folder and markers
visor stop --open-editor                     # ...and opens it for you to edit
visor discard
visor start --folder "$FOLDER"               # record more onto a stopped recording
visor add intro.mp4 ~/Movies/Visor/Outro     # add videos or recordings at its end
visor import phone.mov --name "App tour"     # start a recording from video files
visor voiceover delete 1                     # delete a voiceover (as info lists them)
visor remove 12 19.5                         # remove 0:12–0:19.5 (recorded time)
visor remove 3 4 12 19.5                     # ...or several stretches, as one step
visor trim 2 1:05                            # keep only 0:02–1:05
visor restore                                # bring everything back, in recorded order
visor open                                   # open it in the editor
visor info                                   # clips, markers, blurs, exports…
visor split 30; visor move 30 --before 0     # clips
visor bring-back 13                          # put back what's removed at 0:13
visor marker add 42 "Step 3: share"          # markers on a stopped recording
visor blur add 0.6 0.05 0.35 0.1 --from 5 --to 20   # hide an area (fractions)
visor set --name "Tour" --speed 1.25 --bubble off
visor export --format mp4 --size 1080p       # waits until it's written
visor frame 15                               # save the frame at 0:15 as a JPEG
visor dead-time                              # where nothing happens, to remove
visor help                                   # every option
```

Edits apply to the recording the agent last stopped (or `--folder`), in
recorded time, so they line up with marker times. If the recording is open
in the editor, an agent's change shows there: **⌘Z** undoes its cuts, clip
moves, markers and blurs, and its export runs there, where you can cancel it. Either way
Visor starts in the background if it isn't running. Replies are
JSON. Commands reach Visor through a local port that only your user account
can use (its token is in your settings folder).

## Troubleshooting

- **Logs:** `~/Library/Logs/Visor/visor.log`. Crashes inside Visor (panics)
  are written there with a backtrace; for crashes macOS reports, see
  `~/Library/Logs/DiagnosticReports/visor-*.ips`.
- **Interrupted recordings:** if Visor quits unexpectedly while recording,
  the tracks are kept up to that point. The next time Visor starts it
  recovers them, and the recording opens in the editor like any other.

## Building

### macOS

```sh
brew install ffmpeg pkgconf x264 libvpx opus svt-av1
curl https://sh.rustup.rs -sSf | sh      # if you don't have Rust
cargo test --workspace
cargo run -p visor                       # the app
scripts/package-macos.sh                 # dist/Visor.app and dist/Visor-<version>.dmg
```

Development builds link Homebrew's FFmpeg. Packaging instead builds a
minimal FFmpeg 7.1 (`scripts/build-ffmpeg-macos.sh`: only the codecs, formats
and filters Visor uses, with x264/libvpx/opus linked in) and bundles just
those libraries, which keeps the app small.

Signing: with a *Developer ID Application* certificate in the keychain, the
app is signed with it (hardened runtime), and with a notarytool keychain
profile named `visor-notary` the app and DMG are also notarized and stapled,
so they open without warnings. Without a certificate the `.app` is ad-hoc
signed: on another Mac, open it once, then click *Open Anyway* in System
Settings › Privacy & Security (right-click › *Open* on macOS 14), and macOS
asks for Screen Recording permission again after every rebuild.

Updates use [Sparkle](https://sparkle-project.org) (fetched by
`scripts/fetch-sparkle.sh`). The feed is `site/appcast.xml` on the product
site; updates are signed with an EdDSA key kept in the keychain
(`target/sparkle/bin/generate_keys --account com.rrramirez.visor`). To release:
bump the version in `Cargo.toml`, add the release notes to
`CHANGELOG.md`, then `scripts/release-macos.sh --publish`.

### Linux

Needs FFmpeg 7, PipeWire, GTK 3 and appindicator development packages (see
`docker/linux.Dockerfile` for the exact Debian list). To build and test in a
container:

```sh
scripts/linux-check.sh
```

### Windows

Needs an FFmpeg 7.1 shared development build (`FFMPEG_DIR`) and the MSVC
toolchain; then `cargo build -p visor`.

## Developer tools

`visor-cli` records without the UI, which is handy for testing capture:

```sh
cargo run -p visor-cli -- list
cargo run -p visor-cli -- record --mic default --system-audio --camera default --out /tmp/rec
```

Type `p` + Enter to pause/resume, Enter to stop.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
