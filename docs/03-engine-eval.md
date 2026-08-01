# The evaluation zone

`crates/rfish-engine/src/eval/` — the NNUE network and the classical fallback.

Golden: `Stockfish/src/evaluate.cpp`, `Stockfish/src/nnue/`.

## Status: the forward pass is ported and bit-exact

`cargo xtask nnue-check` drives rfish and a pristine upstream build over the same positions
and compares the raw network output — the number upstream's `eval` prints as "internal
units", with no optimism blend and no fifty-move damping on top. Every position in
`tools/cases/eval.fens` matches **exactly**, including 60 reached by random legal play,
which is what makes it a check on the feature indexing rather than on a list the port was
written against.

What is **not** done is upstream's per-move accumulator delta. See "The accumulator is
updated by diffing feature sets" below, which also carries the measured gap to upstream and
the list of attempts that failed to close it.

## Shape

```text
  position ──> 3 feature sets ──> FeatureTransformer ──> 1024 u8
                                       │                     │
                                       └──> PSQT head        └──> fc_0 (1024→32)
                                            (8 buckets)            ├─ sqr_relu ─┐
                                                                   └─ relu ─────┤
                                                                       fc_1 (64→32)
                                                                   ┌─ sqr_relu ─┤
                                                                   └─ relu ─────┤
                                                                       fc_2 (128→1)
```

Eight output heads exist; `(pieces - 1) / 4` selects one. Both the PSQT score and the
positional score come back separately, because the search blends them by their
**disagreement** — a position the two heads argue about is one to be less confident in, and
the optimism term weighs more there.

## The three feature sets

A feature is an index into the transformer's weight table. The network sees a position only
as the SET of indices that are active, so an index computed one off is not a small error: it
reads a different column of weights.

| set | dims | encodes |
|---|---|---|
| `HalfKAv2_hm` | 22528 | own king square × (piece, square) |
| `FullThreats` | 59808 | (attacker, from) attacking (attacked, to) |
| `PP_3Wide` | 4560 | pairs of pawns within one file of each other |

Three details that are easy to get subtly wrong, and that a wrong answer would not announce:

- **The two orientations are opposites.** `HalfKAv2_hm` mirrors so the king lands on the
  **e-h** files; `FullThreats` and `PP_3Wide` mirror so it lands on the **a-d** files. A
  test asserts they disagree on every square.
- **Threat indices are ranked within the EMPTY-BOARD attack set**, not the occupancy-aware
  one. Which attacks *exist* depends on occupancy; where each sits in the numbering does not.
- **Same-kind threats are recorded once.** Two pieces of one kind attacking each other are
  symmetric, so only the `from > to` direction is encoded — and same-colour pawn pairs are
  excluded outright, because that relationship is `PP_3Wide`'s job.

The threat and pawn-pair sets share one weight array, with the pawn-pair block starting at
the threat set's dimension count. That is why `PP_INDEX_BASE == THREAT_DIMENSIONS`, and a
test asserts it rather than trusting the two constants to stay in step.

## The accumulator is updated by diffing feature sets

Upstream patches the accumulator from a per-move delta: `do_move` records which features a
move creates and destroys, and the accumulator applies exactly those. That needs the board
zone to derive a threat delta from the move's geometry — several hundred lines of dense case
analysis.

**rfish takes a different route to the same place.** The accumulator is a *sum over the
active set*, so any two positions' accumulators differ by exactly the set difference of their
features. Recomputing the active set is cheap — it is bitboard work — while applying a
feature is expensive, because each touches 1024 weights. So rfish recomputes the SET and
diffs it against the last one, then applies only what changed.

Three things that buys:

- **Correct by construction rather than by case analysis.** There is no delta logic to get
  wrong. The from-scratch path is not a separate implementation either: with no cached state
  the old set is empty, so the "diff" is every feature and one code path serves both.
- **No king-bucket cache.** The case upstream needs one for — a king move invalidating every
  king-piece feature — is simply a large diff.
- **No blocker.** It does not wait on the board zone.

Measured, on the bench at depth 11, three runs each, comparing CPU time because the machine
was contended:

