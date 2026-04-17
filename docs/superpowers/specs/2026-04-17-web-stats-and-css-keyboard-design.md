# calcite-web: perf stats parity + CSS keyboard

Date: 2026-04-17

## Motivation

Two gaps between `calcite-web` (browser runner) and the rest of the project:

1. **Perf stats are thin.** The CLI bench reports ticks/sec, cycles/sec, and
   percentage of a real 4.77 MHz 8086. The web UI only shows ticks/sec.
2. **Keyboard is a JS hack.** `index.html` listens for `keydown`/`keyup` and
   pushes scancodes into the BDA through a wasm method. That violates the
   spirit of the project: the CSS is supposed to be the program. CSS-DOS
   already emits `:active`-based keyboard rules; calcite-web just isn't
   wiring up the DOM to match.

## Scope

- `web/index.html` — stats readout, on-screen keyboard DOM, remove JS kbd path.
- `web/calcite-worker.js` — stop forwarding keyboard messages from the page.
  (wasm `set_keyboard` stays, debugger still uses it.)
- `CSS-DOS/transpiler/src/template.mjs` — extend `KEYBOARD_KEYS` with arrows,
  Tab, Backspace. Update the HTML footer markup to match.
- `run-web.bat` — selectable BIOS (ASM vs C) at the menu. Toggle with `b`.

Out of scope: debugger, CLI, wasm bindings (no signature changes), any of the
evaluation engine.

## Part 1 — Perf stats in the web UI

### What to show

The CLI reports:

```
Ticks:      N
Cycles:     N
Elapsed:    t
Ticks/sec:  N
Cycles/sec: H (pct% of 4.77 MHz 8086)
Avg cyc/tick: N
Avg us/tick:  t
```

The web status bar is narrow. Show a single dense line:

```
{ticks} ticks | {tps} t/s | {cps} ({pct}% of 4.77 MHz) | {cpt} cyc/tick
```

Plus a **visual speed bar** — a thin horizontal bar under the status bar that
fills proportionally to `cps / REAL_8086_HZ`, clamped to [0, 1]. Bar colour
shifts green→yellow→red across 0–100 %+. Above 100 % the bar stays full red
(CSS-DOS running faster than a real 8086 is the goal and should look
triumphant).

### How to read cycles

`wasm_bindgen` already exposes `get_state_var(name)`. Call
`engine.get_state_var("cycleCount")` at the end of each tick batch.

The constant `REAL_8086_HZ = 4_772_727` matches `bench.rs`.

### Timing

- `startTime` captured at `startRunning()` — already there.
- Batched: `totalTicks` and `totalCycles` both accumulate.
- On each `tick-result`, worker includes `cycleCount` in the message
  (`engine.get_state_var("cycleCount")` is cheap — one slot read).

### UI layout

Under the existing `.status-bar`, add:

```html
<div class="speed-meter">
  <div class="speed-bar" id="speed-bar"></div>
  <div class="speed-tick" style="left:100%"></div>
</div>
```

Styling: 6px tall, beveled sunken border matching `.field-dark`. The `.speed-tick`
is a 1px vertical line at 100 % so the user sees the "real 8086" threshold.

## Part 2 — CSS keyboard

### The mechanism (already exists)

`CSS-DOS/transpiler/src/template.mjs` has:

```js
export function emitKeyboardRules() {
  // Emits 39 rules like:
  //   &:has(key-board button:nth-child(N):active) { --keyboard: VALUE; }
}
```

This is already included in every `.css` output (called from
`emit-css.mjs:316`). The `--keyboard` property drives
addresses 1280/1281 which the BIOS reads via INT 16h / port 0x60.

So the **CSS is already keyboard-aware.** What's missing in calcite-web is
the `<key-board>` DOM so `:has(...)` has something to match.

### Changes to calcite-web

1. Remove:
   - `document.addEventListener('keydown', …)`
   - `document.addEventListener('keyup', …)`
   - The `dosKeyMap` and `keyToCode()` helper
   - `case 'keyboard'` forwarding (both sides)

2. Add a `<key-board>` element inside `.window-body`, under the output, with
   buttons in the **same order** as `KEYBOARD_KEYS` in `template.mjs`. Styled
   to match the Windows 1.0 / DOS look — reuse `.btn` for each key.

3. The button list MUST match the `:nth-child` order the CSS expects. To
   avoid drift, copy the labels from `KEYBOARD_KEYS` verbatim into a JS array
   in the page and render buttons in a loop. If the CSS-DOS list grows in the
   future, the web page must be updated in lockstep — we add a comment
   pointing at `template.mjs` so future edits don't miss it.

### Changes to CSS-DOS

