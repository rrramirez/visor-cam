---
name: ui-test
description: Test Visor's UI by hand, as a person would, on macOS. Launch a test instance with its own settings and recordings, click, type, scroll, drag and drop files onto it, and screenshot only its windows. Use for the live check in the `feature` workflow, for "run the manual tests yourself", or whenever a change shows up in the editor, recorder panel or menus and automated tests can't reach it (menus, pickers, drag and drop, keyboard shortcuts, progress, focus).
---

# UI testing on macOS

Automated tests stop at the Slint UI. This skill covers what's left: driving the real app
with mouse and keyboard and looking at it. Everything goes through
`.claude/skills/ui-test/ui.sh` (run it from the repo root; `ui.sh help` lists commands).

## Ground rules

- **Only the test instance.** `ui.sh launch` starts Visor with its own home folder
  (`target/ui-test/home`, gitignored): its own settings (agent control on, global shortcuts
  off), recordings and agent port. Never drive, quit or read the user's own Visor or
  `~/Movies/Visor`. `ui.sh stop` kills only processes started with the test home.
- **Screenshots of its windows only:** `ui.sh shot <window> <file>`. Never capture the whole
  screen: it shows whatever else the user has open.
- **Tell the user first.** The mouse moves on its own while a test runs, so they should keep
  their hands off. Ask before the first run in a session, and say when it's done.
- If input isn't allowed (below), don't look for a way around it. Test what the CLI and
  agent tools reach (`ui.sh cli …`, the `visor-dev` MCP tools), and hand the rest to the
  user as a checklist.

## 1. Permission

```bash
.claude/skills/ui-test/ui.sh check     # true: input works
```

`false` means macOS blocks this session from sending input. The user has to turn on
Accessibility for the app hosting the session (their terminal, e.g. Warp or Terminal).
`ui.sh prompt` asks macOS to show its own request, which leads to the right setting in any
macOS version. Some versions don't show an "Accessibility" pane by that name. After they
allow it, run `check` again; the terminal may need a restart.

## 2. Set up

```bash
U=.claude/skills/ui-test/ui.sh
d=$($U sample Demo 20)                                 # a 20 s recording (screen + mic)
$U video target/ui-test/IMG_0001.MOV 3 640x360 --rotate -90   # a phone video, held upright
$U video target/ui-test/quiet.mp4 2 320x240 --silent
$U launch --edit "$d"           # debug build; --dmg runs dist/Visor.app (build it first)
$U windows                      # id x y w h layer title, e.g. 8770 274 74 1180 832 0 Visor — Demo
```

Prefer the packaged app: `VISOR_NOTARY_PROFILE=skip scripts/package-macos.sh` (a couple of
minutes; skips notarizing), then `launch --dmg`. It's what ships (its own FFmpeg decoders
matter for imported videos), and macOS lets System Events bring an app bundle forward, which
it often refuses for the bare debug binary. For a hand-made project with takes, markers or
blurs, edit its `project.json` before launching.

Open a window that can come forward: `--edit <folder>` or `--library`. With only the
recorder panel showing, Visor is a menu bar app: it can't be brought forward, and clicks on
the panel don't register.

## 3. Drive and look

**Other windows in the way.** `click`, `dclick`, `drag` and `scroll` refuse unless the window
under the point is the test instance's (or the drag source's), and `key`/`type` refuse unless
it's the frontmost app, so input never lands in the user's apps. When `front` can't bring
Visor forward over a window covering it, ask the user: hide that app (System Events
`set visible of process "…" to false`, and show it again after), or let them move it. Check
`swift`'s window order (`CGWindowListCopyWindowInfo`) after hiding one app: the next one
behind may now cover the test window. Don't work around a refusal by calling
`target/ui-test/input` directly.

```bash
$U front                        # before every input: the terminal takes focus back after each command
$U click 1269 236               # screen points; window x/y from `windows`
$U key 6 cmd                    # ⌘Z (keycodes: z 6, a 0, g 5, space 49, return 36, esc 53, ← 123, → 124)
$U key 6 cmd,shift              # ⇧⌘Z
$U type "/path/to/file"         # text
$U scroll 1318 369 -200         # sidebar down 200 px; positive scrolls up
$U shot 8770 target/ui-test/after-undo.png   # then Read the PNG
```

**Finding where to click.** Shot images are resized (1000 px wide by default). A point in the
image maps to the screen as `window_x + px * window_w / 1000` (and the same for y with the
window's height scale). Take a shot after every step and check that it did what you expected
before going on.

**Checking the result.** Don't trust the picture alone. Read `project.json` in the test
recording (duration, `edit.keep`, takes, markers), list its folder, or ask through the CLI
(`ui.sh cli status`, `ui.sh cli frame 12 --out x.jpg --folder "$d"`).

**File pickers** (NSOpenPanel): `key 5 cmd,shift` (Go to folder), `type` the folder path,
`key 36`. To pick several files, click one and `click X Y cmd` the others. ⌘A does nothing
there, because Visor has no Edit menu. Find the panel with `windows`: it's titled "Open".

**Drag and drop:** `$U dragsrc FILE...` opens a small window ("Visor drag source") whose
contents drag those files or folders, and prints its `id x y w h`. Drag from its middle to
the editor with `$U drag x1 y1 x2 y2`. The drag moves slowly on purpose, so AppKit starts a
drag session.

**Timing:** to catch something brief (a progress label), take several shots in a loop right
after the action. For work that takes a while, make the input big (e.g. pad a video with
`dd if=/dev/zero bs=1m count=6000 >> file.mp4`; `ui.sh video` writes its index at the
front, so the padded file still reads fine) and delete it afterwards.

## Gotchas

- Clicking the preview toggles playback; click the title bar or the timeline instead.
- ⌘ shortcuts only work with real modifier presses, which `key` sends; flags alone are ignored.
- Popups (the *Add video…* menu) are part of the editor window, so `shot` of the editor shows
  them. Pickers and the countdown are separate windows: `windows` lists them, and the
  countdown isn't on layer 0.
- `launch` refuses while a test instance runs. `stop` first, and `stop` at the end.
- Visor reads ⌘ and ⇧ for clicks from key presses (winit), not from the click: ⌘-click and
  ⇧-click selection only work while Visor is the active app and `key` can press the modifier.
  If it isn't active, cover multi-selection with unit tests and list it under **To test**.
- Confirmations (`rfd::MessageDialog`) shown while Visor isn't the active app come up as a
  system alert owned by UserNotificationCenter, not Visor, so the click guard refuses them.
  Answer them by hand, or test from a state where Visor is active.
- A window that rebuilds its model on every click (`set_cards` with a new model) never sees a
  double-click: the second click lands on a new element. Update rows in place.

## 4. Report

Add what you checked, and what you couldn't, to the feature note's test scenarios and to the
PR description's **Live** section. Send the user one or two telling screenshots with
SendUserFile. Clean up with `ui.sh stop`. Leave `target/ui-test/` (it's ignored), but
delete big files you made.
