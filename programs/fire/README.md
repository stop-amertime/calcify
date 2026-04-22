# fire — the Mode 13h quest's first real target

Classic demoscene fire effect in a single .COM file. Sets VGA Mode 13h,
programs the DAC with a black→red→orange→yellow→white palette, runs the
cellular-automaton fire loop, exits on any key.

- **Source:** `fire.asm` — adapted from Hans Wennborg's rewrite of Jare's
  1993 firedemo, https://www.hanshq.net/fire.html. No explicit licence;
  the author's page presents it as freely shared example code.
- **Binary:** `fire.com` (build output, ~570 bytes).

## Why we care about it

Sub-1KB .COM. Exercises exactly the things CSS-DOS's Mode 13h path
doesn't yet handle:

- `OUT 0x3C8` + `OUT 0x3C9` — DAC palette programming (currently ignored
  by the CSS side, so colours will be wrong until the mode13 quest
  Phase 1 lands).
- `INT 10h AH=00h AL=13h` — set Mode 13h (works).
- `INT 1Ah AH=00h` — tick counter timing (works).
- `INT 16h AH=01h/00h` — non-blocking keyboard check (works).
- `INT 21h AH=4Ch` — DOS exit (works, needs DOS — so not the hack preset).

Does **not** poll `0x3DA` (vsync) and does **not** touch the CRTC or
sequencer. It's the cleanest possible first test for the DAC path alone.

See [`../../CSS-DOS/MODE13-QUEST.md`](../../CSS-DOS/MODE13-QUEST.md) for
the overall plan.

## How to run as a CSS-DOS cart

Because a bare `.com` is a complete cart on its own (defaults to the
`dos-corduroy` preset, autorun-infers from the single runnable):

```sh
# From the CSS-DOS repo root:
node builder/build.mjs ../calcite/programs/fire/fire.com -o fire.css

# Then run in Calcite:
../calcite/target/release/calcite-cli.exe -i fire.css
```

No `program.json` needed.

## Building the binary

```sh
nasm -f bin -o fire.com fire.asm
```

NASM path on Windows: `C:\Users\...\AppData\Local\bin\NASM\nasm.exe`
(see root CLAUDE.md).

## CSS-DOS patch

`shr ax, 6` on line 100 of Wennborg's original is a **186** instruction
(opcode `C1 /5`). Our CPU core is 8086-only, so it's been replaced with:

```
mov cl, 6
shr ax, cl
```

Three bytes larger, identical semantics. Everything else in the file is
stock 8086. See the comment at the patch site.

## Known caveats

- **Colours will be wrong until the DAC port decode lands.** The program
  will still run — it'll write palette indices 0–63 to the framebuffer
  and poll the keyboard normally — but the player will render those
  indices against whatever fallback palette it has, not the fire
  gradient the program uploaded. That's the point: we're using fire as
  the test for whether DAC decode works.
- **Banner text (`www.hanshq.net/fire.html`) may not appear.** The
  Corduroy BIOS doesn't implement `INT 10h AH=13h` (write string). The
  fire itself doesn't depend on this, so it's cosmetic.
- **Scratch buffer aliasing.** Fire uses CS + 0x1000 as a scratch
  framebuffer segment. If DOS loads the .COM near the top of
  conventional memory, CS + 0x1000 could alias the real VGA framebuffer
  at 0xA000. Unlikely under our 640 KB EDR-DOS configuration (the .COM
  loads low), but something to watch if the fire ever looks like it's
  double-drawing into itself.