| | user time |
|---|---|
| recomputed from scratch | 31.34 / 31.58 / 30.80 s |
| diffed | 21.35 / 20.29 / 21.23 s |

**A 1.48× speedup, with the node count bit-identical** — the same 1 240 003 nodes, which is
what proves the evaluation did not change. `cargo xtask nnue-check` still matches upstream on
all 109 positions.

Applying the diff is one sweep of the accumulator, not one per changed feature: the merge
walk collects what changed and a tiled fold applies the whole collection, so the accumulator
is read once and written once however many rows go into it. That is the shape of upstream's
`update_accumulator_incremental`, and it is safe here for a reason worth stating — the
accumulator is wrapping `i16` and the PSQT head `i32`, and both are associative and
commutative under the additions applied, so collecting before applying cannot change a value.

### One slot, not a stack — measured

The cache is a SINGLE slot holding the last position evaluated anywhere, not one slot per
ply. Upstream keeps a stack and always updates a child from its parent, so its diff is
always one move's worth; the obvious inference is that rfish should too, and it was tried:
`EvalScratch` grew a `Vec<Option<Cached>>` indexed by ply, `evaluate` took a ply, and the
base became the nearest filled ancestor.

**It was worse — 4,769M search instructions to 5,197M** on `bench 16 1 8`. Updating from the
parent means COPYING the parent's 4 KiB accumulator into the child's slot first, on every
evaluation. The single slot pays nothing at all in the common case, because a depth-first
search evaluates a node and then its child, and the slot already holds the parent; it loses
only on a subtree return. Copying every time to make the bad case cheap cost more than the
bad case did.

It was then tried a SECOND time, with the obvious objection to the first attempt answered:
the fold already loads every entry and stores every entry, so having it load from the parent
and store to the child absorbs the copy at no instruction cost, and no separate `clone_from`
is needed. That version is also worse — **3,528M to 3,806M**. Two independent implementations
losing says the effect is real and not an artefact of either: the single slot holds the
parent on the descent, which is the common case, and the stack pays to touch two 4 KiB
accumulators per evaluation where the slot touches one. Do not re-derive this a third time.

### What the gap is now

Against a pristine upstream build at the same tier, on `bench 16 1 8` with the same 166,964
nodes and startup subtracted by a `quit`-only run:

| | search instructions | ratio |
|---|---|---|
| before this work | 6,592,566,790 | 3.367 |
| sparse first affine layer | 4,769,344,411 | 2.436 |
| static affine output width | 4,436,458,866 | 2.266 |
| one fold per accumulator sweep | 3,715,310,646 | 1.897 |
| sixteen-bit pairwise step | 3,673,041,357 | 1.876 |
| 128-wide fold tile | 3,598,539,776 | 1.838 |
| merged threat-index tables | 3,589,208,299 | 1.833 |
| unrolled threat attacker dispatch | 3,548,072,611 | 1.812 |
| chunk-walked sparse weight rows | 3,528,438,652 | 1.802 |
| `std::simd` non-zero scan in the sparse layer | 3,202,332,317 | 1.635 |
| accumulator cache, one slot per king square | 3,095,210,600 | 1.581 |
| king-piece features from a board diff | 2,986,237,131 | 1.525 |
| one threat scan for both perspectives | 2,979,106,126 | **1.521** |
| **upstream** | **1,958,088,252** | **1.000** |

### Falsified, with numbers

Each of these is a reasonable idea that measured WORSE. They are listed so the next
attempt starts past them rather than at them.

| attempt | result |
|---|---|
| per-ply accumulator stack, as upstream keeps | 4,769M → 5,197M |
| zero-skipping in groups of four, as upstream tests | 4,769M → 5,784M |
| pairing non-zero inputs to sum two products in `i16` | 3,673M → 4,298M |
| multiplying the sparse layer in the 16-bit domain | 3,673M → 3,757M |
| folding both feature kinds in a single sweep | 3,599M → 3,800M |
| per-ply stack again, with the parent read fused into the fold | 3,528M → 3,806M |
| fold tile of 32 / of 256 | 4,093M / 4,037M against 3,599M at 128 |
| the fold rewritten with `std::simd`, one vector and two | a wash at `native`, worse at `nehalem` |
| refreshing the king-square cache on every evaluation | 3,202M → 3,199M, i.e. nothing |

