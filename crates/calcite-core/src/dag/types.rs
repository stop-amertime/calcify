//! DAG node vocabulary and supporting types.
//!
//! See `docs/v2-rewrite-design.md` § DAG node vocabulary for the
//! design rationale and the cardinal-rule audit.

use crate::types::RoundStrategy;

/// Index into a `Dag::nodes` vec.
pub type NodeId = u32;

/// Address in the unified slot space, matching v1's `State::read_mem`
/// addressing: negative for state vars, non-negative for memory bytes.
pub type SlotId = i32;

/// Tick position for a `LoadVar` read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TickPosition {
    /// Read the current tick's value: per-tick property cache first,
    /// then committed state on cache miss.
    Current,
    /// Read committed state directly, bypassing the cache. Lowered from
    /// `--__0` / `--__1` / `--__2` prefixed reads (all three resolve to
    /// the same slot in v1).
    Prev,
}

/// `Calc` op kind — mirrors `crate::types::CalcOp` 1:1.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CalcKind {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Min,
    Max,
    Clamp,
    Round(RoundStrategy),
    Pow,
    Sign,
    Abs,
    Neg,
}

/// Predicate sub-AST attached to `If` branches. Not a top-level
/// `DagNode` because it never produces a value, only gates one.
#[derive(Debug, Clone)]
pub enum StyleCondNode {
    /// `style(--prop: value)` — desugars to `lhs == value`. The LHS is
    /// stored as a `NodeId` (not a `SlotId`) so that function inlining
    /// can substitute params correctly: when `--prop` is a function
    /// parameter inside an inlined body, the LHS is the call-site arg's
    /// NodeId rather than a (non-existent) tick-global slot. For
    /// top-level `style(--AX: 0)`, the LHS lowers to a `LoadVar` node.
    Single { lhs: NodeId, value: NodeId },
    /// `cond1 and cond2 and …` — short-circuits left-to-right.
    And(Vec<StyleCondNode>),
    /// `cond1 or cond2 or …` — short-circuits left-to-right.
    Or(Vec<StyleCondNode>),
}

/// A single DAG node.
///
/// Phase 1 lands the pure expression nodes plus `WriteVar` plus the
/// minimum super-nodes needed to consume the parser fast-path output
/// (`IndirectStore` for prebuilt broadcast writes). Everything else
/// (Switch, Hold, Mux, BitField, BitOp) is Phase 2.
#[derive(Debug, Clone)]
pub enum DagNode {
    // ---- Pure expression nodes ----------------------------------
    /// Numeric literal.
    Lit(f64),

    /// String literal (display path only, e.g. `--i2char` results).
    LitStr(String),

    /// Read a slot from state (current tick or prior, per `kind`).
    LoadVar { slot: SlotId, kind: TickPosition },

    /// Generic arithmetic. Arity matches the `CalcKind`:
    /// - Add/Sub/Mul/Div/Mod/Pow: 2 args.
    /// - Sign/Abs/Neg: 1 arg.
    /// - Clamp: 3 args (min, val, max).
    /// - Round(strategy): 2 args (val, interval).
    /// - Min/Max: 1+ args.
    Calc { op: CalcKind, args: Vec<NodeId> },

    /// Pure user-`@function` call. The function body is its own
    /// sub-DAG rooted at `Dag::function_results[fn_id]`; this node
    /// supplies the call-site arguments and triggers a body walk.
    /// At walk time, the walker pushes a call frame containing
    /// `args` (caller-context NodeIds) and walks the body root;
    /// `Param(i)` references in the body resolve to `args[i]`
    /// evaluated in the caller's context.
    ///
    /// Replaces the v1-style "delegate to interpreter" `FuncCall` —
    /// kept that variant name above for callers that haven't migrated;
    /// new lowering emits `Call`.
    Call { fn_id: u32, args: Vec<NodeId> },

    /// Parameter placeholder inside a function body sub-DAG. The body
    /// is lowered once into the global node arena; `Param(i)` reads
    /// the i-th argument of the enclosing `Call`. Resolved at walk time
    /// against the topmost call frame.
    Param(u32),

