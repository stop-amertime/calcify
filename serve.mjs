#!/usr/bin/env node
// CSS-DOS dev server — serves the site and generates CSS from .com/.exe uploads.
//
// Usage: node serve.mjs [--port 8080]
//
// Endpoints:
//   GET /*           Static files from site/
//   POST /generate   Upload a .com/.exe, get back generated CSS
//                    Body: multipart/form-data with field "program"
//                    Response: streams the generated .css file

import { createServer } from 'http';
import { readFileSync, readdirSync, existsSync, statSync, createReadStream, createWriteStream, writeFileSync, mkdirSync, unlinkSync } from 'fs';
import { resolve, dirname, extname, join, basename } from 'path';
import { fileURLToPath } from 'url';
import { execSync } from 'child_process';

const __dirname = dirname(fileURLToPath(import.meta.url));
const SITE_DIR = resolve(__dirname, 'site');
const CSS_DOS_DIR = resolve(__dirname, '..', 'CSS-DOS');
const GENERATOR = resolve(CSS_DOS_DIR, 'transpiler', 'generate-dos.mjs');
const OUTPUT_DIR = resolve(__dirname, 'output');

const PORT = parseInt(process.argv.find((_, i, a) => a[i - 1] === '--port') || '8080');

const MIME = {
  '.html': 'text/html',
  '.css': 'text/css',
  '.js': 'application/javascript',
  '.mjs': 'application/javascript',
  '.json': 'application/json',
  '.wasm': 'application/wasm',
  '.gz': 'application/gzip',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.svg': 'image/svg+xml',
};

if (!existsSync(OUTPUT_DIR)) mkdirSync(OUTPUT_DIR, { recursive: true });

const server = createServer(async (req, res) => {
  // CORS for local dev
  res.setHeader('Access-Control-Allow-Origin', '*');
  res.setHeader('Access-Control-Allow-Methods', 'GET, POST, OPTIONS');
  res.setHeader('Access-Control-Allow-Headers', 'Content-Type');
  if (req.method === 'OPTIONS') { res.writeHead(204); res.end(); return; }

  // GET /list — list available CSS files in output/ and .com/.exe in programs/
  if (req.method === 'GET' && req.url === '/list') {
    const css = readdirSync(OUTPUT_DIR).filter(f => f.endsWith('.css') || f.endsWith('.css.gz')).map(f => ({
      name: f, size: statSync(resolve(OUTPUT_DIR, f)).size, type: 'css',
    }));
    const PROG_DIR = resolve(__dirname, 'programs');
    let programs = [];
    if (existsSync(PROG_DIR)) {
      programs = readdirSync(PROG_DIR).filter(f => /\.(com|exe)$/i.test(f)).map(f => ({
        name: f, size: statSync(resolve(PROG_DIR, f)).size, type: 'program',
      }));
    }
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ css, programs }));
    return;
  }

  // POST /generate — upload .com/.exe, generate CSS
  if (req.method === 'POST' && req.url === '/generate') {
    return handleGenerate(req, res);
  }

  // POST /upload — save a file to output/ (for large .css files)
  if (req.method === 'POST' && req.url === '/upload') {
    const filename = req.headers['x-filename'] || 'upload.css';
    const outPath = resolve(OUTPUT_DIR, basename(filename));
    const ws = createWriteStream(outPath);
    req.pipe(ws);
    ws.on('finish', () => {
      const size = statSync(outPath).size;
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ file: basename(filename), size }));
    });
    ws.on('error', (err) => {
      res.writeHead(500, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: err.message }));
    });
    return;
  }

  // Static file serving
  let urlPath = new URL(req.url, 'http://localhost').pathname;
  if (urlPath === '/') urlPath = '/index.html';

  // /output/ serves from the output directory (generated CSS files)
  // /programs/ serves from the programs directory (.com/.exe binaries)
  let filePath;
  const PROG_DIR = resolve(__dirname, 'programs');
  if (urlPath.startsWith('/output/')) {
    filePath = resolve(OUTPUT_DIR, urlPath.slice('/output/'.length));
    if (!filePath.startsWith(OUTPUT_DIR)) { res.writeHead(403); res.end('Forbidden'); return; }
  } else if (urlPath.startsWith('/programs/')) {
    filePath = resolve(PROG_DIR, urlPath.slice('/programs/'.length));
    if (!filePath.startsWith(PROG_DIR)) { res.writeHead(403); res.end('Forbidden'); return; }
  } else if (urlPath.startsWith('/web/')) {
    const WEB_DIR = resolve(__dirname, 'web');
    filePath = resolve(WEB_DIR, urlPath.slice('/web/'.length));
    if (!filePath.startsWith(WEB_DIR)) { res.writeHead(403); res.end('Forbidden'); return; }
  } else {
    filePath = resolve(SITE_DIR, '.' + urlPath);
    if (!filePath.startsWith(SITE_DIR)) { res.writeHead(403); res.end('Forbidden'); return; }
  }

  if (!existsSync(filePath) || statSync(filePath).isDirectory()) {
    res.writeHead(404); res.end('Not found'); return;
  }

  const ext = extname(filePath);
  const mime = MIME[ext] || 'application/octet-stream';
  res.writeHead(200, { 'Content-Type': mime });
  createReadStream(filePath).pipe(res);
});

