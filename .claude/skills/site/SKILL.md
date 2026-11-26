---
name: site
description: Keep Visor's product site (site/index.html, deployed to visor-cam.pages.dev at release) accurate when features are added or improved, covering the feature grid, the how-it-works steps, the FAQ, meta descriptions, platform claims and screenshots. Use in every feature that changes what a user can do or see, at the `feature` skill's site step, or when the user asks to update, check or refresh the site or landing page.
user-invocable: true
---

# Product site

`site/` is the public landing page: one static `index.html`, `style.css` and images in
`assets/`. It's deployed to Cloudflare Pages by `scripts/release-macos.sh --publish`
(`scripts/deploy-site.sh`). So a site change committed in a feature PR **goes live with the
next release**, when the feature can actually be downloaded. Don't run `deploy-site.sh`
yourself: it would advertise what isn't released, and it publishes the site.

## When a feature needs a site change

Ask: would someone deciding whether to download Visor care, or would the site now say
something false?

- **New capability** a user would choose Visor for (blur, clips, agent control): add it to,
  or fold it into, the feature grid (`#features`).
- **Improvement to something listed** (a new export format, a wider speed range): update that
  card's text.
- **Changes the flow** (how recording, editing or exporting goes): check the three steps in
  "From idea to shareable video in a minute".
- **Changes a fact**: platforms or OS versions, permissions, where recordings go, network
  use (updates). Fix the FAQ, the `fineprint` line under the hero, and the `<meta>` and
  `og:` descriptions.
- **Changes how the UI looks** in `assets/editor.webp` or `assets/panel.webp`: the screenshot is
  now stale. Don't fake one. Note it in the PR's **Not included** section and in the
  hand-over, so the user can take a new one. A new screenshot replaces the old one at the same
  size (`width` / `height` attributes), as `.webp`.
- Internal changes, fixes the site never mentioned, and agent-only details: no site change.
  Say "Site: no change needed" in the PR.

## House style

Match the existing copy:

- Short, plain, second person: "Cut the stumbles, trim the ends and play it back at 0.5× to
  2×". No marketing superlatives, no jargon (no "MCP", "Slint", "FFmpeg", "NV12").
- A card is an `<article>` with an `<h3>` of 2–4 words and one sentence of about 10–20 words.
  Keep the grid even. Eight cards today, so merge related cards rather than growing to an
  odd count.
- Talk about Visor as it is today on the platforms it ships on. Windows and Linux are "being
  built" until a release ships them.
- Name UI controls the way the app does (*Blur an area*, *Add marker*).

## Constraints

`site/_headers` sets a strict Content-Security-Policy: `default-src 'none'; img-src 'self';
style-src 'self'`. So:

- no scripts at all, no inline `style="…"` attributes, and no `<style>` blocks. Styles go in
  `style.css`.
- no external images, fonts or embeds. Assets live in `site/assets/`.

Keep the page accessible as it is: `alt` text on every image, a heading order without gaps,
and real links (not buttons) for navigation.

## Every claim must be true as stated

Each sentence on the site is a promise to someone deciding whether to download Visor. Check
each claim you write against the code, not only the README, and check its **scope**: a claim
true for one format, platform or case but written for all of them is false. Reviews of the
first big site update kept finding exactly this. For example:

- The cover frame is MP4 only, chapters are MP4 and WebM, and GIF has no size choice and no
  audio (`crates/visor-media/src/export.rs`, `crates/visor/ui/editor.slint`).
- *Cut pauses* looks at the screen track and listens to mic and system audio, not the camera
  (`dead_time.rs`).
- Agents look at the frames they choose (`view_frame`), and those frames go to the agent's AI
  service. Keep that next to any "nothing leaves your Mac" claim.
- Use UI labels exactly as the app spells them (grep `crates/visor/ui/*.slint`).

Describe only released behaviour: the latest `CHANGELOG.md` version section, not *Unreleased*,
unless this PR is the one releasing it.

## Check it

Run the bundled Playwright check:

```bash
npm --prefix .claude/skills/site install     # once per checkout (Playwright, pinned)
node .claude/skills/site/check.mjs           # or: check.mjs <site-dir> <screenshot-dir>
```

It serves `site/` with the exact CSP from `site/_headers` and loads it at 1280px, 820px and
390px. It fails on CSP violations (inline styles, scripts), console errors, missing assets,
horizontal overflow, a ragged feature grid, links to missing anchors, images without `alt`,
and a nav link whose heading lands under the sticky nav. Exit 0 means everything passed. It
also prints a screenshot folder: **look at the screenshots** too, because a passing check
doesn't mean the page reads well.

Use this rather than the Chrome extension, which can't open `file://` pages or servers on this
machine's localhost. If `npm install` needs to download and you haven't been told you may,
ask first. Playwright reuses the Chromium in `~/Library/Caches/ms-playwright` when its version
matches the pin.

Then re-read the changed text next to `CHANGELOG.md`. They should agree, with the site at the
"why you'd want it" level and the changelog at the "what changed" level.

Commit the site change with the feature (`Mention <feature> on the site`).
