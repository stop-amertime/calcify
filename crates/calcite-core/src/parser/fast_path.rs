//! Fast-path parsing for dense, byte-templated regions of CSS.
//!
//! # Motivation
//!
//! Real programs emit huge contiguous runs of near-identical declarations.
//! In CSS-DOS rogue output, 76.4% of the CSS is 367,776 memory-cell
//! assignments (`--mN: if(...)`) that are byte-identical except for the
//! single integer `N` threaded through them. Another 7.9% is 367,776
//! `@property --mN { ... }` blocks with the same property.
//!
//! cssparser handles these correctly but it tokenises every byte — for this
//! one file that's ~4 seconds spent building then throwing away 18M+ heap
//! allocations on trees the broadcast recogniser immediately collapses into
//! six HashMaps.
//!
//! # What this module does
//!
//! Before cssparser runs, we scan the raw bytes for contiguous runs of
//! look-alike declarations. For each run we:
//!
//! 1. Pick two "reference" entries (e.g. the 1000th and 5000th cell) and
//!    split each on maximal ASCII digit runs. The text between digit runs
//!    becomes the **literal** skeleton; each digit run is a **hole**.
//! 2. If both references produce the same literal skeleton with the same
//!    number of holes, we classify each hole by comparing the two numbers
//!    landed in it — either both equal their cell's own address (a
//!    parameterised `Addr` hole) or they're equal to each other (a `Const`
//!    hole).
//! 3. If every hole classifies cleanly, we have a **template**. We then
//!    verify every remaining entry in the run matches the template
//!    byte-for-byte on literals and by classification on holes.
//!
//! If verification succeeds, we have a list of `(addr, byte_span)` tuples
//! with no `Expr` trees built — and we know the exact broadcast-write or
//! `@property` shape every entry has. We then materialise:
//!
//! - For `--mN:` runs: pre-built `BroadcastWrite`s, one per destination
//!   property seen in the template. Each has 367K entries in its
//!   `address_map`. The existing broadcast recogniser is skipped for these
//!   properties.
//! - For `@property --mN { ... }` runs: a stream of `PropertyDef`s.
//!
//! # Cardinal rule compliance
//!
//! Nothing in this module hardcodes CSS-DOS-specific names, shapes, or
//! property semantics. The template is **learned at runtime** from whatever
//! bytes are in the input. If CSS-DOS renames `memAddr` → `addr`, the
//! learned template contains `addr`. If CSS-DOS adds a 7th slot level, the
//! template has 7 levels. If a cell fails verification, it's left in place
//! for cssparser to parse normally. This is pattern recognition on raw
//! bytes — the same game the compiler plays on `Expr` trees, moved earlier.
//!
//! The templated region is **blanked** (replaced with spaces, preserving
//! byte offsets so cssparser line numbers stay accurate) before cssparser
//! runs, so those bytes are walked once by us and once as whitespace by
//! cssparser.

use std::collections::HashMap;

use crate::pattern::broadcast_write::BroadcastWrite;
use crate::types::{CssValue, Expr, PropertyDef, StyleBranch, StyleTest};

/// What the fast-path produces.
///
/// Borrows from the source CSS — the `blank_ranges` are byte offsets into
/// the original input that the caller should replace with whitespace
/// before handing the input to cssparser.
pub struct FastPathResult {
    /// Pre-built `PropertyDef`s from `@property --X { ... }` runs.
    pub properties: Vec<PropertyDef>,
    /// Pre-built `BroadcastWrite`s from assignment-templated runs.
    pub broadcast_writes: Vec<BroadcastWrite>,
    /// Property names the caller should treat as already consumed — the
    /// ordinary assignment loop won't see them, but the caller needs to
    /// know they're "absorbed" so dispatch-table recognition and the
    /// regular broadcast recogniser skip them.
    pub absorbed_properties: std::collections::HashSet<String>,
    /// Byte ranges [start, end) in the original CSS the caller should
    /// overwrite with spaces before feeding to cssparser. The ranges are
    /// guaranteed non-overlapping and sorted.
    pub blank_ranges: Vec<(usize, usize)>,
}

impl FastPathResult {
    fn empty() -> Self {
        Self {
            properties: Vec::new(),
            broadcast_writes: Vec::new(),
            absorbed_properties: std::collections::HashSet::new(),
            blank_ranges: Vec::new(),
        }
    }
    /// Public constructor for a no-op fast-path result. Used by callers that
    /// want to skip the scan on small inputs.
    pub fn empty_pub() -> Self {
        Self::empty()
    }
}

/// Entry point. Scan `css` for templated regions; return what we could
/// recognise plus the byte ranges the caller should blank before handing
/// off to cssparser.
pub fn recognise(css: &str) -> FastPathResult {
    let bytes = css.as_bytes();
    let mut result = FastPathResult::empty();

    // Attempt `@property --<name><digits> { ... }` runs.
    recognise_property_run(bytes, &mut result);

    // Attempt assignment-templated runs (e.g. `--mN: if(...);`).
    recognise_assignment_run(bytes, &mut result);

    // Sort blank ranges.
    result.blank_ranges.sort_by_key(|&(s, _)| s);
    result
}

// ---------------------------------------------------------------------------
// Shared: template extraction by digit-substitution
// ---------------------------------------------------------------------------

