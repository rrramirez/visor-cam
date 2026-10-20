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
   With several displays, the chosen one is outlined in red.
4. Use the floating control bar (or the menu bar menu) to pause, resume, stop
   or discard. The control bar and camera bubble never appear in the recording.
5. Stopping opens the editor:
   - **Space** play/pause, **← →** step a frame (**⇧** for 10), click the
     timeline to seek, drag on it to select.
   - **I / O** set the selection from the playhead, **⌫** cut the selection,
     **T** trim to it, **⌘Z / ⇧⌘Z** undo/redo, *Restore all cuts* to start over.
   - **Speed** (0.5×–2×) speeds up or slows down the whole video; speech
     keeps its pitch. The preview plays at that speed.
   - Drag, resize, reshape, mirror or hide the camera bubble. It starts where
     it was on screen while recording, mirrored as you saw it.
   - **Export** to MP4, WebM (VP9 or AV1) or GIF, at original size, 1080p or
     720p. Files go to the recording's `exports/` folder.

Recordings are saved to `~/Movies/Visor/<date time> Recording/` (configurable
in Preferences) as separate tracks plus `project.json`, so edits are
non-destructive and you can always re-export.

Command line: `visor --edit <recording folder>` opens a recording;
`visor --record [--stop-after SECONDS]` starts recording with the saved
preferences as soon as the saved camera is ready, optionally stopping after
that many seconds of recorded time.

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

The `.app` is ad-hoc signed (not notarized): on another Mac, right-click it and
choose *Open* the first time. macOS ties permissions to the signature, so after
rebuilding you may be asked for Screen Recording permission again.

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
