//! Runtime period-and-affine-evolution detection for hot inner loops.
//!
//! ## Why
//!
//! Some CSS workloads (notably the bootle-ctest splash-screen fill) spend
//! millions of ticks repeating a fixed-length microcode loop whose state
//! evolves affinely across iterations: a counter increments by a constant,
//! the cycle count increments by a constant, and a memory write advances by
//! a fixed stride each iteration. The docs/superpowers/specs file
//! `2026-04-17-memoisation-viability.md` characterised this pattern exactly
//! for bootle-ctest: period `P = 26` ticks, 99.6% of the splash phase, one
//! memory offset per iteration that advances by `+1` in address with a
//! constant fill value.
//!
//! This module detects that pattern at runtime and skips forward in one
//! bulk step: `memset(range) + scalar update of affine state vars`.
//!
//! ## Cardinal-rule safety
//!
//! This module never inspects x86 opcodes, never knows about "x86", and
//! never rewrites or suggests CSS changes. It only observes:
//!
//!   (a) per-tick fingerprints of `state_vars` (state the caller already
//!       maintains), and
//!   (b) per-tick `(addr, value)` pairs from `state.write_log`.
//!
//! It induces a period over the fingerprint stream and an affine law over
//! the state-var / memory-write evolution, the same way
//! [`pattern::dispatch_table`] induces a direct-mapped table from a literal
//! if-chain. The detection is over the tick axis rather than the op axis,
//! but the reasoning is analogous.

use crate::state::State;

/// Minimum period worth detecting.
///
/// P=1 corresponds to "every tick does the same thing" — a REP STOSB-style
/// tight loop where the only evolution is affine (addr++, counter--, etc.).
/// Detecting P=1 requires the affine-verification step below to reject
/// workloads that merely look stable but evolve non-affinely across ticks.
const MIN_PERIOD: usize = 1;

/// Maximum period searched.
const MAX_PERIOD: usize = 512;

/// Calibration buffer length. During Cold mode we keep this many recent
/// samples in a flat `Vec` (no ring wrapping during detection) so that
/// absolute indices into it are stable until the next reset.
///
/// Sized to balance two constraints:
/// - Detection needs enough periodic samples to pass MIN_MATCHES with the
///   configured MATCH_RATE (95%).
/// - Each failed calibration costs CALIB_LEN ticks before we can try again.
///   On a ~200K-tick splash phase, large buffers mean few chances to lock.
///
/// 1024 is a deliberate compromise: 128 matches at 95% covers a 134-sample
/// overlap — fine for P=1 (1023-sample overlap, far more) and still usable
/// for P up to ~256. Smaller buffers mean more attempts per workload.
const CALIB_LEN: usize = 256;

/// Fingerprint match rate (in /1000) required to accept a period as the
/// winner. 950 = 95% of the overlap must match.
const MATCH_RATE_NUM: usize = 950;
const MATCH_RATE_DEN: usize = 1000;

/// Minimum absolute number of overlap samples we need to call a period.
/// Scaled with CALIB_LEN so small buffers don't spuriously lock on a
/// handful of coincidental matches. 128 matches at 95% match rate covers
/// a 134-sample overlap — enough signal to trust.
const MIN_MATCHES: usize = 32;

/// How many full iterations (beyond the first) we need to confirm affine
/// structure before locking.
const CONFIRM_ITERS: usize = 3;

/// Maximum iterations to project per batch.
const MAX_PROJECT_ITERS: usize = 4096;

/// Initial projection budget. Doubles on successful validate, reset on
/// miss. The zero-crossing cap in `project()` bounds runaway projection,
/// so we can start more aggressively — if a lock is wrong, we lose one
/// wasted projection but the rollback path keeps state correct.
const INITIAL_BUDGET: usize = 64;


#[derive(Clone, Debug)]
struct Sample {
    vars: Box<[i32]>,
    mem_write: Option<(i32, i32)>,
}

#[derive(Clone, Copy, Debug)]
pub struct LockedWrite {
    /// Offset within the period where this write occurs.
    pub offset: usize,
    /// Address at iteration 0.
    pub base_addr: i32,
    /// Address stride per iteration.
    pub addr_stride: i32,
    /// Constant value written (val_stride must be 0 to lock).
    pub value: i32,
}

/// Tracker state machine.
#[derive(Clone, Debug)]
enum Mode {
    /// Warming up / hunting for a period.
    Cold,
    /// Locked: can project.
    Locked {
        period: usize,
        /// per-slot delta across one iteration.
        per_iter_delta: Box<[i32]>,
        writes: Box<[LockedWrite]>,
        /// Vars snapshot at the iteration that starts with alignment offset 0.
        /// Used by `can_project` to verify we're aligned.
        anchor_vars: Box<[i32]>,
        /// Number of iterations completed since lock (projected + real).
        iters_since_lock: usize,
        /// Projection budget; doubled on validate, reset on miss.
        budget: usize,
    },
    /// Missed a prediction. Wait `remaining` ticks before re-entering Cold.
    Cooldown { remaining: usize },
}

