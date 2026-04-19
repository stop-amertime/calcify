//! Structural cycle detection from per-tick write signatures.
//!
//! # The idea
//!
//! A CSS tick that does "the same work" as another CSS tick — writing to
//! the same set of state-var slots, making the same structural pattern of
//! memory writes — has the same STRUCTURAL signature, even if the concrete
//! values differ. Two ticks that are structurally identical did the same
//! mechanical work; they're iterations of the same loop body.
//!
//! Per tick we hash:
//!   (a) the sorted list of state-var slot indices written this tick, and
//!   (b) the sequence of memory-write offsets RELATIVE to the first write
//!       address — if the first write is at address `A` and subsequent writes
//!       are at `A+1, A+5, A+1`, the signature is `[0, 1, 5, 1]`. The absolute
//!       address is captured separately so we can recover the per-iter stride.
//!
//! # Detection
//!
//! A period-P cyclic loop produces a signature stream where `sig[t] ==
//! sig[t+P]` for all `t`. A last-seen-at map gives us a period candidate in
//! O(1) per tick: when we observe a sig whose hash we've seen before at tick
//! `t0`, the candidate period is `t - t0`. We then confirm by requiring
//! `CONFIRM_CYCLES` consecutive cycles to match before locking.
//!
//! This replaces the old `tick_period` autocorrelation approach. Benefits:
//!  - Detects the cycle in O(N × P) rather than O(N²); in practice ~10 ticks
//!    rather than 131K.
//!  - No false locks on "5 consecutive samples that happen to be affine" —
//!    the signature check is exact structural equality.
//!  - No tuning knobs (CALIB_LEN, MIN_MATCHES, MATCH_RATE_NUM): the detection
//!    is deterministic given the signature stream.
//!
//! # Cardinal-rule safety
//!
//! Reads only the write_log (addresses written, and which state slots were
//! updated — derived from comparing pre/post tick state_vars). No x86
//! knowledge. Same shape of reasoning as `pattern::dispatch_table` — we're
//! inducing structure from the evaluator's output, not from opcode awareness.

use std::collections::HashMap;

/// How many consecutive matching cycles before we lock. Must be ≥ 3 so
/// that `build_projection_state` has enough history to verify the affine
/// model against a third snapshot (otherwise harmonics of longer periods
/// can pass structural-sig matching at stride P while violating the value-
/// level affine law we need for projection).
const CONFIRM_CYCLES: usize = 3;

/// Maximum period we'll detect.
const MAX_PERIOD: usize = 64;

/// Per-tick structural signature.
#[derive(Clone, Copy, Debug)]
pub struct TickSig {
    /// Hash of (state-slot-write-set, relative-mem-write-offsets).
    pub hash: u64,
    /// First memory-write address this tick, if any. Used to compute the
    /// per-iteration address stride once we lock.
    pub first_mem_addr: Option<i32>,
    /// Number of memory writes this tick. Used to derive the total fill
    /// volume per cycle.
    pub mem_write_count: u16,
    /// Value written by the first memory write (if any). We track this so
    /// the lock can detect "constant fill value" cycles.
    pub first_mem_value: Option<i32>,
}

