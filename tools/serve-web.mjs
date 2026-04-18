#!/usr/bin/env node
// Tiny static server for calcite's web runner.
//
//   /             → calcite repo root
//   /web/...      → web/index.html and friends
//   /output/...   → built cabinets
//   /pkg/...      → wasm-pack output (served from web/pkg for convenience)
//
// Usage: node tools/serve-web.mjs [--port 8766]
//
// A probe endpoint GET /__calcite is included so run-web.bat can tell whether
// the process on a given port is actually this server (vs. CSS-DOS's
// player/serve.mjs, which defaults to 8765 and confuses naive port checks).

import { createServer } from 'node:http';
import { statSync, createReadStream, existsSync } from 'node:fs';
import { resolve, dirname, join, extname, normalize } from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(__dirname, '..');

const port = (() => {
  const i = process.argv.indexOf('--port');
  return i >= 0 ? parseInt(process.argv[i + 1], 10) : 8766;
})();

const MIME = {
  '.html': 'text/html; charset=utf-8',
  '.css':  'text/css; charset=utf-8',
  '.js':   'application/javascript',
  '.mjs':  'application/javascript',
  '.json': 'application/json',
  '.wasm': 'application/wasm',
  '.png':  'image/png',
  '.svg':  'image/svg+xml',
  '.ico':  'image/x-icon',
  '.map':  'application/json',
};

function safeJoin(root, reqPath) {
  const p = normalize(join(root, reqPath));
  if (!p.startsWith(root)) return null;
  return p;
}

const server = createServer((req, res) => {
  let url = decodeURIComponent(req.url.split('?')[0]);

  if (url === '/__calcite') {
    res.writeHead(200, { 'Content-Type': 'text/plain' });
    res.end('calcite-serve-web');
    return;
  }

  if (url === '/') url = '/web/index.html';

  const filePath = safeJoin(repoRoot, url);
  if (!filePath || !existsSync(filePath)) {
    res.writeHead(404, { 'Content-Type': 'text/plain' });
    res.end(`Not found: ${url}`);
    return;
  }

  let stat;
  try { stat = statSync(filePath); } catch {
    res.writeHead(404); res.end(); return;
  }
  if (stat.isDirectory()) {
    res.writeHead(403, { 'Content-Type': 'text/plain' });
    res.end('Directory listing disabled');
    return;
  }

  const mime = MIME[extname(filePath).toLowerCase()] || 'application/octet-stream';
  res.writeHead(200, {
    'Content-Type': mime,
    'Content-Length': stat.size,
    'Cache-Control': 'no-store, no-cache, must-revalidate, max-age=0',
    'Pragma': 'no-cache',
  });
  createReadStream(filePath).pipe(res);
});

server.listen(port, () => {
  const url = `http://localhost:${port}/web/index.html`;
  console.log(`calcite web server on ${url}`);
  console.log(`  cabinets served from /output/`);
});