/// The period tracker.
#[derive(Clone, Debug)]
pub struct PeriodTracker {
    /// Calibration buffer. Pre-allocated to exactly CALIB_LEN Sample slots
    /// at construction; reused across calibration cycles via
    /// `copy_from_slice` into the next slot. `calib_len` is the cursor —
    /// the number of slots currently valid.
    calib: Vec<Sample>,
    calib_len: usize,
    mode: Mode,
    n_vars: usize,
    pub stats: PeriodStats,
}

#[derive(Clone, Debug, Default)]
pub struct PeriodStats {
    pub ticks_observed: u64,
    pub calibrations_attempted: u64,
    pub lock_count: u64,
    pub miss_count: u64,
    pub iters_projected: u64,
    pub ticks_projected: u64,
    pub bytes_memset: u64,
    /// Best period found during the last try_calibrate (0 = none found).
    pub last_best_period: u32,
    /// Best match count observed in the last try_calibrate for the winner.
    pub last_best_matches: u32,
    /// Reason we didn't lock on the last attempt (for debugging).
    pub last_reject_reason: &'static str,
    /// Count of cyclic slots used in the last fingerprint.
    pub last_cyclic_count: u32,
}

impl PeriodTracker {
    pub fn new(state: &State) -> Self {
        let n_vars = state.state_var_count();
        // Pre-allocate all Sample boxes up front so observe() never allocates.
        // Each Sample owns its own Box<[i32; n_vars]>; we reuse them via
        // copy_from_slice on subsequent calibration cycles.
        let mut calib = Vec::with_capacity(CALIB_LEN);
        for _ in 0..CALIB_LEN {
            calib.push(Sample {
                vars: vec![0i32; n_vars].into_boxed_slice(),
                mem_write: None,
            });
        }
        Self {
            calib,
            calib_len: 0,
            mode: Mode::Cold,
            n_vars,
            stats: PeriodStats::default(),
        }
    }

    /// Record one tick's worth of observation. `pre_tick_vars` is the
    /// `state_vars` snapshot BEFORE the tick ran; `mem_write` is the first
    /// `(addr, value)` from that tick's `write_log` with `addr >= 0`.
    ///
    /// Hot path: zero heap allocations. The calibration buffer is fixed-size
    /// and pre-populated at construction time; observe() fills the next slot
    /// in-place via copy_from_slice.
    pub fn observe(&mut self, pre_tick_vars: &[i32], mem_write: Option<(i32, i32)>) {
        self.stats.ticks_observed += 1;
        match self.mode {
            Mode::Cold => {
                // Copy into the next pre-allocated slot (no alloc).
                let slot = &mut self.calib[self.calib_len];
                slot.vars.copy_from_slice(pre_tick_vars);
                slot.mem_write = mem_write;
                self.calib_len += 1;
                if self.calib_len >= CALIB_LEN {
                    self.stats.calibrations_attempted += 1;
                    self.try_calibrate();
                    // Whether or not we locked, reset the calibration
                    // cursor — if we didn't lock, starting fresh is
                    // cheaper than trying to maintain a sliding window.
                    self.calib_len = 0;
                }
            }
            Mode::Locked { .. } => {
                // Normally the caller doesn't call observe while locked.
                // If they do (e.g., doing a validation iteration), ignore —
                // `validate_iteration` is the right entry point.
            }
            Mode::Cooldown { ref mut remaining } => {
                if *remaining <= 1 {
                    self.mode = Mode::Cold;
                    self.calib_len = 0;
                } else {
                    *remaining -= 1;
                }
            }
        }
    }

