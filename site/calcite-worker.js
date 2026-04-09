/**
 * CSS-DOS Calcite Worker — runs the WASM engine off the main thread.
 *
 * Protocol:
 *   Main -> Worker:
 *     { type: 'load', cssBytes: Uint8Array }  — parse + compile CSS from raw bytes
 *     { type: 'load-url', url: string }       — fetch CSS (or .css.gz), parse + compile
 *     { type: 'tick', count: number }         — run N ticks, return video bytes
 *     { type: 'keyboard', key: number }       — update keyboard state
 *
 *   Worker -> Main:
 *     { type: 'progress', detail }            — loading progress updates
 *     { type: 'ready', video }                — engine ready, video config
 *     { type: 'tick-result', videoBytes, ticks }
 *     { type: 'error', message }
 */

let engine = null;
let videoConfig = null;
let wasmModule = null;

async function initWasm() {
  if (!wasmModule) {
    wasmModule = await import('./pkg/calcite_wasm.js');
    await wasmModule.default();
  }
}

self.onmessage = async function (event) {
  const { type, ...data } = event.data;

  try {
    switch (type) {
      case 'load-url': {
        self.postMessage({ type: 'progress', detail: 'Loading Calcite engine...' });
        await initWasm();

        // Fetch the CSS file
        self.postMessage({ type: 'progress', detail: `Fetching ${data.url}...` });
        const resp = await fetch(data.url);
        if (!resp.ok) throw new Error(`HTTP ${resp.status}: ${data.url}`);

        let stream = resp.body;
        if (data.url.endsWith('.gz')) {
          self.postMessage({ type: 'progress', detail: 'Decompressing...' });
          stream = stream.pipeThrough(new DecompressionStream('gzip'));
        }

        // Read into Uint8Array chunks
        const reader = stream.getReader();
        const chunks = [];
        let totalBytes = 0;
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          chunks.push(value);
          totalBytes += value.length;
          if (totalBytes % (50 * 1024 * 1024) < 65536) {
            self.postMessage({ type: 'progress', detail: `Reading CSS... ${(totalBytes / 1024 / 1024).toFixed(0)} MB` });
          }
        }

        // Concatenate
        const cssBytes = new Uint8Array(totalBytes);
        let offset = 0;
        for (const chunk of chunks) {
          cssBytes.set(chunk, offset);
          offset += chunk.length;
        }

        self.postMessage({ type: 'progress', detail: `Compiling ${(totalBytes / 1024 / 1024).toFixed(0)} MB of CSS...` });
        engine = wasmModule.CalciteEngine.new_from_bytes(cssBytes);

        videoConfig = JSON.parse(engine.detect_video());
        self.postMessage({ type: 'ready', video: videoConfig });
        break;
      }

      case 'load': {
        self.postMessage({ type: 'progress', detail: 'Loading Calcite engine...' });
        await initWasm();

        const cssBytes = data.cssBytes;
        self.postMessage({ type: 'progress', detail: `Compiling ${(cssBytes.length / 1024 / 1024).toFixed(0)} MB of CSS...` });
        engine = wasmModule.CalciteEngine.new_from_bytes(cssBytes);

        videoConfig = JSON.parse(engine.detect_video());
        self.postMessage({ type: 'ready', video: videoConfig });
        break;
      }

      case 'tick': {
        if (!engine) throw new Error('Engine not initialised');

        const count = data.count || 500;
        engine.tick_batch(count);

        let videoBytes = null;
        if (videoConfig) {
          videoBytes = engine.read_video_memory(videoConfig.addr, videoConfig.width, videoConfig.height);
          if (!self._tickLog) {
            self._tickLog = true;
            console.log('[worker tick] videoConfig:', videoConfig);
            console.log('[worker tick] videoBytes type:', videoBytes?.constructor?.name, 'length:', videoBytes?.length);
            console.log('[worker tick] first 20:', videoBytes ? Array.from(videoBytes.slice(0, 20)) : 'null');
            console.log('[worker tick] non-zero:', videoBytes ? videoBytes.filter(b => b !== 0).length : 0);
          }
        } else if (!self._tickLog) {
          self._tickLog = true;
          console.warn('[worker tick] no videoConfig — screen will be blank');
        }

        self.postMessage({
          type: 'tick-result',
          videoBytes,
          ticks: count,
        }, videoBytes ? [videoBytes.buffer] : []);
        break;
      }

      case 'keyboard': {
        if (engine) {
          engine.set_keyboard(data.key || 0);
        }
        break;
      }
    }
  } catch (err) {
    self.postMessage({ type: 'error', message: err.message || String(err) });
  }
};
