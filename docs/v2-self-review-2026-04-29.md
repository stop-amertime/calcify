# v2 self-review — 2026-04-29

Unedited self-review of the v2 DAG rewrite, conducted at the user's
request after Phase 2 idiom work (function inlining, dispatch tables).
The brief was: ask questions thoroughly, take nothing for granted, see
if there are more elegant ways to do what's already been done.
Presented without editing.

> **Second-pass corrections (appended at end).** A re-read flagged
> several over-claims and one major omission. See the "Corrections on
> re-read" section at the bottom before acting on anything in this doc.

---

OK. Going to do this in the open. No structure imposed — I'll think
through what I've built and write down what comes up, including the
half-formed stuff.

---

Let me start by re-reading what's actually in the DAG and the lowering
with fresh eyes.

`DagNode` has: Lit, LitStr, LoadVar, Calc, FuncCall, If, Switch,
Concat, WriteVar, IndirectStore. Ten variants. Right away — why is
`LitStr` a separate variant? It exists because v1's
`Expr::StringLiteral` is a separate variant. Strings in this system
only appear in `Concat` for display output (textBuffer rendering).
Numeric eval returns 0 for strings. That's three lines of code burning
a whole node variant. A cold start might handle strings as a flag on
Concat, or as a tagged Lit, or by punting display rendering to a
separate pass entirely. The CSS-DOS display system is small and v1's
handling of it is messy — it's a place where the CSS does something
genuinely outside the numeric DAG, and forcing it into the same IR is
awkward. I haven't thought about this enough.

`FuncCall` — I keep this around for the recursion-fallback case but I
just argued recursion is impossible in CSS `@function`. So `FuncCall`
is dead code in any correct program. The "defensive" framing is honest
but it's also a smell — I'm carrying a node variant for a case that
can't happen. A cold start would either (a) prove inlining always
succeeds and eliminate the variant, or (b) report a parse-time error
if recursion is detected, or (c) inline with a depth limit and emit a
Lit(0) on overflow. Any of those is cleaner than "carry a node forever
just in case." This is propagation: v1 has `FunctionCall` because at
compile time it doesn't know whether to inline; v2 *does* know, and I
should commit to that.

`If` and `Switch` — I split them. But `Switch` is `If` where every
branch has shape `key == constant`. A cleaner design has one node —
call it Match or Cond — that carries an LHS NodeId and a list of
`(predicate, branch)` pairs, where predicate is `Eq(NodeId)`,
`Lt(NodeId)`, etc. The dispatch optimisation is then: at lowering
time, if every predicate is `Eq(literal)` and the LHS is the same,
build a HashMap on the side. The walker checks the HashMap-side once
and falls through to linear scan if it's empty. That's two lines of
branching in the walker instead of a separate node. It also
generalises naturally — if some predicates are constants and some
aren't, you can have a hybrid ("check the hash table first, then fall
through to scan the residual"). My current Switch can't do that.

Even more aggressive: `style(--p: v)` is just `var(--p) == v`. And
`if(cond1: e1; cond2: e2; else: ef)` is just nested ternaries. There's
a version of this IR with no If/Switch/StyleCondNode at all — just
`Eq`, `Select(cond, then, else)`, and lookup tables. The CSS surface
syntax wants if-chains, but the IR doesn't have to. **This is the
deepest version of your question.** A cold start might say: lower CSS
into something resembling MIR or LLVM IR with primitive operations,
not a tree mirroring CSS syntax. The DAG would be smaller,
optimisations would be more uniform, and `Switch` would be a
constant-folding pass on `Select` chains rather than its own variant.

What's the cost of going there? Probably not as much as I'd think. The
lowering is a tree-walk and the new lowering would still be a
tree-walk. The walker becomes simpler (fewer node kinds). The downside
is that pattern-recognising "this is a dispatch chain" gets harder
because the chain is now a deep nested Select tree instead of a flat
list. Hmm. So there's a real tradeoff: keeping If's flat list of
branches is what makes dispatch detection a cheap shape check, vs.
nested Selects which would require a tree traversal. v1 happens to win
this one — the flat-branch shape of `Expr::StyleCondition` is
genuinely useful for pattern recognition. So this might be a case
where the v1-shaped IR is actually right, not just incidentally
inherited.

