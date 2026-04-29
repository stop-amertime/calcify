//! Byte-level period detection.
//!
//! Walks a byte slice and identifies regions where a sequence of P bytes
//! repeats K times back-to-back. The shape-only signal we feed the
//! fusion-site simulator: "candidates for unrolled-body fusion live at
//! these offsets."
//!
//! This is the byte-level twin of `replicated_body`'s op-level period
//! detector. Same algorithm, different alphabet:
//!
//!   - replicated_body operates on compiled Ops (one per CSS dispatch
//!     entry). The doom column drawer's 16 reps don't show up there
//!     because Kiln emits one dispatch entry per opcode, so the static
//!     op stream sees fragments, not unrolled bodies.
//!
//!   - This module operates on rom-disk memory bytes — the actual cart
//!     program text, where the 16 unrolled bodies sit literally
//!     back-to-back as 21-byte sequences.
//!
//! The detector knows nothing about x86, opcodes, or Doom. It finds
//! byte-level periodicity. Whether a found region is *fusible* is a
//! separate question answered by the simulator downstream.
//!
//! Genericity: the same detector fires on any cart that emits unrolled
//! bodies as repeated byte sequences — 6502 unrolled blits, brainfuck
//! `>+>+>+>+`, Wolfenstein column drawers, etc. Pattern recognition
//! over byte shape, not over what those bytes mean.
//!
//! Wildcards (varying immediates across reps) are intentionally
//! out-of-scope for this first cut. If reps differ in any byte, the
//! region is not reported. Doom8088's column drawer is fully literal
//! (all 16 reps share the same `EC 00` stride immediate), so this is
//! enough to find it.
//!
//! Future extension: a second phase that identifies positions varying
//! linearly across reps (constant stride per-rep) and emits a
//! wildcard-with-stride descriptor — same logic as
//! `replicated_body::classify_body`'s operand stride classifier, just
//! at the byte level.
//!
//! Algorithm. For each candidate start position s in [0, len):
//!   1. For each candidate period p in [MIN_PERIOD, MAX_PERIOD]:
//!      a. Find the longest run starting at s where bytes[i] ==
//!         bytes[i + p] for all i in [s, s + run - p).
//!      b. reps = run / p (truncated). If reps >= MIN_REPS, record
//!         (s, p, reps).
//!   2. After collecting all (s, p, reps) candidates, suppress
//!      overlaps: prefer the region with greater reps*p (longest
//!      total span); on tie, prefer smaller p (tighter pattern).
//!
//! The MAX_PERIOD bound keeps the inner loop O(len * MAX_PERIOD) total.
//! For doom-shaped fusion candidates, periods are small (the doom
//! column drawer is 21 bytes; 6502 unrolled blits are typically
//! <40 bytes per rep). MAX_PERIOD = 256 is generous and still cheap:
//! 256 * 4 MB = 1 GB byte-compares, ~1 s on modern hardware. We
//! tighten this further at call sites if needed.
//!
//! MIN_REPS = 4 is the threshold below which fusion likely doesn't pay
//! for the dispatch-wiring overhead. The doom column drawer at K = 16
//! is well above this; we want to catch shorter unrolls too (e.g.,
//! 4-rep blits) so the floor is conservative.

/// A periodic region in a byte slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRegion {
    /// Offset into the input slice where the region starts.
    pub start: usize,
    /// Period length in bytes (each rep is this many bytes).
    pub period: usize,
    /// Number of repetitions. Total span is period * reps bytes.
    pub reps: usize,
}

impl ByteRegion {
    pub fn end(&self) -> usize {
        self.start + self.period * self.reps
    }
    pub fn span(&self) -> usize {
        self.period * self.reps
    }
}

pub const DEFAULT_MIN_PERIOD: usize = 4;
pub const DEFAULT_MAX_PERIOD: usize = 256;
pub const DEFAULT_MIN_REPS: usize = 4;