    /// Legacy fallback: delegates to v1's `eval_function_call`. Only
    /// emitted when v2 lowering can't resolve a callee (unknown name,
    /// recursion guard). Phase 1 default; new lowering emits `Call`.
    FuncCall { fn_id: u32, args: Vec<NodeId> },

    /// `if(cond1: e1; cond2: e2; ...; else: ef)` conditional.
    If {
        branches: Vec<(StyleCondNode, NodeId)>,
        fallback: NodeId,
    },

    /// Dispatch-table specialisation of `If`: every branch tests
    /// `style(<same key>: <integer-literal>)` so the chain compiles
    /// to an O(1) keyed lookup instead of an O(N) scan. Lowered when
    /// the StyleCondition shape passes `dispatch_table::recognise_dispatch`.
    ///
    /// At eval time: read `key` (one node, evaluated once), look up
    /// in `table`, evaluate the matched branch — or `fallback` on miss.
    Switch {
        key: NodeId,
        table: std::collections::HashMap<i64, NodeId>,
        fallback: NodeId,
    },

    /// Space-separated string concatenation.
    Concat(Vec<NodeId>),

    // ---- Side-effecting nodes -----------------------------------
    /// Top-level property assignment: `--prop: <expr>`. Terminal node.
    WriteVar { slot: SlotId, value: NodeId },

    // ---- Super-nodes (Phase 1 emits IndirectStore from the parser
    //      fast-path; the rest are Phase 2.) ---------------------
    /// Broadcast write: when `gate` (if any) is 1, write `val` into
    /// the slot named by `addr` (gated by the dispatch shape baked
    /// into the node's `port_id`). Phase 1 stores the v1
    /// `BroadcastWrite` / `PackedSlotPort` index as `port_id` and
    /// delegates evaluation to v1's executor; Phase 2 promotes this
    /// to a fully self-contained DAG super-node.
    IndirectStore { port_id: u32, packed: bool },
}

/// V2-native broadcast write: every property reference resolved to a
/// `SlotId`, and the value expression lowered to a `NodeId` that the
/// walker evaluates directly. Replaces the runtime `name_to_slot`
/// lookups + v1-interpreter delegation that the bridge version did.
///
/// Built at lowering time from `pattern::broadcast_write::BroadcastWrite`
/// (which carries v1-shaped names + `Expr`); the original is kept on
/// `Dag::broadcast_writes` until the bridge is fully removed.
#[derive(Debug, Clone)]
pub struct DagBroadcast {
    /// Optional outer gate. The whole port is skipped when the gate's
    /// committed value isn't 1.
    pub gate_slot: Option<SlotId>,
    /// Slot holding the destination byte address. Read once per tick.
    pub dest_slot: SlotId,
    /// DAG node producing the value to write on a direct hit.
    pub value_node: NodeId,
    /// addr → cell-var slot, pre-resolved.
    pub address_map: std::collections::HashMap<i64, SlotId>,
    /// Word-write spillover: addr → (high-byte cell-var slot, hi value
    /// node). Mirrors `BroadcastWrite::spillover_map` with names and
    /// Exprs replaced by slots and NodeIds.
    pub spillover_map: std::collections::HashMap<i64, (SlotId, NodeId)>,
    /// Slot for the spillover guard (e.g. `--isWordWrite`). When set,
    /// spillover only fires if the slot's committed value is 1.
    pub spillover_guard: Option<SlotId>,
}

/// V2-native packed broadcast: same shape as `PackedSlotPort` but every
/// property name is pre-resolved to a slot. The port's value comes from
/// reading `val_slot` directly (no expression to evaluate), so this
/// doesn't need a NodeId — only slot resolution.
#[derive(Debug, Clone)]
pub struct DagPackedBroadcast {
    pub gate_slot: SlotId,
    pub addr_slot: SlotId,
    pub val_slot: SlotId,
    pub width_slot: SlotId,
    /// Cell-base byte address → cell-var slot.
    pub address_map: std::collections::HashMap<i64, SlotId>,
    pub pack: u8,
}

