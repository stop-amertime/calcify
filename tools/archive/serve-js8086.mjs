#!/usr/bin/env node
// serve-js8086.mjs — Simple HTTP server for the JS8086 browser emulator.
//
// Usage: node tools/serve-js8086.mjs [port]
//
// Serves:
//   /            -> web/js8086.html
//   /web/*       -> calcite/web/ (worker, etc.)
//   /css-dos/*   -> CSS-DOS/ (js8086.js, kernel.sys, disk.img)
//   /output/*    -> calcite/output/ (for CSS files if needed)

import { createServer } from 'http';
import { readFileSync, existsSync } from 'fs';
import { resolve, extname, dirname } from 'path';
import { fileURLToPath } from 'url';

const __dirname = dirname(fileURLToPath(import.meta.url));
const calciteRoot = resolve(__dirname, '..');
const cssDosRoot = resolve(calciteRoot, '..', 'CSS-DOS');
const port = parseInt(process.argv[2] || '8086');

const MIME = {
  '.html': 'text/html',
  '.js': 'text/javascript',
  '.mjs': 'text/javascript',
  '.css': 'text/css',
  '.json': 'application/json',
  '.sys': 'application/octet-stream',
  '.img': 'application/octet-stream',
  '.com': 'application/octet-stream',
  '.exe': 'application/octet-stream',
  '.bin': 'application/octet-stream',
  '.wasm': 'application/wasm',
};

function servePath(res, filePath) {
  if (!existsSync(filePath)) {
    res.writeHead(404);
    res.end('Not found: ' + filePath);
    return;
  }
  const ext = extname(filePath).toLowerCase();
  const mime = MIME[ext] || 'application/octet-stream';
  const data = readFileSync(filePath);
  res.writeHead(200, { 'Content-Type': mime, 'Content-Length': data.length });
  res.end(data);
}

const server = createServer((req, res) => {
  const url = decodeURIComponent(req.url.split('?')[0]);

  if (url === '/' || url === '/index.html') {
    return servePath(res, resolve(calciteRoot, 'web', 'js8086.html'));
  }
  if (url.startsWith('/web/')) {
    return servePath(res, resolve(calciteRoot, url.slice(1)));
  }
  if (url.startsWith('/css-dos/')) {
    return servePath(res, resolve(cssDosRoot, url.slice('/css-dos/'.length)));
  }
  if (url.startsWith('/output/')) {
    return servePath(res, resolve(calciteRoot, url.slice(1)));
  }
  // Default: try web/ directory
  if (url.endsWith('.js') || url.endsWith('.html') || url.endsWith('.css') || url.endsWith('.wasm')) {
    return servePath(res, resolve(calciteRoot, 'web', url.slice(1)));
  }

  res.writeHead(404);
  res.end('Not found');
});

server.listen(port, () => {
  console.log(`JS8086 emulator running at http://localhost:${port}`);
  console.log(`  CSS-DOS: ${cssDosRoot}`);
  console.log(`  Calcite: ${calciteRoot}`);
  console.log();
  console.log(`Open http://localhost:${port} in your browser.`);
});