/// Scan `bytes` for periodic regions and return them with overlaps suppressed.
///
/// `min_period` / `max_period` bound the period search; `min_reps` is the
/// minimum repetition count to report. Use `DEFAULT_*` constants for
/// reasonable defaults.
pub fn detect_byte_periods(
    bytes: &[u8],
    min_period: usize,
    max_period: usize,
    min_reps: usize,
) -> Vec<ByteRegion> {
    if bytes.len() < min_period * min_reps {
        return Vec::new();
    }
    let max_period = max_period.min(bytes.len() / min_reps);
    let mut candidates: Vec<ByteRegion> = Vec::new();
    let mut s = 0usize;
    while s + min_period * min_reps <= bytes.len() {
        let mut best_at_s: Option<ByteRegion> = None;
        for p in min_period..=max_period {
            if s + p * min_reps > bytes.len() {
                break;
            }
            let reps = count_reps(bytes, s, p);
            if reps >= min_reps {
                let region = ByteRegion { start: s, period: p, reps };
                best_at_s = match best_at_s {
                    None => Some(region),
                    Some(prev) => {
                        if region.span() > prev.span()
                            || (region.span() == prev.span() && region.period < prev.period)
                        {
                            Some(region)
                        } else {
                            Some(prev)
                        }
                    }
                };
            }
        }
        if let Some(region) = best_at_s {
            candidates.push(region);
            s = region.end();
        } else {
            s += 1;
        }
    }
    suppress_overlaps(candidates)
}

/// Count how many times the period-p sequence starting at `s` repeats.
///
/// Returns the largest k such that for all i in [0, p*k),
/// bytes[s + i] == bytes[s + i + p]... no wait. The largest k such that
/// the byte block bytes[s..s+p] equals bytes[s+p..s+2p] equals ... equals
/// bytes[s+(k-1)*p..s+k*p]. So k=1 means "the seed body fits at s" (always
/// true if s+p <= len). k=2 means the second copy matches. Etc.
fn count_reps(bytes: &[u8], s: usize, p: usize) -> usize {
    if s + p > bytes.len() {
        return 0;
    }
    let mut k = 1usize;
    while s + (k + 1) * p <= bytes.len() {
        let mut all_match = true;
        for i in 0..p {
            if bytes[s + i] != bytes[s + k * p + i] {
                all_match = false;
                break;
            }
        }
        if !all_match {
            break;
        }
        k += 1;
    }
    k
}