async function handleGenerate(req, res) {
  try {
    // Read the entire body
    const chunks = [];
    for await (const chunk of req) chunks.push(chunk);
    const body = Buffer.concat(chunks);

    // Parse multipart — simple boundary-based parser
    const contentType = req.headers['content-type'] || '';
    let programBytes, programName;

    if (contentType.includes('multipart/form-data')) {
      const boundary = contentType.split('boundary=')[1];
      if (!boundary) throw new Error('Missing boundary');
      const { name, data } = parseMultipart(body, boundary);
      programName = name;
      programBytes = data;
    } else {
      // Raw binary upload with filename in header
      programName = req.headers['x-filename'] || 'program.com';
      programBytes = body;
    }

    if (!programBytes || programBytes.length === 0) {
      res.writeHead(400, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: 'No program data received' }));
      return;
    }

    const base = basename(programName, extname(programName));
    const tmpPath = resolve(OUTPUT_DIR, programName);
    const cssPath = resolve(OUTPUT_DIR, base + '.css');

    // Write the uploaded binary
    writeFileSync(tmpPath, programBytes);

    console.log(`Generating CSS for ${programName} (${programBytes.length} bytes)...`);
    const t0 = Date.now();

    // Run the transpiler
    try {
      execSync(
        `node --max-old-space-size=8192 "${GENERATOR}" "${tmpPath}" -o "${cssPath}"`,
        { stdio: ['pipe', 'pipe', 'pipe'], timeout: 300000 }
      );
    } catch (e) {
      const stderr = e.stderr?.toString() || e.message;
      console.error('Generate failed:', stderr);
      res.writeHead(500, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: 'CSS generation failed', details: stderr }));
      // Clean up
      try { unlinkSync(tmpPath); } catch (_) {}
      return;
    }

    const elapsed = ((Date.now() - t0) / 1000).toFixed(1);
    const size = statSync(cssPath).size;
    console.log(`Generated ${cssPath} (${(size / 1024 / 1024).toFixed(1)} MB) in ${elapsed}s`);

    // Clean up the uploaded binary
    try { unlinkSync(tmpPath); } catch (_) {}

    // Return the filename — browser will fetch from /output/ to stream to WASM
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({
      file: base + '.css',
      size,
      elapsed,
    }));

  } catch (err) {
    console.error('Generate error:', err);
    res.writeHead(500, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify({ error: err.message }));
  }
}

// Minimal multipart parser — extracts the first file field
function parseMultipart(body, boundary) {
  const sep = Buffer.from('--' + boundary);
  const parts = [];
  let start = 0;
  while (true) {
    const idx = body.indexOf(sep, start);
    if (idx === -1) break;
    if (start > 0) parts.push(body.slice(start, idx));
    start = idx + sep.length;
    // Skip \r\n after boundary
    if (body[start] === 0x0D) start += 2;
    else if (body[start] === 0x0A) start += 1;
  }

  for (const part of parts) {
    const headerEnd = part.indexOf('\r\n\r\n');
    if (headerEnd === -1) continue;
    const headers = part.slice(0, headerEnd).toString();
    const data = part.slice(headerEnd + 4);
    // Trim trailing \r\n
    const trimmed = data.length >= 2 && data[data.length - 2] === 0x0D
      ? data.slice(0, -2) : data;

    const nameMatch = headers.match(/filename="([^"]+)"/);
    if (nameMatch) {
      return { name: nameMatch[1], data: trimmed };
    }
  }
  throw new Error('No file found in multipart upload');
}

server.listen(PORT, () => {
  console.log(`CSS-DOS dev server running at http://localhost:${PORT}/`);
  console.log(`  Site:     ${SITE_DIR}`);
  console.log(`  CSS-DOS:  ${CSS_DOS_DIR}`);
  console.log(`  Output:   ${OUTPUT_DIR}`);
  console.log(`\n  Open http://localhost:${PORT}/run.html to get started`);
  console.log(`  Drop a .com/.exe file to generate + run it\n`);
});
