/**
 * CSS-DOS Calcite Worker — runs the WASM engine off the main thread.
 *
 * Protocol:
 *   Main -> Worker:
 *     { type: 'load', url: string }         — fetch .css.gz, decompress, parse, compile
 *     { type: 'tick', count: number }        — run N ticks, return video + string data
 *     { type: 'keyboard', key: number }      — update keyboard state
 *
 *   Worker -> Main:
 *     { type: 'progress', phase, detail }    — loading progress updates
 *     { type: 'ready', video }               — engine ready, video config if detected
 *     { type: 'tick-result', videoBytes, stringProperties, ticks }
 *     { type: 'error', message }
 */

let engine = null;
let videoConfig = null;

async function loadWasm() {
  const wasm = await import('./pkg/calcite_wasm.js');
  await wasm.default();
  return wasm;
}

let wasmModule = null;

self.onmessage = async function (event) {
  const { type, ...data } = event.data;

  try {
    switch (type) {
      case 'load': {
        // Phase 1: Load WASM module
        self.postMessage({ type: 'progress', phase: 'wasm', detail: 'Loading Calcite engine...' });
        if (!wasmModule) {
          wasmModule = await loadWasm();
        }

        // Phase 2: Compile CSS (passed directly as string)
        const css = data.css;
        const mb = (css.length / 1024 / 1024).toFixed(0);
        self.postMessage({ type: 'progress', phase: 'compile', detail: `Compiling ${mb} MB of CSS...` });

        engine = new wasmModule.CalciteEngine(css);

        // Detect video
        const videoJson = engine.detect_video();
        videoConfig = JSON.parse(videoJson);

        self.postMessage({ type: 'ready', video: videoConfig });
        break;
      }

      case 'tick': {
        if (!engine) throw new Error('Engine not initialised');

        const count = data.count || 100;
        engine.tick_batch(count);

        // Read video memory if available
        let videoBytes = null;
        if (videoConfig) {
          videoBytes = engine.read_video_memory(videoConfig.addr, videoConfig.width, videoConfig.height);
        }

        // Read string properties
        const stringProps = JSON.parse(engine.get_string_properties());

        self.postMessage({
          type: 'tick-result',
          videoBytes,
          stringProperties: stringProps,
          ticks: count,
        });
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