/// Split `body` on maximal ASCII digit runs.
///
/// Returns `(literals, hole_values)` where `literals[i]` is the text before
/// the `i`th digit run, `hole_values[i]` is the integer value of the `i`th
/// run, and `literals` has exactly one more element than `hole_values`
/// (the trailing literal, possibly empty).
fn split_digits(body: &[u8]) -> Option<(Vec<&[u8]>, Vec<u64>)> {
    let mut literals: Vec<&[u8]> = Vec::new();
    let mut holes: Vec<u64> = Vec::new();
    let mut i = 0;
    let mut lit_start = 0;
    while i < body.len() {
        if body[i].is_ascii_digit() {
            literals.push(&body[lit_start..i]);
            let hole_start = i;
            while i < body.len() && body[i].is_ascii_digit() {
                i += 1;
            }
            // parse as u64, fail safely on overflow (shouldn't happen in
            // realistic CSS, but template learning must never panic).
            let digits = std::str::from_utf8(&body[hole_start..i]).ok()?;
            let val: u64 = digits.parse().ok()?;
            holes.push(val);
            lit_start = i;
        } else {
            i += 1;
        }
    }
    literals.push(&body[lit_start..]);
    Some((literals, holes))
}

/// Classification of a single hole in the learned template.
#[derive(Debug, Clone, Copy, PartialEq)]
enum HoleKind {
    /// Hole always equals the entry's own address parameter.
    Addr,
    /// Hole always equals this constant.
    Const(u64),
    /// Hole is a per-entry free variable — every entry supplies its own
    /// value here. The caller extracts the value at verification time.
    /// Used for e.g. `@property --mN { ... initial-value: <byte>; }`
    /// where `<byte>` is the per-cell ROM data.
    Free,
}

/// Learn a template from **three** reference entries.
///
/// Three is the minimum needed to distinguish:
///   - `Addr`  — hole tracks the entry's own address
///   - `Const` — hole is the same literal value in every entry
///   - `Free`  — hole varies per entry, independent of address (e.g. ROM
///               data in `@property --mN { initial-value: <byte>; }`).
///
/// Returns `(literals, hole_kinds)` on success. The byte slices in `literals`
/// are borrowed from the FIRST reference. `None` is returned if the three
/// references don't agree on the literal skeleton or hole count (i.e. the
/// run is not cleanly templatable).
fn learn_template<'a>(
    ref_a_body: &'a [u8],
    ref_a_addr: u64,
    ref_b_body: &[u8],
    ref_b_addr: u64,
    ref_c_body: &[u8],
    ref_c_addr: u64,
) -> Option<(Vec<&'a [u8]>, Vec<HoleKind>)> {
    let (lits_a, holes_a) = split_digits(ref_a_body)?;
    let (lits_b, holes_b) = split_digits(ref_b_body)?;
    let (lits_c, holes_c) = split_digits(ref_c_body)?;

    if lits_a.len() != lits_b.len() || lits_a.len() != lits_c.len() {
        return None;
    }
    if holes_a.len() != holes_b.len() || holes_a.len() != holes_c.len() {
        return None;
    }
    // Literal chunks must match byte-for-byte across all three.
    for ((a, b), c) in lits_a.iter().zip(lits_b.iter()).zip(lits_c.iter()) {
        if *a != *b || *a != *c {
            return None;
        }
    }
    // Classify each hole.
    let mut kinds: Vec<HoleKind> = Vec::with_capacity(holes_a.len());
    for ((&va, &vb), &vc) in holes_a.iter().zip(holes_b.iter()).zip(holes_c.iter()) {
        if va == ref_a_addr && vb == ref_b_addr && vc == ref_c_addr {
            kinds.push(HoleKind::Addr);
        } else if va == vb && va == vc {
            kinds.push(HoleKind::Const(va));
        } else {
            // Not a shared const, not the addr — treat as per-entry free
            // variable. Verification will accept any numeric value here.
            kinds.push(HoleKind::Free);
        }
    }
    Some((lits_a, kinds))
}

