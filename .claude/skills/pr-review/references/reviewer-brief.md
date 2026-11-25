You are reviewing pull request #{{PR}} in the Visor repository (the current working
directory): a Rust screen recorder with crates `visor-core` (the edit model and project format),
`visor-media` (FFmpeg decode, render, export, player), `visor-recorder`, `visor-capture`
(macOS, Windows and Linux backends), `visor-cli`, and `visor` (the Slint app and the agent
MCP tools). This is review round {{ROUND}}.

Your job is to find real defects before a person tests this PR. You are read-only: don't
edit files, commit, push or comment on the PR. Report back to me instead.

## What to read

- The diff: `gh pr diff {{PR}}`. The PR description: `gh pr view {{PR}}`.
- The feature's requirements note, if any: {{NOTE}}. Check the code against its acceptance
  criteria (AC-n): an AC the code doesn't meet is a finding. The note is private: never quote
  it into anything public.
- The surrounding code, not only the diff. Most real bugs are in how a change meets the code
  next to it: callers of a changed function, other places that assume the old behaviour,
  saved projects written by older versions.
- `DESIGN.md` and the `docs/` knowledge base for the architecture, invariants and conventions.

You can run things: `node .claude/skills/tdd/tdd.mjs test <filter> -p <crate>` runs tests.
Writing a throwaway test under a temp dir to prove a bug is fine, but delete it after and
don't touch tracked files.

## What counts

Look hardest at:

- **Correctness**: wrong results, off-by-one errors in time ranges, source time vs output
  time mixed up (times are µs; cuts, speed and clip order map one to the other), wrong
  behaviour after undo, edge cases (empty edit, zero-length range, the last frame, a
  recording with no camera or audio).
- **Robustness**: panics on bad input (`unwrap` on user or agent data, out-of-range
  indices, NaN), resources left open, threads that never stop.
- **Compatibility**: projects saved by an earlier version must still open (serde defaults),
  and agent tool arguments must keep their meaning.
- **Platforms**: `#[cfg(target_os)]` code paths that won't compile or behave differently on
  Windows or Linux.
- **Tests**: a test that would pass even if the feature were broken, a behaviour change with
  no test, a test asserting on internals rather than behaviour.
- **Drift**: code that does something different from what the requirements or the PR
  description say.
- **Docs and site**: pages in `docs/`, `DESIGN.md`, `site/index.html` and the
  `CHANGELOG.md` *Unreleased* entry must be true of the code after this PR. Look for a claim
  that's now false, an area this PR changed whose `docs/` page wasn't updated, and a
  user-visible change the site or changelog should mention but doesn't. The site's CSP
  (`site/_headers`) forbids scripts, inline styles and external assets.

Don't report formatting or clippy-level lint (that's checked automatically). Don't report
personal style preferences that differ from the repo's existing conventions, or speculation
you can't tie to a concrete input. One real bug matters more than ten nits.

## Earlier rounds

{{PRIOR}}

For each item marked fixed, check that the fix is correct and complete. Don't raise an item
marked won't fix again unless you have new, concrete evidence that its reason is wrong. If
so, say what that evidence is.

## Report format

Return only this, with findings ordered from most to least severe:

```
FINDINGS: <count>

#1 [high|medium|low] <file>:<symbol or line> — <one-line statement of the defect>
   Scenario: <concrete input/state → what goes wrong>
   Evidence: <what you read or ran that shows it>
   Suggested fix: <one line, optional>

#2 ...

CHECKED, NO ISSUES: <short list of the areas you examined and found sound>
```

Severity: **high** means wrong behaviour a user or agent will hit, data loss, a crash, an
unmet AC, a broken platform build, or a public doc or site claim that's false. **Medium**
means a real defect in an edge case, or a test that doesn't test what it says. **Low** means a minor issue worth a line.

If you find nothing, say `FINDINGS: 0`, and still list what you checked. "No findings"
after a thorough look is a useful result, so don't invent issues to fill the report.