OK now `StyleCondNode`. I changed `lhs` from SlotId to NodeId this
session and that was clearly correct. But why does `StyleCondNode`
exist at all as a sub-IR? Because `style(--p: v)` predicates can be
combined with `and`/`or`. And/Or could just be normal boolean DAG
nodes — `BoolAnd`, `BoolOr`, `Eq` — with the `If` node taking a NodeId
for the condition. Then `style(--p: v)` is just `Eq(LoadVar(p), v)`.
No StyleCondNode, no separate eval path. Why didn't I do that? Because
`Expr::StyleCondition::branches` is shaped as `Vec<StyleBranch>` where
each branch has its own condition, and the mechanical lowering
preserved that shape. **That is propagation.** A cold start would lift
conditions to first-class boolean nodes and the dispatch recogniser
would still work — it'd look for "If with branches of shape
`Eq(LoadVar(same_slot), Literal)`" instead of "If with
StyleCondNode::Single branches." Same complexity, fewer concepts.

`LoadVar { slot, kind: TickPosition }`. The kind enum has Current and
Prev. Prev exists because of `--__1foo` triple-buffer reads. v1
implements Prev as "bypass the per-tick property cache, go to
committed state." Is that what Prev means in v2 or did I just port the
implementation? Let me think. In v2, "current tick value with
substitution semantics" is what `LoadVar Current` means — read the
per-tick cache, fall back to committed. "Prior tick value" is what
`LoadVar Prev` means — read committed only. These actually do
correspond to two different semantics in CSS: a self-reference within
a tick reads pre-cascade state (committed, not your own current
write), while `--__1foo` is the user explicitly asking for prior-tick
state via the triple-buffer convention. v1 conflates these because
`--__1foo` and self-reference both bypass the cache, but they're
conceptually different operations.

Is the conflation safe? For all the CSS-DOS cabinets I've seen, yes —
both produce committed state. But Chrome's actual semantics for
self-reference are subtle: a self-reference within a single
declaration is invalid-at-computed-value, which means it falls back to
the registered initial value. v1's "read committed state" is a
happens-to-work approximation because the prior tick's committed value
is usually what the program wants. **A cold start would notice that
"prev" and "self-ref" aren't the same thing and might split the read
kinds further.** I won't do that now without a fixture that
distinguishes them, but it's a flag.

The slot model. I've got SlotId = i32 with three regions: `slot < 0`
for state vars, `0 <= slot < TRANSIENT_BASE` for memory, `slot >=
TRANSIENT_BASE` for transients. This is straight-up v1. A few things I
notice:

- The negative/positive split is a v1 implementation detail. v1 chose
  negative for state vars because `state.state_vars[i]` is indexed by
  `(-slot - 1)` and that's how the original author wired it up.
  There's no semantic reason state vars need to live in negative space
  in v2. I could have a Slot enum: `enum Slot { StateVar(u32),
  Memory(u32), Transient(u32) }`. The walker dispatches on the enum
  tag instead of doing arithmetic on signed integers. More memory per
  slot reference (1 byte tag + 4 bytes index = effectively 8 with
  alignment, vs 4 for i32) but vastly more obvious code. This is a
  perf tradeoff masquerading as a design choice and I never measured.

- TRANSIENT_BASE = 1<<30 is a hack. It works because real memory
  addresses are <256MB, but it's fragile — any cabinet with >1GB
  memory would collide. CSS-DOS isn't going to have that, but it's the
  kind of arbitrary constant that says "we should just use a sum
  type." This is exactly the kind of v1-shape propagation where I
  picked the easier path.

- Memory addresses being i32 means we can address ±2GB. CSS-DOS uses
  up to a few MB. There's a lot of slack, which is fine, but it points
  at a more fundamental question: do memory and state vars really need
  to share an address space at all? In v1 they had to because
  `state.read_mem(addr)` had to handle both. In v2 they don't have to
  — the walker could call different methods for different slot kinds.
  The unified address space is purely a v1 artefact. **Cold start
  would split them.**

`Dag::name_to_slot` is a HashMap<String, SlotId>. Why? Because the
broadcast executor (and packed broadcast executor) needs to resolve
property names at runtime to slot ids. But broadcasts are *built at
lowering time* — they should already have slot ids baked in. This is
a smell. The reason name_to_slot exists is that the broadcast
value_expr still goes through v1's interpreter, which wants property
names. Once broadcast value evaluation is native, name_to_slot
probably collapses to nothing — every name reference would have been
resolved at lowering. **The map is scaffolding for the v1 bridge.**
Once the bridge is gone, the map should go too. Note for later.