#[derive(Clone, Debug)]
enum Mode {
    /// Collecting signatures; no period candidate yet.
    Hunting,
    /// Saw a repeat at stride P; waiting for `remaining` more full cycles
    /// to match before locking. `start_tick` is the tick index at which the
    /// current candidate period was first detected (so we can anchor).
    Candidate {
        period: usize,
        start_tick: u64,
        confirmed_cycles: usize,
    },
    /// Locked. Holds the anchor signature and the derived affine model.
    Locked {
        period: usize,
        /// Per-cycle stride of the first memory write. +2 for a 2-byte/cycle
        /// fill, +1 for byte fills, etc.
        addr_stride_per_cycle: i32,
        /// Number of memory writes per cycle.
        writes_per_cycle: u16,
        /// Value of the (constant) fill byte, if all writes in the cycle
        /// carry the same value.
        fill_value: Option<i32>,
        /// The tick at which the lock was established.
        lock_tick: u64,
        /// Per-slot delta across one full cycle (period ticks). Used to
        /// advance state_vars during projection.
        per_cycle_delta: Vec<i32>,
        /// Pre-tick state_vars at the START of the cycle just prior to lock
        /// (i.e., back = period ticks from lock moment). Serves as the
        /// anchor for alignment and projection math.
        anchor_vars: Vec<i32>,
        /// Address offsets within one cycle for every memory write, relative
        /// to the first mem-write address of the cycle. Used to replay the
        /// correct write pattern for each projected iteration. Example: a
        /// STOSB-style 2-byte-per-cycle fill has write_offsets = [0, 1].
        write_offsets: Vec<i32>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct CycleStats {
    pub ticks_observed: u64,
    pub hunting_ticks: u64,
    pub candidate_ticks: u64,
    pub locked_ticks: u64,
    /// Tick at which Candidate was first entered.
    pub first_candidate_tick: u64,
    /// Tick at which Lock was first established.
    pub first_lock_tick: u64,
    pub detected_period: u32,
    pub project_calls: u64,
    pub project_success: u64,
    pub project_zero_alignment: u64,
    pub iters_projected: u64,
    pub validate_fails: u64,
}

pub struct CycleTracker {
    mode: Mode,
    tick: u64,
    /// Most recently seen signature hash → tick index. Cleared on period
    /// lock to save memory (once locked we don't hunt anymore).
    last_seen: HashMap<u64, u64>,
    /// Ring of recent signatures, capped at MAX_PERIOD. Used to verify a
    /// candidate period by checking sig equality at stride P across the
    /// confirmation window.
    sig_ring: Vec<TickSig>,
    /// Ring of recent pre-tick state_vars snapshots (parallel to sig_ring).
    /// Capacity `2 * MAX_PERIOD` so that at lock time we can reach back two
    /// full periods to compute `per_cycle_delta`. Each entry is a cached
    /// Vec that we overwrite via `copy_from_slice` — no allocation in the
    /// hot path after warmup.
    pre_vars_ring: Vec<Vec<i32>>,
    /// Parallel ring of all mem writes for each observed tick (used at lock
    /// time to recover the full write-offset list within one cycle).
    mem_writes_ring: Vec<Vec<(i32, i32)>>,
    ring_head: usize, // next write index (mod RING_LEN)
    ring_len: usize,  // number of valid entries (min with RING_LEN)
    ring_capacity: usize, // = 2 * MAX_PERIOD once initialised
    n_vars: usize,
    pub stats: CycleStats,
}

/// Ring length = two full periods, so build_lock_info can reach back 2P.
const RING_LEN: usize = 2 * MAX_PERIOD;

impl Default for CycleTracker {
    fn default() -> Self { Self::new() }
}

impl CycleTracker {
    pub fn new() -> Self {
        Self::with_n_vars(0)
    }

    pub fn with_n_vars(n_vars: usize) -> Self {
        Self {
            mode: Mode::Hunting,
            tick: 0,
            last_seen: HashMap::with_capacity(256),
            sig_ring: vec![
                TickSig { hash: 0, first_mem_addr: None, mem_write_count: 0, first_mem_value: None };
                RING_LEN
            ],
            pre_vars_ring: (0..RING_LEN).map(|_| vec![0i32; n_vars]).collect(),
            mem_writes_ring: (0..RING_LEN).map(|_| Vec::with_capacity(8)).collect(),
            ring_head: 0,
            ring_len: 0,
            ring_capacity: RING_LEN,
            n_vars,
            stats: CycleStats::default(),
        }
    }

    /// Observe one tick. `pre_vars` is the state_vars snapshot BEFORE the
    /// tick ran; `write_log` is the ordered list of `(addr, value)` writes
    /// this tick produced (both memory writes with `addr >= 0` and state-var
    /// writes with `addr < 0`).
    ///
    /// Returns `Some(LockInfo)` on the tick we transition into Locked mode;
    /// `None` otherwise.
    pub fn observe(
        &mut self,
        pre_vars: &[i32],
        post_vars: &[i32],
        write_log: &[(i32, i32)],
    ) -> Option<LockInfo> {
        self.tick += 1;
        self.stats.ticks_observed += 1;

        let sig = make_sig(pre_vars, post_vars, write_log);

        // Store sig + pre_vars + mem writes into the ring.
        let slot = self.ring_head;
        self.sig_ring[slot] = sig;
        // Lazy resize if n_vars wasn't set at construction.
        if self.pre_vars_ring[slot].len() != pre_vars.len() {
            self.pre_vars_ring[slot].resize(pre_vars.len(), 0);
        }
        self.pre_vars_ring[slot].copy_from_slice(pre_vars);
        self.mem_writes_ring[slot].clear();
        self.mem_writes_ring[slot].extend(write_log.iter().filter(|&&(a, _)| a >= 0).copied());
        self.ring_head = (self.ring_head + 1) % self.ring_capacity;
        if self.ring_len < self.ring_capacity { self.ring_len += 1; }

        match self.mode.clone() {
            Mode::Hunting => {
                self.stats.hunting_ticks += 1;
                // Check if we've seen this hash before within MAX_PERIOD ticks.
                if let Some(&prev_tick) = self.last_seen.get(&sig.hash) {
                    let period = (self.tick - prev_tick) as usize;
                    if (1..=MAX_PERIOD).contains(&period) {
                        // Candidate found.
                        self.mode = Mode::Candidate {
                            period,
                            start_tick: prev_tick,
                            confirmed_cycles: 0,
                        };
                        if self.stats.first_candidate_tick == 0 {
                            self.stats.first_candidate_tick = self.tick;
                        }
                    }
                }
                self.last_seen.insert(sig.hash, self.tick);
                None
            }
            Mode::Candidate { period, start_tick, confirmed_cycles } => {
                self.stats.candidate_ticks += 1;
                // Check that sig[tick] matches sig[tick - period].
                let prior = self.sig_at_offset_back(period);
                if prior.map(|p| p.hash == sig.hash).unwrap_or(false) {
                    let new_confirmed = confirmed_cycles + 1;
                    if new_confirmed >= CONFIRM_CYCLES * period {
                        // Candidate has been stable — try to lock. If the
                        // pattern has no memory writes per cycle, it's an
                        // idle/no-op loop; projecting it forward is legal
                        // but useless (zero speedup, adds overhead). Stay
                        // in Candidate until something changes.
                        let info = self.build_lock_info(period, start_tick);
                        if info.writes_per_cycle == 0 || info.addr_stride_per_cycle == 0 {
                            // Nothing to project. Stay in Candidate and keep
                            // counting — if the pattern holds, we just wait
                            // for something more interesting. Cap the counter
                            // so it doesn't overflow.
                            self.mode = Mode::Candidate {
                                period,
                                start_tick,
                                confirmed_cycles: new_confirmed.min(1 << 30),
                            };
                            None
                        } else {
                            // Compute per-cycle delta from the two most
                            // recent periods of pre_vars, and capture anchor
                            // vars + write offsets.
                            let (per_cycle_delta, anchor_vars, write_offsets) =
                                self.build_projection_state(period);
                            // Verify the affine model fits. If the detected
                            // "period" is actually a harmonic of a longer
                            // real period, per_cycle_delta computed from
                            // two snapshots won't hold for a third snapshot.
                            // Check against a sample at back=3*period-1.
                            if !self.verify_affine_across_three(period, &per_cycle_delta) {
                                // False harmonic. Give up on this candidate;
                                // keep hunting. Subsequent observations may
                                // find the true period.
                                self.mode = Mode::Hunting;
                                self.last_seen.clear();
                                self.last_seen.insert(sig.hash, self.tick);
                                return None;
                            }
                            self.mode = Mode::Locked {
                                period,
                                addr_stride_per_cycle: info.addr_stride_per_cycle,
                                writes_per_cycle: info.writes_per_cycle,
                                fill_value: info.fill_value,
                                lock_tick: self.tick,
                                per_cycle_delta,
                                anchor_vars,
                                write_offsets,
                            };
                            if self.stats.first_lock_tick == 0 {
                                self.stats.first_lock_tick = self.tick;
                            }
                            self.stats.detected_period = period as u32;
                            // Free the last_seen map; we don't need it anymore.
                            self.last_seen.clear();
                            self.last_seen.shrink_to(0);
                            Some(info)
                        }
                    } else {
                        self.mode = Mode::Candidate {
                            period,
                            start_tick,
                            confirmed_cycles: new_confirmed,
                        };
                        None
                    }
                } else {
                    // Mismatch — bust the candidate. Go back to hunting from
                    // scratch (clear last_seen so stale entries don't
                    // immediately re-trigger the same bogus period).
                    self.mode = Mode::Hunting;
                    self.last_seen.clear();
                    self.last_seen.insert(sig.hash, self.tick);
                    None
                }
            }
            Mode::Locked { .. } => {
                self.stats.locked_ticks += 1;
                None
            }
        }
    }

    /// Verify that `per_cycle_delta` holds across a THIRD observation, not
    /// just the two we used to derive it. This rejects "structural harmonics"
    /// where the write-signature pattern repeats at sub-period P but the
    /// values cycle on a longer real period (e.g., 2P or 4P).
    ///
    /// Check: pre_vars[back=period-1] - pre_vars[back=3*period-1] == 2*delta.
    /// If yes: delta is consistent across 2 cycles. If no: bogus harmonic.
    fn verify_affine_across_three(&self, period: usize, delta: &[i32]) -> bool {
        let newest_back = period - 1;
        let oldest_back = 3 * period - 1;
        if oldest_back >= self.ring_len { return false; }
        let newest_slot = (self.ring_head + self.ring_capacity - 1 - newest_back)
            % self.ring_capacity;
        let oldest_slot = (self.ring_head + self.ring_capacity - 1 - oldest_back)
            % self.ring_capacity;
        let newest = &self.pre_vars_ring[newest_slot];
        let oldest = &self.pre_vars_ring[oldest_slot];
        if newest.len() != oldest.len() || newest.len() != delta.len() { return false; }
        for i in 0..delta.len() {
            let expected = oldest[i].wrapping_add(delta[i].wrapping_mul(2));
            if expected != newest[i] { return false; }
        }
        true
    }

    /// Look up the signature at `back` ticks ago, where `back=0` is the
    /// just-inserted sig (most recent) and `back=k` is `k` inserts older.
    ///
    /// After insertion, `ring_head` points one past the last-written slot,
    /// so the most recent sig is at `(ring_head - 1) mod N`. `back=k` older
    /// is at `(ring_head - 1 - k) mod N`.
    fn sig_at_offset_back(&self, back: usize) -> Option<TickSig> {
        if back >= self.ring_len { return None; }
        let idx = (self.ring_head + MAX_PERIOD - 1 - back) % MAX_PERIOD;
        Some(self.sig_ring[idx])
    }

    /// When locking, derive the per-cycle affine law from the last `period`
    /// signatures in the ring.
    fn build_lock_info(&self, period: usize, _start_tick: u64) -> LockInfo {
        // Find the offset-within-cycle that has a memory write (if any).
        // For a REP STOSB fill the typical shape is:
        //   tick 0 of cycle: writes at addresses A, A+1
        //   ticks 1..period-1: no memory writes
        // So first_mem_addr of cycle offset 0 advances by `addr_stride_per_cycle`
        // each cycle.
        //
        // We have two full cycles in the ring now (just-locked tick is the
        // last sample). Compare cycle[-2*period..-period] against
        // cycle[-period..]: for each offset-within-cycle `o`, the first-mem-addr
        // delta gives the per-cycle stride.
        //
        // In practice most offsets have no mem write; we take the first
        // offset that has non-None first_mem_addr in both cycles.
        let mut addr_stride_per_cycle = 0i32;
        let mut writes_per_cycle: u16 = 0;
        let mut fill_value: Option<i32> = None;
        let mut saw_fill_value = false;
        let mut all_same_value = true;

        for o in 0..period {
            // "Older" sample: offset o of cycle-just-before-just-completed.
            // "Newer" sample: offset o of just-completed cycle.
            //
            // Using the back=0-is-most-recent convention: the newest sig
            // has back=0, so offset o of the just-completed cycle is at
            // back = (period - 1 - o). The same offset in the prior cycle
            // is one full period older: back = (2*period - 1 - o).
            let older = self.sig_at_offset_back(2 * period - 1 - o);
            let newer = self.sig_at_offset_back(period - 1 - o);
            if let (Some(a), Some(b)) = (older, newer) {
                writes_per_cycle = writes_per_cycle.saturating_add(b.mem_write_count);
                if let (Some(ao), Some(bo)) = (a.first_mem_addr, b.first_mem_addr) {
                    if addr_stride_per_cycle == 0 {
                        addr_stride_per_cycle = bo.wrapping_sub(ao);
                    }
                    // Track fill value consistency.
                    if let (Some(av), Some(bv)) = (a.first_mem_value, b.first_mem_value) {
                        if !saw_fill_value {
                            fill_value = Some(bv);
                            saw_fill_value = true;
                        } else if Some(bv) != fill_value || Some(av) != fill_value {
                            all_same_value = false;
                        }
                    } else {
                        all_same_value = false;
                    }
                }
            }
        }
        if !all_same_value {
            fill_value = None;
        }
        if std::env::var("CALCITE_CYCLE_DEBUG").is_ok() {
            eprintln!(
                "[build_lock_info] period={} stride={} writes/cycle={} fill={:?}",
                period, addr_stride_per_cycle, writes_per_cycle, fill_value
            );
        }
        LockInfo {
            period,
            addr_stride_per_cycle,
            writes_per_cycle,
            fill_value,
        }
    }

    pub fn is_locked(&self) -> bool {
        matches!(self.mode, Mode::Locked { .. })
    }

    pub fn locked_period(&self) -> Option<usize> {
        if let Mode::Locked { period, .. } = self.mode { Some(period) } else { None }
    }

    /// True if we're locked AND `state_vars` currently aligns to a cycle
    /// boundary. Only when this returns true is it worthwhile to call
    /// `project()` — otherwise alignment_of will return None and project
    /// is a no-op.
    pub fn can_project(&self, state_vars: &[i32]) -> bool {
        self.is_locked() && self.alignment_of(state_vars).is_some()
    }

    /// Validate that, after running one real iteration (period ticks)
    /// following a project() call, the resulting state_vars are still
    /// consistent with our affine model. Specifically: state should equal
    /// `pre_check + per_cycle_delta` (advance by one cycle).
    pub fn validate_after_real_iter(&self, pre_check: &[i32], post: &[i32]) -> bool {
        let Mode::Locked { per_cycle_delta, .. } = &self.mode else { return false; };
        if pre_check.len() != post.len() || pre_check.len() < per_cycle_delta.len() {
            return false;
        }
        for (i, &d) in per_cycle_delta.iter().enumerate() {
            let expected = pre_check[i].wrapping_add(d);
            if expected != post[i] { return false; }
        }
        true
    }

    pub fn locked_info(&self) -> Option<LockInfo> {
        if let Mode::Locked {
            period,
            addr_stride_per_cycle,
            writes_per_cycle,
            fill_value,
            ..
        } = self.mode
        {
            Some(LockInfo {
                period,
                addr_stride_per_cycle,
                writes_per_cycle,
                fill_value,
            })
        } else {
            None
        }
    }

    /// Reset to hunting mode. Called after a projection miss so we can
    /// re-establish a lock (the workload may have shifted regime).
    pub fn unlock(&mut self) {
        self.mode = Mode::Hunting;
        self.last_seen.clear();
        self.ring_len = 0;
        self.ring_head = 0;
    }

    /// At lock time, extract from the ring:
    ///   - per_cycle_delta: the change in each state_var slot across one
    ///     full cycle of `period` ticks.
    ///   - anchor_vars: pre_vars at the START of the most-recent-but-one
    ///     cycle (back = 2*period - 1). Paired with per_cycle_delta, the
    ///     formula `anchor + k*delta` gives the pre-state at the start of
    ///     cycle `k+1` from the anchor — directly usable when projecting.
    ///   - write_offsets: addresses written within one cycle, relative to
    ///     the first memory-write address of that cycle.
    fn build_projection_state(&self, period: usize) -> (Vec<i32>, Vec<i32>, Vec<i32>) {
        // Two same-phase snapshots: back=period and back=2*period are both
        // `period` ticks apart and therefore at the same phase within the
        // cycle. Their difference IS per_cycle_delta (one cycle elapsed).
        //
        // Key insight: the "current state_vars" (just after the
        // just-completed tick) is at back=-1, which has the same phase as
        // back=(-1 + period) = back=period-1 — NOT back=0. To make the
        // post-lock alignment check work without tricky phase math, we
        // pick the anchor at the SAME phase as current state_vars.
        //
        // Current state_vars ≡ pre_vars of the NEXT tick (tick T+1 where
        // T is the just-observed tick). Phase of T+1 = phase of (T+1 - period)
        // = phase of (T - period + 1). Observe call inserted sigs for ticks
        // T, T-1, ..., T-RING_LEN+1 at back=0,1,...,RING_LEN-1. So "tick
        // T-period+1" is at back = period - 1.
        //
        // Therefore: current state_vars has the same phase as pre_vars at
        // back = period - 1. BUT that's a pre-state, not a post-state —
        // the pre-state of tick T-period+1 is what we observed at back=period-1
        // slot of the ring.
        //
        // Hmm actually, think of it this way: after observe() for tick T,
        // the pre_vars_ring[back=period-1] contains the pre-state of tick
        // T-period+1 (i.e., before that tick ran). And current state_vars
        // is the pre-state of tick T+1. These two pre-states are
        // (T+1) - (T-period+1) = period ticks apart, so differ by exactly
        // one per_cycle_delta.
        //
        // Anchor = pre_vars at back=period-1. Older = pre_vars at
        // back=2*period-1. Both at the same phase. Delta = Anchor − Older.
        let anchor_back = period - 1;
        let older_back = 2 * period - 1;
        let anchor_slot = (self.ring_head + self.ring_capacity - 1 - anchor_back)
            % self.ring_capacity;
        let older_slot = (self.ring_head + self.ring_capacity - 1 - older_back)
            % self.ring_capacity;

        let anchor = &self.pre_vars_ring[anchor_slot];
        let older = &self.pre_vars_ring[older_slot];
        let n = anchor.len().min(older.len());
        let mut delta = vec![0i32; n];
        for i in 0..n {
            delta[i] = anchor[i].wrapping_sub(older[i]);
        }

        // Anchor: pre_vars at back=period-1. Current state_vars (right
        // after lock) equals anchor + per_cycle_delta (one cycle ahead).
        // Alignment will solve for k=1 on the first check, then k=2, etc.
        let anchor_vars = anchor.clone();

        // Gather writes across one full cycle: offset o in [0, period)
        // corresponds to back = period - 1 - o (newest cycle). The "first"
        // write address for the cycle is the first write we encounter in
        // offset order.
        let mut first_mem_addr: Option<i32> = None;
        let mut write_offsets: Vec<i32> = Vec::new();
        for o in 0..period {
            let back = period - 1 - o;
            let slot = (self.ring_head + self.ring_capacity - 1 - back)
                % self.ring_capacity;
            for &(addr, _val) in &self.mem_writes_ring[slot] {
                if addr < 0 { continue; }
                if first_mem_addr.is_none() {
                    first_mem_addr = Some(addr);
                }
                let offset = addr.wrapping_sub(first_mem_addr.unwrap());
                write_offsets.push(offset);
            }
        }

        (delta, anchor_vars, write_offsets)
    }

    /// Project forward `n` iterations, advancing state_vars and filling
    /// memory. Caller is responsible for enforcing any invariants (e.g.,
    /// counter not going negative) — we honour `n` as given.
    ///
    /// Returns the number of iterations actually projected (may be less
    /// than requested if no lock is active).
    pub fn project(
        &mut self,
        state: &mut crate::state::State,
        n: usize,
    ) -> usize {
        self.stats.project_calls += 1;
        let (addr_stride_per_cycle, fill_value, per_cycle_delta, write_offsets) =
            match &self.mode {
                Mode::Locked {
                    addr_stride_per_cycle,
                    fill_value,
                    per_cycle_delta,
                    write_offsets,
                    ..
                } => (
                    *addr_stride_per_cycle,
                    *fill_value,
                    per_cycle_delta.clone(),
                    write_offsets.clone(),
                ),
                _ => return 0,
            };
        if n == 0 { return 0; }

        // Find the current alignment: how many cycles forward from anchor
        // are we? If state_vars don't match `anchor + k*per_cycle_delta`
        // for any non-negative integer k, we're misaligned — return 0.
        let k = match self.alignment_of(&state.state_vars) {
            Some(k) => k,
            None => {
                self.stats.project_zero_alignment += 1;
                return 0;
            },
        };

        // Bound `n` by zero-crossing on any non-zero-delta slot.
        let mut n = n;
        for (i, &d) in per_cycle_delta.iter().enumerate() {
            if d == 0 || i >= state.state_vars.len() { continue; }
            let cur = state.state_vars[i];
            if d < 0 && cur > 0 {
                let max = (cur as i64) / (-(d as i64));
                if (max as usize) < n { n = max as usize; }
            } else if d > 0 && cur < 0 {
                let max = (-(cur as i64)) / (d as i64);
                if (max as usize) < n { n = max as usize; }
            }
        }
        if n == 0 { return 0; }

        // Advance state_vars by n * per_cycle_delta.
        for (i, &d) in per_cycle_delta.iter().enumerate() {
            if d == 0 || i >= state.state_vars.len() { continue; }
            let cur = state.state_vars[i];
            state.state_vars[i] = cur.wrapping_add(d.wrapping_mul(n as i32));
        }

        // Fill memory. The first memory write of cycle k (after lock) sits
        // at base + k*addr_stride, where base was captured as the first
        // write of the anchor cycle. We recover `base` from the current
        // state: on entry, the NEXT cycle's first write will be at
        // base + (k+1)*addr_stride. But we already have `k` from alignment
        // and the lock gives us the stride.
        //
        // Simpler: the lock captured `write_offsets` within one cycle
        // relative to that cycle's first mem address. For every projected
        // iteration i in [0, n), the first write of iteration (k+1+i) is at
        // first_of_anchor + (k+1+i)*stride. We need first_of_anchor — take
        // it from the ring's anchor cycle.
        if !write_offsets.is_empty() && fill_value.is_some() {
            let first_of_anchor = self.first_write_addr_of_anchor_cycle();
            if let Some(base) = first_of_anchor {
                let stride = addr_stride_per_cycle;
                let val = fill_value.unwrap() & 0xFF;
                for iter in 0..n {
                    let cycle_first = base
                        .wrapping_add(stride.wrapping_mul((k + 1 + iter) as i32));
                    for &off in &write_offsets {
                        let addr = cycle_first.wrapping_add(off);
                        if addr >= 0 && (addr as usize) < state.memory.len() {
                            state.memory[addr as usize] = val as u8;
                        }
                    }
                }
            }
        }

        self.stats.project_success += 1;
        self.stats.iters_projected += n as u64;
        n
    }

    /// Compute `k` such that state_vars == anchor_vars + k * per_cycle_delta,
    /// or None if no consistent integer k exists.
    fn alignment_of(&self, state_vars: &[i32]) -> Option<usize> {
        let Mode::Locked { per_cycle_delta, anchor_vars, .. } = &self.mode else { return None; };
        if state_vars.len() != anchor_vars.len() { return None; }
        // Pick first slot with non-zero delta to solve for k.
        let anchor_slot = per_cycle_delta.iter().position(|&d| d != 0)?;
        let d = per_cycle_delta[anchor_slot] as i64;
        let diff = (state_vars[anchor_slot] - anchor_vars[anchor_slot]) as i64;
        if d == 0 || diff % d != 0 {
            if std::env::var("CALCITE_CYCLE_DEBUG").is_ok() {
                eprintln!("[align] anchor_slot={} d={} diff={} → not divisible", anchor_slot, d, diff);
            }
            return None;
        }
        let k = diff / d;
        if k < 0 {
            if std::env::var("CALCITE_CYCLE_DEBUG").is_ok() {
                eprintln!("[align] k={} < 0", k);
            }
            return None;
        }
        // Verify consistency across all other slots.
        for (i, &di) in per_cycle_delta.iter().enumerate() {
            let expected = anchor_vars[i].wrapping_add((di as i64 * k) as i32);
            if expected != state_vars[i] {
                if std::env::var("CALCITE_CYCLE_DEBUG").is_ok() {
                    eprintln!(
                        "[align] k={} slot {}: anchor={} delta={} expected={} actual={}",
                        k, i, anchor_vars[i], di, expected, state_vars[i]
                    );
                }
                return None;
            }
        }
        Some(k as usize)
    }

    /// Look up the absolute first-memory-write address of the anchor cycle
    /// by scanning the oldest of the two rings-worth of mem_writes at the
    /// appropriate offsets.
    fn first_write_addr_of_anchor_cycle(&self) -> Option<i32> {
        let Mode::Locked { period, .. } = &self.mode else { return None; };
        // Anchor cycle starts at back = period - 1 and ends at back = 0.
        // We scan the mem writes at each offset in order and return the
        // first one we find.
        for o in 0..*period {
            let back = period - 1 - o;
            let slot = (self.ring_head + self.ring_capacity - 1 - back)
                % self.ring_capacity;
            for &(addr, _val) in &self.mem_writes_ring[slot] {
                if addr >= 0 { return Some(addr); }
            }
        }
        None
    }
}

#[derive(Clone, Copy, Debug)]
pub struct LockInfo {
    pub period: usize,
    pub addr_stride_per_cycle: i32,
    pub writes_per_cycle: u16,
    pub fill_value: Option<i32>,
}

/// Build a TickSig from a pre/post state_vars snapshot and a write_log.
///
/// - `pre_vars`, `post_vars`: state_vars before and after the tick. Slots
///   that differ are the ones the tick wrote.
/// - `write_log`: all `(addr, value)` pairs from this tick. `addr >= 0`
///   means a memory address; `addr < 0` means a state-var slot (and we
///   already have that info from pre/post, so we ignore it here).
pub fn make_sig(pre_vars: &[i32], post_vars: &[i32], write_log: &[(i32, i32)]) -> TickSig {
    // FNV-1a-style running hash.
    let mut h: u64 = 0xcbf29ce484222325;

    // Include which state-var slots changed. Since we iterate them in
    // slot-index order, the hash captures set membership deterministically.
    let n = pre_vars.len().min(post_vars.len());
    for i in 0..n {
        if pre_vars[i] != post_vars[i] {
            h ^= i as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }

    // Collect mem writes (addr >= 0 only).
    let mut first_mem_addr: Option<i32> = None;
    let mut first_mem_value: Option<i32> = None;
    let mut mem_write_count: u16 = 0;
    for &(addr, val) in write_log {
        if addr < 0 { continue; }
        if first_mem_addr.is_none() {
            first_mem_addr = Some(addr);
            first_mem_value = Some(val);
        }
        // Relative offset from first write address.
        let rel = addr.wrapping_sub(first_mem_addr.unwrap()) as u64;
        h ^= rel.wrapping_mul(0x9e3779b97f4a7c15);
        h = h.wrapping_mul(0x100000001b3);
        mem_write_count = mem_write_count.saturating_add(1);
    }

    TickSig {
        hash: h,
        first_mem_addr,
        mem_write_count,
        first_mem_value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Observe with synthetic pre/post/log. Each tick flips slot `t` so
    /// that pre != post across ticks. `slot_changes` specifies additional
    /// slots that differ (for structural sig variation).
    fn obs(
        tracker: &mut CycleTracker,
        slot_changes: &[usize],
        mem: &[(i32, i32)],
    ) -> Option<LockInfo> {
        let pre = vec![0i32; 32];
        let mut post = vec![0i32; 32];
        for &i in slot_changes {
            post[i] = 1;
        }
        tracker.observe(&pre, &post, mem)
    }

    #[test]
    fn detects_simple_period_4() {
        let mut tracker = CycleTracker::with_n_vars(32);
        let mut lock_info: Option<LockInfo> = None;
        let cycles: &[&[(usize, Vec<(i32, i32)>)]] = &[
            &[
                (0, vec![(100, 8), (101, 8)]),
                (1, vec![]),
                (2, vec![]),
                (3, vec![]),
            ],
        ];
        // Unroll many cycles of a period-4 workload with
        // addr_stride_per_cycle = 2. CONFIRM_CYCLES * period + extra
        // samples ensures the lock fires even with the 3-cycle affine
        // verification gate.
        for cycle_idx in 0..10i32 {
            for (off, (shape, mems)) in cycles[0].iter().enumerate() {
                let _ = off;
                let mems_adj: Vec<(i32, i32)> = mems
                    .iter()
                    .map(|&(a, v)| (a + cycle_idx * 2, v))
                    .collect();
                // slot_changes for this offset
                let changes: Vec<usize> = match shape {
                    0 => vec![0, 1, 3],
                    1 => vec![1, 2, 4, 5],
                    2 => vec![6, 7, 8],
                    3 => vec![9],
                    _ => vec![],
                };
                if let Some(i) = obs(&mut tracker, &changes, &mems_adj) {
                    if lock_info.is_none() { lock_info = Some(i); }
                }
            }
        }
        let info = lock_info.expect("expected lock on period-4 workload");
        assert_eq!(info.period, 4);
        assert_eq!(info.addr_stride_per_cycle, 2);
        assert_eq!(info.fill_value, Some(8));
    }

    #[test]
    fn refuses_chaotic_input() {
        // Chaotic = structurally different every tick. Randomise which
        // slot changes each tick so no two consecutive sigs share the
        // same "which slots changed" fingerprint.
        let mut tracker = CycleTracker::with_n_vars(16);
        for t in 0..200u64 {
            let pre = vec![0i32; 16];
            let mut post = pre.clone();
            // Pick a pseudo-random set of 3 slots to flip, deterministic
            // but chaotic.
            let seed = t.wrapping_mul(0x9e3779b97f4a7c15);
            for k in 0..3u64 {
                let idx = ((seed >> (k * 4)) & 0x0f) as usize;
                post[idx] = 1;
            }
            // Also add a couple of random mem writes with varying
            // relative offsets.
            let mems = [
                (100 + ((seed & 0xff) as i32), 42),
                (200 + (((seed >> 16) & 0xff) as i32), 42),
            ];
            let _ = tracker.observe(&pre, &post, &mems);
        }
        assert!(!tracker.is_locked());
    }

    #[test]
    fn detects_p1() {
        let mut tracker = CycleTracker::with_n_vars(4);
        for t in 0..10i32 {
            let pre = vec![t, 0, 0, 0];
            let post = vec![t + 1, 0, 0, 0];
            let _ = tracker.observe(&pre, &post, &[(100 + t, 0xAA)]);
        }
        assert!(tracker.is_locked(), "should lock on P=1 with constant sig");
        let info = tracker.locked_info().unwrap();
        assert_eq!(info.period, 1);
        assert_eq!(info.addr_stride_per_cycle, 1);
    }
}
