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
| macOS capture      | ScreenCaptureKit via `screencapturekit`   | Display, window, app and area (`sourceRect`), system audio, per-app audio (app-filtered content filter), and the mic (macOS 15+ `captureMicrophone`; `cpal` on older systems). |
| Windows capture    | Windows.Graphics.Capture via `windows-capture` | Window, display, area (crop). Audio: WASAPI loopback (`wasapi`); per-app audio uses process loopback (Win10 2004+). |
| Linux capture      | xdg-desktop-portal ScreenCast (`ashpd`) + PipeWire (`pipewire`) | Works on both Wayland and X11 desktops with a portal. The portal's picker selects window/display (an App target records one window the user picks); Area crops the chosen monitor. System audio comes from PipeWire directly: the default sink monitor, or per-app by linking the app's output stream nodes (matched by `application.process.id`, including child processes) to Visor's capture stream. See `crates/visor-capture/src/linux.rs`. |
| Camera             | `nokhwa`                                 | AVFoundation / Media Foundation / V4L2. |
| Microphone (fallback) | `cpal`                                | CoreAudio / WASAPI / ALSA-PulseAudio-PipeWire. |
| Serialization      | `serde` + `serde_json`                    | `project.json`. |

Prior art worth reading (not dependencies): **OBS Studio** (capture backends,
libobs), **Cap** (open source Loom alternative, Rust), **Kooha** (GNOME,
portal + GStreamer).

## Architecture

```
┌─────────────────────────── visor (Slint app) ───────────────────────────┐
│ tray menu · source picker · area selector · countdown · control bar ·   │
│ camera bubble · editor (preview + timeline)                              │
└───────────────┬──────────────────────────────────────┬───────────────────┘
                │ RecorderHandle (start/pause/resume/stop)│ Project / EditList
┌───────────────▼──────────────┐        ┌───────────────▼──────────────────┐
│ visor-capture                │        │ visor-media                      │
│  trait ScreenSource          │ frames │  TrackWriter (encode + mux)      │
│  trait AudioSource           ├───────►│  Player (decode for preview)     │
│  trait CameraSource          │ +pts   │  Exporter (filter graph → file)  │
│  macos/ windows/ linux/      │        │                                  │
└───────────────┬──────────────┘        └───────────────┬──────────────────┘
                └─────────────── visor-core ────────────┘
          types: Source, RecordingConfig, Project, EditList, MediaClock
```

### Crates

- `visor-core`: platform-agnostic types and logic. Includes `Project`
  (serialized to `project.json`), `EditList` (kept ranges; cut and trim
  operations), `MediaClock` (pause-aware timestamps), and `RecordingConfig`.
  Pure Rust, unit-tested, with no FFI.
- `visor-capture`: capture traits and per-OS backends behind `cfg(target_os)`.
  Backends push timestamped frames and audio buffers into channels.
- `visor-media`: FFmpeg wrapper. Covers per-track writers used while recording,
  a decoder/player for the editor preview, and the exporter.
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
   **excluded from capture**: an `SCContentFilter` that excludes Visor's windows
   on macOS, and `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` on Windows.
   On Linux the portal can't exclude windows, so the bubble is hidden while
   recording a display there.
5. On stop, the writers flush and `project.json` is written.

### Project folder

```
~/Movies/Visor/2026-10-19 18.42 Untitled/
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

- The preview player decodes `screen.mp4` (and `camera.mp4`) with FFmpeg,
  uploads frames into a `slint::Image`, and plays mixed audio through `cpal`.
  The camera bubble is composited in the UI for the preview.
- The timeline shows a waveform (from the mic track) and thumbnails, with
  in/out handles for trimming and split-and-delete for cutting middle sections.
- Edits only mutate `EditList`. **Raw tracks are never modified.**
- Undo/redo works through snapshots of the `EditList`.

### Export

This is a single FFmpeg filter graph built from the project:

- Each kept range becomes `trim`/`atrim` → `setpts`, and the ranges are
  `concat`enated.
- Camera: `scale` → circular `alphamerge` mask → `overlay` at the layout
  position.
- Audio: mic + system → `amix` (with per-track gain).
- Encoders by target:
  - **MP4**: H.264 (VideoToolbox / NVENC / QSV / VAAPI, falling back to x264)
    + AAC, `+faststart`.
  - **WebM**: VP9 (libvpx-vp9) or AV1 (SVT-AV1) + Opus.
  - **GIF**: `fps=15,scale=…,split[a][b];[a]palettegen[p];[b][p]paletteuse`,
    with no audio.

Export runs on a worker thread, reports progress to the UI, and can be
cancelled.

### UI surfaces (Slint)

1. **Tray / menu bar menu**: New recording, Open recordings folder, recent
   recordings, Preferences, Quit.
2. **Recorder panel**: pick the source (Display / Window / App / Area), camera
   device, mic device, system audio on/off, per-app audio target. Start.
3. **Area selector**: full-screen transparent overlay to drag a rectangle,
   with resize handles and dimensions shown.
4. **Countdown**: a 3-2-1 overlay, which can be skipped.
5. **Floating control bar**: timer, pause/resume, stop, discard. Always on
   top and excluded from capture.
6. **Camera bubble**: live camera preview, draggable and resizable. Its final
   position seeds `camera_overlay`.
7. **Editor window**: preview, timeline, export dialog.

### Permissions (macOS)

Visor needs Screen Recording, Camera and Microphone permission (TCC). Visor
ships as a signed `.app` with `NSCameraUsageDescription` and
`NSMicrophoneUsageDescription`. During development, permissions are granted to
the terminal that runs `cargo run`. Visor detects a denied permission and
deep-links to System Settings.

## Milestones

| #  | Milestone | Done when |
|----|-----------|-----------|
| M0 | **Spike**: workspace, `visor-core` types, `visor-cli` records the main display to MP4 on macOS | `visor-cli record` produces a playable `screen.mp4` |
| M1 | Recording core (macOS): window/app/area, pause/resume, mic + system + per-app audio, camera track, project folder + `project.json` | All tracks play in sync after pause/resume cycles |
| M2 | App shell: tray, recorder panel, area selector, countdown, control bar, camera bubble, capture exclusion | Full record flow from the menu bar without a terminal |
| M3 | Editor + export: player, timeline, trim/cut, export MP4/WebM/GIF | A trimmed recording exports correctly in all three formats |
| M4 | Windows backend (WGC + WASAPI) | M1–M3 pass on Windows 10/11 |
| M5 | Linux backend (portal + PipeWire) | M1–M3 pass on GNOME/KDE Wayland |
| M6 | Packaging: signed/notarized `.app`/`.dmg`, MSI, Flatpak/AppImage; bundled FFmpeg | Install from a release artifact on a clean machine |

## Open questions

- Is a minimum macOS version of **13** acceptable? It is required for
  ScreenCaptureKit audio. Mic capture through ScreenCaptureKit needs 15, with
  `cpal` as the fallback.
- Bundle FFmpeg statically or dynamically? Dynamic is easier for development;
  the release choice can wait until M6.
- Linux per-app audio links PipeWire nodes outside the portal, which needs
  direct PipeWire access (not available in a strict Flatpak sandbox).