`Dag::absorbed_properties` is a HashSet<String>. Used at lowering to
skip assignments that broadcasts have absorbed. After lowering, it's...
still there, never read again. Should be a local variable in
`lower_parsed_program`, not a Dag field. Minor but it's lying about
its lifetime.

`Dag::transient_slot_count: usize`. Used to size the transient_cache.
Fine, but: why is the cache vec'd by index? Because the slot id
encodes the index. A cold start might use a HashMap<SlotId, i32> for
transients and not bother with the cache vec at all. The cache vec
optimises for dense allocation; transients in CSS-DOS cabinets are
sparse (maybe 50 total across a million addresses). Either is fine
but it's worth noticing the asymmetry: state_var_cache is dense,
memory_cache is HashMap, transient_cache is dense. There's no
principle here, just three independent decisions.

`DagNode::Concat(Vec<NodeId>)`. Walker returns 0 for Concat because
numeric eval doesn't apply. But Concat has to do *something* — it's
the textBuffer rendering. Currently it's not implemented in v2 at all.
v1 has a separate string-evaluation path. v2 will need one too
eventually. Has anyone made a cabinet that renders text via v2? No —
none of my tests touch Concat. So Concat is currently aspirational. A
cold start might decide to keep numeric and string evaluation as fully
separate IRs and not pretend Concat is a numeric node. The current
shape ("everything is in one DAG, walker returns 0 for non-numeric")
is going to make string rendering awkward when I get to it. **Forcing
string and numeric into one IR is propagation from `Expr` again.**
They're really different operations.

`IndirectStore { port_id, packed }`. This is a placeholder — it
doesn't carry the broadcast logic, just an index into a side table. A
cold start would fold the broadcast into the DAG: the broadcast's
gate, address, value-expression, and address map would all be sub-DAG
nodes. Then there'd be one terminal kind (Write) parameterised by what's
being written. That's the broadcast peel I keep punting. The current
design has broadcasts as opaque side tables exactly because I'm still
calling v1's executor — once I write the native one, IndirectStore
should evaporate and become a normal sub-DAG.

The walker. Recursive, takes &mut self, threads four cache references
through every call. Every recursive call in `dag_eval_node` repeats
`dag, args[idx], state_var_cache, memory_cache, transient_cache,
state` — six arguments. That's not just ugly, it's a code smell
saying "these belong together." A cold start would pack them into a
`Walker` struct with methods, or make them all live on `self`. The
reason they're separate is that some of them came from `mem::take`'d
state on the Evaluator and the borrow checker forced threading. That's
an implementation accident, not a design.

More importantly: the walker is recursive. CSS-DOS dispatch chains can
be hundreds of branches deep (after Switch lowering it's flat, but
inside each Switch entry the body is itself a chain of operations,
sometimes deeply nested). Recursion + per-call cache cloning
(`branches.clone()` in the If walker, `table.clone()` in Switch) is a
perf problem waiting to happen. v1's bytecode walker is iterative; my
v2 walker is recursive AST-walk with clones. **This is the biggest
perf risk and it's directly inherited from "I built it like an
interpreter."** A cold start would emit a flat instruction array (the
DAG topologically sorted into a Vec<Op>) and walk it iteratively with
an explicit value stack. That's a meaningful redesign and it's the
thing I'd want a benchmark gate to force me into.

The clones are particularly bad. `branches.clone()` in If is cloning a
Vec<(StyleCondNode, NodeId)> per evaluation. Inside a hot function
called millions of times per tick. Why is the clone there? Because of
borrow checker — `dag` is borrowed and I want to call `dag_eval_node`
on `self` which mutably borrows everything. The clone breaks the
borrow. **But the clone is only necessary because of the architecture
I picked.** A flat Op array would have no such issue. This is real
performance debt and I knew it when I wrote it.

Function inlining as it stands. The frame is a HashMap<String,
NodeId>. Lookups are by string. Why? Because parameter references are
`var(--name)` and the lowering stays at the string-name level until it
resolves to a slot. A cold start might intern parameter names to small
integer ids (per-function, not globally) and use a Vec<NodeId> indexed
by param ordinal. Faster lookup, no string hashing. But this only
matters at lowering time, which is one-shot, so it's basically
irrelevant for runtime perf. Noted but not worth it.

