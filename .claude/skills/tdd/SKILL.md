---
name: tdd
description: Drive test-driven development in Visor's cargo workspace: write a failing test first, prove it red, implement, prove it green, then verify (fmt, clippy, all tests). Use whenever adding or changing behaviour in any crate (edit model, project format, rendering, export, player, recorder, agent tools), when running or filtering cargo tests, or when checking a change before committing, even if the user doesn't say "TDD".
---

# TDD for Visor

All paths are relative to the repo root. Any change to **behaviour** is test-first: write the
test, prove it fails, then write the production code. That isn't required for refactors,
renames, dependency bumps, docs or config.

Everything goes through one driver:

```bash
node .claude/skills/tdd/tdd.mjs help
```

It wraps `cargo test` and adds what cargo doesn't give you. The **red gate** fails when your
new test passes before you've written any production code. The **green gate** won't count
"0 tests ran" as passing. Each failure prints just its panic message and location, not
everything libtest prints.

## The loop

```bash
# 1. RED: write the test, then prove it fails. Exit 0 only if it fails or doesn't compile yet.
node .claude/skills/tdd/tdd.mjs red split_keeps_both_halves -p visor-core

# 2. Write the minimum production code, then GREEN. Exit 0 only if it compiles, runs and passes.
node .claude/skills/tdd/tdd.mjs green split_keeps_both_halves -p visor-core

# 3. Refactor, rerun green on the crate, then run the full gate before calling it done.
node .claude/skills/tdd/tdd.mjs green -p visor-core
node .claude/skills/tdd/tdd.mjs verify
```

**`red` exiting 1 (`NOT RED`)** is the signal that matters. The test passed before you
implemented anything, so it doesn't pin the new behaviour. Strengthen the assertion, or check
whether the filter matched an older test.

**`red` exiting 0 with `RED (compile)`** is a legitimate first red only when the errors are
about the item the test is for: the new function, field or variant doesn't exist yet. Read
them. A typo or unrelated breakage isn't red, it's broken. Fix it and rerun until the only
errors left are the ones you expect, or the test fails for the right reason.

Always pass `-p <crate>` during the loop. A workspace run builds the Slint app and every
FFmpeg test, while `-p visor-core` runs in about a second.

| Command | What it does |
|---|---|
| `red <filter>` | exit 0 **only** if a matching test fails or doesn't compile |
| `green [filter]` | exit 0 **only** if it compiles, at least one test ran, and all passed |
| `test [filter]` | run and report either way |
| `list [filter]` | matching tests per test binary |
| `verify` | `cargo fmt --check`, `clippy --workspace --all-targets -D warnings`, all tests |

Exit codes: 0 means the gate passed, 1 that it failed, 2 a build failure, 3 that no test
matched (a typo, or the file isn't saved). Filters are libtest substrings of the test path
(`edit::tests::cut_middle_splits_range`). Add `--exact` for one test.

## Where a test goes

| Behaviour | Where |
|---|---|
| Edit model, project format, time, config (`visor-core`) | `#[cfg(test)] mod tests` at the bottom of the module (see `edit.rs`) |
| Frame maths: blur, composite, scale, tempo, dead time (`visor-media/src`) | unit tests in the module, on synthetic frames |
| Decode, export, player, writer, recovery (real FFmpeg files) | `crates/visor-media/tests/<area>.rs`, using `tests/common/mod.rs`: `project()`, `checkerboard_project()`, `write_video()`, `write_tone()`, `nv12()`, `mean_luma()`, `temp_dir()` / `cleanup()` |
| Capture timing and geometry (`visor-capture`) | `capture_math.rs`, `host_clock.rs`: keep the maths pure so it's testable without a device |
| Recorder state machine (`visor-recorder`) | unit tests in `lib.rs` |
| Agent tools (`crates/visor/src/mcp.rs`, `app/agent.rs`), CLI, settings | unit tests in those modules |
| Slint UI (`crates/visor/ui`, `editor.rs`) | no render harness. Move the logic into a plain function or into `visor-core`/`visor-media` and test it there. Check the screen itself live (see the `feature` skill's verification step) |

House style: test names are sentences about behaviour in snake_case
(`clips_split_move_and_play_in_their_order`), not `test_split_1`. Build the values directly. No
mocks of FFmpeg: write a tiny real file with the helpers above, which is the point of those
tests. Assert on what a user would notice (frame contents, timings in µs, chapter order), not
on internals that a refactor would change.

## Gotchas

- **Capture devices aren't available in tests.** Anything that needs Screen Recording
  permission, a camera or a mic belongs behind a pure function you can test. Don't write a test
  that only passes on a machine with permissions granted.
- **Times are microseconds (`*_us`) in source time.** Edits map source time to output time.
  Say which one a test asserts on, because mixing them is the classic bug here.
- **`visor-media` is built with `opt-level = 2` in dev** (see `Cargo.toml`), so its first
  build after a change is slower. That's expected, not a hang.
- **Platform code.** Changes under `visor-capture/src/{linux,windows}.rs` or with
  `#[cfg(target_os)]` can't be tested on macOS. Run `scripts/linux-check.sh` (Docker:
  build, tests and clippy) and `scripts/windows-check.sh` (clippy only) as well.
- **The driver isn't the last step.** `verify` is: fmt and clippy failures block a PR just
  like test failures. `cargo fmt --all` fixes formatting. Fix clippy warnings by hand, and
  don't `#[allow]` them unless there's a stated reason.