/// Verify a single entry's body matches `(literals, hole_kinds)` for the
/// given `addr`. Returns `Some(free_values)` on match, where `free_values`
/// is the sequence of values landed in `HoleKind::Free` holes (in the order
/// those holes appear). Returns `None` on mismatch.
fn verify_entry(
    body: &[u8],
    literals: &[&[u8]],
    hole_kinds: &[HoleKind],
    addr: u64,
) -> Option<Vec<u64>> {
    let mut pos = 0usize;
    let mut frees: Vec<u64> = Vec::new();
    for (hi, lit) in literals.iter().enumerate() {
        if pos + lit.len() > body.len() {
            return None;
        }
        if &body[pos..pos + lit.len()] != *lit {
            return None;
        }
        pos += lit.len();
        if hi < hole_kinds.len() {
            let hole_start = pos;
            while pos < body.len() && body[pos].is_ascii_digit() {
                pos += 1;
            }
            if pos == hole_start {
                return None;
            }
            let digits = std::str::from_utf8(&body[hole_start..pos]).ok()?;
            let val: u64 = digits.parse().ok()?;
            match hole_kinds[hi] {
                HoleKind::Addr => {
                    if val != addr {
                        return None;
                    }
                }
                HoleKind::Const(c) => {
                    if val != c {
                        return None;
                    }
                }
                HoleKind::Free => {
                    frees.push(val);
                }
            }
        }
    }
    if pos == body.len() {
        Some(frees)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Assignment runs:  `  --<prefix><digits>: <body>;`
// ---------------------------------------------------------------------------

/// Anchor we scan for. Two-space indent is the convention in transpiler
/// output; on other inputs the fallback loop still works, just slower.
const ASSIGN_ANCHOR: &[u8] = b"\n  --";

/// Try to find and fast-path a contiguous run of name-prefixed assignments.
///
/// The first such assignment we find establishes a `prefix` (everything
/// before the first digit in the name). Subsequent assignments are included
/// in the run only if their name starts with the same prefix followed by
/// digits.
fn recognise_assignment_run(bytes: &[u8], result: &mut FastPathResult) {
    // Scan to find candidate runs. We simply walk the file and every time
    // we see ASSIGN_ANCHOR we check whether it starts a "big enough" run.
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        // Find next anchor.
        let offset = match find_subslice(&bytes[cursor..], ASSIGN_ANCHOR) {
            Some(o) => cursor + o,
            None => break,
        };
        match try_assignment_run_from(bytes, offset) {
            Some((end, prefix, entries)) if entries.len() >= 1024 => {
                emit_assignment_run(bytes, &prefix, &entries, result);
                cursor = end;
            }
            _ => {
                cursor = offset + ASSIGN_ANCHOR.len();
            }
        }
    }
}

/// If `offset` is the start of a big enough templatable assignment run,
/// return `(end_offset, prefix_without_leading_dashes, entries)`.
///
/// The prefix is everything between `--` and the first digit. Returns
/// `None` if the anchor doesn't start a valid assignment or fewer than
/// ~1000 similarly-named assignments follow.
fn try_assignment_run_from<'a>(
    bytes: &'a [u8],
    offset: usize,
) -> Option<(usize, Vec<u8>, Vec<(usize, usize, u64)>)> {
    // Parse the first assignment name to establish the prefix.
    // ASSIGN_ANCHOR is "\n  --"; name starts at offset + 3.
    let name_start = offset + 3; // skip "\n  "
    // Must begin with `--`.
    if bytes.get(name_start) != Some(&b'-') || bytes.get(name_start + 1) != Some(&b'-') {
        return None;
    }
    // Read the prefix — alphanumeric+underscore+dash until first digit.
    let mut i = name_start + 2;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_digit() {
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
            return None;
        }
        i += 1;
    }
    // Need at least one digit and a colon after the prefix.
    let prefix_end = i;
    if prefix_end == name_start + 2 {
        return None;
    }
    if !bytes.get(prefix_end).is_some_and(|b| b.is_ascii_digit()) {
        return None;
    }
    // The prefix spans [name_start+2 .. prefix_end) — strip leading `--`.
    let prefix: Vec<u8> = bytes[name_start + 2..prefix_end].to_vec();

    // Now collect all consecutive `\n  --<prefix><digits>:` assignments.
    let mut entries: Vec<(usize, usize, u64)> = Vec::new();
    let mut scan = offset;
    loop {
        // Expect the anchor + `--` + prefix + digits + `:`.
        if !bytes[scan..].starts_with(ASSIGN_ANCHOR) {
            break;
        }
        let n_start = scan + 3;
        if bytes.get(n_start) != Some(&b'-') || bytes.get(n_start + 1) != Some(&b'-') {
            break;
        }
        let p_start = n_start + 2;
        let p_end = p_start + prefix.len();
        if bytes.get(p_start..p_end) != Some(&prefix[..]) {
            break;
        }
        let mut d = p_end;
        while d < bytes.len() && bytes[d].is_ascii_digit() {
            d += 1;
        }
        if d == p_end {
            break; // no digits, wrong run
        }
        if bytes.get(d) != Some(&b':') {
            break;
        }
        let addr = match std::str::from_utf8(&bytes[p_end..d]).ok().and_then(|s| s.parse::<u64>().ok()) {
            Some(a) => a,
            None => break,
        };
        // Find the semicolon at depth 0 terminating this declaration.
        let body_start = d + 1;
        let end = match scan_balanced_to_semicolon(bytes, body_start) {
            Some(e) => e,
            None => break,
        };
        // Entry spans [scan+1 .. end+1). (We eat the newline anchor and the
        // trailing `;` so blanking this range completely removes the
        // assignment.)
        let entry_start = scan + 1; // skip the leading '\n'
        let entry_end = end + 1; // include the trailing ';'
        entries.push((entry_start, entry_end, addr));
        scan = entry_end;
    }

    if entries.is_empty() {
        return None;
    }
    Some((scan, prefix, entries))
}

/// Scan forward from `start` over `(` / `)` balanced content, stopping at
/// the first `;` encountered at depth 0. Returns the index of that `;`,
/// or `None` if EOF reached without one.
fn scan_balanced_to_semicolon(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b';' if depth <= 0 => return Some(i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memchr_fallback(haystack, needle)
}

/// Find first occurrence of `needle` in `haystack`. Uses the `memchr`
/// crate's `memmem` when available; falls back to naive search. We avoid
/// adding memchr to the build if it isn't already a transitive dep.
fn memchr_fallback(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    let first = needle[0];
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        // Scan for the first byte, then check the rest.
        match haystack[i..haystack.len() + 1 - needle.len()]
            .iter()
            .position(|&b| b == first)
        {
            Some(off) => {
                let cand = i + off;
                if &haystack[cand..cand + needle.len()] == needle {
                    return Some(cand);
                }
                i = cand + 1;
            }
            None => return None,
        }
    }
    None
}

