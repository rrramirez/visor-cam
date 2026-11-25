#!/usr/bin/env node
// TDD driver for Visor (cargo workspace).
//
// Wraps `cargo test` with what a red/green/refactor loop needs and cargo doesn't give you:
// a gate that fails when a new test passes before the production code exists, a gate that
// refuses "0 tests ran" as green, and a short report (which binary matched, the panic of
// each failure) instead of the full libtest output.
//
// Run `node .claude/skills/tdd/tdd.mjs help` for usage.

import { spawnSync } from 'node:child_process';
import { closeSync, mkdtempSync, openSync, readFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const SKILL_DIR = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(SKILL_DIR, '..', '..', '..');
const OUT = mkdtempSync(join(tmpdir(), 'visor-tdd-'));

const c = {
  red: (s) => `\x1b[31m${s}\x1b[0m`,
  green: (s) => `\x1b[32m${s}\x1b[0m`,
  yellow: (s) => `\x1b[33m${s}\x1b[0m`,
  dim: (s) => `\x1b[2m${s}\x1b[0m`,
  bold: (s) => `\x1b[1m${s}\x1b[0m`,
};

const die = (msg, code = 2) => {
  console.error(c.red(msg));
  process.exit(code);
};

// ---------------------------------------------------------------- args

function parseArgs(argv) {
  const opts = { crates: [], exact: false, filter: null };
  const rest = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a === '-p' || a === '--package') opts.crates.push(argv[++i]);
    else if (a === '--exact') opts.exact = true;
    else rest.push(a);
  }
  opts.filter = rest[0] ?? null;
  return opts;
}

const scope = (opts) => (opts.crates.length ? opts.crates.flatMap((p) => ['-p', p]) : ['--workspace']);

// ---------------------------------------------------------------- run cargo

/** Runs cargo with stdout and stderr in one file, so "Running <binary>" lines stay in order. */
function cargo(args) {
  const log = join(OUT, `cargo-${Date.now()}.log`);
  const fd = openSync(log, 'w');
  const t0 = Date.now();
  const r = spawnSync('cargo', args, {
    cwd: REPO,
    stdio: ['ignore', fd, fd],
    env: { ...process.env, CARGO_TERM_COLOR: 'never', RUST_BACKTRACE: '0' },
  });
  closeSync(fd);
  return { status: r.status, out: readFileSync(log, 'utf8'), seconds: (Date.now() - t0) / 1000, log };
}

/** Compiler errors, deduplicated, as `error[E0425]: ... --> path:line`. */
function compileErrors(out) {
  const errs = [];
  const lines = out.split('\n');
  for (let i = 0; i < lines.length; i++) {
    if (/^error(\[E\d+\])?:/.test(lines[i]) && !/could not compile|aborting due to/.test(lines[i])) {
      const at = lines.slice(i + 1, i + 4).find((l) => l.trim().startsWith('-->'));
      errs.push(`${lines[i]}${at ? `  ${c.dim(at.trim())}` : ''}`);
    }
  }
  return [...new Set(errs)];
}

/** Parses libtest output across every test binary cargo ran. */
function parseTests(out) {
  const tests = [];
  let binary = '?';
  for (const line of out.split('\n')) {
    const run = /^\s*(Running|Doc-tests)\s+(.*)$/.exec(line);
    if (run) {
      // "Running unittests src/lib.rs (target/debug/deps/visor_core-abc)" -> visor_core (unittests)
      const m = /\(.*\/deps\/([A-Za-z0-9_]+)-[0-9a-f]+\)/.exec(run[2]);
      const kind = run[1] === 'Doc-tests' ? 'doc' : run[2].split(' ')[0].replace(/^unittests$/, 'unit');
      binary = run[1] === 'Doc-tests' ? `${run[2].trim()} (doc)` : `${m ? m[1] : run[2]} (${kind})`;
      continue;
    }
    const t = /^test (.+?) \.\.\. (ok|FAILED|ignored)/.exec(line);
    if (t) tests.push({ name: t[1].replace(/ - should panic$/, ''), status: t[2], binary });
  }
  return tests;
}