/// Suppress overlapping regions, keeping the one with the larger span
/// (on ties, smaller period). Greedy interval scheduling on
/// span-sorted candidates.
fn suppress_overlaps(mut regions: Vec<ByteRegion>) -> Vec<ByteRegion> {
    regions.sort_by(|a, b| {
        b.span()
            .cmp(&a.span())
            .then_with(|| a.period.cmp(&b.period))
            .then_with(|| a.start.cmp(&b.start))
    });
    let mut kept: Vec<ByteRegion> = Vec::new();
    for r in regions {
        let r_end = r.end();
        let overlaps = kept.iter().any(|k| {
            let k_end = k.end();
            r.start < k_end && k.start < r_end
        });
        if !overlaps {
            kept.push(r);
        }
    }
    kept.sort_by_key(|r| r.start);
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_simple_period() {
        let bytes: Vec<u8> = b"ABCDABCDABCDABCDABCD".to_vec();
        let regions = detect_byte_periods(&bytes, 2, 16, 4);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0], ByteRegion { start: 0, period: 4, reps: 5 });
    }

    #[test]
    fn ignores_non_periodic_prefix() {
        let mut bytes: Vec<u8> = b"XYZ".to_vec();
        bytes.extend_from_slice(&b"ABCDABCDABCDABCDABCD".repeat(1));
        let regions = detect_byte_periods(&bytes, 2, 16, 4);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].start, 3);
        assert_eq!(regions[0].period, 4);
        assert_eq!(regions[0].reps, 5);
    }

    #[test]
    fn min_reps_filters_short_runs() {
        let bytes: Vec<u8> = b"ABCABCAB".to_vec();
        let regions = detect_byte_periods(&bytes, 2, 8, 4);
        assert_eq!(regions, Vec::new());
    }

    #[test]
    fn picks_longest_when_multiple_periods_match() {
        // "AAAA..." matches period=1 and period=2 and period=4.
        // We prefer the longest span, ties broken by smallest period.
        let bytes = vec![0xAAu8; 20];
        let regions = detect_byte_periods(&bytes, 1, 8, 4);
        assert_eq!(regions.len(), 1);
        // p=1 reps=20 span=20; p=2 reps=10 span=20; same span, prefer smaller p.
        assert_eq!(regions[0].period, 1);
        assert_eq!(regions[0].reps, 20);
    }

    #[test]
    fn finds_two_disjoint_regions() {
        let mut bytes: Vec<u8> = b"ABCDABCDABCDABCD".to_vec();
        bytes.extend_from_slice(b"NOISEPAD"); // 8 bytes with no internal period >=4
        bytes.extend_from_slice(&b"XYZWXYZWXYZWXYZW".repeat(1));
        let regions = detect_byte_periods(&bytes, 2, 16, 4);
        // Should find ABCD-region at 0 and XYZW-region at 24.
        let r0 = regions.iter().find(|r| r.start == 0).expect("ABCD region");
        let r1 = regions.iter().find(|r| r.start == 24).expect("XYZW region");
        assert_eq!(r0.period, 4);
        assert_eq!(r0.reps, 4);
        assert_eq!(r1.period, 4);
        assert_eq!(r1.reps, 4);
    }

    #[test]
    fn doom_shaped_pattern_is_found() {
        // The actual doom8088 column-drawer body: 21 bytes, 16 reps.
        let body: [u8; 21] = [
            0x88, 0xF0, 0xD0, 0xE8, 0x89, 0xF3, 0xD7, 0x89, 0xCB, 0x36, 0xD7, 0x88, 0xC4, 0xAB,
            0xAB, 0x81, 0xC7, 0xEC, 0x00, 0x01, 0xEA,
        ];
        let mut bytes: Vec<u8> = vec![0u8; 100]; // padding
        for _ in 0..16 {
            bytes.extend_from_slice(&body);
        }
        bytes.extend_from_slice(&[0u8; 100]); // trailing padding

        let regions = detect_byte_periods(&bytes, 4, 64, 4);
        let doom = regions
            .iter()
            .find(|r| r.period == 21 && r.reps == 16)
            .expect("expected to find doom-shaped 21x16 region");
        assert_eq!(doom.start, 100);
    }

    #[test]
    fn nearly_periodic_one_byte_off_does_not_match() {
        let mut bytes: Vec<u8> = b"ABCDABCDABCDABCDABCD".to_vec();
        bytes[10] = b'X'; // corrupt one byte mid-stream
        // Now bytes[8..12] = "ABXD" which breaks the period at the third rep.
        // We expect either no region, or shorter regions on each side that
        // both stay below min_reps=4.
        let regions = detect_byte_periods(&bytes, 4, 16, 4);
        for r in &regions {
            // No region should claim to cross the corrupted byte.
            assert!(
                r.end() <= 8 || r.start >= 12,
                "region {:?} crosses corrupted byte at index 10",
                r
            );
        }
    }

    #[test]
    fn empty_or_tiny_input_returns_empty() {
        assert!(detect_byte_periods(&[], 2, 16, 4).is_empty());
        assert!(detect_byte_periods(&[1, 2, 3], 2, 16, 4).is_empty());
    }

    #[test]
    fn count_reps_matches_naive() {
        let bytes: Vec<u8> = b"ABABABABXY".to_vec();
        assert_eq!(count_reps(&bytes, 0, 2), 4);
        assert_eq!(count_reps(&bytes, 0, 4), 2);
        assert_eq!(count_reps(&bytes, 8, 2), 1);
    }

    #[test]
    fn overlapping_candidates_resolve_to_disjoint_set() {
        // Construct a slice where period=2 and period=4 both match the
        // same prefix, and verify suppress_overlaps keeps the
        // higher-span (or lower-period on tie) one without leaving
        // overlapping siblings.
        let bytes = vec![1, 2, 1, 2, 1, 2, 1, 2, 1, 2, 1, 2];
        let regions = detect_byte_periods(&bytes, 2, 8, 4);
        for i in 0..regions.len() {
            for j in (i + 1)..regions.len() {
                let a = regions[i];
                let b = regions[j];
                assert!(
                    a.end() <= b.start || b.end() <= a.start,
                    "regions {:?} and {:?} overlap",
                    a,
                    b
                );
            }
        }
    }
}