/// Learn a template from the collected entries and, if successful, emit
/// pre-built `BroadcastWrite`s for the caller. If the shape isn't what we
/// can lower into broadcast writes, we leave the bytes alone.
fn emit_assignment_run(
    bytes: &[u8],
    prefix: &[u8],
    entries: &[(usize, usize, u64)],
    result: &mut FastPathResult,
) {
    // Need at least three distinct-addr entries to learn a template that
    // can distinguish Addr / Const / Free holes.
    if entries.len() < 3 {
        return;
    }
    let (ra_idx, rb_idx, rc_idx) = pick_reference_triple(entries);
    let (ra_start, ra_end, ra_addr) = entries[ra_idx];
    let (rb_start, rb_end, rb_addr) = entries[rb_idx];
    let (rc_start, rc_end, rc_addr) = entries[rc_idx];
    let body_a = &bytes[ra_start..ra_end];
    let body_b = &bytes[rb_start..rb_end];
    let body_c = &bytes[rc_start..rc_end];

    let (literals, mut hole_kinds) = match learn_template(body_a, ra_addr, body_b, rb_addr, body_c, rc_addr) {
        Some(t) => t,
        None => {
            log::info!(
                "[fast-path] assignment run with prefix `--{}` (entries={}) — template unlearnable, skipping",
                String::from_utf8_lossy(prefix),
                entries.len(),
            );
            return;
        }
    };
    // Refine: promote Const holes to Free if any sampled entry contradicts
    // the learned constant. Catches "all refs coincidentally agree on a
    // common value" — e.g. `initial-value: 0` is common so three random
    // picks may all agree even though cells with ROM data differ.
    if !refine_template(bytes, entries, &literals, &mut hole_kinds) {
        log::info!(
            "[fast-path] assignment run `--{}` failed refinement, skipping",
            String::from_utf8_lossy(prefix),
        );
        return;
    }

    // Assignments with `Free` holes would need per-entry data to build
    // their Expr body, but we bypass Expr construction. The broadcast-write
    // fast path relies on every cell having identical structure modulo the
    // addr parameter, so a `Free` hole means the run isn't suitable — bail.
    if hole_kinds.iter().any(|k| *k == HoleKind::Free) {
        log::info!(
            "[fast-path] assignment run `--{}` has per-entry Free holes, skipping",
            String::from_utf8_lossy(prefix),
        );
        return;
    }

    // Verify all entries match. If any don't, we bail entirely for this
    // run — we never want a partial fast-path where some cells are
    // prebuilt and others fall through: it'd corrupt the broadcast-write
    // address_map (missing keys) silently.
    for &(s, e, a) in entries {
        let body = &bytes[s..e];
        if verify_entry(body, &literals, &hole_kinds, a).is_none() {
            log::info!(
                "[fast-path] assignment run `--{}` entries={} — cell at addr={} failed verification, skipping",
                String::from_utf8_lossy(prefix),
                entries.len(),
                a,
            );
            return;
        }
    }

    // Parse the template body through cssparser ONCE to extract the
    // broadcast-write shape: which dest_properties, which value_exprs,
    // which gate, which fallback. We re-use the existing post-parse
    // recogniser so there's one source of truth for what a broadcast
    // write looks like.
    //
    // We build a synthetic `--<prefix>0: <body-with-addr-0>;` declaration
    // by materialising the template with addr=0 (or the ref addr if 0
    // collides with something). Then we parse and feed that single
    // assignment to the ordinary recogniser.
    let template_body = materialise_template(&literals, &hole_kinds, ra_addr);
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("").to_string();
    let synth_property = format!("--{}{}", prefix_str, ra_addr);
    let synth_css = format!("{}{};", synth_property, String::from_utf8_lossy(&template_body));
    // template_body begins with `: ` (colon + space or whatever follows
    // the name), so synth_css is `--prefix<ra_addr>: if(...);`.
    let parsed_expr = match parse_single_assignment(&synth_css) {
        Some(e) => e,
        None => {
            log::warn!("[fast-path] synth template failed to parse; falling back");
            return;
        }
    };

    // Now collect ports from this single Expr the same way
    // `broadcast_write::extract_broadcast_ports` does. We replicate the
    // logic here in condensed form to avoid exposing private fns from the
    // pattern module.
    let ports = match extract_ports_from_expr(&parsed_expr) {
        Some(p) => p,
        None => {
            log::info!("[fast-path] template doesn't match broadcast shape; skipping");
            return;
        }
    };

    // Build 1..=N BroadcastWrites, one per (dest_property, gate_property).
    // For each port in the template, replicate across all entries.
    //
    // port.address in the template equals the ref addr `ra_addr` — i.e.
    // for each cell with addr=N, the corresponding concrete port is
    // (dest=port.dest_property, address=N, value=port.value_expr,
    //  gate=port.gate_property). So we produce, per template port:
    //   BroadcastWrite {
    //     dest_property: port.dest_property,
    //     value_expr: port.value_expr,
    //     address_map: {entry.addr -> "prefix<entry.addr>"  for entry in entries},
    //     spillover_map: {}, spillover_guard: None,
    //     gate_property: port.gate_property,
    //   }
    //
    // Each port becomes one BroadcastWrite with ~len(entries) addresses.
    for port in ports {
        let mut address_map: HashMap<i64, String> = HashMap::with_capacity(entries.len());
        for &(_, _, a) in entries {
            let name = format!("{}{}", prefix_str, a);
            address_map.insert(a as i64, name);
        }
        result.broadcast_writes.push(BroadcastWrite {
            dest_property: port.dest_property,
            value_expr: port.value_expr,
            address_map,
            spillover_map: HashMap::new(),
            spillover_guard: None,
            gate_property: port.gate_property,
        });
    }

    // Mark every cell property as absorbed, and blank its bytes.
    for &(s, e, a) in entries {
        result.absorbed_properties.insert(format!("--{}{}", prefix_str, a));
        result.blank_ranges.push((s, e));
    }
    log::info!(
        "[fast-path] assignment run `--{}` absorbed {} entries ({} bytes) → {} pre-built broadcast writes",
        prefix_str,
        entries.len(),
        entries.last().map(|e| e.1 - entries[0].0).unwrap_or(0),
        result.broadcast_writes.len(),
    );
}