    /// Try to lock with period=1. Returns true if locked.
    ///
    /// A P=1 affine loop has: constant per-tick delta for every slot,
    /// a memory write every tick at addresses forming an arithmetic
    /// progression, and a constant fill value. That's exactly a REP STOSB.
    ///
    /// Strategy:
    ///  1. Find the longest run of consecutive-pair ticks with a constant
    ///     per-slot delta across all n_vars slots.
    ///  2. Require the run to be at least half the calibration window, so
    ///     one-off transient regimes don't lock us onto a bogus model.
    ///  3. Verify the memory-write progression (constant stride, value).
    ///  4. Anchor at the END of the run (most recent): we're most confident
    ///     the workload is still in this regime moving forward.
    ///  5. Cap future projections by the observed run length — we trust
    ///     the model for roughly as many ticks as we just observed.
    fn try_lock_p1(&mut self) -> bool {
        let len = self.calib_len;
        if len < 32 {
            return false;
        }
        let n_vars = self.n_vars;

        // Compute per-tick delta for every consecutive pair, then find the
        // longest run of identical delta-vectors. Rather than materialise
        // all deltas (that's n × n_vars ints), walk the buffer once with a
        // running "is the delta at t equal to the delta at t-1?" check.
        //
        // "Identical delta" = for every slot i, (vars[t][i] - vars[t-1][i])
        // == (vars[t+1][i] - vars[t][i]). Equivalent to
        // vars[t+1][i] + vars[t-1][i] == 2 * vars[t][i] (affine second-
        // difference is zero).
        let mut best_start = 0usize;
        let mut best_end = 0usize;   // exclusive
        let mut cur_start = 0usize;
        for t in 1..(len - 1) {
            // Check affinity between (t-1, t) and (t, t+1).
            let mut affine = true;
            for i in 0..n_vars {
                let a = self.calib[t - 1].vars[i];
                let b = self.calib[t].vars[i];
                let c = self.calib[t + 1].vars[i];
                if b.wrapping_sub(a) != c.wrapping_sub(b) {
                    affine = false;
                    break;
                }
            }
            if !affine {
                cur_start = t + 1;
            } else {
                // The run spans [cur_start, t+1] inclusive.
                let run = t + 2 - cur_start;
                let best = best_end - best_start;
                if run > best {
                    best_start = cur_start;
                    best_end = t + 2;
                }
            }
        }
        let run_len = best_end - best_start;

        // Require a substantial run. Half the buffer is a reasonable gate —
        // false lock here causes thousands of mis-filled bytes, so be strict.
        if run_len * 2 < len {
            return false;
        }

        // Extract the delta from the first consecutive pair of the run.
        let mut delta = vec![0i32; n_vars].into_boxed_slice();
        for i in 0..n_vars {
            delta[i] = self.calib[best_start + 1].vars[i]
                .wrapping_sub(self.calib[best_start].vars[i]);
        }
        // Need at least one non-zero delta.
        if delta.iter().all(|&d| d == 0) {
            return false;
        }

        // Verify memory-write progression across the run. Every tick must
        // write to memory with stride `addr_stride` from `a0`, value `v0`.
        let Some((a0, v0)) = self.calib[best_start].mem_write else {
            return false;
        };
        let Some((a1, v1)) = self.calib[best_start + 1].mem_write else {
            return false;
        };
        let addr_stride = a1.wrapping_sub(a0);
        if v1 != v0 {
            return false;
        }
        for t in (best_start + 2)..best_end {
            let Some((at, vt)) = self.calib[t].mem_write else {
                return false;
            };
            let k = (t - best_start) as i32;
            let expected_a = a0.wrapping_add(addr_stride.wrapping_mul(k));
            if at != expected_a || vt != v0 {
                return false;
            }
        }

        // Anchor at the END of the run (most recent observed tick that's
        // still in the regime). Project forward from there.
        //
        // tail is best_end - 1 (inclusive); that sample is the pre-tick
        // state of the (best_end - 1)'th observation. Since the tracker
        // lives on the pre-tick stream and the caller has ALREADY advanced
        // past it by the time calibration runs, we anchor on the last
        // sample so alignment() finds k = (current - anchor) / delta
        // very close to zero, making the first projection's math easy.
        let anchor_idx = best_end - 1;
        let anchor_vars = self.calib[anchor_idx].vars.clone();
        // LockedWrite's base_addr is the address at anchor_idx (k=0).
        let base_addr = a0.wrapping_add(addr_stride.wrapping_mul((anchor_idx - best_start) as i32));
        let write = LockedWrite {
            offset: 0,
            base_addr,
            addr_stride,
            value: v0,
        };
        self.stats.lock_count += 1;
        self.stats.last_best_period = 1;
        self.stats.last_best_matches = run_len as u32;
        self.stats.last_reject_reason = "";
        self.mode = Mode::Locked {
            period: 1,
            per_iter_delta: delta,
            writes: Box::new([write]),
            anchor_vars,
            iters_since_lock: 0,
            // Budget starts small; doubles on validate. Capped at
            // MAX_PROJECT_ITERS by project(). A small initial budget means
            // we validate frequently while ramping up — catches boundary
            // exits (like CX hitting zero in REP STOSB) before we fill
            // far past the end.
            budget: INITIAL_BUDGET,
        };
        true
    }

