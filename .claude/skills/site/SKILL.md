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
- Name UI controls the way the app does (*Blur an area*, *Split at playhead*).

## Constraints

`site/_headers` sets a strict Content-Security-Policy: `default-src 'none'; img-src 'self';
style-src 'self'`. So:

- no scripts at all, no inline `style="…"` attributes, and no `<style>` blocks. Styles go in
  `style.css`.
- no external images, fonts or embeds. Assets live in `site/assets/`.

Keep the page accessible as it is: `alt` text on every image, a heading order without gaps,
and real links (not buttons) for navigation.

## Check it

- `open site/index.html` (or look at it with the browser tools) at desktop and phone width,
  and check the grid still lines up.
- `grep -n 'style=\|<script\|http://' site/index.html` should print nothing new.
- Re-read the changed text next to `CHANGELOG.md` *Unreleased*. They should agree, with the
  site at the "why you'd want it" level and the changelog at the "what changed" level.

Commit the site change with the feature (`Mention <feature> on the site`).