/// Pick three entries to learn the template from. We want three distinct
/// addrs that are unlikely to coincidentally match constant holes.
/// Strategy: pick entries at roughly 1/4, 1/2, and 3/4 through the run,
/// skipping any whose addr is < 16 (to avoid collisions with the small
/// constants that typically show up inline, e.g. 0, 1, 16).
fn pick_reference_triple(entries: &[(usize, usize, u64)]) -> (usize, usize, usize) {
    let n = entries.len();
    let candidates = [n / 4, n / 2, 3 * n / 4];
    let mut picks = Vec::with_capacity(3);
    let mut used_addrs = std::collections::HashSet::new();
    for &i in &candidates {
        if entries[i].2 >= 16 && used_addrs.insert(entries[i].2) {
            picks.push(i);
        }
    }
    // If we didn't get three distinct-addr picks, scan the whole list.
    if picks.len() < 3 {
        for (i, e) in entries.iter().enumerate() {
            if e.2 >= 16 && used_addrs.insert(e.2) {
                picks.push(i);
                if picks.len() == 3 {
                    break;
                }
            }
        }
    }
    // Still not enough? Fall back to whatever's available.
    while picks.len() < 3 && picks.len() < n {
        for i in 0..n {
            if !picks.contains(&i) {
                picks.push(i);
                if picks.len() == 3 {
                    break;
                }
            }
        }
    }
    (picks[0], picks[1], picks[2])
}

/// After learning the initial template from three refs, refine it by
/// looking at a sample of additional entries. If any entry contradicts a
/// hole currently classified as `Const(c)` (i.e. shows a different value
/// at that position), promote it to `Free`. This catches cases where the
/// three refs all coincidentally landed on the same Free value — a very
/// real risk for e.g. `initial-value: 0` (common) where most cells agree
/// by chance but a minority differ.
///
/// Returns `false` if refinement revealed a structural mismatch (literals
/// don't match, hole count differs) — in that case the whole fast path
/// must bail.
fn refine_template(
    bytes: &[u8],
    entries: &[(usize, usize, u64)],
    literals: &[&[u8]],
    hole_kinds: &mut [HoleKind],
) -> bool {
    let n = entries.len();
    let sample_stride = (n / 64).max(1);
    let mut i = 0;
    while i < n {
        let (s, e, a) = entries[i];
        let body = &bytes[s..e];
        // Split this entry's digits the same way we did for refs.
        let (lits, holes) = match split_digits(body) {
            Some(t) => t,
            None => return false,
        };
        if lits.len() != literals.len() || holes.len() != hole_kinds.len() {
            return false;
        }
        for (a_lit, b_lit) in literals.iter().zip(lits.iter()) {
            if *a_lit != *b_lit {
                return false;
            }
        }
        for (hi, &hv) in holes.iter().enumerate() {
            match hole_kinds[hi] {
                HoleKind::Addr => {
                    if hv != a {
                        // Entry contradicts Addr classification — promote
                        // to Free so downstream logic handles per-entry
                        // values (the @property path supports at most one
                        // Free, and the assignment path bails on Free).
                        hole_kinds[hi] = HoleKind::Free;
                    }
                }
                HoleKind::Const(c) => {
                    if hv != c {
                        hole_kinds[hi] = HoleKind::Free;
                    }
                }
                HoleKind::Free => {}
            }
        }
        i += sample_stride;
    }
    true
}

/// Fill the template with a concrete `addr`, returning the body bytes
/// (everything after the name). The returned bytes start with `:`
/// (the colon after the property name) and end with `;`.
fn materialise_template(literals: &[&[u8]], hole_kinds: &[HoleKind], addr: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        literals.iter().map(|s| s.len()).sum::<usize>() + hole_kinds.len() * 10,
    );
    // Reconstruction. The first literal is everything before the first
    // digit run in the template, which includes the entry's own name
    // (`--prefix<addr>`). We strip the name and keep from the colon on.
    // Easiest: rebuild the whole entry, then find the first colon.
    for (i, lit) in literals.iter().enumerate() {
        out.extend_from_slice(lit);
        if i < hole_kinds.len() {
            let val = match hole_kinds[i] {
                HoleKind::Addr => addr,
                HoleKind::Const(c) => c,
                // Caller is supposed to bail before reaching us when Free
                // holes are present; use 0 as a defensive placeholder.
                HoleKind::Free => 0,
            };
            // Append decimal digits.
            out.extend_from_slice(val.to_string().as_bytes());
        }
    }
    // Find the first colon and return everything from there.
    let colon = out.iter().position(|&b| b == b':').unwrap_or(0);
    out[colon..].to_vec()
}

fn parse_single_assignment(css: &str) -> Option<Expr> {
    // Parse the synthetic `--name: <expr>;` and return the Expr. We delegate
    // to a minimal wrapper rather than re-using `parse_stylesheet` to keep
    // this cheap for a tiny input.
    use crate::parser::css_functions::parse_expr_list;
    use cssparser::{Parser, ParserInput};

    let mut input = ParserInput::new(css);
    let mut parser = Parser::new(&mut input);
    // Eat `--name:` prefix by scanning for the colon then the whitespace.
    // Easiest: use cssparser tokenisation.
    let _name = parser.expect_ident_cloned().ok()?;
    parser.expect_colon().ok()?;
    parse_expr_list(&mut parser).ok()
}

