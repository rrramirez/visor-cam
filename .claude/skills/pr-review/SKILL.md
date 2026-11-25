---
name: pr-review
description: Review a Visor pull request in a loop. A fresh subagent reviews the PR, you check and sort each finding (fix / won't fix, with reasons), fix test-first, post the round as a PR comment, and repeat until a round has no must-fix findings (at most 4 rounds), then mark the draft ready. Use after opening a draft PR, when the `feature` skill reaches review, or when the user asks to review, re-review or finish off a PR.
user-invocable: true
---

# PR review loop

The goal is a PR the user can test without first finding the bugs themselves. A reviewer
that only shares your context tends to confirm what you already believe. So every round goes
to a **fresh subagent** that sees the code and the requirements, not your reasoning. Its findings are
claims, not orders: you check each one against the code and decide, and the PR comment
records why.

## Before round 1

- The PR is a **draft** and is pushed (`gh pr view --json isDraft,headRefOid`).
- The automatic checks for what the PR touches pass. Don't spend a review round on what a
  script already tells you:
  - Rust code: `node .claude/skills/tdd/tdd.mjs verify` (fmt, clippy, tests)
  - `site/`: `node .claude/skills/site/check.mjs` (CSP, layout, anchors)
  - only docs: nothing to run, but every claim gets checked against the code.
- Note the PR number `<n>`, the checkout the branch lives in (the main checkout or a worktree
  such as `../visor-cam-<feature>`), and the local feature note
  (`.claude/features/<feature>.md`, in the main checkout), if there is one. The note is
  gitignored, but the reviewer runs on this machine and can read it.

## A round

### 1. Spawn the reviewer

Use the Agent tool with `subagent_type: "general-purpose"`. Don't use a fork, because a fork
inherits this conversation and its blind spots. The prompt is the brief in
`references/reviewer-brief.md`, with its placeholders filled in:

- `{{PR}}`: the PR number
- `{{WORKDIR}}`: the absolute path of the checkout the PR branch is in
- `{{NOTE}}`: the feature note's absolute path, or "none"
- `{{ROUND}}`: the round number
- `{{PRIOR}}`: for round 2 and later, the earlier decisions as a list (`#id title: fixed in
  <sha>` / `won't fix: <reason>`), so the reviewer checks the fixes and doesn't re-raise
  dismissed items without new evidence. For round 1, write "none".

Don't add your own view of what's risky or what you think is fine. Wait for the result.

### 2. Check and sort each finding

For each finding, read the code it points at, and reproduce it when you can (a quick test, a
`tdd.mjs test` run). Then put it in one bucket:

| Decision | When |
|---|---|
| **Must fix** | A real defect: wrong behaviour, an AC not met, docs or site now saying something false, a panic or crash, data loss, old projects that no longer open, a broken platform build, a test that doesn't actually test what it claims, a security issue |
| **Fix** | Not a defect, but cheap and clearly better: a misleading name, a missing edge-case test, dead code from this PR, a doc out of date |
| **Won't fix** | The finding is wrong (say why, citing the code), out of scope (add it to `DESIGN.md` → *Known limitations and next steps* if it's real), or a style preference that differs from the repo's own conventions |

Be honest in both directions. Don't wave off a real bug because fixing it is work, and don't
"fix" a false finding just to make the list shorter. That would add churn and risk to the
PR. If you can't tell whether a finding is real, it's must-fix until you can show it isn't.

A finding that changes what the feature *does*, such as a product decision or a trade-off the
requirements didn't settle, isn't yours to decide alone. List it under **Needs your call** in the
comment and in your final report, pick the safer option for now, and log it in the
feature note's *Decisions log*.

### 3. Fix

- Fix behaviour test-first: write a regression test that reproduces the finding, prove it
  `red`, fix it, prove it `green` (the `tdd` skill).
- Use one commit per finding or per small group of findings. The message is a plain sentence
  saying what changed (`Keep a blur inside the frame when the clip moves`).
- Run the PR's automatic checks again (`tdd.mjs verify`, `site/check.mjs`), then push.

### 4. Post the round

Post one PR comment per round (`gh pr comment <n> --body-file <file>`), in this format:

```markdown
### Review round <N>

<k> findings: <a> fixed, <b> won't fix.

| # | Finding | Severity | Decision |
|---|---|---|---|
| 1 | `crates/visor-core/src/edit.rs` `EditList::split`: splitting at a clip edge makes a 0-length clip | high | Fixed in abc1234, with test `split_at_an_edge_keeps_the_clip_whole` |
| 2 | `export.rs`: allocate the blur buffer once per export | low | Won't fix: it's already allocated once, in `Renderer::new` |

**Needs your call:** <only if any, else omit>

Next: <round N+1 | ready for review | stopped (see below)>
```

Keep it scannable. One line per finding, with the reason in the decision.

### 5. Stop or go again

- **The round had no must-fix findings** (everything was a non-defect or won't fix, and any
  "fix" items were done) → stop. Mark the PR ready.
- **Must-fix findings were fixed** → start the next round with a new reviewer, including the
  decisions so far in `{{PRIOR}}`.
- **Round 4 still had must-fix findings** → stop without marking ready. Say so in the round
  comment and in your report to the user: what's still open, and why it isn't converging. The
  loop exists to catch bugs, not to grind forever on a design question that the user should
  settle.

## Marking ready

1. Update the PR body (`gh pr edit <n> --body-file <file>`) so it describes the final code.
   Use the test totals from the last `verify`, and keep the live-check and *Not included*
   notes current. Add `Review: <N> rounds (see comments)`.
2. Run `gh pr ready <n>`.
3. **Never merge.** Merging is the user's call, after they've tested it.