    fn try_calibrate(&mut self) {
        let len = self.calib_len;
        if len < 3 {
            self.stats.last_reject_reason = "buffer too small";
            return;
        }
        let n_vars = self.n_vars;

        // P=1 fast-path. If every slot evolves with a constant per-tick delta
        // over the calibration window, and at least one memory write occurs
        // every tick with constant addr-stride and constant value, we're in
        // a REP STOSB-style tight fill loop. Lock immediately — no voting
        // needed (voting depends on cyclic absolute-value matches, which
        // don't exist in a pure affine loop).
        //
        // This runs before the general P≥2 voting path because it's much
        // cheaper and the common case for the splash fill.
        if self.try_lock_p1() {
            return;
        }

        // Identify "cyclic" slots: those whose unique value count over the
        // calibration window is small enough that absolute-value
        // periodicity is plausible. Slots with many unique values are
        // "iterating counters" (e.g., cycleCount growing by 148/iter) —
        // we include them only in the per-iter-delta/affinity check, not
        // the fingerprint.
        //
        // Threshold chosen from the spec's data: splash-phase uOp has 1
        // unique value, IP has ~26 unique values; CX/SI/DI have thousands.
        // Anything with ≤ 64 unique values is "cyclic" for our purposes.
        // Threshold is evaluated relative to the window length: a slot is
        // "cyclic" if its unique-value count is ≤ min(32, window/64).
        // We want this to be small enough to exclude linearly-growing
        // counters (which in a window of `len` ticks typically have
        // len/stride unique values — for cycleCount with ~5.7 cycles per
        // tick, that's ~700 unique values in 4096 ticks), but large enough
        // to include dispatchy slots like IP/uOp (≤ period unique values,
        // typically ≤ 32).
        // Clamp: at least 8 unique values always count as cyclic (covers
        // small dispatch spaces like uOp/IP), at most 32 (avoids treating
        // monotonic counters as cyclic).
        let cyclic_unique_threshold: usize = (len / 8).clamp(8, 32);
        let mut cyclic_slots: Vec<usize> = Vec::new();
        for s in 0..n_vars {
            let mut seen: std::collections::HashSet<i32> = std::collections::HashSet::new();
            for sample in &self.calib[..len] {
                seen.insert(sample.vars[s]);
                if seen.len() > cyclic_unique_threshold { break; }
            }
            if seen.len() <= cyclic_unique_threshold {
                cyclic_slots.push(s);
            }
        }

        // Per-slot voting autocorrelation.
        //
        // For each state-var slot s independently, find the period P that
        // maximises the count of `t` where `vars[t][s] - vars[t-1][s] ==
        // vars[t+P][s] - vars[t+P-1][s]`. Then aggregate: each slot casts
        // a vote for its best P (weighted by its match count). The period
        // with the highest total weight wins — provided at least half the
        // slots voted for it and total match rate is acceptable.
        //
        // This is robust to high-entropy affine counters: they *also* have
        // perfectly periodic deltas, but hashing their values together
        // with low-entropy slots would require all slots to have the same
        // periodic structure. Voting per-slot lets unrelated noise (e.g.,
        // a chaotic flag bit) simply vote weakly without breaking detection.

        if cyclic_slots.is_empty() {
            self.stats.last_reject_reason = "no cyclic slots found";
            return;
        }
        self.stats.last_cyclic_count = cyclic_slots.len() as u32;

        // Per-slot PERIOD VOTING on absolute values.
        //
        // For each cyclic slot s, find the smallest period P for which
        // `vars[t][s] == vars[t+P][s]` holds for ≥ 95% of valid t.
        // Slots whose "best periodic P" agrees vote together; slots that
        // are non-periodic (like `flags`) produce no strong signal and
        // are ignored.
        //
        // The final period is the one with the most votes. This is robust
        // to one or two noisy slots without requiring us to hand-identify
        // them.
        let max_p = MAX_PERIOD.min(len / 3);
        let mut period_votes: Vec<u32> = vec![0; max_p + 1];
        // Constant slots don't help discriminate period — ignore them.
        let mut voting_slots: Vec<usize> = Vec::new();
        for &s in &cyclic_slots {
            // Skip truly-constant slots (same value for whole window).
            let first = self.calib[0].vars[s];
            if self.calib[..len].iter().all(|x| x.vars[s] == first) { continue; }
            voting_slots.push(s);

            for p in MIN_PERIOD..=max_p {
                let overlap = len - p;
                let mut matches = 0usize;
                for t in 0..overlap {
                    if self.calib[t].vars[s] == self.calib[t + p].vars[s] {
                        matches += 1;
                    }
                }
                if matches * MATCH_RATE_DEN >= overlap * MATCH_RATE_NUM
                    && matches >= MIN_MATCHES
                {
                    period_votes[p] += 1;
                    break; // record only smallest P that qualifies per slot.
                }
            }
        }

        if std::env::var("CALCITE_TICK_PERIOD_DEBUG").is_ok() {
            eprintln!(
                "[tick_period] calib len={}, cyclic={}, voting={}",
                len, cyclic_slots.len(), voting_slots.len()
            );
            let mut top: Vec<(usize, u32)> = period_votes
                .iter()
                .enumerate()
                .filter(|(_, &v)| v > 0)
                .map(|(p, &v)| (p, v))
                .collect();
            top.sort_by(|a, b| b.1.cmp(&a.1));
            for (p, v) in top.iter().take(5) {
                eprintln!("  P={:>3} votes={}", p, v);
            }
        }

        // Pick winner. Require at least half the voting slots to agree.
        // Require at least half the voting slots to agree, with a
        // minimum of 1. (If only a single slot is dispatchy, it alone
        // determines the period — safe only because the affine-verify
        // step below re-tests the full state over CONFIRM_ITERS+1
        // iterations.)
        let min_votes = ((voting_slots.len() as u32) / 2).max(1);
        let mut winner_p = 0usize;
        let mut winner_votes = 0u32;
        for p in MIN_PERIOD..=max_p {
            if period_votes[p] > winner_votes {
                winner_votes = period_votes[p];
                winner_p = p;
            }
        }
        self.stats.last_best_period = winner_p as u32;
        self.stats.last_best_matches = winner_votes;
        if winner_votes < min_votes {
            self.stats.last_reject_reason = "insufficient vote consensus";
            return;
        }
        let period = winner_p;

        // Anchor at the end of the buffer: take the last possible full
        // iteration that's followed by another full iteration. The
        // affine-verification step below will reject this anchor if the
        // region around it isn't truly P-periodic.
        //
        // We anchor late rather than at the start of a contiguous run so
        // that the locked state corresponds to the most recent observed
        // behaviour — if the workload shifted modes, we want to track the
        // current mode, not an old one.
        let anchor = len.saturating_sub(period * (CONFIRM_ITERS + 2));
        if anchor + period * (CONFIRM_ITERS + 1) > len {
            self.stats.last_reject_reason = "not enough ticks for confirm window";
            return;
        }
        // How many full iterations of the anchor's iteration do we have?
        //   iter 0 spans [anchor .. anchor + period)
        //   iter k spans [anchor + k*period .. anchor + (k+1)*period)
        //   we have iteration k if anchor + (k+1)*period <= len.
        let n_iters = (len - anchor) / period;
        if n_iters < CONFIRM_ITERS + 1 {
            self.stats.last_reject_reason = "too few iterations past anchor";
            return;
        }

        // Base state vars and per-offset base writes from iter 0.
        let base_vars: Box<[i32]> = self.calib[anchor].vars.clone();
        let mut base_writes: Vec<Option<(i32, i32)>> = Vec::with_capacity(period);
        for o in 0..period {
            base_writes.push(self.calib[anchor + o].mem_write);
        }

        // Per-iter-delta from iter 0 → iter 1.
        let iter1 = &self.calib[anchor + period].vars;
        let mut per_iter_delta = vec![0i32; self.n_vars].into_boxed_slice();
        for i in 0..self.n_vars {
            per_iter_delta[i] = iter1[i].wrapping_sub(base_vars[i]);
        }

        // Per-offset write delta from iter 0 → iter 1. None = unknown /
        // non-affine. (0,0) is legal and means "same write both iterations".
        let mut write_delta: Vec<Option<(i32, i32)>> = Vec::with_capacity(period);
        for o in 0..period {
            let w0 = self.calib[anchor + o].mem_write;
            let w1 = self.calib[anchor + period + o].mem_write;
            write_delta.push(match (w0, w1) {
                (Some((a0, v0)), Some((a1, v1))) => {
                    Some((a1.wrapping_sub(a0), v1.wrapping_sub(v0)))
                }
                (None, None) => Some((0, 0)),
                _ => None,
            });
        }

        // Validate affinity over iterations 2..n_iters.
        let mut var_ok = vec![true; self.n_vars];
        let mut write_ok = vec![true; period];
        for k in 2..n_iters {
            let vars_k = &self.calib[anchor + k * period].vars;
            for i in 0..self.n_vars {
                if !var_ok[i] { continue; }
                let expected = base_vars[i].wrapping_add(per_iter_delta[i].wrapping_mul(k as i32));
                if expected != vars_k[i] {
                    var_ok[i] = false;
                }
            }
            for o in 0..period {
                if !write_ok[o] { continue; }
                let wk = self.calib[anchor + k * period + o].mem_write;
                let w0 = base_writes[o];
                let d = write_delta[o];
                match (w0, wk, d) {
                    (Some((a0, v0)), Some((ak, vk)), Some((da, dv))) => {
                        let pred_a = a0.wrapping_add(da.wrapping_mul(k as i32));
                        let pred_v = v0.wrapping_add(dv.wrapping_mul(k as i32));
                        if pred_a != ak || pred_v != vk {
                            write_ok[o] = false;
                        }
                    }
                    (None, None, _) => {}
                    _ => { write_ok[o] = false; }
                }
            }
        }

        // For a non-affine var, reject only if its base delta is non-zero.
        // If delta==0 (the var holds the same value across most iterations,
        // like `flags`), projection multiplies zero by N, leaving it
        // untouched. A flip at the real loop boundary is caught by the
        // post-projection validation iteration.
        //
        // Additionally: for a non-affine var we must verify it actually
        // held the same value across every sampled iteration — otherwise
        // calling it "delta == 0" is a lie. `var_ok[i]` was set to false
        // precisely when some sample broke the affine prediction; since
        // delta came from iter0→iter1, if delta==0 this reduces to "all
        // iters had the same value at slot i as iter 0". The check is
        // implicit in var_ok's definition.
        for i in 0..self.n_vars {
            if !var_ok[i] && per_iter_delta[i] != 0 {
                self.stats.last_reject_reason = "non-affine var with nonzero delta";
                return;
            }
        }
        // Zero out delta for any non-affine slot so projection truly
        // doesn't touch it. (It already is zero if we passed the above
        // gate, but be explicit.)
        for i in 0..self.n_vars {
            if !var_ok[i] {
                per_iter_delta[i] = 0;
            }
        }

        // Build locked writes. Require val_stride == 0 (constant fill)
        // for the memset lowering.
        let mut writes: Vec<LockedWrite> = Vec::new();
        for o in 0..period {
            if !write_ok[o] { continue; }
            let Some((a0, v0)) = base_writes[o] else { continue; };
            let Some((da, dv)) = write_delta[o] else { continue; };
            if dv != 0 { continue; }
            writes.push(LockedWrite {
                offset: o,
                base_addr: a0,
                addr_stride: da,
                value: v0,
            });
        }
        if writes.is_empty() {
            // Nothing to lower. Don't lock.
            self.stats.last_reject_reason = "no affine memset-style writes";
            return;
        }

        // We lock. The anchor frames iteration 0 at calibration-buffer
        // offset `anchor`. But the tracker is about to clear the calib
        // buffer; we need to know the anchor state_vars for future
        // alignment checks. `base_vars` is that.
        //
        // Important: the caller's CURRENT pre-tick state is NOT the anchor
        // state unless the buffer happened to end exactly on an iteration
        // boundary. We reconstruct "current expected vars" from the sample
        // at the end of the buffer.
        //
        // The LAST sample in calib is the pre-tick state of the most
        // recently observed tick. The tick after it has just run (that's
        // where we are now). So the vars at the END of the calib buffer
        // represent the state AT the start of tick `len-1` within the
        // calibration window. Since the caller has since run that tick,
        // their current state is the post-tick state of calib[len-1].
        //
        // That means we can't safely project until we're aligned to an
        // iteration boundary. The caller's `can_project` will check this
        // by comparing current pre-tick vars against
        // `anchor_vars + k*delta` for some integer k.

        self.stats.lock_count += 1;
        self.mode = Mode::Locked {
            period,
            per_iter_delta,
            writes: writes.into_boxed_slice(),
            anchor_vars: base_vars,
            iters_since_lock: 0,
            budget: INITIAL_BUDGET,
        };
    }

