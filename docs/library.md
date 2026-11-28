# Recordings window

The Recordings window lists every recording in the save folder, to open in the editor,
rename, show in the file manager or move to the Trash (`crates/visor/src/library.rs`,
`ui/library.slint`, `app/recordings.rs`; `visor-core` `library.rs`; `visor-media`
`library.rs` for thumbnails).

## How it works

**Listing.** `visor_core::library::entries(root)` scans the save folder, by its real path
(`project::real_path`, as editors and agents know folders), (`list_projects`:
folders with a `project.json`, newest first by folder creation time) and reads each
`project.json` into an `Entry`: its title (the folder's name when it has none), when it was
recorded and last changed, and, if it could be read, its recorded length, its length as
exported (`Project::output_duration`) and its marker count. There's no database; a scan of a
few hundred folders is fast.

**Opening it.** The recorder panel's *Recordings* button, the menu bar's *Recordings…*, and
`visor --library` (handed to a running Visor like `--edit`, as `Request::Library`) call
`App::open_library`. There's one window (`App::library`); while it shows, Visor has a Dock
icon (`update_dock`).

**Staying current.** `App::library_changed` lists again when the window shows. It runs from
`refresh_tray`, which every change to recordings already calls (a recording saved, a folder
renamed, an editor closed, Preferences' save folder), when an editor opens, and when the
window gains focus (winit's `Focused`), which picks up changes made in Finder. Cards are
updated row by row (`set_row_data`), not replaced, so a double-click lands on the card it
started on, and the selection follows recordings by folder across a refresh.

**Thumbnails.** `visor_media::library::cached_thumbnail` renders the recording's thumbnail
frame as the export shows it (`export::thumbnail_time`, `frame_jpeg`: blur and camera bubble
included) as a JPEG in the user's cache folder (`<cache>/Visor/thumbnails`), named by a hash of
the folder and of `project.json`'s modification time and size. An edit makes a new name, and
the older ones for that folder are removed; `prune_thumbnails` drops those of recordings no
longer listed. One background thread renders them, one at a time; cards show without a
thumbnail until theirs is ready.

**In use.** `App::busy_for` tells whether a recording is being recorded (or recorded more
onto), open in an editor, or being added to (`App::copies`). Cards say *Recording* or *Open*.
One being recorded doesn't open (`open_refusal`): its `project.json` isn't finished, and
opening would run crash recovery on files still being written.
Renaming a recording open in an editor goes through that editor's name
(`EditorWindow::rename`; the folder follows when it closes); otherwise the title is saved
and the folder renamed at once (`rename_to_title`). The window keeps which recording is
being renamed by folder (`LibraryWindow::renaming`), so a refresh that moves cards doesn't
move the edit; a click on the selected card's name starts it after a moment, so a
double-click opens instead. Moving to the Trash (the `trash` crate; on macOS through
`NSFileManager`, not by asking Finder, which a signed app may not do) asks first
(`trash_question`) and is refused for any recording in use (`trash_refusal`).

**Selecting.** `library::Selection` keeps the selected cards and the last one clicked: a
click picks one, ⌘-click toggles, ⇧-click picks a range from the last one clicked (as Finder
does), arrow keys move by one or by a row (`columns`, computed in Slint from the width).

## Invariants

- Nothing is written into recording folders except by an explicit rename; thumbnails live
  in the cache folder.
- A recording in use is never moved to the Trash: its editor, recorder or copy would go on
  writing to a folder that's gone.

## Gotchas

- Replacing the card model on every click (a new `VecModel`) recreates each card's
  `TouchArea`, so the second click of a double-click never reaches the first one's.
- Compare folders by `project::real_path`, not as configured nor by plain `canonicalize`: a
  save folder reached through a link differs from its real path, and on Windows
  `canonicalize` adds a `\\?\` prefix that Explorer (`platform::reveal`) doesn't take.
- `rfd` confirmations shown while Visor isn't the active app come up as a system alert
  (UserNotificationCenter), outside Visor's windows.
- ⌘ and ⇧ on clicks come from winit's modifier state (key events), not from the click.

## Tests

`visor-core` `library` (listing, unreadable projects), `visor-media` `tests/library.rs`
(rendered once, re-rendered after an edit, pruned), `visor` `library` (selection, trash
refusals and question), `instance` (`--library` handed over), `cli` (`visor list`). The
window itself is checked live with the `ui-test` skill.
