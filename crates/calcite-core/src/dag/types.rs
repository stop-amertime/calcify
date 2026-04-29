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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// sub-DAG (not represented as a node here — looked up by `fn_id`
    /// at walk time).
    FuncCall { fn_id: u32, args: Vec<NodeId> },

    /// `if(cond1: e1; cond2: e2; ...; else: ef)` conditional.
    If {
        branches: Vec<(StyleCondNode, NodeId)>,
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
    /// Per-`@function` sub-DAG roots, indexed by `fn_id`.
    pub function_roots: Vec<NodeId>,
    /// Function name → `fn_id`, for resolving `Expr::FunctionCall`.
    pub function_names: std::collections::HashMap<String, u32>,
    /// Broadcast-write ports (recognised + prebuilt-from-parser-fast-path),
    /// indexed by `IndirectStore::port_id` when `packed = false`.
    pub broadcast_writes: Vec<crate::pattern::broadcast_write::BroadcastWrite>,
    /// Packed-cell broadcast ports, indexed by `IndirectStore::port_id`
    /// when `packed = true`.
    pub packed_broadcast_ports: Vec<crate::pattern::packed_broadcast_write::PackedSlotPort>,
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
