#!/usr/bin/env node
// Calcite primitive conformance — Chrome ground-truth runner.
//
// Walks tests/conformance/primitives/, loads each <name>.css against
// runner.html via headless Chromium, reads getComputedStyle for every
// property listed in <name>.expect.json, and compares against
// expected_str.
//
// Two modes:
//   default          run all fixtures, fail on first mismatch
//   --capture        write Chrome's values into expected_str (authoring
//                    aid; review the resulting diffs before committing)
//   --filter=<glob>  only run fixtures whose basename contains <glob>
//
// Cardinal rule: if calcite disagrees with Chrome, calcite is wrong.
// This runner defines what Chrome says.

import { readFile, readdir, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { extname, join, dirname, basename } from 'node:path';
import { fileURLToPath } from 'node:url';
import { stat } from 'node:fs/promises';

import { chromium } from 'playwright';

const HERE = dirname(fileURLToPath(import.meta.url));
const PRIMITIVES_DIR = join(HERE, 'primitives');

const args = process.argv.slice(2);
const CAPTURE = args.includes('--capture');
const filterArg = args.find(a => a.startsWith('--filter='));
const FILTER = filterArg ? filterArg.slice('--filter='.length) : null;

// ---------------------------------------------------------------------------
// Tiny static server. The Playwright MCP can't load file:// URLs and neither
// can we reliably across browsers, so we serve the conformance dir over HTTP
// on an ephemeral port for the lifetime of the run.
// ---------------------------------------------------------------------------
const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
};

function startServer() {
  return new Promise((resolve, reject) => {
    const server = createServer(async (req, res) => {
      try {
        const url = new URL(req.url, 'http://x');
        let path = decodeURIComponent(url.pathname);
        if (path === '/') path = '/runner.html';
        const file = join(HERE, path);
        if (!file.startsWith(HERE)) {
          res.writeHead(403); res.end('forbidden'); return;
        }
        const s = await stat(file).catch(() => null);
        if (!s || !s.isFile()) { res.writeHead(404); res.end('not found'); return; }
        const body = await readFile(file);
        res.writeHead(200, { 'content-type': MIME[extname(file)] || 'application/octet-stream' });
        res.end(body);
      } catch (e) {
        res.writeHead(500); res.end(String(e));
      }
    });
    server.listen(0, '127.0.0.1', () => {
      const port = server.address().port;
      resolve({ server, port });
    });
    server.on('error', reject);
  });
}

// ---------------------------------------------------------------------------
// Fixture discovery + IO.
// ---------------------------------------------------------------------------
async function listFixtures() {
  const entries = await readdir(PRIMITIVES_DIR);
  const cssFiles = entries.filter(f => f.endsWith('.css')).sort();
  const fixtures = [];
  for (const css of cssFiles) {
    const name = css.slice(0, -'.css'.length);
    if (FILTER && !name.includes(FILTER)) continue;
    const expectPath = join(PRIMITIVES_DIR, `${name}.expect.json`);
    let expect;
    try {
      expect = JSON.parse(await readFile(expectPath, 'utf8'));
    } catch (e) {
      console.error(`!! ${name}: missing or invalid expect.json (${e.message})`);
      process.exit(2);
    }
    fixtures.push({ name, cssPath: `primitives/${css}`, expectPath, expect });
  }
  return fixtures;
}

// ---------------------------------------------------------------------------
// Run.
// ---------------------------------------------------------------------------
async function main() {
  const fixtures = await listFixtures();
  if (fixtures.length === 0) {
    console.log('no fixtures found');
    return;
  }

  const { server, port } = await startServer();
  const baseUrl = `http://127.0.0.1:${port}`;
  console.log(`runner: serving ${HERE} on ${baseUrl}`);
  console.log(`runner: ${CAPTURE ? 'CAPTURE mode (will overwrite expected_str)' : 'CHECK mode'}`);

  const browser = await chromium.launch();
  const context = await browser.newContext();
  const page = await context.newPage();

  let pass = 0, fail = 0, captured = 0;
  const failures = [];

  for (const fx of fixtures) {
    const url = `${baseUrl}/runner.html?css=${encodeURIComponent(fx.cssPath)}`;
    await page.goto(url, { waitUntil: 'networkidle' });
    const names = fx.expect.read.map(r => r.property);
    const got = await page.evaluate((ns) => window.readProps(ns), names);

    if (CAPTURE) {
      const updated = { ...fx.expect, read: fx.expect.read.map(r => ({
        ...r,
        expected_str: got[r.property],
      })) };
      await writeFile(fx.expectPath, JSON.stringify(updated, null, 2) + '\n');
      const summary = fx.expect.read.map(r => `${r.property}=${JSON.stringify(got[r.property])}`).join(' ');
      console.log(`CAP ${fx.name}: ${summary}`);
      captured++;
      continue;
    }

    const localFails = [];
    for (const r of fx.expect.read) {
      const actual = got[r.property];
      if (actual !== r.expected_str) {
        localFails.push({ property: r.property, expected: r.expected_str, actual });
      }
    }
    if (localFails.length === 0) {
      console.log(`PASS ${fx.name}`);
      pass++;
    } else {
      fail++;
      console.log(`FAIL ${fx.name}`);
      for (const lf of localFails) {
        console.log(`  ${lf.property}: expected ${JSON.stringify(lf.expected)}, got ${JSON.stringify(lf.actual)}`);
      }
      failures.push({ fixture: fx.name, fails: localFails });
    }
  }

  await browser.close();
  server.close();

  if (CAPTURE) {
    console.log(`\ncaptured ${captured} fixture(s) — review the diffs before committing.`);
    return;
  }

  console.log(`\n${pass} passed, ${fail} failed`);
  if (fail > 0) process.exit(1);
}

main().catch(err => {
  console.error(err);
  process.exit(2);
});