The fold entry is worth reading with the disassembly beside it: it was ALREADY emitting
`vpaddw` on four `zmm` registers, so explicit SIMD had nothing left to give it. Check what
the compiler emits before rewriting a kernel by hand — the sparse layer's branch was worth
`std::simd` and the fold was not, and only `objdump` distinguishes them.

The scalar entries all say the same thing: the sparse layer's inner loop and the fold are
at a local optimum for what LLVM will emit from safe scalar Rust, and reshaping the
arithmetic to *look* more like upstream's vector kernels makes it worse, because the
shapes upstream chose are the ones its instructions reward and not the ones the
autovectoriser does.

### The ratio is a property of the TIER, and of the TOOLCHAIN

Every row of the ledger above was measured at `nehalem`, against an oracle built by `g++`
with no PGO. Two of those three choices were wrong, and the ledger's ratio column is
therefore a record of PROGRESS — each row against the row above it — and not a statement of
where the port stands. Read the standing from the table below instead.

The tier was the first correction: `nehalem` stopped being neutral the moment the sparse
layer moved to `std::simd`, because explicit vectors widen with the register file and an
SSE-4.1 build cannot show it. The toolchain was the larger one. rfish is compiled by rustc,
whose backend is LLVM, and upstream's own shipped recipe is `make profile-build` — PGO on
top of LTO. Comparing a rustc build that never saw a profile against a `g++` build that
never saw one either held NEITHER variable fixed.

Rebuilt with everything held equal — both sides clang/LLVM at rustc's own major, both sides
PGO on top of LTO, both trained on the same `bench`, same tier, same 166,964 nodes, startup
subtracted by a `quit`-only profile:

| both sides | rfish | upstream | ratio |
|---|---|---|---|
| `g++`, no PGO, `nehalem` (the ledger's own tier) | 2,979,106,126 | 1,958,088,252 | 1.521 |
| `g++`, no PGO, avx2 | 2,170,601,764 | 1,460,813,993 | 1.486 |
| clang + PGO + LTO, avx2 | 2,171,591,691 | 1,246,593,188 | 1.742 |
| the same, after the membership diff below | 2,080,763,904 | 1,246,598,569 | 1.669 |
| **the same, re-measured at the `c5aef2bf1` pin** | **2,108,359,414** | **1,240,674,464** | **1.699** |

The last row is the current one. It is not a regression in this port: it is the same code
measured against a moved target, and upstream gained slightly more from its own retune than
rfish did — 1.7% on this axis. Re-measure BOTH sides when the pin moves; carrying an old
ratio across a sync compares two different upstreams.

**The gap WIDENS when the comparison is made fair, and the reason is one-sided.** PGO is
worth almost nothing to rustc here and 15% to clang, so the honest figure is the largest of
the three, not the smallest. A number quoted from either of the first two rows understates
where the port is; `docs/09-tooling-ci.md` records what has to be held equal, and
`cargo xtask pgo` / `cargo xtask oracle` / `cargo xtask perf` are what hold it.

`cargo xtask signature` against the PGO binary reproduces the golden exactly, which is the
property that makes PGO admissible in a measurement at all: the profile steers block layout
and inlining and cannot move the tree.

### Wall clock, and the honest ceiling

Time is measured by a PAIRED A/B — interleaved, the order alternating each round, the median
of the paired ratios reported with its spread — because a batched best-of-N reads the
thermal state as much as the binaries. Default `bench` (depth 13), nine rounds, core-pinned,
both sides clang + PGO + LTO:

| | median paired time vs upstream | spread |
|---|---|---|
| **rfish, avx2 vs avx2 oracle, at the `c5aef2bf1` pin** | **1.53x** | 1.33..1.65 |
| rfish, avx2, before the sync | 1.52x | 1.29..1.82 |
| rfish, native vs native oracle (before the membership diff) | 1.77x | 1.38..1.99 |
| **spine, avx2, at the `c5aef2bf1` pin** | **1.31x** | 1.18..1.39 |

**Every time figure on this page was taken on a BUSY box, and the spreads say so.** This is a
laptop part under WSL2, measured in sessions that were also compiling and running callgrind,
and a spread of 1.33..1.65 around a median of 1.53 is what that looks like. The pairing
protocol removes the ORDER bias and the thermal drift between batches; it does not turn a
noisy machine into a quiet one. Treat these as one significant figure — "about half again
upstream's time" — and never read a few per cent off them. A number that has to be tighter
than that needs a quiet box or the instruction axis, which is unaffected because callgrind
counts are deterministic: the same binary re-profiled reproduces its instruction total
exactly, and every Ir ratio on this page is repeatable in a way no time ratio here is.

Both spreads exclude 1.000, so the direction holds at this sample size; neither is tight
enough to read a few per cent from. The instruction axis is the one that resolves small
effects, and it has a tier ceiling — callgrind implements no AVX-512, so `native` has a time
ratio and no instruction ratio.

The sibling ports measured the same way, each against an oracle at ITS OWN upstream base —
only the ratios compare, never the raw counts:

| | instructions vs own upstream | time vs own upstream |
|---|---|---|
| rfish, avx2, at `c5aef2bf1` | 1.699 | 1.53x |
| ../mcfish, avx2, as measured at `f4bcd404` | **0.872** | 1.05x (spread straddles 1.000) |

`../mcfish` retires FEWER instructions than the upstream it clones and still does not beat
it on time. Two caveats before reading its lead over rfish as a verdict on safe Rust. Its
base at the time of that measurement predates the threat feature set entirely, and
recomputing threats rather than delta-updating them is exactly the gap this page documents
below. And its row has NOT been re-measured since both ports moved to `c5aef2bf1` — it is
kept as the last figure actually taken, not as a current comparison.

**36.5 features are applied per evaluation, over 61,341 evaluations** — measured with an
instrumented build, not inferred, and upstream evaluates the same bench exactly 61,341 times
too. The counts MATCH. Whatever is left is therefore per-feature overhead and the cost of
recomputing the active set at all; it is not that this design applies more features than
upstream's per-move delta does.

### How much of the delta is done

The delta splits in two, and the halves are not equally hard.

**The king-piece half is done**, and it needed nothing from the board zone. Two placements
differ on some set of squares, and each contributes at most one feature to remove and one to
add — a diff of the BOARD, not of the move, so there is no case analysis and no `do_move`
plumbing. That removed the set, its sort and its merge walk outright, worth 109M.

**The threat half was built and is NOT kept.** It was written as a localised RESCAN rather
than as case analysis, which keeps the correct-by-construction property: a `View` the scan
can run against either placement, an affected-attacker set (every piece on a changed square,
plus every piece attacking one under either occupancy — a superset, and the argument that it
is one is four lines), a rescan of just those attackers on both placements, and a merge to
cancel what did not really change. It is bit-exact: `nnue-check` 109 of 109, bench unchanged
at 166,964 nodes.

It is also SLOWER, and the reason is arithmetic rather than fixable:

| | search Ir | ratio |
|---|---|---|
| full rescan (kept) | 2,979,106,126 | **1.521** |
| affected-attacker delta | 3,079,792,922 | 1.573 |
| the same, with the board diffed in eight-square chunks | 3,030,318,828 | 1.548 |

A quiet move changes two squares, and the pieces attacking those two squares are typically
six to twelve — on TWO placements, so twelve to twenty-four attacker scans against the full
scan's thirty on one. The affected set is not small enough relative to the whole set for the
halving to appear, and on top of it sit `attackers_to` per changed square per placement, two
view copies, and the cancelling sorts. The king-piece delta wins for the opposite reason: its
affected set really is two or three squares out of sixty-four.

Closing this properly needs upstream's actual per-move delta, which knows exactly which
threats change without rescanning anything — that is the case analysis this design has always
declined, and it is bounded by the ~230M the whole threat machinery costs.

Below that: the first affine layer would have to match `vpdpbusd`, which `std::simd` has no
operation for, so a further constraint would have to go before parity is even the right word.

### What remains, measured per symbol rather than estimated

The "roughly 300M for recomputing the active set" this section used to carry was an
estimate. A per-symbol profile of the PGO build against the PGO oracle, both at avx2 and
startup subtracted, splits the evaluation gap as:

| | rfish | upstream |
|---|---|---|
| affine layers | ~375M | ~165M (`Eval::evaluate`) |
| accumulator update and transform | ~610M | ~411M (`AccumulatorStack::evaluate_side`) |
| building the threat and pawn-pair set | ~158M | 0 — it has no equivalent |

So the three ways it splits are the first affine layer, where upstream's four-way byte dot
does in one instruction what takes four here and four scalar attempts to recover it have all
lost; the accumulator fold, now within about a third of upstream's incremental update; and
recomputing the active set, which is **~158M and not ~300M**. Quote the measured row.

**The per-move delta is a REDESIGN, not an edit, and the reason is the refresh cache.**
Upstream can apply a delta because its accumulator is a stack that moves in lockstep with
`do_move`/`undo_move`, so the base is always the parent. rfish keys its accumulator on the
position instead, and diffs against whatever the live slot holds — which after a king move is
a *cache* slot for that king square, a different board, and a different board PER PERSPECTIVE.
A delta therefore cannot be shared between the two perspectives in exactly the case the cache
exists to serve, and the per-ply stack that would fix that has been measured twice and lost
twice (see the falsified table above). Anyone taking this on should cost the bookkeeping
first: the prize is the ~158M row, not the whole evaluation gap.

### The search spine is a SEPARATE gap, and it is not closed

Swap both sides to a material evaluation — `eval-material` here, the same formula patched
into the oracle by `cargo xtask oracle --spine` — and what is left is the spine: movegen,
movepick, the histories, the TT, the pruning arithmetic. Same tier, same toolchain, same PGO,
identical trees at 625,992 nodes:

| oracle | rfish | upstream | ratio |
|---|---|---|---|
| as upstream ships it | 1,445,638,904 | 1,518,970,470 | 0.952 |
| with the NNUE threat scan compiled out | 1,445,638,857 | 1,297,100,189 | 1.115 |
| the same, after the two dispatch fixes below | 1,405,589,511 | 1,297,103,102 | 1.084 |
| **the same, re-measured at the `c5aef2bf1` pin** | **1,421,995,718** | **1,301,230,180** | **1.093** |

The first three rows were measured at the previous pin, over 625,992 nodes; the last is the
current one, over 657,500. Rows from different pins are different workloads and only their
RATIOS are comparable.

**The first row is the trap, and this page published its ancestor for a long time.** Upstream
maintains the threat feature set inside `do_move`, writing a `DirtyThreats` that
`nnue/nnue_accumulator.cpp` reads and that NOTHING else reads. Under a material evaluation
nobody reads it at all — so leaving it in charges upstream for NNUE bookkeeping while rfish,
which recomputes threats inside its evaluation, is charged for none. Compiling it out leaves
both sides doing the same work, and the node count is unchanged either way, which is what
proves the scan was dead rather than load-bearing.

So the spine is **not** at parity, and the earlier "1.022x and ahead on every cache axis" was
an artefact of that asymmetry plus a `g++` oracle. Paired time at depth 13 reads 1.31x, worse
than the instruction ratio — the spine has an IPC deficit on top of its instruction deficit.
`../mcfish` measures 1.074x on the same corrected harness against its own base, so roughly a
tenth over upstream is what both ports currently pay for the spine, and neither port's
constraint explains the other's number.

### What the counters say after the dispatch fixes

Both axes, same instrument, clang + PGO on both sides, startup subtracted, node counts
identical. "was" is this branch's starting point, "now" is HEAD:

| | NNUE was | NNUE now | spine was | spine now |
|---|---|---|---|---|
| instructions | 1.742 | **1.699** | 1.115 | **1.093** |
| data reads | 1.449 | 1.475 | 1.223 | 1.228 |
| data writes | 0.741 | **0.683** | 0.698 | **0.707** |
| D1 read misses | 1.222 | 1.201 | 1.062 | 1.057 |
| L1 icache misses | 2.102 | **1.269** | 1.998 | **1.931** |
| conditional branches | 1.337 | 1.369 | 0.984 | **0.977** |
| conditional mispredicts | 2.569 | **2.106** | 1.414 | **1.398** |
| indirect branches | 1.479 | **0.872** | 2.265 | **1.125** |
| indirect mispredicts | 1.490 | **0.779** | 2.248 | **1.005** |

"was" is this branch's starting point, at the previous pin; "now" is HEAD at `c5aef2bf1`. The
two columns are therefore different workloads as well as different code, which is why the
instruction row moves the "wrong" way while every branch row improves — see the note under
the toolchain table.

Three things to read off it, and one of them is a cost rather than a win:

- **The indirect-branch gap is closed.** It came from one defect repeated: a piece type known
  at the call site but passed at RUNTIME, so the match inside `piece_attacks` could not fold
  and left an indirect branch per attacker. Writing the types out at each site brought
  indirect mispredicts to 1.009 of upstream's on the spine and BELOW upstream's on the NNUE
  axis. Look for this shape before looking anywhere else.
- **Instruction-cache pressure on the spine is the worst counter here**, at 1.931. The
  unrollings that closed the indirect-branch gap traded code size for branch behaviour, and
  this is the bill: it peaked at 2.213 before the sync moved the tree. It is paid for several
  times over — the spine now retires FEWER conditional branches than upstream — but the next
  unrolling has to be measured against this counter rather than assumed free.
- **Read traffic did not move**, 1.449 to 1.475 across every change on this branch, and the
  drift is the workload rather than the code. It is the accumulator diff touching two feature
  sets, and only the per-move delta above will move it.

## The quantisation is the specification

Everything is integer arithmetic on a fixed scale, and the shifts are where the scale
changes rather than rounding conveniences:

- The transformer's two halves are clamped to `[0, 255]` and multiplied pairwise, then
  divided by 512. That pairwise product is what gives the first hidden layer a quadratic
  term without a second matrix.
- `ClippedReLU` is `clamp(x >> shift, 0, 127)`.
- `SqrClippedReLU` is `min(127, x² >> (2·shift + 7))`. The extra seven bits stand in for a
  division by 127 that would otherwise cost an instruction; **the trainer knows and
  compensates**, which is why it cannot be "corrected".
- A skip connection adds `fc_0[30] - fc_0[31]` straight to the output. Dropping it costs the
  network its linear term.
- The final scale is `fwd × 600 × 16 / (128 × 2⁶ × 2)`, in `i64`, truncating toward zero.

## No intrinsics

Upstream's kernels are hand-vectorised behind one `#if` per instruction set. `std::arch`
intrinsics are `unsafe` and `std::simd` is nightly, so rfish writes ordinary loops over
fixed-size arrays and lets LLVM vectorise them under `-C target-cpu`.

The arithmetic implemented is upstream's **scalar fallback** — which upstream keeps
precisely so its vector paths have something to be bit-identical to. That is what makes
`nnue-check` a meaningful comparison rather than a comparison of two approximations.

## The net is a runtime input, never embedded

`cargo xtask net` fetches it into `resources/`. The engine looks in the working directory,
then `resources/`, then beside the executable — which is why every gate runs it from
`resources/`.

**A missing net is not an error**, but it is a different engine. `evaluate` falls back to
[`classical`](../crates/rfish-engine/src/eval/classical.rs) and the engine says so on
startup and in `eval`'s own output. A file that exists but fails to load is reported as the
failure it is, rather than falling back silently.

**Check for the `info string NNUE evaluation using …` line before believing any node
count.** A measurement taken without a net is a measurement of a different engine, and the
number looks entirely plausible.

## The classical term is not a feature

Do not tune it. Do not extend it. Do not let it acquire callers NNUE will not satisfy. It
exists so the engine starts and plays without a 90 MiB download, and so every gate above the
evaluation can run without one.

Two properties it does have to hold, because the search depends on them: **antisymmetry** (a
position and its colour-flipped mirror score the same from each mover's point of view — the
tempo bonus is added *after* taking the mover's point of view for exactly this reason) and
**material dominance**.
