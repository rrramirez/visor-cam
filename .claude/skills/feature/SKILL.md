---
name: feature
description: Run a whole Visor feature from idea to a ready-for-review PR. Interview the user for requirements, branch, build it test-first (tdd), verify, update the docs knowledge base and the product site, open a draft PR, review it in a loop with a fresh subagent and fix what matters (pr-review), then mark it ready and hand it over for the user to test. Use whenever the user wants to start, build, add or implement a feature or a sizeable improvement in Visor ("let's add X", "start a feature for Y", "/feature", "I want Visor to..."), even if they don't name the steps.
user-invocable: true
---

# Feature workflow

One feature, from the first question to a PR that's ready for the user to test. Each phase
uses a sub-skill:

```
1 Requirements (requirements) ──user approves──▶ 2 Branch ─▶ 3 Build (tdd) ─▶ 4 Verify
  ─▶ 5 Docs, site, changelog (knowledge-base, site) ─▶ 6 Draft PR ─▶ review loop (pr-review)
  ─▶ ready for review ─▶ hand over
```

The only planned stop is the end of phase 1. Once the user approves, work through to a ready
PR on your own, because they want to come back to something testable rather than a series of
check-ins. Stop early only when something is genuinely theirs to decide (see *When to stop
and ask*).

The repo is public. Requirements and plans stay in the local, gitignored note
`.claude/features/<feature-name>.md`. What's committed is the code, tests, the `docs/`
knowledge base, the site and the changelog.

## 1. Requirements: the `requirements` skill

Load the `requirements` skill and follow it: read the code, interview the user in rounds,
write the local feature note (acceptance criteria, design, plan, test scenarios), and confirm.
Ask everything here, up front, so the build doesn't stall halfway on a question you could
have asked now. Don't skip the interview just because the feature looks small: the user
wants to be consulted on requirements.

The phase ends when the user says to build. The note's status becomes **Approved**.

## 2. Branch

Use a **worktree** when the user asks for one, or when the main checkout has uncommitted
changes or sits on another feature's branch. Several features can then be built in parallel,
and nobody's work in progress gets touched:

```bash
git fetch origin
git worktree add -b <feature-name> ../visor-cam-<feature-name> origin/main
```

Then do everything in `../visor-cam-<feature-name>`: edits, builds, checks, commits, pushes.
The feature note stays in the main checkout's `.claude/features/`. Pass the worktree's path
to `pr-review` as `{{WORKDIR}}`. Leave the worktree in place after the PR is ready, because
follow-up changes go there too. Removing it after the merge is the user's call.

Otherwise branch in the main checkout:

```bash
git fetch origin
git status --short          # must be clean. If it isn't, don't stash or switch: ask, or use a worktree
git switch -c <feature-name> origin/main
```

If you're already on a branch with unrelated commits, ask before branching off it.

## 3. Build: the `tdd` skill

Load the `tdd` skill. Work through the note's **Plan** in order:

1. Write the step's test → `tdd.mjs red <filter> -p <crate>` → write the code →
   `tdd.mjs green <filter> -p <crate>` → refactor → `green -p <crate>`.
2. Tick the step in the note, and tick any test scenario the test now covers.
3. Commit the step (code and test). The message is a plain sentence about what it does, like
   the repo's history (`Split a clip at the playhead`), ending with the attribution line that
   the session's instructions give.

When the plan turns out wrong mid-build (a missing step, a better design), update the note
first, then the code, and record the call in its *Decisions log*. If it changes the
*behaviour* the user approved, that's a stop-and-ask.

## 4. Verify

Run the checks for what the feature touches:

- Rust code: `node .claude/skills/tdd/tdd.mjs verify` exits 0 (fmt, clippy `-D warnings`, all
  tests). Skip it for a PR that touches no Rust, and say so in the PR.
