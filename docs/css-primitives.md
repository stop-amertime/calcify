# CSS primitives — calcite's parse surface

Phase 0.5.1 deliverable. Source: `crates/calcite-core/src/types.rs` (`Expr`,
`CalcOp`, `StyleTest`, `PropertyDef`, `FunctionDef`) and `crates/calcite-core/
src/parser/` end-to-end.

This document enumerates every CSS construct calcite recognises. It bounds
Phase 1's DAG-lowering scope: anything not listed here is either ignored by
the parser or (worse) silently misparsed, and either way doesn't need a
DAG node. Anything listed here MUST round-trip through the conformance
suite agreeing with Chrome (cardinal rule: if calcite disagrees with
Chrome, calcite is wrong).

The list is the union of:
- **Expression-level primitives** that show up inside an `Expr` tree
  (literals, var, calc, if, function calls, concat).
- **Top-level constructs** that produce or consume those expressions
  (`@property`, `@function`, style-rule declarations).

Where the parser's behaviour diverges from CSS spec, the divergence is
called out. Conformance tests must exercise both happy paths and the
edges noted here.

---

## 1. Top-level constructs

### 1.1 `@property` declaration

```css
@property --AX {
  syntax: "<integer>";
  inherits: false;
  initial-value: 0;
}
```

Parsed into `PropertyDef { name, syntax, inherits, initial_value }`.

- **Syntax recognised:** `<integer>`, `<number>`, `<length>`, `*` (universal),
  anything else → `PropertySyntax::Custom(s)`. (`property.rs::parse_syntax`)
- **`inherits`:** strict `true`/`false` ident; anything else is a parse error.
- **`initial-value`:** `Number` token → `CssValue::Integer(value as i64)`.
  Ident or QuotedString → `CssValue::String(s)`. Anything else: read the
  rest of the declaration as a raw string. **Note:** integers are stored
  as i64 internally even when syntax is `<number>` — calcite is integer-
  centric and Chrome's number parsing is not fully simulated. Conformance
  cases stick to integers.
- **Unknown descriptors** (anything besides syntax/inherits/initial-value)
  are logged and skipped.

Edges to test:
- Missing `initial-value` → `None` (Chrome falls back to the syntax's
  guaranteed-invalid value; calcite reads it as 0 via state default).
- Negative initial-value (handled because `parse_initial_value` reads a
  signed `Number`).
- Custom property syntax string (e.g. `"<integer> | none"`) — calcite
  stores the string; treats as opaque.

### 1.2 `@function` definition

```css
@function --readMem(--at <integer>) returns <integer> {
  --slot: calc(var(--at) + 1);
  result: if(style(--at: 0): var(--m0); else: 0);
}
```

Parsed into `FunctionDef { name, parameters, locals, result }`.

- **Name** must start with `--`.
- **Parameters:** comma-separated `--name <type>` pairs inside `()`. Type
  brackets `<integer>`, `<number>`, `<length>` are recognised; anything
  else becomes `PropertySyntax::Custom`. Type missing → `PropertySyntax::Any`.
- **`returns <type>` clause** is parsed and discarded (calcite doesn't
  type-check).
- **Locals:** `--name: <expr>;` declarations — order matters; later locals
  can reference earlier ones via `var()`.
- **Result:** the `result: <expr>;` declaration; if absent, defaults to
  `Expr::Literal(0.0)`.

Edges to test:
- Zero-parameter function (just `--name()` or `--name`).
- Function with no locals.
- Function whose result calls another function (composition).
- Function recursing on itself — calcite's runtime contract is
  intentionally undefined here (there's no recursion limit beyond stack
  blow-up). Conformance cases avoid recursion.

### 1.3 Style rule declarations

```css
:root {
  --AX: calc(var(--AX) + 1);
  --result: if(style(--cond: 1): 42; else: 0);
}
```

Parsed into `Vec<Assignment>` where `Assignment { property, value }`.

- Selectors are **not parsed** — every qualified rule's body is
  scanned for `--name: <expr>;` declarations regardless of the
  selector. (Calcite reads CSS that's already been authored to one
  selector — `:root` or `.cpu` — and treats them as a flat list.)
- Non-custom properties inside the block (e.g. `color: red`) are
  skipped.