Inlining model — I push frames, lookup is innermost-first scan. CSS
doesn't have nested scopes other than function calls, so the scan is
at most a few deep. Fine. But: **I'm not handling shadowing correctly**
for the case where a sub-call has a parameter with the same name as
the caller's parameter. Let me re-check. Frame stack, lookup
innermost-first. Inner frame has `--x → A`, outer has `--x → B`.
Inside inner's body, `var(--x)` finds A first. Yes, correct. Restored
on pop. Yes.

But wait — what about a function's local that shadows a tick-global
state var? CSS-DOS's `--at` is a function parameter; `--at` is also
presumably an `@property` somewhere if it's used in a top-level
assignment. If it's both, my lowering uses the inline frame inside the
function (correct) and the slot outside (correct). But what if the
function body reads `var(--AX)` — that's not a param, not a local,
it's a real register. My `lookup_inline` returns None, falls through
to slot lookup, gets the AX slot. Correct.

What about when the function body is mid-evaluation and `--AX` has
been written this tick by another assignment that hasn't run yet?
`LoadVar Current` reads the per-tick cache, falls back to committed.
If `--AX`'s WriteVar hasn't fired yet (topo sort places it later), the
cache miss falls through to committed. v1 does the same thing. CSS
spec says cascade is single-pass with substitution, so this is the
right answer. OK that's fine.

But here's something I didn't think about: **my inlining lowers locals
in the order they appear in the source.** v1's `eval_function_call`
evaluates locals in order and shadows `self.properties`
accumulatively. Same semantics. But what if a local references a
parameter — does my lowering get that right? Local body has
`var(--x)`; `--x` is a parameter; inline frame has `--x → arg0_node`.
Local lowers, hits `Expr::Var`, lookup_inline finds `--x`, returns
arg0_node. Yes. The local's node references arg0_node directly,
sharing the sub-DAG. v1 evaluates the param, stores in
self.properties, then evaluates the local which reads self.properties.
Same result, but v2 is a graph not a sequence. If arg0 is expensive (a
function call), v1 evaluates it once, v2 evaluates it... once, because
the DAG node is shared and the walker memoises via the cache. Wait,
does it?

Let me check. The walker doesn't memoise. Every call to
`dag_eval_node` re-evaluates the node. So if arg0_node is referenced
10 times in the inlined body, it's evaluated 10 times. **That's a real
correctness AND perf issue.** v1 evaluates each arg once at call time
and stores the result. v2 inlines the arg expression and evaluates it
everywhere it's referenced. For pure expressions this is just slow.
For expressions with side effects... wait, do any expressions have
side effects in this IR? `FuncCall` could (we delegate to v1's
eval_function_call). `IndirectStore` does. WriteVar does. None of
those should ever be sub-expressions. Pure expressions: Lit, LoadVar,
Calc, If, Switch, Concat, FuncCall (returns value). Hmm, FuncCall
delegates to v1 which can recursively evaluate things, but it
shouldn't mutate state. So we're probably correctness-clean on side
effects.

But **perf-wise**, repeated arg evaluation is bad. If `--writeMem(--at,
--val)` is called with `--val: --readMem(complicated_expr)`, the
readMem call gets duplicated everywhere `--val` appears in writeMem's
body. CSS-DOS body shapes that I've seen mostly use each param once or
twice, so this might not bite. But it's definitely worse than v1.

A cold start would either (a) memoise the walker per-tick (a value
cache keyed by NodeId), (b) hoist multi-use args into pre-computed
nodes that get evaluated once and cached, or (c) emit a flat
instruction stream with explicit temporaries. v1 effectively does (b)
by binding args to property names. **I should add per-tick memoisation
to the walker.** It's a straightforward change — keep a Vec<Option<i32>>
sized to dag.nodes.len(), check before evaluating, write after. That's
also closer to what a flat-Op compiler would do naturally.

Wait — this is a real bug, not just perf. If `--readMem(addr)` reads
memory that another assignment writes later in the same tick, then
evaluating it twice in the inlined body could observe different
states. Actually no — within a tick, a LoadVar Current either hits the
cache (committed state) or hits the cache after a WriteVar fired. Once
in either state, repeated evaluation is consistent. So it's perf only,
not correctness. OK, less urgent than I thought, but still worth
fixing.