- `site/`: `node .claude/skills/site/check.mjs` exits 0, and look at its screenshots.
- Platform code touched (`visor-capture/src/{windows,linux}.rs`, `#[cfg(target_os)]`) → also
  `scripts/linux-check.sh` (needs Docker) and `scripts/windows-check.sh`. If one can't run
  here, say so in the PR rather than skipping it silently.
- **Live check**: anything a user can see gets looked at in the real app. Run it
  (`cargo run -p visor`, or the `run` skill) and use the `visor-dev` agent tools to drive it
  and look at it (`open_recording`, `view_frame`, `status`). For web pages, use Playwright
  (as `site/check.mjs` does). The Chrome extension can't open `file://` pages or servers on
  this machine's localhost. Check the note's scenarios that
  automated tests can't cover. Don't move the user's mouse or keyboard without asking. Note
  what you checked live and what you didn't try.

## 5. Docs, site, changelog

All three are part of the feature, not follow-ups. They ship in the same PR, so they're
never out of step with the code.

- **Knowledge base**: load the `knowledge-base` skill and update the `docs/` pages for every area the
  diff touches, creating any that are missing. Fix `DESIGN.md` if it now says something false.
- **Site**: load the `site` skill. Update `site/` if the feature changes what a user can
  do or see, or makes something on the site false. It goes live at the next release. If a
  screenshot is now stale, note it for the hand-over. If nothing needs changing, say so in
  the PR.
- **Changelog**: add an entry to `CHANGELOG.md` under *Unreleased* (`### New` / `### Fixed`),
  in the user's terms, like the existing entries.

Then run the checks from phase 4 again and commit.

## 6. Draft PR → review loop → ready

1. Push and open a **draft**:
   ```bash
   git push -u origin <feature-name>
   gh pr create --draft --title "<plain sentence, like the repo's PR titles>" --body-file <file>
   ```
   The body follows the repo's style (see PRs #18 and #20). Start with a one-line summary of
   what the user gets. Then add bold-labelled sections per area (**Model**, **Editor**,
   **Export**, **Agents**…), saying what changed and why, concisely. Add **Docs / site**:
   which pages changed, or "no change needed". Add **Tests**: what's covered, with totals
   from `verify` (and Linux if run). Add **Live**: what was checked in the app and what
   wasn't tried. Add **Not included**: deliberate gaps and stale screenshots. End with the PR
   attribution line from the session's instructions. The body is public, so describe the
   result, not the private requirements discussion.
2. Write the PR number into the note. Load the `pr-review` skill and run its loop. Each round
   goes to a fresh reviewer subagent, you sort the findings into fix / won't fix, fix
   test-first, and post a PR comment per round. Stop when a round has no must-fix findings,
   at most 4 rounds.
3. Mark it ready (`gh pr ready`), as `pr-review` describes. **Never merge.**
4. Set the note's status to **Built**.

## Hand over

The user tests next, so the final message is what they need for that:

- the PR link, and one line on what the feature does
- **To test**: the scenarios that only a person can check (things done by hand, real
  devices, other platforms), as a short checklist
- **Needs your call**: decisions from the *Decisions log* that the user didn't explicitly
  approve, including review findings marked won't fix that touch behaviour
- stale screenshots, and anything skipped or not tried, plainly

After this the user may come back with changes or suggestions. Treat each one as a small
loop on the same branch and PR: update the note if behaviour changes, then tdd → verify →
docs/site/changelog if affected → push → one review round (`pr-review`, with the earlier
decisions as `{{PRIOR}}`) → tell them what changed. Don't open a new PR unless they ask.

## When to stop and ask

Stop the autonomous run and ask (with `AskUserQuestion`) only when:

- a decision would change behaviour the user approved, or the requirements left it open and
  the choice matters to them
- the working tree or branch has someone else's uncommitted or unrelated work
- verification needs something you can't do here (permissions, a device, credentials)
- the review loop hit its 4-round cap with must-fix findings left

Otherwise make the reasonable call, log it in the note, and keep going.