- Cascade order in the source determines evaluation order for the
  topological sort; later assignments to the same property win in
  steady state, but during a tick all assignments are evaluated against
  the pre-tick state, then applied in writeback. Conformance cases
  must exercise this (assigning the same property twice).

### 1.4 Other at-rules

`@keyframes`, `@container`, `@media`, `@supports`, `@layer`, etc. are
silently consumed (`AtRulePrelude::Unknown` → drain block). Calcite does
not implement them. Conformance cases SHOULD include a "known unknown"
fixture: a stylesheet that mixes unsupported at-rules with supported
ones, verifying the supported ones still produce identical output to
Chrome.

---

## 2. Expression-level primitives (`Expr`)

The `Expr` enum has 7 variants. Each is a primitive calcite emits and
must lower in Phase 1.

### 2.1 `Expr::Literal(f64)`

```css
--x: 42;
--y: -17;
```

- `Number` token → `Literal(value as f64)`.
- Unary minus is parsed as `Calc(Negate(Literal))` rather than a negative
  literal (see `parse_unary`). Chrome treats both the same; calcite's
  evaluator must collapse them.

Edges: zero, negative, large positive (>2^31), values that don't fit i32
(calcite's downstream eval is i32-centric — out-of-range conformance
cases are deliberately out of scope for Phase 0.5).

### 2.2 `Expr::StringLiteral(String)`

```css
--char: "A";
```

- `QuotedString` token. Only used by display-style functions
  (`--i2char`, `--getInstStr` in CSS-DOS); calcite preserves them
  through `Concat` for cabinet output.

Edges: empty string, string with spaces, string with unicode.

### 2.3 `Expr::Var { name, fallback }`

```css
--a: var(--AX);
--b: var(--undef, 99);
--c: var(--undef, calc(var(--AX) + 1));
```

- Bare `--ident` token in a value position is also parsed as
  `Var { name, fallback: None }` (atom parser short-circuits).
- Inside `var()`, the first arg must be `--`-prefixed. Comma-separated
  fallback is a full nested `Expr`.
- **Undefined + no fallback:** Chrome resolves to the property's
  guaranteed-invalid value (initial-value or 0 for `<integer>`). Calcite
  reads the slot which is 0 if uninitialised. Conformance must test
  this matches.
- Recursive `var()` (`--x: var(--x)`) — Chrome detects the cycle and
  uses the guaranteed-invalid value. Calcite's behaviour: depends on
  the topological sort; cycles either fail to compile or read the
  pre-tick value. **Likely divergence point.** Conformance must pin
  this down — either calcite must match Chrome or the case is
  excluded from the suite with a documented gap.

Edges to test:
- Defined variable.
- Undefined + literal fallback.
- Undefined + calc/var fallback.
- Undefined + no fallback.
- Self-reference (cycle).
- Forward reference (uses pre-tick value).

### 2.4 `Expr::Calc(CalcOp)` — math functions

`CalcOp` has 12 variants. All show up via `parse_function_call` plus the
`+`/`-`/`*`/`/`/unary-minus operators inside `calc(...)`.

| Variant | CSS | Notes |
| --- | --- | --- |
| `Add(a, b)` | `calc(a + b)` | Operands are space-separated (CSS spec) |
| `Sub(a, b)` | `calc(a - b)` | Whitespace required around `-` |
| `Mul(a, b)` | `calc(a * b)` | No whitespace requirement |
| `Div(a, b)` | `calc(a / b)` | **Returns 0 for division by zero** (calcite-specific; Chrome returns NaN/0 depending on context) |
| `Mod(a, b)` | `mod(a, b)` | Standalone function, comma-separated |
| `Min(args)` | `min(a, b, ...)` | ≥1 arg |
| `Max(args)` | `max(a, b, ...)` | ≥1 arg |
| `Clamp(min, val, max)` | `clamp(min, val, max)` | Exactly 3 args |
| `Round(strategy, val, interval)` | `round(strategy, val, interval)` or `round(val, interval)` or `round(val)` | Strategy ident: `nearest`, `up`, `down`, `to-zero`. Default: nearest. Interval default: 1. |
| `Pow(base, exp)` | `pow(base, exp)` | |
| `Sign(val)` | `sign(val)` | Returns -1, 0, +1 |
| `Abs(val)` | `abs(val)` | |
| `Negate(val)` | unary `-val` | Synthetic; never appears as a function call |