/** The "---- name stdout ----" block of each failure: the panic message and location. */
function failureDetails(out) {
  const details = new Map();
  const re = /^---- (.+?) stdout ----\n([\s\S]*?)(?=^---- |^failures:|^test result:)/gm;
  let m;
  while ((m = re.exec(out))) details.set(m[1], m[2].trim());
  return details;
}

function runTests(opts) {
  const args = ['test', ...scope(opts), '--no-fail-fast'];
  const testArgs = [];
  if (opts.filter) testArgs.push(opts.filter);
  if (opts.exact) testArgs.push('--exact');
  const r = cargo(testArgs.length ? [...args, '--', ...testArgs] : args);
  const tests = parseTests(r.out);
  const errors = compileErrors(r.out);
  // Cargo builds every test binary before running any, so a compile error means nothing ran.
  // (It exits 101 for failing tests too, so the status alone can't tell them apart.)
  const compiled = !/^error: could not compile/m.test(r.out);
  return { ...r, tests, errors, compiled, details: failureDetails(r.out) };
}

// ---------------------------------------------------------------- report

function describeMatch(tests) {
  const by = new Map();
  for (const t of tests) by.set(t.binary, (by.get(t.binary) ?? 0) + 1);
  return [...by].map(([b, n]) => `${n} in ${b}`).join(', ');
}

function report(res) {
  const ran = res.tests.filter((t) => t.status !== 'ignored');
  const failed = ran.filter((t) => t.status === 'FAILED');
  const passed = ran.length - failed.length;
  const ignored = res.tests.length - ran.length;
  if (res.tests.length) console.log(c.dim(`matched ${describeMatch(res.tests)}`));
  for (const f of failed) {
    console.log(`\n${c.red('FAIL')} ${c.bold(f.name)}  ${c.dim(f.binary)}`);
    const d = res.details.get(f.name);
    if (d) console.log(`  ${d.split('\n').slice(0, 20).join('\n  ')}`);
  }
  const line = `${passed} passed, ${failed.length} failed, ${ignored} ignored  ${c.dim(`(${res.seconds.toFixed(1)}s)`)}`;
  console.log(`\n${failed.length ? c.red(line) : c.green(line)}`);
  return { ran: ran.length, failed: failed.length, passed };
}

function reportBuildFailure(res, hard) {
  console.log(`${hard ? c.red('BUILD FAILED') : c.yellow('BUILD FAILED')} ${c.dim(`(${res.seconds.toFixed(1)}s)`)}`);
  for (const e of res.errors.slice(0, 15)) console.log(`  ${e}`);
  if (!res.errors.length) console.log(res.out.trim().split('\n').slice(-30).join('\n'));
  console.log(c.dim(`full output: ${res.log}`));
}

// ---------------------------------------------------------------- commands

function cmdRed(opts) {
  if (!opts.filter) die('red needs a filter: tdd.mjs red split_keeps_both_halves');
  const res = runTests(opts);
  if (!res.compiled) {
    reportBuildFailure(res, false);
    console.log(
      `\n${c.red('RED (compile)')} — the test doesn't compile yet. That's a legitimate first red ` +
        `only if the errors above are about the production item the test is for (it doesn't ` +
        `exist yet). If they're typos or unrelated breakage, fix them and re-run.`,
    );
    process.exit(0);
  }
  if (!res.tests.length) die(`no test matches "${opts.filter}" — did you save the file? run \`list\` to see what exists`, 3);
  const s = report(res);
  if (s.failed > 0) {
    console.log(c.red(`RED — ${s.failed} failing. Now write the minimum production code to pass, then \`green\`.`));
    process.exit(0);
  }
  console.log(
    c.yellow(`NOT RED — every matching test passes before the change, so none of them pins the new behaviour. `) +
      c.yellow(`Strengthen the assertion (or the filter matched an old test).`),
  );
  process.exit(1);
}

function cmdGreen(opts) {
  const res = runTests(opts);
  if (!res.compiled) {
    reportBuildFailure(res, true);
    process.exit(2);
  }
  if (opts.filter && !res.tests.length) die(`no test matches "${opts.filter}" — run \`list\` to see what exists`, 3);
  const s = report(res);
  if (s.failed) process.exit(1);
  if (!s.ran) die('0 tests ran — that is not green', 3);
  console.log(c.green('GREEN'));
  process.exit(0);
}

