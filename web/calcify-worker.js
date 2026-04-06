/**
 * calc(ify) Web Worker — runs the WASM engine off the main thread.
 *
 * Protocol:
 *   Main → Worker:
 *     { type: 'init', css: string }       — parse and compile CSS
 *     { type: 'tick', count: number }      — run N ticks, return changes
 *     { type: 'keyboard', key: number }    — update keyboard state
 *
 *   Worker → Main:
 *     { type: 'ready' }                    — engine initialised
 *     { type: 'tick-result', changes: [[name, value], ...], ticks: number }
 *     { type: 'error', message: string }
 */

let engine = null;

// The WASM module will be loaded dynamically
async function loadWasm() {
  // wasm-pack generates this module
  const wasm = await import('./pkg/calcify_wasm.js');
  await wasm.default(); // init WASM
  return wasm;
}

let wasmModule = null;

self.onmessage = async function (event) {
  const { type, ...data } = event.data;

  try {
    switch (type) {
      case 'init': {
        if (!wasmModule) {
          wasmModule = await loadWasm();
        }
        engine = new wasmModule.CalcifyEngine(data.css);
        self.postMessage({ type: 'ready' });
        break;
      }

      case 'tick': {
        if (!engine) {
          throw new Error('Engine not initialised — send "init" first');
        }
        const changesJson = engine.tick_batch(data.count || 1);
        const changes = JSON.parse(changesJson);
        self.postMessage({
          type: 'tick-result',
          changes,
          ticks: data.count || 1,
        });
        break;
      }

      case 'keyboard': {
        if (engine) {
          engine.set_keyboard(data.key || 0);
        }
        break;
      }

      default:
        console.warn(`calc(ify) worker: unknown message type "${type}"`);
    }
  } catch (err) {
    self.postMessage({ type: 'error', message: err.message || String(err) });
  }
};
