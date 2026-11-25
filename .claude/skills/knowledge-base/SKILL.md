---
name: knowledge-base
description: Maintain Visor's knowledge base in docs/, written for contributors and coding agents. It covers how each area works, why it works that way, the invariants that must hold, and the traps already hit. Use when a feature or fix changes how an area works, when the `feature` skill reaches its docs step, when the user asks to document (in the repo's docs/, not a Claude Doc), explain or write up part of the codebase, or when you learn something non-obvious about the code that the next person would otherwise rediscover.
user-invocable: true
---

# Visor knowledge base

`docs/` explains how Visor works to someone about to change it: an outside contributor, or a
future Claude session with no memory of this one. The repo is public, so write for strangers.
Leave out plans, private context and "we decided in chat". Write down what's true of the code,
and why.

It isn't a user guide (that's the site and `TESTING.md`) or a changelog (`CHANGELOG.md`).
`DESIGN.md` stays the one-page overview: goals, stack, crates, milestones. It links into
`docs/` for depth instead of growing.

## Layout

One page per area, named by what a contributor would look for:

```
docs/
  README.md             index: one line per page, and where to start
  project-format.md     the project folder, project.json, compatibility with older versions
  edit-model.md         EditList: kept ranges, clips, cuts, trim, speed, source vs output time
  recording.md          recorder pipeline, tracks, clocks and sync, pause, crash recovery
  capture-<platform>.md macOS / Windows / Linux backends and what's untested
  rendering-export.md   compositing, blur, camera bubble, encoders, chapters
  player.md             preview playback, seeking, redraw rules
  editor-ui.md          Slint editor: state, undo steps, shortcuts, timeline
  agent-tools.md        MCP tools: arguments, time units, what agents can and can't do
  testing.md            where tests go, fixtures, what can't be tested and how it's checked
```

This is the intended shape, not a list to fill in. **Create a page when a change touches its
area** and the page doesn't exist yet. Then write it about the whole area as it stands, not
only about your change. Split a page once it covers two things someone would look up
separately. Update `docs/README.md` whenever you add a page.

## What a page holds

```markdown
# <Area>

<Two or three sentences: what this part does and where it lives (crates, modules).>

## How it works
The model and the flow, in the order a reader needs them. Name real types and functions
(`EditList::split`, `Renderer::render_source`). A small diagram or table where it helps.

## Invariants
Things that must stay true, and what breaks if they don't. These matter most to the next
person changing the code.

## Gotchas
Traps already hit once: the symptom, the cause, and what to do instead.

## Tests
Where this area's tests live and what they rely on.
```

## How to write it

- **Explain why, not only what.** "Times are stored in source time so cuts can be undone
  without re-deriving anything" helps more than "times are in source time".
- **Stay at the right altitude.** Pin down concepts and boundaries. Don't paraphrase code
  line by line: it goes stale the moment the code changes, and the reader can read the code.
- **No line numbers**, and no "new in this PR" wording. A page describes the code as it is.
  History belongs in git and the changelog.
- **Each fact in one place.** Link to another page rather than repeat it.
- **Check every claim against the code** before writing it. A confident wrong doc is worse
  than none.
- Keep pages short enough to read before starting work, around a screen or two. If one grows
  past that, split it.

## In a feature

At the docs step of the plan (or when a fix changes how something works):

1. List the areas the diff touches (`git diff origin/main --stat`).
2. For each one, update its page so it's true after the change: new concepts, changed
   invariants, new gotchas found while building. Create the page if it's missing.
3. If `DESIGN.md` now says something false (crates, pipeline, known limitations), fix it
   there too.
4. Commit the docs with the feature, as `Document <area>` or within the step's own commit.

The PR reviewer checks the docs against the code as well, so they have to be accurate, not
just present.