    /// Returns `Some(iters_offset)` if `pre_tick_vars` matches
    /// `anchor_vars + k*per_iter_delta` for some non-negative integer k,
    /// else `None`. The caller should only call `project` when this
    /// returns `Some`.
    fn alignment(&self, pre_tick_vars: &[i32]) -> Option<usize> {
        let Mode::Locked { per_iter_delta, anchor_vars, .. } = &self.mode else { return None; };
        if pre_tick_vars.len() != anchor_vars.len() { return None; }

        // Find a slot with non-zero delta — use it to extract k.
        let anchor_slot = per_iter_delta.iter().position(|&d| d != 0)?;
        let d = per_iter_delta[anchor_slot];
        let diff = pre_tick_vars[anchor_slot].wrapping_sub(anchor_vars[anchor_slot]);
        // k must satisfy d * k = diff (i32, potentially wrapping). Prefer
        // exact integer division; reject if inconsistent.
        if d == 0 { return None; }
        // Use i64 to avoid division surprises with i32 wrap.
        let d64 = d as i64;
        let diff64 = diff as i64;
        if diff64 % d64 != 0 { return None; }
        let k64 = diff64 / d64;
        if !(0..=i64::from(i32::MAX)).contains(&k64) { return None; }
        let k = k64 as usize;

        // Verify all other slots.
        for i in 0..self.n_vars {
            let expected = anchor_vars[i]
                .wrapping_add(per_iter_delta[i].wrapping_mul(k as i32));
            if expected != pre_tick_vars[i] {
                return None;
            }
        }
        Some(k)
    }