function cmdTest(opts) {
  const res = runTests(opts);
  if (!res.compiled) {
    reportBuildFailure(res, true);
    process.exit(2);
  }
  if (opts.filter && !res.tests.length) die(`no test matches "${opts.filter}"`, 3);
  const s = report(res);
  process.exit(s.failed ? 1 : 0);
}

function cmdList(opts) {
  const args = ['test', ...scope(opts), '--', '--list'];
  const r = cargo(args);
  if (r.status !== 0) {
    reportBuildFailure({ ...r, errors: compileErrors(r.out) }, true);
    process.exit(2);
  }
  let binary = '?';
  const by = new Map();
  for (const line of r.out.split('\n')) {
    const run = /^\s*(Running|Doc-tests)\s+(.*)$/.exec(line);
    if (run) {
      const m = /\(.*\/deps\/([A-Za-z0-9_]+)-[0-9a-f]+\)/.exec(run[2]);
      binary = m ? m[1] : run[2].trim();
      continue;
    }
    const t = /^(.+): test$/.exec(line);
    if (t && (!opts.filter || t[1].toLowerCase().includes(opts.filter.toLowerCase()))) {
      if (!by.has(binary)) by.set(binary, []);
      by.get(binary).push(t[1]);
    }
  }
  if (!by.size) die(`no test matches "${opts.filter}"`, 3);
  for (const [b, names] of by) {
    console.log(c.bold(`${b}  ${c.dim(`(${names.length})`)}`));
    for (const n of names) console.log(`  ${n}`);
  }
}

/** Everything that must pass before a change is done (macOS host). */
function cmdVerify() {
  const steps = [
    ['fmt', ['fmt', '--all', '--check']],
    ['clippy', ['clippy', '--workspace', '--all-targets', '--', '-D', 'warnings']],
  ];
  for (const [name, args] of steps) {
    const r = cargo(args);
    if (r.status !== 0) {
      console.log(c.red(`${name} FAILED`) + c.dim(` (${r.seconds.toFixed(1)}s)`));
      console.log(r.out.trim().split('\n').slice(-40).join('\n'));
      if (name === 'fmt') console.log(c.dim('`cargo fmt --all` fixes it.'));
      process.exit(1);
    }
    console.log(c.green(`${name} ok`) + c.dim(` (${r.seconds.toFixed(1)}s)`));
  }
  const res = runTests({ crates: [], filter: null, exact: false });
  if (!res.compiled) {
    reportBuildFailure(res, true);
    process.exit(2);
  }
  const s = report(res);
  if (s.failed) process.exit(1);
  console.log(c.green(`VERIFIED on macOS — ${s.passed} tests. Platform code touched? Also run scripts/linux-check.sh / scripts/windows-check.sh.`));
}

const HELP = `tdd.mjs — red/green gate for the Visor cargo workspace

  red <filter>      run matching tests; exit 0 ONLY if one fails or the test doesn't compile
  green [filter]    run matching tests; exit 0 ONLY if they compile, >0 ran, and all pass
  test [filter]     run and report either way
  list [filter]     tests per binary (substring, case-insensitive)
  verify            cargo fmt --check, clippy -D warnings, all tests (macOS host)

  -p <crate>        limit to a crate (repeatable), e.g. -p visor-core — much faster
  --exact           filter must equal the full test path (mod::name)

Filters are libtest substrings of the test path (edit::tests::split_keeps_both_halves).
Exit codes: 0 gate passed, 1 gate failed, 2 build failed, 3 nothing matched.`;

const [cmd, ...argv] = process.argv.slice(2);
const opts = parseArgs(argv);
switch (cmd) {
  case 'red':
    cmdRed(opts);
    break;
  case 'green':
    cmdGreen(opts);
    break;
  case 'test':
    cmdTest(opts);
    break;
  case 'list':
    cmdList(opts);
    break;
  case 'verify':
    cmdVerify();
    break;
  default:
    console.log(HELP);
    process.exit(cmd && cmd !== 'help' ? 2 : 0);
}