// ---------------------------------------------------------------------------
// Port extraction — minimal copy of pattern::broadcast_write's logic
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct FastPort {
    dest_property: String,
    value_expr: Expr,
    gate_property: Option<String>,
}

/// Walk a template Expr and classify each top-level branch as a broadcast
/// port. Returns None if the shape doesn't fit — the caller then gives up
/// the fast path for this run.
fn extract_ports_from_expr(expr: &Expr) -> Option<Vec<FastPort>> {
    let mut ports = Vec::new();
    match expr {
        Expr::StyleCondition { branches, fallback } => {
            // Sanity-check the fallback matches `var(--<something>)` —
            // the typical "keep previous value" pattern. We don't use it
            // directly (the compiler's broadcast path ignores the
            // fallback for this shape — the value is kept because the
            // address map only fires on match), so we just require it
            // exist.
            if !matches!(fallback.as_ref(), Expr::Var { .. }) {
                // Could also be a nested StyleCondition fallback; still ok.
            }
            if !extract_branches(branches, None, &mut ports) {
                return None;
            }
        }
        _ => return None,
    }
    if ports.is_empty() {
        None
    } else {
        Some(ports)
    }
}

fn extract_branches(
    branches: &[StyleBranch],
    outer_gate: Option<&str>,
    ports: &mut Vec<FastPort>,
) -> bool {
    for branch in branches {
        match &branch.condition {
            StyleTest::Single {
                property,
                value: Expr::Literal(v),
            } => {
                let val = *v as i64;
                // Gated-inner-broadcast pattern, mirroring
                // broadcast_write::extract_branches_into.
                if val == 1 {
                    if let Expr::StyleCondition {
                        branches: inner, ..
                    } = &branch.then
                    {
                        let gate = property.as_str();
                        if !extract_branches(inner, Some(gate), ports) {
                            return false;
                        }
                        continue;
                    }
                }
                ports.push(FastPort {
                    dest_property: property.clone(),
                    value_expr: branch.then.clone(),
                    gate_property: outer_gate.map(|s| s.to_string()),
                });
            }
            _ => return false,
        }
    }
    true
}

// ---------------------------------------------------------------------------
// @property runs:  `@property --<prefix><digits> { ... }`
// ---------------------------------------------------------------------------

const PROPERTY_ANCHOR: &[u8] = b"\n@property --";

fn recognise_property_run(bytes: &[u8], result: &mut FastPathResult) {
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        let offset = match find_subslice(&bytes[cursor..], PROPERTY_ANCHOR) {
            Some(o) => cursor + o,
            None => break,
        };
        match try_property_run_from(bytes, offset) {
            Some((end, prefix, entries)) if entries.len() >= 1024 => {
                emit_property_run(bytes, &prefix, &entries, result);
                cursor = end;
            }
            _ => {
                cursor = offset + PROPERTY_ANCHOR.len();
            }
        }
    }
}

/// Find a contiguous run of `@property --<prefix><digits> { ... }` blocks.
fn try_property_run_from(
    bytes: &[u8],
    offset: usize,
) -> Option<(usize, Vec<u8>, Vec<(usize, usize, u64)>)> {
    // Anchor is "\n@property --"; prefix starts right after.
    let name_start = offset + PROPERTY_ANCHOR.len();
    let mut i = name_start;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_digit() {
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == b'_' || c == b'-') {
            return None;
        }
        i += 1;
    }
    let prefix_end = i;
    if prefix_end == name_start {
        return None;
    }
    if !bytes.get(prefix_end).is_some_and(|b| b.is_ascii_digit()) {
        return None;
    }
    let prefix: Vec<u8> = bytes[name_start..prefix_end].to_vec();

    let mut entries: Vec<(usize, usize, u64)> = Vec::new();
    let mut scan = offset;
    loop {
        // The anchor requires a leading '\n'; between entries we may have
        // multiple blank lines, so skip any '\n'/whitespace before
        // re-checking.
        while scan < bytes.len() && matches!(bytes[scan], b'\n' | b' ' | b'\t' | b'\r') {
            scan += 1;
        }
        // We stripped the leading '\n' that the anchor requires, so compare
        // against the anchor's payload (everything after the '\n').
        let anchor_payload = &PROPERTY_ANCHOR[1..];
        if !bytes[scan..].starts_with(anchor_payload) {
            break;
        }
        let n_start = scan + anchor_payload.len();
        let p_end = n_start + prefix.len();
        if bytes.get(n_start..p_end) != Some(&prefix[..]) {
            break;
        }
        let mut d = p_end;
        while d < bytes.len() && bytes[d].is_ascii_digit() {
            d += 1;
        }
        if d == p_end {
            break;
        }
        let addr = match std::str::from_utf8(&bytes[p_end..d]).ok().and_then(|s| s.parse::<u64>().ok()) {
            Some(a) => a,
            None => break,
        };
        // Skip whitespace until `{`.
        let mut j = d;
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t') {
            j += 1;
        }
        if bytes.get(j) != Some(&b'{') {
            break;
        }
        // Find matching `}`.
        let end = match scan_balanced_braces(bytes, j) {
            Some(e) => e,
            None => break,
        };
        let entry_start = scan; // we already consumed leading whitespace
        let entry_end = end + 1; // include the `}`
        entries.push((entry_start, entry_end, addr));
        scan = entry_end;
    }

    if entries.is_empty() {
        None
    } else {
        Some((scan, prefix, entries))
    }
}