Extend `KEYBOARD_KEYS` with keys the current JS path supports but the CSS
keyboard doesn't:

| Label | Scancode | ASCII (word) |
|---|---|---|
| `←` | 0x4B | 0x00 (extended) — full value 0x4B00 |
| `→` | 0x4D | 0x00 — full value 0x4D00 |
| `↑` | 0x48 | 0x00 — full value 0x4800 |
| `↓` | 0x50 | 0x00 — full value 0x5000 |
| `Tab` | 0x0F | 0x09 |
| `Bksp` | 0x0E | 0x08 |

Extended keys (arrows) have ASCII low byte 0 — the word stored in `--keyboard`
is `(scancode << 8) | ascii`. For arrows that's `0x4B00` / `0x4D00` / `0x4800`
/ `0x5000`, matching the old `dosKeyMap`.

Update `emitHTMLFooter()` so the standalone-HTML path also includes the new
buttons.

### Keyboard layout

For the web page, lay the keyboard out in a grid (same pattern as CSS-DOS's
`emitHTMLFooter` which uses `grid-template-columns: repeat(10, 1fr)`), but
tuned for the new key set: digits row (10), QWERTY row (10), ASDF row (10 — 9
letters + Enter), ZXCV row (8 + space), then a last row with arrows + Tab +
Bksp + Esc. Exact columns chosen in implementation; the spec just says:
readable, matches DOS look, buttons big enough to click.

### Why no Shift / Ctrl / Alt

Out of scope. The current JS path doesn't handle them either — only ASCII
printables + the named keys listed above. Adding modifiers would require BDA
shift-flag bytes and changes to the BIOS-side key handling; not needed for
rogue, bootle, zork, etc.

## Testing

1. **Stats:** load a program, run, confirm `ticks/sec`, `cycles/sec`, and
   `% of 4.77 MHz` update live. Compare against `calcite-bench` for the
   same program — should be in the same order of magnitude (worker overhead
   will pull it down somewhat).
2. **Speed bar:** fills in proportion, turns red past 100 %.
3. **CSS keyboard:** load rogue, click a button, confirm the game reacts.
   Verify arrows work (they go through the new scancodes).
4. **No regressions:** physical keyboard no longer types into the game (that
   is the intended behaviour — if a user wants to play with their real
   keyboard they use the HTML-mode output directly).

## Part 3 — Selectable BIOS in `run-web.bat`

### Problem

`run-web.bat` builds the C BIOS (`bios/build/bios.bin`) every run but then
calls `generate-dos.mjs`, which uses the ASM BIOS (`bios/css-emu-bios.bin`).
The C BIOS work is wasted. Two transpiler entry points exist:

- `generate-dos.mjs` — ASM BIOS.
- `generate-dos-c.mjs` — C BIOS (adds Mode 13h splash).

### Change

Add a session-level BIOS variable shown in the menu header:

```
=== CSS-DOS — run in browser ===
BIOS: C (press 'b' to switch to ASM)

  1. bootle.com
  …
  b. Toggle BIOS
  q. Quit
```

Default to `C`. When user types `b`, toggle and reprint the menu. When user
picks a program:

- `BIOS=C` → `node ..\CSS-DOS\bios\build.mjs` then `generate-dos-c.mjs`.
- `BIOS=ASM` → skip `build.mjs`; run `generate-dos.mjs` only.

### Why

- Each generator has a real use case (splash vs. no-splash; C vs. ASM BIOS).
- Skipping `build.mjs` in ASM mode saves ~5–10 s per run.
- Pre-built CSS files in `output/` are unaffected — no regeneration happens
  for those.

## Files touched

- `web/index.html` — stats readout, speed bar, `<key-board>` DOM, remove kbd JS.
- `web/calcite-worker.js` — drop `keyboard` message case; include `cycleCount`
  in `tick-result`.
- `../CSS-DOS/transpiler/src/template.mjs` — extend `KEYBOARD_KEYS`,
  update `emitHTMLFooter` markup.
- `run-web.bat` — BIOS toggle, dispatch to the right generator.

No changes to: calcite-core, calcite-wasm, calcite-cli, calcite-debugger,
CSS-DOS emit-css.mjs, BIOS source.

## Risks

- **DOM order must match `:nth-child`.** A mismatch silently breaks keys. A
  comment in both places plus a shared list of labels in `index.html`
  mitigates this.
- **Regenerating CSS.** Existing pre-built `.css` files in `output/` won't
  have the new arrow/Tab/Bksp rules until regenerated. Calling out in the
  log.
- **Mobile / touch.** `:active` fires on touch too, so this should Just Work
  on mobile — bonus.