    pub fn can_project(&self, pre_tick_vars: &[i32]) -> bool {
        self.alignment(pre_tick_vars).is_some()
    }

    /// Project forward. Returns the number of iterations projected.
    pub fn project(&mut self, state: &mut State) -> usize {
        let align = match self.alignment(&state.state_vars) {
            Some(k) => k,
            None => return 0,
        };
        let (period, per_iter_delta, writes, budget, iters_since_lock) = match &self.mode {
            Mode::Locked {
                period, per_iter_delta, writes, budget, iters_since_lock, ..
            } => (
                *period,
                per_iter_delta.clone(),
                writes.clone(),
                *budget,
                *iters_since_lock,
            ),
            _ => return 0,
        };
        let _ = iters_since_lock; // not currently used

        let mut n = budget.min(MAX_PROJECT_ITERS);
        if n == 0 { return 0; }

        // Zero-crossing cap: for any slot with non-zero delta, limit N so the
        // slot doesn't cross zero. If delta is negative and current value is
        // positive, cap at floor(cur / -delta). Symmetric for delta positive
        // and value negative. This is cardinal-rule-safe (just observing that
        // a slot is counting monotonically) and catches REP STOSB-style
        // counter-exhaustion: CX decrements by 1 each iter, we must not
        // project past CX hitting 0.
        for i in 0..self.n_vars {
            let d = per_iter_delta[i];
            if d == 0 { continue; }
            let cur = state.state_vars[i];
            // How many iterations until sign crosses? If same sign or zero,
            // skip. Otherwise cap.
            if d < 0 && cur > 0 {
                // cur + n*d crosses zero when n > cur / -d. Keep strictly
                // positive to leave room for the validation iteration.
                let max = (cur as i64) / (-(d as i64));
                if (max as usize) < n { n = max as usize; }
            } else if d > 0 && cur < 0 {
                let max = (-(cur as i64)) / (d as i64);
                if (max as usize) < n { n = max as usize; }
            }
        }
        if n == 0 { return 0; }

        // Advance state vars.
        for i in 0..self.n_vars {
            let d = per_iter_delta[i];
            if d == 0 { continue; }
            let cur = state.state_vars[i];
            state.state_vars[i] = cur.wrapping_add(d.wrapping_mul(n as i32));
        }

        // Advance memory writes. For each locked write, we need to write
        // at iterations align, align+1, ..., align+n-1.
        for lw in writes.iter() {
            let start = align as i32;
            let count = n as i32;
            if lw.addr_stride == 1 {
                let first = lw.base_addr.wrapping_add(start);
                let byte = (lw.value & 0xFF) as u8;
                let addr_u = first as usize;
                let end = addr_u.wrapping_add(n);
                // Only fast-path when the entire range fits in state.memory
                // and write_log is disabled. If write_log is on we still
                // want correctness; use the slow path.
                if state.write_log.is_none()
                    && addr_u < 0xF0000
                    && end <= state.memory.len()
                    && first >= 0
                {
                    state.memory[addr_u..end].fill(byte);
                    self.stats.bytes_memset += n as u64;
                } else {
                    for i in 0..count {
                        state.write_mem(first.wrapping_add(i), lw.value);
                    }
                }
            } else {
                for i in 0..count {
                    let addr = lw
                        .base_addr
                        .wrapping_add(lw.addr_stride.wrapping_mul(start.wrapping_add(i)));
                    state.write_mem(addr, lw.value);
                }
            }
        }

        state.frame_counter = state.frame_counter.wrapping_add((n * period) as u32);

        if let Mode::Locked { iters_since_lock, .. } = &mut self.mode {
            *iters_since_lock += n;
        }
        self.stats.iters_projected += n as u64;
        self.stats.ticks_projected += (n * period) as u64;

        n
    }