Edges per variant (must be in the conformance suite):
- Each: one happy case (positive integers).
- Each: one boundary case (zero operand, negative result, or strategy).
- `Div`: division by zero (calcite's documented divergence — if Chrome
  produces NaN/Infinity and calcite produces 0, the conformance suite
  must either match Chrome or document this as an explicit gap).
- `Mod`: negative dividend (CSS `mod()` matches dividend sign in spec;
  Rust's `%` matches dividend sign — should agree but worth pinning).
- `Round`: each strategy (`nearest`, `up`, `down`, `to-zero`) on a
  half-integer.
- `Clamp`: value below min, value above max, value in range.
- `Min`/`Max`: 1-arg, 2-arg, 3-arg forms.

### 2.5 `Expr::StyleCondition { branches, fallback }`

```css
--reg: if(
  style(--r: 0): var(--AX);
  style(--r: 1): var(--CX);
  style(--r: 2) and style(--mode: 1): var(--DX);
  else: 0
);
```

- `if(...)` with semicolon-separated branches.
- Each branch is `<style-test>: <expr>;`.
- Final `else: <expr>;` (or implicit fallback expression at the end).
- Trailing `;` on each branch is optional.

`StyleTest` has 3 variants:
- `Single { property, value }` — `style(--prop: <expr>)`. The value is
  itself a full `Expr` (so `style(--x: calc(var(--y) + 1))` is valid).
- `And(Vec<StyleTest>)` — `style(...) and style(...) and ...`.
- `Or(Vec<StyleTest>)` — `style(...) or style(...) or ...`.

`and` and `or` cannot be mixed at the same level in the parser's
implementation (it commits to one operator after the first). Mixed
forms must be parenthesised by the author — but parentheses aren't
parsed inside `if()` either, so this is **a documented limitation**:
conformance fixtures avoid `and`/`or` mixing.

Edges to test:
- 0-branch (just `else`) — should be parser-rejected ("expected style()
  or else in if()" if no branches).
- 1-branch + else.
- 8-branch dispatch (the doom8088 shape).
- `and` chain.
- `or` chain.
- `style()` with non-literal value (calc on rhs).

### 2.6 `Expr::FunctionCall { name, args }`

```css
--mem: --readMem(var(--at));
--word: --readWord(calc(var(--at) + 0), calc(var(--at) + 1));
```

- Any function token starting with `--` is parsed as a custom function
  call; args are comma-separated `Expr`s.
- Resolution happens at evaluation time against the `FunctionDef` table
  built by the stylesheet parser.
- A function call to an undefined name is parsed but will fail at
  evaluator construction (or silently return 0 — Phase 0.5 must pin
  this down; conformance must test the behaviour).

Edges to test:
- 0-arg, 1-arg, multi-arg.
- Argument is itself a function call (composition).
- Argument is a `calc(var())` (mixing).

### 2.7 `Expr::Concat(Vec<Expr>)`

```css
--banner: var(--prefix) " " --i2char(var(--DL));
```

- Space-separated `Expr`s in a value list (`parse_expr_list`) collapse
  to a single `Concat` if there are ≥2 parts.
- Each part can be any `Expr` (commonly `StringLiteral`, `Var`, or
  `FunctionCall` returning a string).
- Used by display/banner CSS in the cabinets.

Edges to test:
- 2-part concat (string + string).
- 3-part concat (mixed types).
- `Concat` containing a function call returning a number — calcite's
  current behaviour: numbers stringify naturally. Worth pinning.

---

## 3. Constructs the parser explicitly does NOT support

These come up in Chrome-evaluated CSS but calcite ignores or drops them.
Conformance fixtures avoid them; the suite documents the gap rather
than testing it.

- **Animations / transitions** (`@keyframes`, `transition`, `animation` —
  not relevant to computational CSS).
- **Selectors** beyond the trivial — calcite reads every declared
  custom property regardless of selector, so `:hover`, `::before`,
  `[attr]`, etc. are functionally invisible.
- **CSS units** (`px`, `em`, `%`, etc.) — `parse_atom` only handles
  `Number` tokens; a `Dimension` token (e.g. `1px`) is "unexpected
  token in expression". Computational CSS is unitless by convention.
- **Color values** (`rgb(...)`, `#hex`, named colors) — not parsed.
- **`url()`, `attr()`, `env()`, `counter()`** — not parsed.
- **CSS nesting** (`& { ... }`) — qualified rules are flat-only.
- **`@supports` feature queries** (the at-rule body is dropped).

---

## 4. Phase 0.5 conformance suite — what to cover

Targeting ~30-50 fixture files, one primitive (or one edge per
primitive) per fixture. Suggested split:

| Group | Cases | Notes |
| --- | --- | --- |
| Literals & negation | 3 | positive, negative, zero |
| `var()` resolution | 5 | defined, fallback hit, fallback miss, undefined+no-fallback, self-cycle |
| `@property` initial-value | 4 | integer / negative / missing / non-numeric ident |
| `CalcOp` happy path | 12 | one per variant |
| `CalcOp` edges | 6 | div-by-zero, mod-negative, each round strategy, clamp boundaries |
| `if(style())` shapes | 6 | 1-branch, multi-branch dispatch (≥4 → recogniser fires), and-chain, or-chain, non-literal rhs, only-else |
| `@function` shapes | 5 | identity, single-arg, multi-arg, with locals, composed |
| `Concat` | 3 | string+string, string+var, function-call+string |
| Cascade order | 2 | same-property double-write within tick, dependent assignments |
| Ignored constructs | 2 | unknown at-rule mixed in, unsupported selector mixed in |

Total: 48 cases. Each fixture is a `<name>.css` plus a `<name>.expect.json`
listing the custom-property values to read after one tick (or after
`getComputedStyle` for Chrome).

---

## 5. Open questions for the conformance writer

These need to be answered before fixtures are finalised; mark each
either "matches Chrome" or "explicit gap" in the final suite. Status
updates as fixtures land.

1. **`var(--x, var(--x))` self-reference fallback** — does Chrome's
   guaranteed-invalid resolution fire? Does calcite's slot read the
   pre-tick value or 0?
   ✅ **Chrome falls back to initial-value**, confirmed by
   `var_self_cycle` (returns 5). Calcite v1 also passes — the cycle
   doesn't reach the eval path because the topological sort silently
   drops the self-edge (`--x` keeps its pre-tick value, which equals
   initial-value on tick 1).
2. **`calc(1 / 0)`** — Chrome behaviour vs calcite's documented "0".
   📎 Pending — fixture not yet written (calc edges group).
3. **Unknown function call** (`--undefinedFunc(1)`) — Chrome resolves to
   guaranteed-invalid; calcite's behaviour is undocumented.
   📎 Pending — fixture not yet written (function group).
4. **`@function` recursion** — explicit gap or runtime contract?
   📎 Pending — fixture not yet written (function group).
5. **Mixed `and`/`or` inside `if(style())`** — explicit gap.
   ✋ Confirmed parser limitation (see § 2.5). Won't fixture-test —
   the suite avoids mixed forms; if(style()) fixtures use single-
   operator chains only.
6. **Number tokens out of i32 range** — explicit gap.
   ✋ Phase 0.5 declares this out-of-scope (calcite's eval is i32-
   centric); fixtures stick to in-range values.
7. **Cascade order with 3+ writes** — does Chrome use last-wins? Does
   calcite topo-sort or last-wins?
   📎 Pending — fixture not yet written (cascade group).

Additional findings landing during fixture authoring:

- **Var on undeclared LHS, no fallback** (`--dst: var(--undef);` where
  `--dst` has initial-value 7): Chrome preserves --dst's initial-value
  (declaration becomes invalid at computed-value time). Calcite v1
  writes 0 — the writeback fires regardless. Marked
  `xfail_v1` on `var_undefined_no_fallback`. Phase 1+ should implement
  invalidity propagation through the DAG so this fixture passes
  without an xfail marker.

- **Var fallback when --src is defined**: Chrome reads --src, ignores
  the fallback. Calcite v1 matches. ✅
- **Var fallback when --undef is undeclared**: Chrome reads the
  fallback. Calcite v1 matches. ✅
- **Initial-value with no assignment**: Chrome surfaces the
  initial-value via getComputedStyle. Calcite v1 matches. ✅
- **All 12 CalcOp variants on simple integer happy paths**: calcite v1
  matches Chrome bit-for-bit. ✅

Phase 0.5.6 reconciles remaining items and updates this doc.
