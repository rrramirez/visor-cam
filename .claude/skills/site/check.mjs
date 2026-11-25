#!/usr/bin/env node
// Checks Visor's product site (site/) in Playwright's Chromium, served with the same
// Content-Security-Policy that Cloudflare applies from site/_headers, at desktop and phone
// width. Fails on: CSP violations or console errors, missing assets, horizontal overflow, a
// ragged feature grid, broken in-page links, images without alt text, and a nav link whose
// section heading ends up under the sticky nav. Saves screenshots for a look by eye.
//
//   node .claude/skills/site/check.mjs [site-dir] [out-dir]
//
// site-dir defaults to <repo>/site; out-dir to a temp folder (printed). Exit 0 = all pass.

import { createRequire } from 'node:module';
import { existsSync, mkdtempSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, extname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const SKILL_DIR = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(SKILL_DIR, '..', '..', '..');
const SITE = resolve(process.argv[2] ?? join(REPO, 'site'));
const OUT = process.argv[3] ? resolve(process.argv[3]) : mkdtempSync(join(tmpdir(), 'visor-site-'));

let chromium;
try {
  ({ chromium } = createRequire(join(SKILL_DIR, 'package.json'))('playwright'));
} catch {
  console.error(
    'Playwright is not installed for this skill. Run once (it downloads from npm; the browser\n' +
      'is reused from ~/Library/Caches/ms-playwright if this version already fetched it):\n' +
      '  npm --prefix .claude/skills/site install\n' +
      '  npx --prefix .claude/skills/site playwright install chromium   # only if launch fails',
  );
  process.exit(2);
}

const csp = /Content-Security-Policy:\s*(.+)/.exec(readFileSync(join(SITE, '_headers'), 'utf8'))?.[1].trim();
if (!csp) {
  console.error(`no Content-Security-Policy in ${join(SITE, '_headers')}`);
  process.exit(2);
}
const TYPES = {
  '.html': 'text/html', '.css': 'text/css', '.png': 'image/png', '.webp': 'image/webp',
  '.jpg': 'image/jpeg', '.svg': 'image/svg+xml', '.xml': 'application/xml', '.ico': 'image/x-icon',
};
const VIEWPORTS = [
  { name: 'desktop', viewport: { width: 1280, height: 900 }, isMobile: false },
  { name: 'tablet', viewport: { width: 820, height: 1180 }, isMobile: true },
  { name: 'phone', viewport: { width: 390, height: 844 }, isMobile: true },
];

const browser = await chromium.launch();
let failed = false;

for (const { name, viewport, isMobile } of VIEWPORTS) {
  const ctx = await browser.newContext({ viewport, isMobile, hasTouch: isMobile, deviceScaleFactor: 2 });
  const page = await ctx.newPage();
  const problems = [];
  page.on('console', (m) => m.type() === 'error' && problems.push(`console: ${m.text()}`));
  page.on('pageerror', (e) => problems.push(`page error: ${e.message}`));
  page.on('requestfailed', (r) => problems.push(`request failed: ${r.url()}`));
  // Serve site/ from a fake origin so the CSP header applies exactly as in production.
  // Off-site requests (GitHub, Ko-fi links are never fetched; anything else is) are aborted.
  await page.route('**/*', async (route) => {
    const url = new URL(route.request().url());
    if (url.host !== 'visor.test') {
      problems.push(`off-site request: ${url.href}`);
      return route.abort();
    }
    const file = join(SITE, url.pathname === '/' ? 'index.html' : decodeURIComponent(url.pathname));
    if (!file.startsWith(SITE) || !existsSync(file)) {
      problems.push(`404: ${url.pathname}`);
      return route.fulfill({ status: 404 });
    }
    return route.fulfill({
      status: 200,
      body: readFileSync(file),
      headers: { 'content-type': TYPES[extname(file)] ?? 'application/octet-stream', 'content-security-policy': csp },
    });
  });

  await page.goto('http://visor.test/', { waitUntil: 'load' });

  const layout = await page.evaluate(() => {
    const doc = document.documentElement;
    const cards = [...document.querySelectorAll('.grid article')];
    const columns = new Set(cards.map((a) => Math.round(a.getBoundingClientRect().left))).size || 1;
    return {
      overflowPx: doc.scrollWidth - doc.clientWidth,
      cards: cards.length,
      columns,
      raggedRow: cards.length % columns !== 0,
      brokenAnchors: [...document.querySelectorAll('a[href^="#"]')]
        .map((a) => a.getAttribute('href'))
        .filter((h) => h.length > 1 && !document.getElementById(h.slice(1))),
      imagesWithoutAlt: [...document.querySelectorAll('img:not([alt])')].map((i) => i.getAttribute('src')),
      navLinks: [...document.querySelectorAll('.nav nav a[href^="#"]')].map((a) => a.getAttribute('href')),
    };
  });

  // Every in-page nav link must land with its section's heading below the sticky nav.
  // On phones the nav hides these links, so jump by hash the same way the link would.
  const jumps = [];
  for (const href of layout.navLinks) {
    await page.evaluate(() => window.scrollTo(0, 0));
    const link = page.locator(`.nav nav a[href="${href}"]`);
    if (await link.isVisible()) await link.click();
    else await page.evaluate((h) => { location.hash = h; }, href);
    await page.waitForFunction(
      (h) => Math.abs(document.querySelector(h).getBoundingClientRect().top - parseFloat(getComputedStyle(document.querySelector(h)).scrollMarginTop || '0')) < 4 ||
        window.innerHeight + window.scrollY >= document.documentElement.scrollHeight - 2,
      href,
      { timeout: 3000 },
    ).catch(() => {});
    const j = await page.evaluate((h) => {
      const navBottom = document.querySelector('.nav').getBoundingClientRect().bottom;
      const heading = document.querySelector(`${h} h2, ${h} h1`) ?? document.querySelector(h);
      return { href: h, navBottom: Math.round(navBottom), headingTop: Math.round(heading.getBoundingClientRect().top) };
    }, href);
    jumps.push({ ...j, clear: j.headingTop >= j.navBottom });
    if (href === '#agents' || href === layout.navLinks[layout.navLinks.length - 1]) {
      await page.screenshot({ path: join(OUT, `${name}-${href.slice(1)}.png`) });
    }
  }
  await page.evaluate(() => window.scrollTo(0, 0));
  await page.screenshot({ path: join(OUT, `${name}-full.png`), fullPage: true });

  const issues = [
    ...problems,
    ...(layout.overflowPx > 0 ? [`page is ${layout.overflowPx}px wider than the viewport`] : []),
    ...(layout.raggedRow ? [`grid: ${layout.cards} cards in ${layout.columns} columns leaves a ragged last row`] : []),
    ...layout.brokenAnchors.map((h) => `link to missing ${h}`),
    ...layout.imagesWithoutAlt.map((s) => `img without alt: ${s}`),
    ...jumps.filter((j) => !j.clear).map((j) => `${j.href}: heading at ${j.headingTop}px is under the nav (bottom ${j.navBottom}px)`),
  ];
  failed ||= issues.length > 0;
  console.log(
    `${issues.length ? 'FAIL' : 'PASS'} ${name} ${viewport.width}px — ${layout.cards} cards in ${layout.columns} columns, ` +
      `nav links ${jumps.map((j) => `${j.href}${j.clear ? '✓' : '✗'}`).join(' ') || 'none'}`,
  );
  for (const i of issues) console.log(`  - ${i}`);
  await ctx.close();
}

await browser.close();
console.log(`screenshots: ${OUT}`);
process.exit(failed ? 1 : 0);