    /// Validate: call this after running one full real iteration
    /// (`period` ticks) following a projection. Pass the pre-iteration
    /// and post-iteration state_vars. Returns `true` if still locked, else
    /// `false` (Cooldown).
    ///
    /// Uses `alignment()` to verify the post-iteration state is achievable
    /// as `anchor + k × delta` for some integer k consistent across all
    /// slots. This is the ground truth of "are we still on the affine
    /// trajectory we locked onto" — it implicitly covers all projected
    /// iterations because each projection just adds `n × delta` and
    /// alignment checks the absolute resulting state against the model.
    ///
    /// Not using a counter like `iters_since_lock` because the anchor may
    /// be an arbitrary number of real iterations before the first
    /// `project()` call (calibration buffer doesn't end on the anchor),
    /// and counting only projected-plus-one misses the real ticks that
    /// happened between anchor and first project.
    pub fn validate_iteration(
        &mut self,
        _pre_iter_vars: &[i32],
        post_iter_vars: &[i32],
    ) -> bool {
        if self.alignment(post_iter_vars).is_some() {
            if let Mode::Locked { iters_since_lock, budget, .. } = &mut self.mode {
                *iters_since_lock += 1;
                *budget = (*budget * 2).min(MAX_PROJECT_ITERS);
            }
            true
        } else {
            if std::env::var("CALCITE_TICK_PERIOD_DEBUG").is_ok() {
                if let Mode::Locked { per_iter_delta, anchor_vars, .. } = &self.mode {
                    // Find a slot with nonzero delta and report the discrepancy.
                    for i in 0..post_iter_vars.len().min(anchor_vars.len()) {
                        if per_iter_delta[i] != 0 {
                            let diff = post_iter_vars[i].wrapping_sub(anchor_vars[i]);
                            let d = per_iter_delta[i];
                            let k = if d != 0 { diff as i64 / d as i64 } else { 0 };
                            let expected =
                                anchor_vars[i].wrapping_add(d.wrapping_mul(k as i32));
                            if expected != post_iter_vars[i] {
                                eprintln!(
                                    "[tick_period] miss at slot {}: anchor={}, delta={}, actual={}, diff={}, k={}",
                                    i, anchor_vars[i], d, post_iter_vars[i], diff, k
                                );
                                break;
                            }
                        }
                    }
                }
            }
            self.stats.miss_count += 1;
            self.mode = Mode::Cooldown { remaining: 64 };
            false
        }
    }

