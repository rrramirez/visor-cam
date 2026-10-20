# Visor — Design

Visor is an offline, open source screen recorder in the spirit of Loom: record a
screen, window, app or area together with camera and audio, pause and resume,
trim the result, and export a file. Nothing leaves the machine. There are no
accounts, no cloud, no link sharing and no asset library.

License: **GPL-3.0-or-later**.

## Goals

- Capture sources: full display, a single window, a single application (all its
  windows), or a user-drawn area.
- Audio: microphone, system audio, and per-application audio.
- Camera: recorded as its own track and shown live as a floating bubble
  (excluded from the screen capture).
- Pause / resume with no gaps or desync in the output.
- Simple editor: trim start/end, cut out middle sections, preview, export.
- Export: MP4 (H.264 + AAC), WebM (VP9 or AV1 + Opus), GIF.
- Cross-platform native UI with **no Electron and no Chromium/webview**.
- macOS first, then Windows, then Linux.

## Non-goals

- Uploading, sharing links, accounts, comments, analytics.
- Asset management or a recordings library database. Recordings are plain
  folders on disk.
- Multi-track NLE features: transitions, titles, effects. The editor stays
  small.

## Stack

| Concern            | Choice                                   | Notes |
|--------------------|------------------------------------------|-------|
| Language           | Rust (stable)                            | One language for capture, media and UI. |
| UI                 | [Slint](https://slint.dev) (GPLv3 option) | Native rendering (femtovg/Skia), no webview. Transparent, frameless, always-on-top windows for the overlay, bubble and control bar. |
| Tray / menu bar    | `tray-icon` + `muda`                      | Same crates Tauri uses, but standalone. |
| Encode / decode / export | FFmpeg (libav*) via `ffmpeg-next`  | Hardware encoders: VideoToolbox (macOS), NVENC/QSV/AMF (Windows), VAAPI (Linux). x264 / libvpx / SVT-AV1 software fallbacks. GPL build is fine because Visor is GPL. |
| macOS capture      | ScreenCaptureKit via `screencapturekit`   | Display, window, app and area (`sourceRect`). System and per-app audio come from a second, audio-only stream with its own content filter (all apps except Visor, or one app). Minimum macOS 14. |
| Windows capture    | Windows.Graphics.Capture via `windows-capture` | Display, window, area (GPU crop); an App target records the app's largest window. Audio: WASAPI process loopback (`wasapi`) — all apps except Visor, or one app (Win10 2004+). See `crates/visor-capture/src/windows.rs`. |
| Linux capture      | xdg-desktop-portal ScreenCast (`ashpd`) + PipeWire (`pipewire`) | Works on both Wayland and X11 desktops with a portal. The portal's picker selects window/display (an App target records one window the user picks); Area crops the chosen monitor. System audio comes from PipeWire directly: the default sink monitor, or per-app by linking the app's output stream nodes (matched by `application.process.id`, including child processes) to Visor's capture stream. See `crates/visor-capture/src/linux.rs`. |
| Camera             | FFmpeg input devices (`avfoundation` / `dshow` / `v4l2`) | One cross-platform code path through the FFmpeg we already link; frames converted to NV12. macOS lists devices via AVFoundation. |
| Microphone         | `cpal`                                   | CoreAudio / WASAPI / ALSA-PulseAudio-PipeWire. The same stream feeds the panel's level meter and the recorder. |
| Settings           | JSON in the OS config dir (`dirs`)       | `~/Library/Application Support/Visor/settings.json` on macOS. |
| Serialization      | `serde` + `serde_json`                    | `project.json`. |

Prior art worth reading (not dependencies): **OBS Studio** (capture backends,
libobs), **Cap** (open source Loom alternative, Rust), **Kooha** (GNOME,
portal + GStreamer).

## Architecture

```
┌─────────────────────────── visor (Slint app) ───────────────────────────┐
│ tray menu · recorder panel · area selector · countdown · control bar ·  │
│ camera bubble · editor (player + timeline + export) · preferences        │
└──────────────┬──────────────────────────────────────────┬────────────────┘
               │ Recorder (start/pause/resume/stop)        │ Player / export
┌──────────────▼───────────────┐                           │
│ visor-recorder               │  one writer thread/track  │
│  pause-aware MediaClock      ├──────────┐                │
└──────────────┬───────────────┘          │                │
┌──────────────▼───────────────┐ ┌────────▼────────────────▼────────────────┐
│ visor-capture                │ │ visor-media                              │
│  Backend: screen + system    │ │  VideoWriter / AudioWriter (recording)   │
│  audio (macos/windows/linux) │ │  VideoReader / AudioReader (decode)      │
│  MicStream (cpal)            │ │  Player (edit-aware preview)             │
│  CameraStream (FFmpeg dev.)  │ │  export (MP4 / WebM / GIF)               │
└──────────────┬───────────────┘ └────────┬─────────────────────────────────┘
               └──────────── visor-core ──┘
     types: CaptureTarget, RecordingConfig, Project, EditList, MediaClock
```

### Crates

- `visor-core`: platform-agnostic types and logic. Includes `Project`
  (serialized to `project.json`), `EditList` (kept ranges; cut and trim
  operations), `MediaClock` (pause-aware timestamps), and `RecordingConfig`.
  Pure Rust, unit-tested, with no FFI.
- `visor-capture`: capture traits and per-OS backends behind `cfg(target_os)`.
  Backends push timestamped frames and audio buffers into channels.
- `visor-media`: FFmpeg wrapper. Covers per-track writers used while recording,
  decoders, the editor's preview player, thumbnails/waveforms, and export.
- `visor-recorder`: the recording engine shared by the app and the CLI. Starts
  capture, subscribes to the mic/camera streams, and runs one writer thread
  per track on a shared pause-aware clock. Can start paused so capture warms
  up during the countdown.
- `visor-cli`: headless recorder for development and testing
  (`visor-cli record --display 0 --mic --out ./rec`).
- `visor` (app): the Slint UI binary.

### Recording pipeline

1. On start, a single `MediaClock` anchors t0 to the host monotonic clock
   (`mach_absolute_time` / `QueryPerformanceCounter` / `CLOCK_MONOTONIC`).
   ScreenCaptureKit, WGC and PipeWire buffers already carry host-clock
   timestamps, so all tracks share one timebase.
2. Each source writes **its own track file** in the project folder:
   - `screen.mp4`: H.264 (hardware), fragmented MP4 so that a crash still
     leaves a playable file.
   - `camera.mp4`: H.264, camera resolution.
   - `mic.m4a`, `system.m4a`: AAC (mic and system kept separate so they can be
     re-balanced or muted later).
3. **Pause / resume:** capture keeps running, but buffers that arrive while
   paused are dropped. `MediaClock` accumulates paused time, and every written
   PTS is `host_ts - t0 - paused_total`. The tracks stay gapless and in sync,
   and resume needs no encoder restart (video gets a forced keyframe on resume).
4. Visor's own windows (the control bar, the camera bubble, the countdown) are
   **excluded from capture**: they are content-protected (`NSWindowSharingNone`
   / `WDA_EXCLUDEFROMCAPTURE` via winit), and on macOS the `SCContentFilter`
   also excludes Visor's app. On Linux the portal can't exclude windows, so
   the bubble is hidden while recording there.
5. Video frames are only delivered when the screen changes, so the writer holds
   the last frame to the stop time; audio writers pad silence. Every track
   therefore starts at 0 and ends at the recording's duration.
6. On stop, the writers flush and `project.json` is written atomically.

### Project folder

```
~/Movies/Visor/2026-10-19 18.42.07 Recording/
  project.json      # sources, track metadata, edit list, camera overlay layout
  screen.mp4
  camera.mp4        # optional
  mic.m4a           # optional
  system.m4a        # optional
  exports/
    Untitled.mp4
```

The default root is `~/Movies/Visor` on macOS, `%USERPROFILE%\Videos\Visor` on
Windows, and `$XDG_VIDEOS_DIR/Visor` on Linux. The root is configurable. There
is no database: the recordings list is a directory scan.

`project.json` (v1):

```json
{
  "version": 1,
  "title": "Untitled",
  "created_at": "2026-10-19T18:42:00Z",
  "duration_us": 93500000,
  "tracks": {
    "screen": { "file": "screen.mp4", "width": 2880, "height": 1800, "fps": 60 },
    "camera": { "file": "camera.mp4", "width": 1280, "height": 720, "fps": 30 },
    "mic":    { "file": "mic.m4a" },
    "system": { "file": "system.m4a" }
  },
  "edit": { "keep": [[0, 12000000], [15500000, 93500000]] },
  "camera_overlay": { "visible": true, "shape": "circle", "x": 0.03, "y": 0.75, "size": 0.2 }
}
```

### Editor

- The preview player (`visor_media::player`) decodes `screen.mp4` and
  `camera.mp4` with FFmpeg into RGBA at preview size on a worker thread; the
  UI polls for new frames at 60 Hz and shows them as `slint::Image`s. Mic and
  system audio are mixed and played through `cpal`, following the same clock.
  Positions are in source time; while playing, the player walks the output
  timeline and skips cut regions.
- The camera bubble is composited in the UI for the preview and can be dragged,
  resized, reshaped (circle / rounded square) or hidden.
- The timeline shows the *source* recording: a filmstrip, a waveform (mic +
  system), cut regions dimmed, a draggable selection and the playhead.
  Selection → Cut / Trim; Restore brings everything back.
- Edits only mutate `EditList`. **Raw tracks are never modified.**
- Undo/redo works through snapshots of the `EditList`.

### Export

Export (`visor_media::export`) renders frames at a constant rate along the
*output* timeline and maps each back to source time through the edit list, so
any number of cuts stay frame-accurate for video and sample-accurate for audio.
(Building one filter graph with `trim`/`concat` was the first plan, but `split`
buffers whole segments of raw frames in memory.)

- Camera: scaled, center-cropped and alpha-blended through an anti-aliased
  circle / rounded-square mask, in Rust, on YUV 4:2:0.
- Audio: mic + system mixed sample-accurately.
- Encoders by target:
  - **MP4**: H.264 (x264 `crf 22`, falling back to VideoToolbox / NVENC / QSV
    / VAAPI) + AAC, `+faststart`.
  - **WebM**: VP9 (libvpx-vp9) or AV1 (SVT-AV1, libaom fallback) + Opus.
  - **GIF**: two passes through libavfilter — `palettegen` over every frame,
    then `paletteuse` — at 15 fps, up to 960 px wide, no audio.
- Optional downscale to 1080p / 720p.

Export runs on a worker thread, reports progress to the UI, and can be
cancelled (a partial file is deleted).

### UI surfaces (Slint)

1. **Tray / menu bar menu**: New recording, Open recordings folder, recent
   recordings, Preferences, Quit.
2. **Recorder panel**: pick the source (Display / Window / App / Area), camera
   device, mic device, system audio on/off, per-app audio target. Start.
3. **Area selector**: full-screen transparent overlay to drag (and move) a
   rectangle, with its pixel size shown; Enter confirms, Esc cancels.
4. **Countdown**: a 3-2-1 overlay, which can be skipped.
5. **Floating control bar**: timer, pause/resume, stop, discard. Always on
   top and excluded from capture.
6. **Camera bubble**: live camera preview, draggable and resizable. Its final
   position seeds `camera_overlay`.
7. **Editor window**: preview, timeline, edit tools, export.
8. **Preferences**: save folder, frame rate, countdown, cursor, default export
   format.

### Permissions (macOS)

Visor needs Screen Recording, Camera and Microphone permission (TCC). Visor
ships as an ad-hoc signed `.app` (`scripts/package-macos.sh`) with
`NSCameraUsageDescription`, `NSMicrophoneUsageDescription` and `LSUIElement`
(menu-bar app). During development, permissions are granted to the terminal
that runs `cargo run`. Visor asks for camera/mic access when a device is
chosen, shows a banner when Screen Recording is missing, and deep-links to
System Settings. Ad-hoc signatures change on every build, so macOS may ask
again after rebuilding.

## Milestones

| #  | Milestone | Status |
|----|-----------|--------|
| M0 | Spike: workspace, core types, `visor-cli` records the main display to MP4 | Done |
| M1 | Recording core: window/app/area, pause/resume, mic + system + per-app audio, camera track, project folder | Done (macOS verified; mic/camera recording verified through tests and device listing) |
| M2 | App shell: tray, recorder panel, area selector, countdown, control bar, camera bubble, capture exclusion | Done (macOS) |
| M3 | Editor + export: player, timeline, trim/cut, export MP4/WebM/GIF | Done (macOS) |
| M4 | Windows backend (WGC + WASAPI) | Written; cross-compiles and lints clean; never run on Windows |
| M5 | Linux backend (portal + PipeWire) | Written; builds, tests and lints clean in Docker (`scripts/linux-check.sh`); never run on a desktop |
| M6 | Packaging | macOS: ad-hoc signed `.app` + `.dmg` with FFmpeg bundled. Windows/Linux installers not started |

## Known limitations and next steps

- Windows and Linux backends need testing on real machines.
- Notarization (Developer ID) isn't set up; the DMG is ad-hoc signed.
- The bundled FFmpeg is Homebrew's full build plus its dependencies (large).
  A minimal FFmpeg build (H.264/AAC/VP9/AV1/Opus/GIF only) would shrink the app.
- App capture on Windows and Linux records one window, not every window of
  the app.
- Linux per-app audio links PipeWire nodes outside the portal, which needs
  direct PipeWire access (not available in a strict Flatpak sandbox).
- No global hotkeys and no per-track volume in the editor yet (not chosen for v1).