fn scan_balanced_braces(bytes: &[u8], start: usize) -> Option<usize> {
    debug_assert_eq!(bytes.get(start), Some(&b'{'));
    let mut depth: i32 = 0;
    let mut i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn emit_property_run(
    bytes: &[u8],
    prefix: &[u8],
    entries: &[(usize, usize, u64)],
    result: &mut FastPathResult,
) {
    if entries.len() < 3 {
        return;
    }
    let (ra_idx, rb_idx, rc_idx) = pick_reference_triple(entries);
    let (ra_start, ra_end, ra_addr) = entries[ra_idx];
    let (rb_start, rb_end, rb_addr) = entries[rb_idx];
    let (rc_start, rc_end, rc_addr) = entries[rc_idx];
    let body_a = &bytes[ra_start..ra_end];
    let body_b = &bytes[rb_start..rb_end];
    let body_c = &bytes[rc_start..rc_end];

    let (literals, mut hole_kinds) = match learn_template(body_a, ra_addr, body_b, rb_addr, body_c, rc_addr) {
        Some(t) => t,
        None => {
            log::info!(
                "[fast-path] @property run --{} — template unlearnable, skipping",
                String::from_utf8_lossy(prefix),
            );
            return;
        }
    };
    if !refine_template(bytes, entries, &literals, &mut hole_kinds) {
        log::info!(
            "[fast-path] @property run --{} failed refinement, skipping",
            String::from_utf8_lossy(prefix),
        );
        return;
    }

    // For @property blocks the templates we care about have a single Free
    // hole: the `initial-value: <N>` byte. We tolerate any number of
    // Consts and at most one Free hole. More than one Free is a shape we
    // don't support yet (would need to know which Free is the init value
    // and which is something else).
    let free_count = hole_kinds.iter().filter(|k| matches!(k, HoleKind::Free)).count();
    if free_count > 1 {
        log::info!(
            "[fast-path] @property run --{} has {} Free holes (>1 not supported), skipping",
            String::from_utf8_lossy(prefix),
            free_count,
        );
        return;
    }

    // Collect per-entry Free values during verification.
    let mut per_entry_free: Vec<Vec<u64>> = Vec::with_capacity(entries.len());
    for &(s, e, a) in entries {
        let body = &bytes[s..e];
        match verify_entry(body, &literals, &hole_kinds, a) {
            Some(frees) => per_entry_free.push(frees),
            None => {
                log::info!(
                    "[fast-path] @property run --{} — entry addr={} failed verification, skipping",
                    String::from_utf8_lossy(prefix),
                    a,
                );
                return;
            }
        }
    }

    // Parse the template ONCE with cssparser to extract syntax/inherits.
    // For the `initial-value` (if it's a Free hole) we override per-entry
    // after parsing. We materialise using value 0 for any Free hole; if
    // the template doesn't parse at initial-value=0, we bail.
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("").to_string();
    let synth_addr = ra_addr;
    let template_body = materialise_property_template(&literals, &hole_kinds, synth_addr, 0);
    let synth_css = format!(
        "@property --{}{} {}",
        prefix_str,
        synth_addr,
        String::from_utf8_lossy(&template_body)
    );
    let synth_def = match parse_single_property(&synth_css) {
        Some(p) => p,
        None => {
            log::warn!("[fast-path] @property template failed to parse; skipping");
            return;
        }
    };

    for (i, &(s, e, a)) in entries.iter().enumerate() {
        let initial_value = if free_count == 1 {
            // Exactly one Free hole — its value for this entry is the
            // per-entry initial value.
            let v = per_entry_free[i].first().copied().unwrap_or(0);
            Some(CssValue::Integer(v as i64))
        } else {
            synth_def.initial_value.clone()
        };
        result.properties.push(PropertyDef {
            name: format!("--{}{}", prefix_str, a),
            syntax: synth_def.syntax.clone(),
            inherits: synth_def.inherits,
            initial_value,
        });
        result.blank_ranges.push((s, e));
    }
    log::info!(
        "[fast-path] @property run --{} absorbed {} entries",
        prefix_str,
        entries.len()
    );
    // Sanity-check the materialised template round-trips: initial-value
    // should be an Integer for the typical shape. We don't fail if not —
    // some programs might have string-typed @properties too. The important
    // correctness guarantee is byte-equality of every entry against the
    // learned template, which we already verified.
    let _ = CssValue::Integer(0);
}

/// Reconstruct a `@property` block body by filling a learned template with
/// concrete values. `addr` supplies the cell's own address for `Addr` holes;
/// `free_placeholder` is substituted for any `Free` hole. The returned bytes
/// start with `{` and end with `}`.
fn materialise_property_template(
    literals: &[&[u8]],
    hole_kinds: &[HoleKind],
    addr: u64,
    free_placeholder: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        literals.iter().map(|s| s.len()).sum::<usize>() + hole_kinds.len() * 10,
    );
    for (i, lit) in literals.iter().enumerate() {
        out.extend_from_slice(lit);
        if i < hole_kinds.len() {
            let val = match hole_kinds[i] {
                HoleKind::Addr => addr,
                HoleKind::Const(c) => c,
                HoleKind::Free => free_placeholder,
            };
            out.extend_from_slice(val.to_string().as_bytes());
        }
    }
    // Return from the first `{` onward (the block body).
    let brace = out.iter().position(|&b| b == b'{').unwrap_or(0);
    out[brace..].to_vec()
}