    /// Force-exit Locked mode (e.g., if the caller decides to stop
    /// projecting for external reasons).
    pub fn unlock(&mut self) {
        if matches!(self.mode, Mode::Locked { .. }) {
            self.mode = Mode::Cooldown { remaining: CALIB_LEN };
        }
    }

    pub fn is_locked(&self) -> bool {
        matches!(self.mode, Mode::Locked { .. })
    }

    pub fn locked_period(&self) -> Option<usize> {
        match &self.mode {
            Mode::Locked { period, .. } => Some(*period),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_state(n_vars: usize) -> State {
        let mut state = State::default();
        for i in 0..n_vars {
            state.state_vars.push(0);
            let name = format!("V{}", i);
            state.state_var_index.insert(name.clone(), i);
            state.state_var_names.push(name);
        }
        state
    }

    #[test]
    fn locks_on_clean_period_3_affine() {
        // Workload: period 3, V0 = t % 3 (dispatchy: cycles through 0,1,2),
        // V1 = t / 3 (affine counter, +1 per iteration). Memory write at
        // offset 2 with stride +1 and constant value 0xAA.
        // The detector uses V0 (dispatchy) to identify the period, then
        // verifies V1's delta and the memory write's affine evolution.
        let mut state = make_state(2);
        state.memory.resize(10_000, 0);
        let mut tracker = PeriodTracker::new(&state);
        for t in 0..(CALIB_LEN + 100) {
            let k = (t / 3) as i32;
            let pre = [(t % 3) as i32, k];
            let mw = if t % 3 == 2 { Some((100 + k, 0xAA)) } else { None };
            tracker.observe(&pre, mw);
        }
        assert_eq!(tracker.locked_period(), Some(3), "expected period-3 lock");
    }

    #[test]
    fn refuses_to_lock_on_chaotic_input() {
        let mut state = make_state(1);
        let mut tracker = PeriodTracker::new(&state);
        let _ = &mut state;
        for t in 0..(CALIB_LEN + 100) {
            // pseudo-random hash
            let mut h = (t as u64).wrapping_mul(0x9e3779b97f4a7c15);
            h ^= h >> 32;
            let pre = [h as i32];
            tracker.observe(&pre, None);
        }
        assert!(tracker.locked_period().is_none());
    }

    #[test]
    fn alignment_finds_integer_k() {
        let mut state = make_state(2);
        state.memory.resize(10_000, 0);
        let mut tracker = PeriodTracker::new(&state);
        for t in 0..(CALIB_LEN + 100) {
            let k = (t / 3) as i32;
            let pre = [(t % 3) as i32, k];
            let mw = if t % 3 == 2 { Some((100 + k, 0xAA)) } else { None };
            tracker.observe(&pre, mw);
        }
        assert!(tracker.is_locked());
        let (a0, a1) = match &tracker.mode {
            Mode::Locked { anchor_vars, .. } => (anchor_vars[0], anchor_vars[1]),
            _ => unreachable!(),
        };
        // V0 = anchor_V0 (offset is determined at lock time — we don't
        // know in advance whether the anchor fell on offset 0 of the
        // iteration).
        let test_pre = [a0, a1 + 42];
        assert!(tracker.can_project(&test_pre));
        // Wrong offset → reject.
        let test_bad = [a0 + 1, a1 + 42];
        assert!(!tracker.can_project(&test_bad));
    }

    #[test]
    fn project_advances_state_vars_and_fills_memory() {
        let mut state = make_state(2);
        state.memory.resize(10_000, 0);
        let mut tracker = PeriodTracker::new(&state);
        for t in 0..(CALIB_LEN + 100) {
            let k = (t / 3) as i32;
            let pre = [(t % 3) as i32, k];
            let mw = if t % 3 == 2 { Some((100 + k, 0xAA)) } else { None };
            tracker.observe(&pre, mw);
        }
        assert!(tracker.is_locked());
        let (a0, a1) = match &tracker.mode {
            Mode::Locked { anchor_vars, .. } => (anchor_vars[0], anchor_vars[1]),
            _ => unreachable!(),
        };
        state.state_vars[0] = a0;
        state.state_vars[1] = a1;
        let projected = tracker.project(&mut state);
        assert!(projected > 0);
        // V1 (the affine counter) should have advanced by `projected`.
        assert_eq!(state.state_vars[1], a1 + projected as i32);
        // V0 stays at anchor's offset within the iteration (delta==0).
        assert_eq!(state.state_vars[0], a0);
        let _ = a0;
        // Memory from (100+a1) to (100+a1+projected-1) should all be 0xAA.
        for i in 0..projected {
            let addr = (100 + a1) as usize + i;
            assert_eq!(state.memory[addr], 0xAA, "pixel {} at {} not filled", i, addr);
        }
    }
}