Topo sort. Kahn's with a min-heap on declaration order. I default to
declaration order on cycle. Fine. But: I'm sorting *terminals*
(WriteVar and IndirectStore), not nodes. And I'm walking the value
sub-tree of each terminal to find LoadVar Current reads to other
terminals' slots. That's an O(N*M) operation where N is terminals and
M is average sub-tree size. For 6000 assignments with deep sub-trees,
this is the lowering's hot loop. v1 does an equivalent operation in
its `compile.rs::topological_sort`. Have I checked if my version
matches v1's complexity? v1 builds a "reads" set per assignment by
walking expressions. Same shape. OK, no asymmetry, but worth noting
that a cold start might use a different sort algorithm or a different
data structure (graph adjacency list explicitly).

I notice `sort_terminals` only sorts WriteVar terminals — IndirectStore
terminals are emitted at the end always. This is correct because
broadcasts read committed state directly (they bypass the cache and
always see fully-cascaded values). v1 does the same. But there's
nothing in the design doc that says broadcasts must be last; it's an
implementation choice that fell out of broadcasts using
`state.read_mem` instead of the cache. **Cold start would notice that
"broadcasts are special and run last" is a v1 implementation choice
and might design differently** — e.g., broadcasts could read the cache
too, and then they'd participate in topo sort like everything else.
Whether that's better is a deeper question I don't have an answer to.

The Phase 0.5 conformance suite. 49 fixtures, each with `expected_str`
from Chrome. We pass 41 / skip 5 / xfail 3. The xfails are interesting
— what are they? I don't remember off the top of my head. Let me make
a note to look at them. **If they're CSS edge cases v2 should handle
but doesn't, that's the contract failing** and I should know them by
heart.

The cabinet differential test. v1-as-oracle, finds first divergent
tick. Useful diagnostic but **it explicitly is not the contract.** If
v2 disagrees with v1 because v1 has a quirk, v2 wins. The test is a
tripwire that requires human judgement on each divergence. That's fine
but I should be writing down each "divergence at tick N — root cause Y
— verdict: v1 wrong / v2 wrong / both fine" note as we go. Right now I
just have "tick 6 divergence, probably broadcast value evaluation,
deferred." That's not enough engineering rigour. **Cold start would be
more disciplined about logging divergences.**

