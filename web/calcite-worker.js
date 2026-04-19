/**
 * calc(ite) Web Worker — runs the WASM engine off the main thread.
 *
 * Protocol:
 *   Main → Worker:
 *     { type: 'init', css: string }       — parse and compile CSS
 *     { type: 'tick', count: number }      — run N ticks, return output
 *     { type: 'keyboard', key: number }    — JS-bridge keypress (see note)
 *
 *   NOTE on keyboard: the CSS-DOS output has pure-CSS `:has(button:active)`
 *   rules that drive --keyboard in Chrome. Calcite can't evaluate selector
 *   matching (no DOM, no :active) so for the web runner we bridge the
 *   on-screen buttons through JS — pointerdown/pointerup on each button
 *   post a message here, and we write the key into BDA via set_keyboard().
 *   The pure-CSS path still works when the .css is opened in Chrome directly.
 *
 *   Worker → Main:
 *     { type: 'ready', video: { text, gfx } }
 *         text/gfx: {addr,size,width,height}|null
 *     { type: 'tick-result', stringProperties, screen, gfxBytes, ticks, cycles, videoMode }
 *         screen  : text-mode rendered string (if text mode detected)
 *         gfxBytes: Uint8ClampedArray.buffer of RGBA pixels (if gfx mode)
 *     { type: 'error', message: string }
 */

let engine = null;
let videoRegions = { text: null, gfx: null };

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
      case 'init': {
        const t0 = performance.now();
        if (!wasmModule) {
          wasmModule = await loadWasm();
        }
        const tWasmLoaded = performance.now();

        const cssBytes = data.css.length;
        engine = new wasmModule.CalciteEngine(data.css);
        const tEngineBuilt = performance.now();

        // Detect video regions. The new JSON shape is {text, gfx}; either
        // can be null. If neither is present, fall back to assuming
        // standard DOS text mode at 0xB8000 so simple programs still work.
        const videoJson = engine.detect_video();
        const tVideoDetected = performance.now();

        const timing = {
          cssBytes,
          wasmLoadMs: +(tWasmLoaded - t0).toFixed(1),
          parseCompileMs: +(tEngineBuilt - tWasmLoaded).toFixed(1),
          detectVideoMs: +(tVideoDetected - tEngineBuilt).toFixed(1),
          totalMs: +(tVideoDetected - t0).toFixed(1),
        };
        console.log('[calcite init]', JSON.stringify(timing));
        const parsed = JSON.parse(videoJson) || {};
        videoRegions = {
          text: parsed.text || null,
          gfx: parsed.gfx || null,
        };
        if (!videoRegions.text && !videoRegions.gfx) {
          videoRegions.text = { addr: 0xB8000, size: 4000, width: 80, height: 25 };
        }

        self.postMessage({ type: 'ready', video: videoRegions, timing });
        break;
      }

      case 'tick': {
        if (!engine) {
          throw new Error('Engine not initialised — send "init" first');
        }
        engine.tick_batch(data.count || 1);
        const stringProps = JSON.parse(engine.get_string_properties());
        const cycles = engine.get_state_var('cycleCount') >>> 0;

        // Read current video mode from BDA (0x0449). This is the runtime
        // source of truth: what INT 10h AH=00h last wrote. The runner uses
        // it to decide which output to show (text vs. canvas).
        const videoMode = engine.get_video_mode();
        const isGfxMode = videoMode === 0x13; // Mode 13h: 320x200x256

        // Diagnostics: what video mode the program asked for (pre-remap),
        // and whether the CPU has hit an unknown opcode. Both stickily
        // latched in the engine; the host surfaces them as warnings.
        const requestedVideoMode = engine.get_requested_video_mode();
        const haltCode = engine.get_halt_code();

        // Text-mode screen: only rendered when not in a graphics mode.
        // HTML variant includes CGA color spans; the UI sets innerHTML.
        let screen = null;
        if (!isGfxMode && videoRegions.text) {
          const t = videoRegions.text;
          screen = engine.render_screen_html(t.addr, t.width, t.height);
        }

        // Graphics-mode framebuffer: only read when in a graphics mode.
        let gfxBytes = null;
        const transfer = [];
        if (isGfxMode && videoRegions.gfx) {
          const g = videoRegions.gfx;
          // read_framebuffer_rgba returns a Uint8Array backed by wasm
          // memory — copy into a new ArrayBuffer we can transfer.
          const wasmView = engine.read_framebuffer_rgba(g.addr, g.width, g.height);
          const buf = new ArrayBuffer(wasmView.length);
          new Uint8Array(buf).set(wasmView);
          gfxBytes = buf;
          transfer.push(buf);
        }

        self.postMessage(
          {
            type: 'tick-result',
            stringProperties: stringProps,
            screen,
            gfxBytes,
            videoMode,
            requestedVideoMode,
            haltCode,
            ticks: data.count || 1,
            cycles,
          },
          transfer,
        );
        break;
      }

      case 'keyboard': {
        if (engine) {
          engine.set_keyboard(data.key || 0);
        }
        break;
      }

      default:
        console.warn(`calc(ite) worker: unknown message type "${type}"`);
    }
  } catch (err) {
    self.postMessage({ type: 'error', message: err.message || String(err) });
  }
};
