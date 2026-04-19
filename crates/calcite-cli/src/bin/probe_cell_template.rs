//! Probe: can we fast-path the `--mN: <repeated shape>` region via a
//! digit-substitution template learned from the first cell?
//!
//! Strategy:
//!   1. Find all `--m<digits>:` assignment starts in the CSS.
//!   2. Extract the bytes of the first cell: `--m<N>: <...body...>;`.
//!   3. Build a template by stripping `--m<N>` from the start and recording
//!      all digit-run positions inside the body. (Every `<number>` becomes
//!      a hole; the template is the surrounding byte-literal sequence.)
//!   4. For each subsequent cell, verify that its body matches the template
//!      byte-for-byte at every literal position, and extract the numbers
//!      that land in each hole.
//!   5. Check that the number in every hole is either:
//!      (a) the cell's own address N (memAddr<k>: N, var(--memVal<k>), etc. —
//!          wait no, these are fixed), or
//!      (b) the cell's address N (the --m<N> on the LHS and the else:
//!          var(--__1m<N>) fallback)
//!   6. If YES: we can fast-path this cell as CellRead { addr: N }.
//!      If NO: bail, fall back to full parse.
//!
//! The probe measures how fast the digit-substitution check is and how many
//! cells in rogue-fresh.css pass it.

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: probe_cell_template <css>");
    let t = Instant::now();
    let css = std::fs::read_to_string(&path).expect("read");
    println!("read_to_string: {:.3}s ({} bytes)", t.elapsed().as_secs_f64(), css.len());

    // Step 1: find cell starts. We look for `\n  --m<digits>:` at line start
    // (two-space indent is the CSS-DOS convention). A rigorous real
    // implementation would be less anchored to whitespace but this is a probe.
    let t = Instant::now();
    let bytes = css.as_bytes();
    let mut cell_starts = Vec::new();
    let needle = b"\n  --m";
    let mut i = 0;
    while i + needle.len() < bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let name_start = i + 3; // position of the `--m`
            let mut j = name_start + 3; // after `--m`
            if !bytes[j].is_ascii_digit() {
                i += 1;
                continue;
            }
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b':' {
                cell_starts.push((name_start, j)); // (start_of_--m, pos_of_colon)
            }
            i = j;
        } else {
            i += 1;
        }
    }
    println!("find cell starts: {:.3}s ({} cells)",
        t.elapsed().as_secs_f64(), cell_starts.len());

    // Step 2: extract the body of each cell — bytes from after the colon
    // to the semicolon at depth 0. The parse is paren-balanced so `;` inside
    // `calc(...)` doesn't terminate.
    let t = Instant::now();
    fn find_cell_end(bytes: &[u8], start_after_colon: usize) -> usize {
        let mut depth: i32 = 0;
        let mut i = start_after_colon;
        while i < bytes.len() {
            match bytes[i] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                b';' if depth <= 0 => return i,
                _ => {}
            }
            i += 1;
        }
        bytes.len()
    }

    let cell_spans: Vec<(usize, usize, usize, u32)> = cell_starts.iter().map(|&(lhs, colon)| {
        let addr_digits = &bytes[lhs + 3..colon];
        let addr: u32 = std::str::from_utf8(addr_digits).unwrap().parse().unwrap();
        let end = find_cell_end(bytes, colon + 1);
        (lhs, colon, end, addr)
    }).collect();
    let total_cell_bytes: usize = cell_spans.iter().map(|(a, _, c, _)| c - a).sum();
    println!("locate cell bodies: {:.3}s (total {} bytes, {:.1}%)",
        t.elapsed().as_secs_f64(),
        total_cell_bytes,
        100.0 * total_cell_bytes as f64 / bytes.len() as f64);

    // Step 3: build a template from two reference cells.
    //
    // Using one cell is ambiguous when its addr collides with a constant
    // hole (e.g. cell 0 has `0` both as its addr and as the slot-0 literal).
    // Using two cells with different addrs disambiguates: for each hole,
    //   - if hole in cell A == addr_A and hole in cell B == addr_B → this
    //     hole is the "addr" parameter.
    //   - if hole in cell A == hole in cell B (and != either addr) → this
    //     hole is a constant.
    //   - otherwise the shape isn't templatable; bail.
    //
    // We deliberately pick two cells whose addrs are NOT in the set of
    // constants we expect to see (slot indices 0..=5), so there's no
    // coincidental match.
    let t = Instant::now();
    fn body<'a>(bytes: &'a [u8], span: &(usize, usize, usize, u32)) -> &'a [u8] {
        let (lhs, _colon, end, _) = *span;
        &bytes[lhs..=end]
    }
    fn split_digits(body: &[u8]) -> (Vec<&[u8]>, Vec<u64>) {
        let mut lits: Vec<&[u8]> = Vec::new();
        let mut holes: Vec<u64> = Vec::new();
        let mut i = 0;
        let mut lit_start = 0;
        while i < body.len() {
            if body[i].is_ascii_digit() {
                lits.push(&body[lit_start..i]);
                let hole_start = i;
                while i < body.len() && body[i].is_ascii_digit() {
                    i += 1;
                }
                let val: u64 = std::str::from_utf8(&body[hole_start..i]).unwrap().parse().unwrap();
                holes.push(val);
                lit_start = i;
            } else {
                i += 1;
            }
        }
        lits.push(&body[lit_start..]);
        (lits, holes)
    }

    // Find a ref cell with addr > 100 (ensures addr isn't 0..=5 slot literals
    // or 1..=255 opcodes etc. — actually memAddr hole values can be large,
    // so pick two cells with clearly distinct large addrs).
    let ref_a = cell_spans.iter().find(|s| s.3 >= 100_000).copied()
        .unwrap_or(cell_spans[cell_spans.len() / 2]);
    let ref_b = cell_spans.iter().rev().find(|s| s.3 >= 200_000 && s.3 != ref_a.3).copied()
        .unwrap_or(*cell_spans.last().unwrap());
    println!("  ref cells: addr {} and addr {}", ref_a.3, ref_b.3);

    let (lits_a, holes_a) = split_digits(body(bytes, &ref_a));
    let (lits_b, holes_b) = split_digits(body(bytes, &ref_b));

    if lits_a != lits_b {
        eprintln!("refs disagree on literals; shape not templatable (lits: {} vs {})",
            lits_a.len(), lits_b.len());
        return;
    }
    if holes_a.len() != holes_b.len() {
        eprintln!("refs disagree on hole count");
        return;
    }

    // Classify holes.
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum HoleKind { Addr, Const(u64) }
    let mut hole_kinds: Vec<HoleKind> = Vec::with_capacity(holes_a.len());
    for (i, (&va, &vb)) in holes_a.iter().zip(holes_b.iter()).enumerate() {
        if va == ref_a.3 as u64 && vb == ref_b.3 as u64 {
            hole_kinds.push(HoleKind::Addr);
        } else if va == vb {
            hole_kinds.push(HoleKind::Const(va));
        } else {
            eprintln!("hole {} can't be classified: a={}, b={} (addr_a={}, addr_b={})",
                i, va, vb, ref_a.3, ref_b.3);
            return;
        }
    }
    let literals = lits_a;
    let addr_holes = hole_kinds.iter().filter(|k| **k == HoleKind::Addr).count();
    println!("build template: {:.3}s ({} literal segments, {} holes, {} are addr)",
        t.elapsed().as_secs_f64(),
        literals.len(),
        hole_kinds.len(),
        addr_holes,
    );

    // Step 4: for each subsequent cell, verify byte-equal literals and matching
    // hole values (either the constant from cell 0, or == cell's own addr).
    let t = Instant::now();
    let mut matched = 0u64;
    let mut mismatched = 0u64;
    let mut mismatch_examples: Vec<(usize, u32)> = Vec::new();
    'cells: for (ci, &(lhs, _colon, end, addr)) in cell_spans.iter().enumerate() {
        let body = &bytes[lhs..=end];
        let mut pos = 0usize;
        for (hi, lit) in literals.iter().enumerate() {
            if pos + lit.len() > body.len() {
                mismatched += 1;
                if mismatch_examples.len() < 3 { mismatch_examples.push((ci, addr)); }
                continue 'cells;
            }
            if &body[pos..pos + lit.len()] != *lit {
                mismatched += 1;
                if mismatch_examples.len() < 3 { mismatch_examples.push((ci, addr)); }
                continue 'cells;
            }
            pos += lit.len();
            // If there's a hole after this literal, parse it
            if hi < hole_kinds.len() {
                let hole_start = pos;
                while pos < body.len() && body[pos].is_ascii_digit() {
                    pos += 1;
                }
                if pos == hole_start {
                    mismatched += 1;
                    if mismatch_examples.len() < 3 { mismatch_examples.push((ci, addr)); }
                    continue 'cells;
                }
                let val: u64 = std::str::from_utf8(&body[hole_start..pos])
                    .unwrap().parse().unwrap();
                let expected = match hole_kinds[hi] {
                    HoleKind::Addr => addr as u64,
                    HoleKind::Const(c) => c,
                };
                if val != expected {
                    mismatched += 1;
                    if mismatch_examples.len() < 3 { mismatch_examples.push((ci, addr)); }
                    continue 'cells;
                }
            }
        }
        if pos != body.len() {
            mismatched += 1;
            if mismatch_examples.len() < 3 { mismatch_examples.push((ci, addr)); }
            continue;
        }
        matched += 1;
    }
    let elapsed = t.elapsed().as_secs_f64();
    println!("verify {} cells against template: {:.3}s ({:.1} MB/s of cell-region)",
        cell_spans.len(),
        elapsed,
        total_cell_bytes as f64 / 1_048_576.0 / elapsed,
    );
    println!("  matched:    {}", matched);
    println!("  mismatched: {}", mismatched);
    if !mismatch_examples.is_empty() {
        println!("  first mismatches: {:?}", mismatch_examples);
    }
}
