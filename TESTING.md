# Testing Visor

Thanks for trying Visor early. It's a screen recorder that keeps everything
on your computer: record your screen, camera and microphone, trim it, and
export a video. Nothing is uploaded.

This is a pre-release build, so things will break. What helps most: try
the steps below, and when something goes wrong, send the log file (see
"Sending a report") with a sentence on what you did.

## Install

### Mac (Apple Silicon, macOS 14 or later)

1. Open `Visor-<version>.dmg` and drag **Visor** to **Applications**.
2. Open Visor and confirm that you want to open an app downloaded from the
   internet. (Visor is signed and notarized by Apple.)
3. Visor lives in the menu bar. When asked, allow **Screen Recording**
   (System Settings may need you to quit and reopen Visor), then camera and
   microphone when you pick them.

### Windows (10 version 2004 or later, 11)

1. Run `Visor-Setup-<version>.exe`. Windows SmartScreen may say "Windows
   protected your PC": click **More info › Run anyway** (the installer
   isn't signed yet).
2. The installer needs no administrator rights. Finish it and launch Visor.
3. Visor lives in the system tray (bottom right; it may be under the ^
   arrow). If your camera or microphone doesn't show up or stays silent,
   check **Settings › Privacy & security › Camera** (and **Microphone**)
   and turn on **Let desktop apps access your camera** (microphone).

## What to try

- [ ] Record the whole screen for ~30 seconds with your microphone.
- [ ] Record again with your camera on; drag the camera bubble around first.
- [ ] Pause and resume during a recording.
- [ ] Record one window, then an area you drag out.
- [ ] System audio: play a video while recording with *System audio* on.
- [ ] In the editor: cut a section, trim the ends, try **Speed** at 1.5×.
- [ ] Export MP4 at 1080p and play the result. Do picture and sound line up?
- [ ] Export a GIF of a short clip.
- [ ] A longer recording (10+ minutes), then export.
- [ ] Mac only: menu bar › **Check for Updates…** (it should say you're up to date).

Windows is the least tested platform: this is the first time Visor runs
there. If recording doesn't start at all, that report alone is very useful.

## Sending a report

Send the log and a short description (what you did, what you expected,
what happened). Screenshots help.

| | Log file | Recordings |
|---|---|---|
| Mac | `~/Library/Logs/Visor/visor.log` | `~/Movies/Visor` |
| Windows | `%LOCALAPPDATA%\Visor\visor.log` | `%USERPROFILE%\Videos\Visor` |

On Windows, paste `%LOCALAPPDATA%\Visor` into the Explorer address bar to
open the log's folder.

The log contains file paths and device names (like "MacBook Pro
Microphone"), but no recordings and nothing you typed.

## Uninstall

- Mac: quit Visor from the menu bar, drag it from Applications to the Trash.
- Windows: Settings › Apps › Installed apps › Visor › Uninstall.

Recordings stay where they were saved until you delete them.