Performance. I've never benchmarked v2. Not once. The whole Phase 2
work has been correctness-driven. The design doc has a node-count
probe (#22) but not a wall-clock target. v1 hits 6k ticks/s on rogue.
If v2 hits 600 ticks/s, we're not shipping it as default. **I don't
know what v2's performance is.** That's a giant unknown and it's the
kind of thing where a cold start would have built a microbenchmark on
day one to keep the design honest.

The recursive walker with clones, no memoisation, HashMap dispatch
table lookups, and string-based broadcast slot resolution — taken
together, I bet v2 is 5-10x slower than v1 on a real cabinet right
now. Maybe more. The design assumed Phase 3 would bring perf parity
but I haven't done any of that work and I don't know how big the gap
is.

Let me look at things I haven't questioned:

- `parse_css` reuse. v2 uses v1's parser. Right call — the parser is
  pure, well-tested, and there's no v1-specific decision in it. No
  propagation risk.
- `State` reuse. v2 uses v1's `State` struct directly. State is
  essentially just memory + state_vars + a few flags. Very thin. Right
  call.
- `ParsedProgram` reuse. v2 uses v1's parser output type. This is the
  source IR. Reusing it is the point of the design.

OK, what about things I might have gotten *right* by accident that a
cold start would also land on? Let me check that side too:

- DAG with terminals as roots, walked top-down: this is how every
  dataflow IR works. Right.
- Topological sort of assignments: required by CSS substitution
  semantics, no v1-specific choice.
- Per-tick value cache: required by correctness (single-pass cascade).
  Right.
- Pure expression nodes with side-effecting terminals (WriteVar):
  clean separation. Right.
- Inlining at lowering time vs runtime: right call, simpler walker.
- Pattern recognisers as pure-shape predicates: right call, separates
  "what to recognise" from "how to compile."

What about the design doc itself? It's pretty good. The cardinal-rule
section is clear, the Phase 0.5 framing is clear, the lowering rules
are clear. The thing I notice on re-read: the doc commits to a specific
node vocabulary that mirrors `Expr`. **The doc itself encodes the
propagation.** A cold start would have written the doc differently —
maybe with primitive boolean ops instead of StyleCondNode, maybe with
separate state-var and memory address spaces. The doc is mine and it's
where the v1-shape leaked into the design before any code got written.

OK, what does all of this add up to?

**Big things I think I'd do differently in a cold start:**

1. Flat Op array, iterative walker, value cache memoised by node id.
   Eliminates clones, eliminates recursive call overhead, opens up
   real benchmarking.
2. Sum-type Slot enum instead of i32 with magic regions. Eliminates
   TRANSIENT_BASE hack, makes intent clear, possibly costs perf —
   would need to measure.
3. Boolean predicate nodes (`Eq`, `And`, `Or`) instead of
   StyleCondNode. Smaller IR, more uniform optimisation.
4. Separate string and numeric IRs. Don't pretend Concat is a numeric
   node.
5. No FuncCall variant — commit to inlining always or to a parse-time
   error on uninlinable. Possibly with a recursion-depth Lit(0)
   sentinel inside lowering instead of a node.
6. Native broadcast value evaluation from day one — don't bridge to v1
   even temporarily. (This is just "don't pick the scaffolding
   strategy I picked" which is a different trade.)
7. Memoised walker. Args evaluated once per tick.
8. Per-divergence logbook on the cabinet differential.
9. Wall-clock benchmark on every Phase 2 commit, not just node-count.

**Things I think I got right that survive scrutiny:**

1. ParsedProgram as source IR, not bytecode.
2. Topo sort + per-tick value cache.
3. Inlining at lowering time.
4. Pattern recognisers as pure shape predicates.
5. Phase 0.5 conformance as the contract.
6. The general DAG-with-terminals shape.

**Things I'm uncertain about:**

1. Whether the DAG node vocabulary should be smaller (fold
   StyleCondNode in, maybe fold Switch in, drop FuncCall, drop LitStr).
   Probably yes for some, no for others — Switch as a separate node is
   genuinely useful for the dispatch fast-path at walk time, vs
   forcing the walker to check "is this If a degenerate Switch?" every
   time.
2. Whether the slot model should be unified or split. Real perf
   tradeoff, no clear answer without measurement.
3. Whether to flatten the DAG to a flat Op array now or continue with
   the tree-walking interpreter and flatten later. Flattening now
   avoids throwing away work; flattening later means I haven't learned
   what shapes the optimiser needs yet.

**The biggest thing I'm worried about:**

The walker is the wrong shape for performance. Recursive, cloning, no
memoisation. I've been treating it as good enough and it's not. **The
cold-start version would have flattened it on day one** because anyone
who's written an interpreter knows tree-walking + clones is slow. I
rationalised this as "Phase 3 will fix it" but Phase 3 hasn't happened
and the longer I push correctness work on top of the wrong walker
shape, the more code depends on the walker's API surface. Walker
rewrite cost grows with every commit.

The honest reading is: **I should rewrite the walker now, before any
more Phase 2 work.** Memoised, iterative if possible (recursive but
with explicit stack if not), no per-call clones. That's the change
with the highest leverage and I've been avoiding it because correctness
was easier to make progress on.

That's the real answer to your question. Most of the propagation risks
are minor IR shape questions that can be cleaned up later. The walker
is the one that's going to bite.

End of self-review. No edits.

---

## Corrections on re-read

After re-reading the doc against `eval.rs` (the actual walker code) and
thinking about what the self-review missed, here are the items I'd
change. Originals stand above; this section is the audit.

### Claims I'd retract

**"Forcing string and numeric into one IR is propagation" (Concat
section).** Wrong. CSS is one expression language; `Concat` participates
in `var()` substitution exactly like numeric expressions. Splitting into
two parallel IRs would mean two walkers, two name-resolution paths, two
topo sorts. The correct fix is to give the walker a string-valued
return path (probably a side table indexed by NodeId) so `Concat` can
produce a real value. One IR, two value kinds. Retract the split-IR
recommendation.

**"5-10x slower than v1" (perf section).** I made this number up. I
have no measurements. The recursive walker plus per-call clones plus no
memoization is consistent with a large gap, but the specific multiple
was invented. The honest version is "I don't know how big the gap is,
build the bench first."

**"Sum-type Slot enum is a clean win blocked on measurement" (slot
model section).** Overstated. Slot reads are on the inner-loop hot
path. Going from `i32` to a tagged enum doubles the size and adds a
dispatch on every read. The TRANSIENT_BASE constant is ugly but cheap;
the burden of proof is on the change, not the status quo. Demote this
from "would do differently" to "wouldn't bother unless something else
forces it."

### Claims I'd reframe

**Repeated arg evaluation as "a real bug, not just perf."** The
dramatic wind-up in the original was misleading — the conclusion two
sentences later (perf only) was right. Within a tick the cache state is
monotonic, so repeated evaluation of pure expressions is consistent.
This is straight perf debt, full stop.

**Prev / self-ref conflation.** The analysis is correct but the
actionable observation is buried. The real point: I don't have a
fixture that distinguishes self-reference from `--__1foo`. Without one,
the discussion is theoretical. Reframe as "build the fixture, then
decide" rather than "cold start would split them."

**Switch as a separate node "is genuinely useful."** Weaker than I
made it sound. The walker arm at `eval.rs:1976` is two lines —
`table.get(&key)` and recurse. That could live inside an `If` arm with
a precomputed `Option<HashMap>` side field. Still defensible (cheap
shape check at lowering time, fast-path is obvious in the walker), but
not the slam dunk I framed it as.

**"Rewrite the walker now, before any more Phase 2 work."** Right
diagnosis (highest-leverage change), wrong sequencing. The cabinet
differential still diverges at tick 6 because broadcast value
evaluation goes through v1's interpreter. Native broadcast eval should
go first: it removes the v1 bridge that `name_to_slot` and the
`FuncCall` arm exist to support, which makes the walker rewrite
strictly smaller and cleaner. Reorder: native broadcasts → walker
rewrite.

### What I missed entirely

**The scalar walker is not the hot path for real cabinets.** The whole
walker section in the original review focused on `dag_eval_node` —
which handles scalar assignments. But CSS-DOS cabinets are
broadcast-dominated: thousands of memory cells written via
`BroadcastWrite` / `PackedSlotPort` per tick. Those go through
`dag_exec_broadcast` (eval.rs:2093) and `dag_exec_packed_broadcast`
(eval.rs:2151), both of which currently delegate the value expression
to v1's `eval_expr`. **No amount of scalar-walker optimisation matters
until the broadcast value path is native.** This is the biggest
omission — the original review optimised the wrong loop.

**No testing strategy for the walker rewrite.** A flat-Op walker is a
substantial change to a load-bearing component. The original said
"rewrite it" without acknowledging that the Phase 0.5 conformance suite
plus the cabinet differential would need to be the gate, and the
existing tree walker should stay around as an oracle during the
transition. That's an implementation-discipline omission.

### Revised priority list

In order:

1. **Native broadcast value evaluation** (highest leverage; unblocks
   tick-6 cabinet divergence; lets `name_to_slot` and the `FuncCall`
   bridge be deleted).
2. **Wall-clock benchmark on a real cabinet** (need numbers before
   making perf-driven design decisions; "5-10x slower" was a guess).
3. **Walker rewrite** — flat Op array, memoised by node id, iterative.
   After (1) and (2), with the existing tree walker kept as an oracle.
4. Smaller IR cleanups (drop FuncCall variant, demote
   absorbed_properties to a local, decide on Concat string semantics).
5. The Prev / self-ref fixture, once we have a CSS that distinguishes
   them.

### Score

Direction-of-travel: ~80% right. Weighting and sequencing: ~60%.
Single biggest correction: broadcasts are the hot path, not scalars.

### Post-implementation update

After landing native broadcast value evaluation (slots pre-resolved at
lowering, `value_expr` lowered to `NodeId`s, both unpacked and packed
executors switched off the v1 bridge), measured wall-clock on
`hello-text.css` (1000 ticks, 100 warmup, release build):

- **v1 bytecode: 187K ticks/sec**
- **v2 dag: 3.6K ticks/sec**
- **v2 is ~52× slower than v1.**

That's far worse than the made-up "5-10×" estimate. The walker is
indeed the bottleneck, and the gap is large enough that the rewrite is
no longer optional.

A second finding: native broadcast value eval did **not** fix the
tick-6 cabinet divergence. The differing properties (`--memAddr0/1/2`,
`--memVal1/2`) are computed *upstream* by scalar assignments, then
read by the broadcast. Bug is in the scalar walker (or lowering),
not in broadcast evaluation. Hypothesis from the original review
("very likely because broadcast value evaluation goes through v1") was
wrong — another argument for measuring before guessing.
