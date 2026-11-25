---
name: requirements
description: Gather a Visor feature's requirements by interviewing the user, then write a local, uncommitted feature note (.claude/features/<name>.md) with acceptance criteria, design notes, an ordered test-first plan and test scenarios. Use when the user wants to plan, scope, define or think through a feature, types /requirements, or when the `feature` skill reaches its requirements phase. Asks questions in rounds and confirms before anything is built.
user-invocable: true
---

# Requirements for a Visor feature

The repo is public, so requirements and plans aren't committed. They live in a local,
gitignored note that the build, the review and later changes all refer to. What should last
goes into the public knowledge base (`docs/`, through the `knowledge-base` skill) and the PR
description.

The user knows what they want better than any guess. Ask before you assume, offer options
with a recommended one first, and confirm before building.

## 1. Setup

- Settle `<feature-name>`: short, kebab-case, also used as the branch name (`blur-regions`,
  `clips`). Confirm it if the request is ambiguous.
- The note is `.claude/features/<feature-name>.md` (gitignored). If it already exists, read it
  and ask whether to continue it or start over. Never overwrite it silently.

## 2. Read, then ask

**Read first**, so the questions are specific ("should a blur move with its clip?") rather
than generic ("any constraints?"). Read `DESIGN.md` (Goals, Non-goals, Known limitations), the
`docs/` pages for the areas involved, and the code itself. Cite real paths and symbols.

Then interview with `AskUserQuestion`, at most 4 questions per call, in rounds until nothing
material is open. Cover:

- the need, and what prompted it
- goals and non-goals: what it deliberately won't do in this pass
- where it shows up: recorder panel, control bar, editor, export, agent (MCP) tools, CLI
- how it meets existing features: cuts, trim, speed, clips, blur, markers, camera bubble,
  undo, crash recovery, older saved projects, the three platforms
- the design choices the code reading turned up: which pattern to follow, reuse or something new
- what done looks like, and how the user will check it

Ask all of this now. Once building starts, the `feature` workflow runs on its own, so a
question left for later stalls it halfway.

## 3. Write the note

Use this template, scaled to the feature. A one-screen change still gets acceptance criteria,
but its design and plan can be a few lines each.

````markdown
# <Feature name>

Status: Draft | Approved | Built · Updated <YYYY-MM-DD> · Branch `<feature-name>` · PR <#n once open>

## Problem
What need this addresses and what prompted it.

## Goals / non-goals
- ...
- Not: ...

## Acceptance criteria
<!-- Observable behaviour: on screen, in an exported file, or through an agent tool. -->
- **AC-1** ...
- **AC-2** ...

## Design
How it fits: the existing code it reuses (paths and symbols), what's new per crate, how older
projects still open, what differs per platform. Key decisions with the reason for each.

## Plan
<!-- Build order. Each behaviour step names the test it starts from (tdd red → green). -->
1. [ ] **<step>**: `crates/...`. Test first: `<crate>` `<module or tests/file.rs>`, <behaviour>
...
n. [ ] Docs (`knowledge-base` skill), site (`site` skill), `CHANGELOG.md` *Unreleased*

## Test scenarios
<!-- Tick when a test covers it or it was checked live (say which). -->
- [ ] AC-1: Given ... When ... Then ...
- Not covered: AC-n, because ...

## Decisions log
<!-- Calls made during the build or review that the user didn't explicitly approve. -->
````

Writing conventions:

- **Concrete examples.** "A 2:00 recording, cut 0:30–0:45, then split at 1:10" beats "a
  recording with some edits". Say whether a time is source time or output time.
- **Real names.** Use paths and symbols that exist (`EditList::split`), not invented ones,
  and no line numbers.
- The usual plan order is: `visor-core` model and project format → `visor-media` render,
  export and player → recorder or capture → app wiring and Slint UI → agent tools (`mcp.rs`,
  `app/agent.rs`) → docs, site, changelog. Slint screens have no render harness: put their
  logic in a testable function, and check the screen live.

## 4. Confirm

Summarize the acceptance criteria, the key decisions and the plan in a few lines, and ask
whether to build. When the user says yes, set the status to **Approved**.

## While building

The note is kept true, not frozen. Tick steps and scenarios as they land. When the design
changes, edit the note first. Record each call the user didn't approve in the *Decisions log*,
because the hand-over reports it. Set the status to **Built** once the PR is ready.
