# Edit model

Edits never touch the recorded tracks. Everything the editor, an agent or the
CLI changes about which parts of a recording play, and in what order, lives in
`EditList` (`crates/visor-core/src/edit.rs`), saved in `project.json` as
`"edit": {"keep": [[start, end], ...]}` in microseconds.

## How it works

**Clips.** Each kept range is a clip: a half-open `Range(start, end)` of
*source* time, the time in the recording. Clips play in the list's order,
which need not be the recording's: `EditList::split` cuts a clip in two at a
source time, and `EditList::move_clip` plays a clip elsewhere. Two clips split
from one touch (`[0, 5)`, `[5, 10)`) and stay separate clips.

**Two timelines.** *Source time* is where something is in the recording;
markers, blurs, the thumbnail and agent tool arguments use it. *Output time*
is where it plays in the edited video, before speed. The mappings:

| Function | Maps |
|---|---|
| `to_source(out)` | output → source; `None` past the end |
| `to_output(src)` | source → output; `None` if cut |
| `kept_before(src)` | source → output, and inside a cut, the output time of the next kept part *in the recording* (the end of the video if none) |
| `clip_span(i)` | where clip `i` plays, in output time |
| `pieces(a, b)` | the source ranges that play over output `[a, b)`, in play order |

`Project::output_time` divides `kept_before` by the speed: that is the time
in the exported file, which clocks, labels and chapters show.

**Operations.** In source time: `cut`, `trim`, and the agent variants
`cut_joining` / `trim_joining`, which also drop what's left next to the cut
if it plays for less than `SLIVER` (0.25 s), counting the clips it plays
straight on with (split but not moved apart) as one. In output time, for the editor's selection:
`cut_output`, `trim_output`. `with_clip_edge_moved` moves one edge of a clip:
out as far as the nearest other clip's part of the recording (or either end of
it), in until the clip is gone. Nothing merges clips, so a cut inside a clip
leaves a clip on each side.

**Chapters.** `Project::chapters` starts one at each marker, at its output
time, sorted by when it plays, so moved clips reorder them.

## Invariants

- Clips are non-empty and don't overlap in source time, in any order
  (`EditList::is_valid`, checked when `project.json` is loaded). Overlap would
  make `to_output` ambiguous: a marker or blur would belong to two places.
- Only the clip list holds the order. Anything that needs "the first kept
  frame" means the first clip (`ranges().first()`); anything that needs "the
  next kept time after a cut" means the next in the recording (the smallest
  `r.0` after it), as `kept_before` and the player's `snap_to_kept` do.
- Operations that remove nothing new change nothing (`cut_joining` keeps short
  pieces then), and the last clip always stays.

## Gotchas

- Code written when clips were always in recording order used
  `partition_point`, `ranges().first().0` as the earliest kept time, or summed
  `clamp`s for `kept_before`. With moved clips those give wrong answers
  without failing. Use the recording-order helpers instead.
- Saved projects with moved clips don't load in Visor 0.8.0 and earlier, whose
  `is_valid` required sorted ranges.

## Tests

Unit tests in `edit.rs` cover split and move, clip edges, output-time cuts
and trims, and joins on reordered clips. `visor-media/tests/export.rs`
(`moved_clips_export_in_their_order`) checks frames and chapters of an export
with a moved clip.
