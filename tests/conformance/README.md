# Calcite primitive conformance suite

Phase 0.5 deliverable. Two runners read the same fixtures and must agree:

1. **Chrome (ground truth)** — `runner.html` loads each fixture as a
   stylesheet, exposes `window.readProps(names)` which returns the
   computed-style string value for each property. Driven via Playwright
   (MCP for ad-hoc, `run-primitives.mjs` for CI — see Phase 0.5.4).
2. **Calcite v1** — `crates/calcite-core/tests/primitive_conformance.rs`
   parses each fixture, ticks once, reads the same properties out of
   `State`, formats them to strings the same way Chrome does. (Phase
   0.5.5.)

If they disagree, calcite is wrong (cardinal rule).

## Fixture layout

```
primitives/<name>.css           — stylesheet under test
primitives/<name>.expect.json   — what to read + ground-truth value
```

The `.expect.json` schema:

```json
{
  "description": "human-readable summary",
  "ticks": 1,
  "read": [
    { "property": "--x", "type": "integer", "expected_str": "42" }
  ]
}
```

Fields:
- `description` — one-line summary, shown in test output on failure.
- `ticks` — how many calcite-side ticks before reading state. Default 1.
  (Chrome doesn't tick — `getComputedStyle` resolves once. For purely-
  declarative fixtures the values are stable so 1 tick matches Chrome.)
- `read[].property` — full custom-property name (with `--` prefix).
- `read[].type` — `integer` | `string`. Drives how calcite formats its
  state slot for comparison.
- `read[].expected_str` — Chrome's `getComputedStyle().getPropertyValue()`
  output, trimmed of leading/trailing whitespace. Canonical form.

The expected_str field is the source of truth. Numeric `expected` fields
exist in some fixtures as a readability aid but aren't compared.

## Running

### Quick path (manual): the local HTTP server + the Playwright MCP

```sh
python -m http.server 8731 --bind 127.0.0.1 \
  --directory <abs-path>/tests/conformance
# in another shell, drive Playwright via the MCP:
#   browser_navigate('http://127.0.0.1:8731/runner.html?css=primitives/<name>.css')
#   browser_evaluate('() => window.readProps([...])')
```

### CI path: `run-primitives.mjs`

```sh
cd tests/conformance
npm install                       # one-time: install playwright
npx playwright install chromium   # one-time: download Chromium binary
node run-primitives.mjs           # CHECK mode — exit 1 on first mismatch
node run-primitives.mjs --capture # write Chrome's values into expected_str
node run-primitives.mjs --filter=calc  # only run fixtures with "calc" in name
```

`--capture` is the authoring aid: write a new fixture's `.css`, leave
`expected_str` blank or guess, then run capture to fill it in from
Chrome. Always review the resulting JSON diffs before committing —
capture mode trusts Chrome and overwrites silently.

Default mode loads each fixture, reads the requested properties via
`getComputedStyle`, diffs against `expected_str`, prints a PASS/FAIL
summary and exits non-zero on first mismatch.

### Calcite path: cargo test (Phase 0.5.5)

```sh
cargo test -p calcite-core --test primitive_conformance
```

## Adding a fixture

1. Write `primitives/<name>.css` — keep it minimal (one primitive per
   case where possible).
2. Run it through Chrome via the manual path above to capture each
   property's `expected_str` value.
3. Write `primitives/<name>.expect.json` with those values.
4. Run the calcite cargo test — it should agree. If it doesn't,
   calcite is wrong; fix calcite or document the gap in
   `docs/css-primitives.md` § 5 (open questions).