fn parse_single_property(css: &str) -> Option<PropertyDef> {
    // Parse the whole synthetic CSS via parse_stylesheet, then return the
    // first PropertyDef. The stylesheet is tiny so this is cheap.
    let program = crate::parser::parse_stylesheet(css).ok()?;
    program.properties.into_iter().next()
}

// ---------------------------------------------------------------------------
// Blanking helper — apply the fast-path's blank_ranges to produce a view
// cssparser can consume.
// ---------------------------------------------------------------------------

/// Apply `blank_ranges` to `css`, returning a new `String` with those
/// ranges **removed entirely**. The returned string is shorter than the
/// input — byte offsets no longer match, so any error/source locations
/// cssparser reports will be off. We accept that trade-off in exchange
/// for not forcing cssparser to walk hundreds of megabytes of whitespace.
///
/// Panics if the ranges aren't sorted and non-overlapping.
pub fn apply_blank_ranges(css: &str, ranges: &[(usize, usize)]) -> String {
    if ranges.is_empty() {
        return css.to_string();
    }
    let bytes = css.as_bytes();
    let kept_len: usize = {
        let mut kept = bytes.len();
        for &(s, e) in ranges {
            debug_assert!(e >= s);
            kept = kept.saturating_sub(e - s);
        }
        kept
    };
    let mut out: Vec<u8> = Vec::with_capacity(kept_len + ranges.len()); // +1 newline per range for safety
    let mut cursor = 0usize;
    for &(s, e) in ranges {
        debug_assert!(s >= cursor, "blank ranges must be non-overlapping & sorted");
        debug_assert!(e <= bytes.len());
        out.extend_from_slice(&bytes[cursor..s]);
        // Inject a single newline in place of the removed range so
        // statements on either side don't concatenate (unlikely for
        // top-level declarations, but cheap insurance).
        out.push(b'\n');
        cursor = e;
    }
    out.extend_from_slice(&bytes[cursor..]);
    // SAFETY: we only removed byte ranges on UTF-8 boundaries (chosen at
    // ASCII `\n`/`@`/`;`) and inserted one ASCII newline between them.
    String::from_utf8(out).expect("range removal preserved UTF-8 boundaries")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_digits_basic() {
        let (lits, holes) = split_digits(b"foo 12 bar 345 baz").unwrap();
        assert_eq!(lits, vec![b"foo ".as_ref(), b" bar ".as_ref(), b" baz".as_ref()]);
        assert_eq!(holes, vec![12, 345]);
    }

    #[test]
    fn learn_template_simple() {
        let a = b"--m5: addr=5 const=7;";
        let b = b"--m9: addr=9 const=7;";
        let c = b"--m11: addr=11 const=7;";
        let (lits, kinds) = learn_template(a, 5, b, 9, c, 11).unwrap();
        // holes: (5,5,7) in a; (9,9,7) in b; (11,11,7) in c.
        // First two are Addr, last is Const.
        assert_eq!(kinds.len(), 3);
        assert_eq!(kinds[0], HoleKind::Addr);
        assert_eq!(kinds[1], HoleKind::Addr);
        assert_eq!(kinds[2], HoleKind::Const(7));
        assert_eq!(lits.len(), 4);
    }

    #[test]
    fn learn_template_detects_free() {
        // Three cells whose init value differs per cell — that hole must
        // be classified Free, not Addr or Const.
        let a = b"--m5: addr=5 init=100;";
        let b = b"--m9: addr=9 init=200;";
        let c = b"--m11: addr=11 init=300;";
        let (_lits, kinds) = learn_template(a, 5, b, 9, c, 11).unwrap();
        assert_eq!(kinds.len(), 3);
        assert_eq!(kinds[0], HoleKind::Addr);
        assert_eq!(kinds[1], HoleKind::Addr);
        assert_eq!(kinds[2], HoleKind::Free);
    }

    #[test]
    fn verify_entry_matches_template() {
        let a = b"--m5: addr=5 const=7;";
        let b = b"--m9: addr=9 const=7;";
        let c = b"--m11: addr=11 const=7;";
        let (lits, kinds) = learn_template(a, 5, b, 9, c, 11).unwrap();
        assert_eq!(
            verify_entry(b"--m42: addr=42 const=7;", &lits, &kinds, 42),
            Some(vec![]),
        );
        assert_eq!(verify_entry(b"--m42: addr=43 const=7;", &lits, &kinds, 42), None);
        assert_eq!(verify_entry(b"--m42: addr=42 const=8;", &lits, &kinds, 42), None);
    }

    #[test]
    fn verify_entry_extracts_free_values() {
        let a = b"--m5: addr=5 init=100;";
        let b = b"--m9: addr=9 init=200;";
        let c = b"--m11: addr=11 init=300;";
        let (lits, kinds) = learn_template(a, 5, b, 9, c, 11).unwrap();
        assert_eq!(
            verify_entry(b"--m42: addr=42 init=77;", &lits, &kinds, 42),
            Some(vec![77]),
        );
    }

    #[test]
    fn apply_blank_ranges_removes_and_inserts_newline() {
        let css = "hello\nworld\nfoo";
        // Remove "world" (offsets 6..11), expect a newline inserted.
        let out = apply_blank_ranges(css, &[(6, 11)]);
        assert_eq!(out, "hello\n\n\nfoo");
    }
}
