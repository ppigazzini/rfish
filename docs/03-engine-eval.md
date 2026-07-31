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

The last four all say the same thing: the sparse layer's inner loop and the fold are
at a local optimum for what LLVM will emit from safe scalar Rust, and reshaping the
arithmetic to *look* more like upstream's vector kernels makes it worse, because the
shapes upstream chose are the ones its instructions reward and not the ones the
autovectoriser does.

What remains splits roughly three ways. The first affine layer, where upstream's four-way
byte dot does in one instruction what takes four here — four separate attempts to recover
that in scalar form are in the table above and all of them lost. The accumulator fold, now
within about a third of upstream's incremental update. And recomputing the active set at
all, worth roughly 300M, which is the per-move delta this section declined to write and the
only one of the three that a code change could still remove.

The search spine is NOT part of this gap and has not been for some time: measured the same
way with a material evaluation on both sides, rfish is at **1.022×** upstream's instructions
and ahead of it on every cache axis. See `docs/09-tooling-ci.md` for the method.

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