/// A complete DAG ready to walk.
#[derive(Debug, Default, Clone)]
pub struct Dag {
    /// All nodes, indexed by `NodeId`. Topological order is *not*
    /// guaranteed by construction — the walker must resolve
    /// dependencies via the per-tick value cache (see walker.rs).
    pub nodes: Vec<DagNode>,
    /// Terminal nodes: every `WriteVar` and `IndirectStore` in
    /// declaration order. The walker drives evaluation from these.
    pub terminals: Vec<NodeId>,
    /// Per-`@function` sub-DAG roots, indexed by `fn_id`. Each entry is
    /// the result NodeId of that function's body; the body lives in
    /// `nodes` alongside top-level nodes and references its parameters
    /// via `Param(i)` placeholders.
    pub function_roots: Vec<NodeId>,
    /// Per-`@function` parameter count, indexed by `fn_id`. Used by
    /// the walker to size call frames and by lowering to validate
    /// `Param(i)` indices.
    pub function_param_counts: Vec<u32>,
    /// Function name → `fn_id`, for resolving `Expr::FunctionCall`.
    pub function_names: std::collections::HashMap<String, u32>,
    /// Broadcast-write ports (recognised + prebuilt-from-parser-fast-path),
    /// indexed by `IndirectStore::port_id` when `packed = false`.
    pub broadcast_writes: Vec<crate::pattern::broadcast_write::BroadcastWrite>,
    /// Packed-cell broadcast ports, indexed by `IndirectStore::port_id`
    /// when `packed = true`.
    pub packed_broadcast_ports: Vec<crate::pattern::packed_broadcast_write::PackedSlotPort>,
    /// V2-native broadcast records: same indexing as `broadcast_writes`,
    /// every name pre-resolved to a slot and `value_expr` lowered to a
    /// `NodeId`. Built at lowering time. The walker prefers these over
    /// the v1-shaped vec above.
    pub dag_broadcasts: Vec<DagBroadcast>,
    /// V2-native packed broadcast records: same indexing as
    /// `packed_broadcast_ports`. Pure slot resolution (no value
    /// expression to lower).
    pub dag_packed_broadcasts: Vec<DagPackedBroadcast>,
    /// Property names absorbed into the broadcast lists. The lowering
    /// loop filters assignments against this set so absorbed cells are
    /// not double-evaluated as both `WriteVar` and `IndirectStore`.
    pub absorbed_properties: std::collections::HashSet<String>,
    /// Transient slot count: the number of properties that appear in
    /// assignments but aren't `@property`-declared (don't have a real
    /// state-var slot or memory address). v1's compiler allocates
    /// transient slots for these — they hold per-tick computed values
    /// that other assignments can read but that don't commit to State
    /// at end of tick. v2 reproduces this with a per-tick scratch
    /// cache.
    ///
    /// Encoding: transient slots use positive integers >= TRANSIENT_BASE
    /// (well above any real memory address). The walker detects them
    /// by `slot >= TRANSIENT_BASE` and routes reads/writes through a
    /// transient cache that does *not* commit to State.
    pub transient_slot_count: usize,
    /// Bare-property-name → SlotId for every name the lowering saw.
    /// Includes both committed slots (from
    /// `eval::property_to_address`) and transient slots allocated for
    /// unregistered names. Consumers (e.g. the broadcast executor)
    /// should consult this map rather than `property_to_address`
    /// directly, because it reflects v2's view of the slot space —
    /// in particular, transient slots that v2 allocates for `--opcode`,
    /// `--_slot0Live`, etc., aren't visible via the global address
    /// map.
    pub name_to_slot: std::collections::HashMap<String, SlotId>,
}

/// Transient-slot encoding base. Memory addresses are <= 0xFFFFFFF
/// (under 256MB); we put transient slots well above that range.
pub const TRANSIENT_BASE: SlotId = 1 << 30;

impl Dag {
    /// Allocate a new node, returning its `NodeId`.
    pub(crate) fn push(&mut self, node: DagNode) -> NodeId {
        let id = self.nodes.len() as NodeId;
        self.nodes.push(node);
        id
    }
}
