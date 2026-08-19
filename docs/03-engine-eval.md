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

What is **not** done is upstream's per-move accumulator delta. "The accumulator is updated by
diffing feature sets" describes what stands in its place, and "The measurement ledger" at the
end of this page carries the gap to upstream and every attempt that failed to close it.

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
what proves the evaluation did not change, and `cargo xtask nnue-check` still matches
upstream on every position it carries.

Applying the diff is one sweep of the accumulator, not one per changed feature: the merge
walk collects what changed and a tiled fold applies the whole collection, so the accumulator
is read once and written once however many rows go into it. That is the shape of upstream's
`update_accumulator_incremental`, and it is safe here for a reason worth stating — the
accumulator is wrapping `i16` and the PSQT head `i32`, and both are associative and
commutative under the additions applied, so collecting before applying cannot change a value.

## From network output to a search value

The forward pass returns two heads, and `eval::evaluate` turns them into the number the
search compares against alpha and beta. Every constant below is upstream's, fitted against
this network: changing one is a strength change, not a refactor.

```rust
let mut nnue = i64::from(out.psqt) + i64::from(out.positional);
let complexity = i64::from((out.psqt - out.positional).abs());
optimism += optimism * complexity / 476;
nnue     -= nnue     * complexity / 18236;

let material = 534 * pawns + non_pawn_material_total;
let v = (nnue * (91000 + material) + optimism * 7675) / 91000;

let v = v - (v * pos.rule50_count() / 199).get();
v.clamp(VALUE_TB_LOSS_IN_MAX_PLY + 1, VALUE_TB_WIN_IN_MAX_PLY - 1)
```

Each line is doing something specific, and none of them is a scale factor:

- **Complexity is the disagreement between the two heads.** Where the material head and the
  positional head are far apart the position is sharp, so the network is trusted less and the
  search's own expectation is trusted more — which is why the same term amplifies `optimism`
  and damps `nnue`.
- **Optimism is the search's disposition, not the position's.** It arrives per colour from
  the worker and is blended in at a fixed weight, while only the network's own term scales
  with material. It is one of the things that make Lazy-SMP threads explore differently from
  each other, so it belongs to [04-multithreading.md](04-multithreading.md) as much as to
  this page.
- **The fifty-move damping pulls the score toward zero as the halfmove clock runs.** An
  advantage that cannot be converted before the rule draws the game is not worth its nominal
  value. This one is applied to the classical fallback too, because it is a fact about the
  game rather than about the network.
- **The clamp keeps the value strictly inside the tablebase band.** An evaluation must never
  be mistaken for a tablebase verdict or a mate; those are three distinct kinds of score and
  [02-engine-search.md](02-engine-search.md) keeps them that way.

**The two heads are summed here and nowhere else.** `Add<Value>` between them is deliberately
absent, so the one place where two components become one score is written out rather than
falling out of an operator — see [09-type-design.md](09-type-design.md).

`cargo xtask nnue-check` compares the raw network output, **above** this blend. That is the
right boundary for it: the blend is arithmetic on two integers this repository can read off
the source, while the feature indexing behind `out` is where a port goes silently wrong.

`is_material_draw` sits beside this and is not part of it — king against king and the lone
minors are dead draws whatever any network says, so the search answers them without an
evaluation at all.

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

## No intrinsics, and one nightly feature

Upstream's kernels are hand-vectorised behind one `#if` per instruction set. Every
`std::arch` intrinsic is an `unsafe fn`, so that route is closed here whatever it would buy.
`std::simd` is not: it is safe, needs no `unsafe` block, and leaves `cargo xtask unsafe-lint`
asserting exactly what it always did. That is what the dated nightly pin in
`rust-toolchain.toml` was bought for, and `eval/nnue/layers.rs` and `eval/nnue/transformer.rs`
are where it is spent.

**Which kernels are written in it is a measurement, not a policy.** Where an ordinary loop
over fixed-size arrays already emits upstream's instruction shape, it stays a loop and LLVM
vectorises it under `-C target-cpu`; an explicit rewrite of one such fold cost +177M and was
reverted. [08-idiomatic-rust.md](08-idiomatic-rust.md) §8 records what the constraint cost,
§11 carries that reverted fold with the rest of the falsified list, and §12's vectorisation
subsection records what actually governs the lowering here.

The arithmetic implemented is upstream's **scalar fallback** either way — the one upstream
keeps precisely so its vector paths have something to be bit-identical to. That is what
makes `nnue-check` a meaningful comparison rather than a comparison of two approximations,
and it is why `cargo xtask arch-determinism` exists: `std::simd` lowers differently per tier,
so a saturation that behaves differently at one width is a divergence `signature` cannot see.

## Loading the net, and the axis every gate subtracts

**`cargo xtask perf-budget` subtracts a `quit`-only profile, so no number on this page above
this line includes the net load.** It is not small: startup was **1,281M instructions against
a 1,524M search**, so a `bench` spent nearly half its instructions before it searched a node.
Measure it with the profile the budget subtracts:

```sh
cd resources && echo quit | valgrind --tool=callgrind --callgrind-out-file=/dev/null \
    --cache-sim=no ../target/release/stockfish 2>&1 | grep "I *refs"
/usr/bin/time -f "%e s  %M KB peak" sh -c 'echo quit | ./stockfish > /dev/null'
```

| | startup Ir | peak RSS |
|---|---:|---:|
| before | 1,281,194,773 | 253,344 KB |
| the LEB128 decode, into the destination width | 1,156,853,329 | 190,208 KB |
| **the magic search, sliced to its own guard** | **1,063,324,214** | 190,200 KB |
| the slider's rays decided at compile time | **1,024,945,506** | 190,200 KB |

**−20.0% overall, and −63 MiB.** The first two rows were the same defect — a runtime-length
slice reached by a composite index, which is
[docs/08-idiomatic-rust.md](08-idiomatic-rust.md) §18.1's shape in its second and third zones.

**The fourth row is the one that had no gate until it did.** Startup was measured, subtracted,
printed and never judged, so nothing decided whether it could move; the axis added to
`perf-budget` and `budget-ab` gates it now, and its FIRST run found this. `build_magics` took
its four ray directions as an argument, so one non-generic instantiation served both the rook
and the bishop and every ray step loaded a direction that both call sites pass as a `const`:
−28,659,951 Ir at `avx2` and −27,929,385 at `sse41`, 2.72% and 2.94%, with the search figure
flat and 172,793 nodes on both sides of both runs. Two tiers, same sign, same magnitude — the
shape a deleted-work change has. `../Stockfish refish` files the pattern as P7 and its tell is
what found it: **the callee survives in the profile as ONE symbol however literal every call
site looks.**

What is left on this axis, in order: the net reader at ~624M and the magic build at ~270M are
87% of what remains, and 98.4M of the whole bench process is `memset` whose allocator is not
yet established — see [12-references.md](12-references.md).

- `leb128_i16` decoded into a `vec![0i32; out.len()]` and narrowed afterwards. On the main
  weight block that is 23,068,672 entries — 92 MiB, allocated and page-faulted to be read once.
  It also tested `i == out.len()` once per BYTE where a value takes one or two, and reached the
  destination through a bounds-tested `out[i]`. Walking `out.iter_mut()` and pulling bytes from
  an iterator fixes all three.
- `build_magics` indexed four runtime-length slices per iteration of a loop whose real work is
  a multiply and a shift, and 70.2M of it was `core::slice::index`. Its own `idx >= size` guard
  was already there — reslicing `table` and `epoch` to exactly `size` is what let LLVM use it.

**What rfish cannot do here, and it is the constraint working as intended:** ../mcfish loads
by `mmap` and records loading in a fifth of upstream's cycles because of it. `mmap` is behind
an `unsafe fn`, every crate wrapping it is `unsafe` internally, and the engine crate has zero
dependencies under `forbid(unsafe_code)`. The comparison rfish's decode has to win is against
upstream's own `read_leb_128`, not against a memory map.

One thing remains and it is blocked rather than undone: the compressed block is still
materialised whole, because streaming it needs a refill test per byte and the falsified row
in §18.10 is the measurement saying a per-byte refill costs more than the allocation.

**The block's declared length is a HINT, and the bound is the file.** It is a `u32` read
straight out of the net, so a twenty-two byte file claiming `0xFFFFFFFF` used to reserve four
gibibytes before discovering there was nothing to read into it — upstream's `network.cpp:304`
defect at a second site, the description length one zone over having already been bounded at
`1 << 16`. `take` stops at the declared count and `read_to_end` stops at end-of-input, so a
short file is a short read and becomes `Truncated`. `MAX_BLOCK_HINT` caps what may be
reserved up front at 64 MiB, which clears the largest block a shipped net carries — the
feature transformer's 23,068,672 weights at about 1.28 bytes each — so the common case is
still one allocation. It is a hint and not a maximum: a larger legitimate block still reads,
growing as it goes, and a test pins that distinction so the bound cannot quietly become a
maximum net size that fails the next architecture with the wrong error.

It costs nothing. Same recipe both sides — profiling build at `avx2`, `isready` then `quit`
under callgrind, run from `resources/`:

| | startup Ir | peak RSS |
|---|---:|---:|
| `vec![0u8; byte_count]`, believing the header | 1,077,610,558 | 217,604 KB |
| **bounded against the file** | **1,077,404,501** | 217,536–217,676 KB |

**The RSS is unmoved, and that is the interesting half.** `vec![0u8; n]` at this size is
`alloc_zeroed`, which takes its zeroes from fresh `mmap` pages rather than from a `memset`,
so there was never a zeroing pass to save — an earlier reading of a 26 MiB drop compared a
default-arch binary against an `avx2` one and is withdrawn. The −206,057 instructions are the
allocation path, not the fill. A safety bound that reads as if it should cost something and
measures at the null floor is worth recording as exactly that.

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

## The measurement ledger

Everything above this line describes the evaluation as it is. Everything below it is the
record of what the accumulator cost and what moved it, kept for the same reason
[08-idiomatic-rust.md](08-idiomatic-rust.md) keeps its own: a falsified idea that is not
written down is re-derived, and a measurement is a fact about the tree rather than a story
about the week. [13-writing.md](13-writing.md) names both pages where it forbids history in
shipped prose.

Read it before proposing a change to the accumulator. The rows are the numbers any further
attempt has to beat, and the falsified ones are the attempts already made.

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
| the sparse layer in groups of four, folded by `vpmaddwd` | 2,108M → 2,759M (32 lanes) / 2,698M (64) |

### Groups of four, with the fold this time — and it still loses

The sparse layer's largest single block is the first affine layer, and the reason is known:
upstream's `vpdpbusd` dots FOUR inputs against SIXTEEN outputs in one instruction, and
`std::simd` has no operation for it. The obvious answer is to reach the same shape without
naming the instruction, and both sibling ports point at how — a group-of-four weight layout
plus a deinterleaved widening multiply-add, which LLVM lowers to `vpmaddwd` from generic IR.
../zfish uses exactly that as its portable tier and measures it at +32% against a
hand-written `pmaddubsw`.

It was built here — upstream's scrambled layout `g*OUT*4 + j*4 + m`, the group zero test, the
two-stage even/odd fold — and it is **bit-exact and 28% SLOWER**. Both are worth recording:

- **The fold materialised.** The build emits 42 `vpmaddwd`, against none before. This is not
  LLVM refusing to cooperate, and the next attempt should not go looking for a way to coax
  the pattern out; the pattern was there.
- **The shuffles cost more than the fold saves.** Reaching `vpmaddwd` from portable vectors
  needs a deinterleave of both operands per chunk, and a chunk is 8 outputs at 32 lanes or 16
  at 64. Upstream's instruction does 16 outputs with NO shuffle at all. Widening the chunk
  helped and did not come close to paying: 2,759M at 32 lanes, 2,698M at 64, against 2,108M
  for the per-input kernel.
- **The density argument in this file survives, and now has a second leg.** A group of four
  at ~40% input density survives the zero test about 87% of the time, so the coarser test
  skips almost nothing. The earlier row measured that with the per-input kernel underneath
  and could fairly have been dismissed as testing the wrong cost model — this run replaced
  the cost model and the conclusion held.

What that leaves: the gap on this layer is the INSTRUCTION, not the shape, and closing it
needs `std::arch` — which is out, every intrinsic in it being an `unsafe fn`. Treat the first
affine layer as the price of the constraint until `std::simd` grows a four-way byte dot, and
spend effort on the blocks where rfish is behind for reasons it can still fix.

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
PGO on top of LTO, both trained on the same `bench`, same tier, identical trees, startup
subtracted by a `quit`-only profile. The first four rows are 166,964 nodes and the last two are
163,081; the pin moved the tree, so only the ratios compare across that break:

| both sides | rfish | upstream | ratio |
|---|---|---|---|
| `g++`, no PGO, `nehalem` (the ledger's own tier) | 2,979,106,126 | 1,958,088,252 | 1.521 |
| `g++`, no PGO, avx2 | 2,170,601,764 | 1,460,813,993 | 1.486 |
| clang + PGO + LTO, avx2 | 2,171,591,691 | 1,246,593,188 | 1.742 |
| the same, after the membership diff below | 2,080,763,904 | 1,246,598,569 | 1.669 |
| the same, re-measured at the `c5aef2bf1` pin | 2,108,359,414 | 1,240,674,464 | 1.699 |
| the same, at HEAD | 2,100,251,134 | 1,240,666,337 | 1.693 |
| **the same, after the memory-layout fixes** | **2,067,659,994** | **1,240,666,478** | **1.667** |

The `c5aef2bf1` row is where the pin moved. That step is not a regression in this port: it is
the same code measured against a moved target, and upstream gained slightly more from its own
retune than rfish did — 1.7% on this axis. Re-measure BOTH sides when the pin moves; carrying
an old ratio across a sync compares two different upstreams.

**The last row is two memory-layout defects, and both were invisible to every gate.** Neither
moves a node count, so nothing in the tree could see them:

- `Network::evaluate` allocated the transformed-feature buffer with `vec![0u8; L1]` — a
  malloc, a 1 KiB zero-fill and a free per evaluation, 61,341 of them over this bench, for a
  buffer `transform` overwrites in full before anything reads it. Hoisting it into
  `EvalScratch`, which exists for exactly this and says so in its own doc comment, is **38.4M**.
- Every NNUE weight table was a `Vec`, so its alignment was the ELEMENT's — two bytes for an
  `i16` — and glibc returns the large ones sixteen bytes past a cache line. Upstream declares
  all of them `alignas(CacheLineSize)`; see the section below for what that is worth here and
  what it costs.

Together they are 32.6M, and the same fix for the search tables had already been in
`history.rs` for a long time. **The NNUE arrays never got it.** When an instruction ledger
stops moving, check what the allocator is doing before concluding the gap is algorithmic.

**The gap WIDENS when the comparison is made fair, and the reason is one-sided.** PGO is
worth almost nothing to rustc here and 15% to clang, so the honest figure is the largest of
the three, not the smallest. A number quoted from either of the first two rows understates
where the port is; `docs/10-tooling-ci.md` records what has to be held equal, and
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
| | **not reproducible — see below** | |
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
effects, and it has a tier ceiling — callgrind implements no AVX-512, so the AVX-512 tiers
have a time ratio and no instruction ratio.

**The `native` row above cannot be reproduced, and is kept only as a record.** It was taken
when `native` meant `-C target-cpu=native` on both sides, so it describes two binaries that
were a property of this box rather than of any tier. `native` now SELECTS an enumerated tier
(`avx512icl` here) — see `docs/10-tooling-ci.md` — so a rerun measures a differently
compiled pair and the numbers are not comparable. Re-take it under a named tier before
quoting it.

### The three-way, measured head to head

All three ports are pinned at the SAME upstream commit, so this is not a comparison of
ratios-against-different-baselines: it is four binaries on one box, one bench, one net
(`nn-ab28990d4ea3`), startup subtracted from each by its own `quit`-only profile, and the
**identical 163,081-node tree on all four**, which is what makes the raw counts comparable
rather than only the ratios.

| engine | search instructions | vs upstream |
|---|---|---|
| ../mcfish, clang + PGO + LTO, avx2 | 1,088,367,365 | **0.877** |
| upstream `c5aef2bf1`, clang + PGO + LTO, avx2 | 1,240,667,060 | 1.000 |
| ../zfish, Zig 0.16 + LTO, **no PGO**, avx2 | 1,241,966,545 | **1.001** |
| **rfish, clang-major LLVM + PGO + LTO, avx2** | **2,067,660,319** | **1.667** |

**Every rfish row in the table above is a PGO count taken before the accumulator work below,
and it is stale by 9.3%.** Re-taken non-PGO against ../mcfish built the same way, on the
IDENTICAL 163,081-node tree at avx2, startup subtracted from each by its own `quit`-only
profile:

| avx2, non-PGO, one tree | search instructions | vs rfish |
|---|---|---|
| ../mcfish, `MCFISH_SIMD_VECTOR` (its shipped path) | 1,186,194,103 | 0.79 |
| **rfish, at HEAD** | **1,505,452,714** | **1.00** |
| ../mcfish, its NNUE dot forced to the portable body | 4,755,614,984 | 3.16 |
| ../mcfish, `-DMCFISH_SIMD_SCALAR` | 4,906,184,642 | 3.26 |

**rfish is 1.27x ../mcfish, where this page's PGO table records 1.90x.** The gap closed
because the accumulator sections below are what moved, not because the instrument changed.

**The split this page called unmeasured is now measured, and it is nearly the whole of the
difference.** The build it asked for — every vector op left in place, only `nnue_dot_step`
and `nnue_affine1_dot` forced to their portable branch — is the third row, and it is
**3,569.4M dearer than the shipped one**, four times ../mcfish's whole search. So:

- **the dot intrinsics are worth 3,569.4M there**, and they are `_mm256_maddubs_epi16` /
  `_mm512_dpbusd_epi32`, which is the instruction no safe Rust reaches;
- **the nnz movemask intrinsics are worth 516 instructions** — nothing. rfish's
  `simd_ne(…).to_bitmask()` is already at parity with `_mm256_movemask_ps` there. Disabling
  the movemask paths as well moves the count from 4,755,614,984 to 4,755,614,468;
- so **rfish is 3.16x cheaper than a ../mcfish with no NNUE intrinsics**, and the port's
  standing against the SHIPPED one is what the first two rows say.

Do not read the third row as "what portable vectors cost": ../mcfish's fallback dot is a
scalar lane loop kept so `simd-scalar` can pin bit-identity, not an optimised portable kernel.
It bounds the intrinsic's value from below and says nothing about the best a portable dot
could do — which rfish's own `propagate_sparse` is a much better witness for.

Reproduce by adding a `0 &&` to the six `nnue_dot_step` / contiguous-dot guards in
`../mcfish/src/engine/eval/nnue/simd.h` and rebuilding; every build benches the same 163,081
nodes, so the trees compare directly.

Re-take the PGO row before quoting it; the instrument is `cargo xtask perf` and it rebuilds
both sides.

**rfish retires 1.90x ../mcfish's instructions and 1.67x ../zfish's.** Both siblings are at or
below the engine they clone. This port is 67% above it, and it is the only one of the three
that is. ../zfish reaches parity with NO profile-guided build at all, so the gap to it is
understated here rather than flattered.

**This kills the explanation this page used to offer.** The gap was attributed to the
`unsafe` ban, on the grounds that upstream's first affine layer is a `vpdpbusd` no safe Rust
can reach. ../zfish is a safe-SIMD port — Zig's `@Vector`, no unsafe blocks needed — and it
sits at 1.001. The constraint is not what costs 827M instructions.

**What costs them is the accumulator design, and the siblings say so in their source.**
../zfish implements `update_accumulator_incremental_both` and `update_accumulator_hybrid`
against a `DirtyPiece`; ../mcfish carries a seven-byte packed `NnueDirtyPiece`. Both do
upstream's per-move delta. rfish rebuilds both feature sets every evaluation and diffs them,
and it is the only port of the three that does.

**Do not read the sibling ports as evidence that a portable kernel reaches upstream's affine
layer.** ../mcfish's accumulator row ops are portable vector extensions, but every x86 tier of
its `nnue_dot_step` is `immintrin.h` — `_mm512_dpbusd_epi32`, `_mm256_maddubs_epi16`,
`_mm_maddubs_epi16` — and that is the instruction rfish has no safe route to. Its portable
fallback body is a scalar lane loop kept so `simd-scalar` can pin bit-identity, not the tier
any measurement on this page was taken against. The affine gap and the accumulator gap are
different arguments and only the second one the siblings settle.

That reframes the row below. This page has called the per-move delta "a redesign, not an
edit" and priced it at "the ~158M row, not the whole evaluation gap". Against a measured 827M
excess, and two sibling ports that took the delta and landed at 0.877 and 1.001, that price
is the floor of the estimate and not the ceiling. Anyone costing this work should start from
the three-way above, not from the per-symbol split.

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

**Below that is the affine layer, and this page had the reason WRONG.** It said the layer
"would have to match `vpdpbusd`, which `std::simd` has no operation for". Disassembled, the
avx2 oracle contains **zero** `vpdpbusd` — that instruction is VNNI and appears only at
`x86-64-vnni512`. At the tier every measurement here is taken at, upstream's affine layer is:

| | `vpdpbusd` | `vpmaddubsw` | `vpmaddwd` |
|---|---|---|---|
| upstream, avx2 | 0 | **72** | 76 |
| rfish, avx2 | 0 | **0** | 42 |

Neither instruction needs `unsafe`, and rfish already emits 42 `vpmaddwd`. What it emits none
of is `vpmaddubsw`, which does 32 `u8`x`i8` multiply-adds per issue. rfish's kernel widens the
`i8` weights to `i16` and works in the i16 domain, so it moves half the lanes per instruction
and pays for the widening — the `vpmovsxbw` in the fold is the same tax. That is a
representation choice in the kernel, not a missing instruction, and the `vpmaddwd` LLVM
already matches is the harder pattern of the two.

Do not repeat the `vpdpbusd` line. It is only true at a tier nothing here measures, and it
stopped a search for the real difference.

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

**The delta's cost side is measured, and it is 85.7M.** Wiring `do_move_recording` into the
search — recording the threat delta on every move, with nothing yet reading it — moves
`bench 16 1 8` from 2,067,660,319 to 2,153,337,184 at an identical 163,081 nodes. Against the
~158M the threat and pawn-pair rebuild costs, the whole milestone is therefore worth about
**72M net, 1.667 to roughly 1.61** — not the 827M the gap to upstream might suggest.

That figure is a port ratio rather than a defect. Compiling upstream's threat scan out of the
spine oracle sheds 263M at 657,500 nodes, which scales to ~65M on this workload, so upstream
pays ~65M for the bookkeeping rfish pays 85.7M for. Roughly 1.3x, which is where the rest of
this port sits.

### The delta was built, and it loses on a hit rate

Built in full and bit-exact — `update_piece_threats` ported, recorded through `do_move`,
consumed by a fast path in `transform` that derives the child's threat set from the parent's
instead of rebuilding it. The anchor held, `nnue-check` matched upstream throughout, and a gate-build
assertion comparing the derived set against the rebuild on every evaluation.

| | search Ir |
|---|---|
| rebuild (kept) | 2,067,660,319 |
| recording only, nothing reading it | 2,153,337,184 |
| recording + delta path | 2,152,866,131 |

**The delta saves 0.47M against the recording it requires, which costs 85.7M.** The mechanism
is a hit rate, measured rather than inferred: the fast path fires on **11%** of evaluations.
Recording happens on 100% of `do_move`s — 163,081 of them — while the delta is usable only
when the live slot holds this move's parent, and pruning and qsearch mean the next position
evaluated is usually not the immediate child of the last one evaluated. 11% of the ~158M
rebuild is ~17M against 85.7M paid.

**The stack was then built anyway, with the delta, and measured.** The rows in "One slot, not
a stack" were taken with BOTH designs rebuilding the feature set, so they say nothing about a
stack whose whole purpose is to make a delta apply. That objection is correct and the
experiment was redone: ../zfish's architecture, the delta seeded from ply `si - 1` instead of
from the live slot, plumbed through `evaluate` as a stack index.

| | search Ir |
|---|---|
| rebuild, single slot (kept) | 2,067,660,319 |
| single-slot delta, 11% hit | 2,152,866,131 |
| **per-ply stack + delta** | **2,246,124,890** |

Bit-exact — the anchor, `nnue-check` and perft all clean — and **178.5M worse than
the rebuild**. The hit rate is fixed; the seeding is not. Rolling forward from the parent ply
means copying that ply's accumulator AND its threat set into the working slot, ~7.4 KB per
evaluation over 61,341 of them, and that copy costs more than the 158M rebuild it removes.
That copy was then removed, ../zfish's way. `applyCombinedDelta` splits target from source and
takes BOTH feature sets in one pass, so the fold reads the parent and writes the working slot
in the sweep it was already making. Ported here as `fold_combined`: one walk of the 1024
entries applying the king-piece and threat rows together, where `fold_changed` walks twice.

| | search Ir | vs rebuild |
|---|---|---|
| rebuild, single slot (kept) | 2,067,660,319 | — |
| single-slot delta, 11% hit | 2,152,866,131 | +85.2M |
| stack + delta, copy-seeded | 2,246,124,890 | +178.5M |
| **stack + delta, combined no-copy fold** | **2,214,297,204** | **+146.6M** |

Removing the seeding copy recovered 31.8M and left the architecture 146.6M behind the rebuild,
bit-exact throughout. One copy remains — the working slot is saved back into the ply after the
fold, because the tail of `transform` reads the live slot — and on the 31.8M the first copy
cost, removing it is worth roughly another 30M. That still lands near +115M.

Then the parent-was-evaluated restriction was lifted too, which was the last thing separating
this from upstream's `AccumulatorStack::evaluate`: walk back to the nearest COMPUTED ancestor,
concatenate every hop's records, and roll forward in one fold. Netting over the whole chain is
what makes it legal — a feature created at one ply and destroyed at a later one cancels in
either order, and the king square is constant across the chain by construction.

| | search Ir | vs rebuild |
|---|---|---|
| stack + delta, combined no-copy fold, one hop | 2,214,297,204 | +146.6M |
| **the same, multi-hop walk-back (cap 8)** | **2,219,315,470** | **+151.7M** |

**Multi-hop is 5M WORSE than single-hop.** The extra evaluations it makes eligible do not pay
for concatenating the chain and netting over it, and chains break early anyway because every
hop must be pawn-quiet. That closes the argument that the 11% hit rate was the problem: it was
not, and lifting it changes nothing.

So the gap is not the copies and not the hit rate. The recording costs 85.7M on every
`do_move` — 163,081 of them — against a rebuild worth ~158M that only 61,341 evaluations ever
pay, and rfish's existing diff already applies the same 36.5 feature rows per evaluation that
upstream's delta does. The delta is buying the SET CONSTRUCTION only, on a design that had
already made the expensive half cheap.

**Raising that hit rate is the twice-lost stack.** The only way the parent's accumulator is
always available is a slot per ply, which "One slot, not a stack" measured losing twice. A
record stack without accumulators does not rescue it either: after a subtree return the live
slot holds a COUSIN, and no chain of recorded moves connects a cousin to the current position
at all. Upstream escapes this by keeping the accumulator per ply, which is the trade this port
has already measured and rejected.

So the per-move delta is not blocked on effort or on the board zone — both are done and tested
— it is blocked on the accumulator architecture this port chose, and that architecture wins on its own
measurement. **The board-zone half is kept** (`board/threats.rs`, `do_move_recording`) with
its differential tests: it is the expensive, error-prone part, it is proven correct, and
anything that revisits this needs it. Nothing calls it.

### The ablation that reframes all of the above

../zfish ran the ablation this page never did: the SAME port, built three ways, bit-exact on
one 163,081-node tree (`f876cb5b`, `bench 16 1 8` at `x86-64-sse41-popcnt`, box spread
0.00063%).

| ../zfish build | instructions | vs shipped |
|---|---|---|
| incremental + recording (shipped) | 2,756,229,512 | 1.0000 |
| rebuild every eval + recording | 3,785,329,102 | 1.3734 |
| rebuild every eval, no recording — rfish's shape | 3,731,454,159 | 1.3538 |

The recording costs 53.9M there, 1.44% of its own rebuild baseline, and the delta is worth
975.2M — the incremental build is 26.1% CHEAPER than the rebuild. rfish measured the same
architecture 7.1% DEARER than its rebuild. Same design, opposite sign, a 33-point swing.

**The recording was never the variable.** 53.9M there and 85.7M here sit in the same band as
upstream's ~65M. What differs is what the delta gets to SKIP. ../zfish applies its dirty
records straight into accumulator rows and materialises a feature set only on a refresh.
rfish's threat and pawn-pair sets are a materialised `Vec<u32>` that `diff_apply` needs as the
"old" side, so even on the delta path it must build the child's set to leave one behind for
the next ply — it pays the rebuild's dominant cost either way and is left contesting the fold
alone. That is why the rows above move by tens of millions when the prize is in the hundreds,
and why multi-hop could not rescue it: multi-hop buys more hops, and the cost that cannot be
escaped is per-ply set derivation.

**What the set construction actually costs here, measured.** `threat_active` and
`pawn_pair_active` inline wholly into `transform`, so callgrind attributes them by SOURCE FILE
rather than by symbol. Summing the rows a `--profile profiling` build charges to `transform`
from the files only the set walk touches — `features.rs` 126.0M, `board/types.rs` 27.4M,
`board/bitboard.rs` 22.2M, `board/attacks.rs` 5.8M — plus the `Vec` push traffic that builds
the sets — `alloc/vec/mod.rs` 36.6M, `ptr/non_null.rs` 30.8M, `raw_vec` 8.7M — gives **~257M**.
The earlier estimate on this page was ~158M and it was low.

So the prize is ~257M against 85.7M of recording, and the earlier build measured +146.6M
instead of about −170M. The 316M swing is the set construction it never stopped paying. **The
conclusion below — that the architecture wins on its own measurement — was reached in the
wrong regime and does not hold.** What has to change with the stack, and what no previous
attempt changed, is `Side::threats` and `EvalScratch::next_threats`: the delta path must not
build a set at all, and `active_sets` must run only where the accumulator refreshes.

### The chain must be MATERIALISED, not concatenated — and that was the cap

The section below records a delta that beat the rebuild by 38.4M with `HOP_CAP` at two, and
priced the cap as a knob whose value moves with the fold. Both readings were right about the
code they were taken on and wrong about why the cap was small.

**Instrumented, `bench 16 1 8` at avx2, over the same 163,081 nodes:** `rollforward_base`
failed 59,054 times, and the reason was the CAP 54,527 times, a king move 4,357 times and the
root 170 times. **A hop with no records: zero.** The walk-back was never blocked on a broken
chain — it was blocked on the constant put there to contain the concatenation.

And the concatenation is what made the cap necessary, for two reasons rather than one. The
known one is that records do not cancel before they are applied, so a piece that moved twice
contributes four rows. The one this page missed is that a concatenated chain writes only its
LAST ply, so a chain walked once is walked again by the next evaluation. Uncapped, that folds
**28.1 rows per roll-forward against a refresh's 17.3** — the walk-back had become dearer than
the thing it was avoiding.

Upstream does not concatenate. `AccumulatorStack::evaluate` walks back to the nearest computed
ancestor and materialises every ply on the way forward, so each hop applies its own records
and each ply it passes becomes a base the next evaluation reaches in one. Ported here:
`PlySlot` carries the placement `do_move` reached it with and that placement's two king
squares, `roll_forward` diffs each hop's own two boards, and `rollforward_base` tests the WHOLE
chain for a king move rather than only the base — an intermediate stamped against a king square
the position did not have would read as valid to the next evaluation, which concatenation never
had to care about because it wrote no intermediates.

The cap inverts with it. Search instructions at avx2, 163,081 nodes, startup subtracted:

| `HOP_CAP` | 2 | 4 | 8 | 12 | 16 | 32 | 64 |
|---|---|---|---|---|---|---|---|
| concatenated | 2,025M | 2,044M | 2,232M | — | — | — | — |
| hop by hop | 1,682M | 1,628M | 1,603M | **1,601M** | 1,601M | 1,601M | 1,601M |

Twelve is a PLATEAU, not a peak — it is where the chains this search walks run out, so every
larger cap describes the same walk. The cap has stopped being a tuning knob and become a bound
on the worst case: the measured average is **1.32 hops per roll-forward** over 101,639 of them.

| | before | after |
|---|---|---|
| refreshes, of 125,950 perspective computations | 59,054 (46.9%) | **24,311 (19.3%)** |
| — of which a KING MOVE | 4,357 | **24,148 (99.3%)** |
| — of which the cap | 54,527 | **26** |
| search instructions, avx2 | 1,714,434,277 | **1,601,455,733** |

That is **113.0M**, and it is the largest single row this page has carried since the delta
landed. **The cap has stopped being a cause at all** — 99.3% of what refreshed after it was a
king move, which is exactly the shape upstream has, and that named the next lever precisely.
It is taken in the section below.

### A king move is a DELTA, not a refresh

A king move re-indexes every king-piece feature a perspective sees, so the parent's
accumulator looks worthless after one. **Only its king-piece half is.** Its threat half is
still exactly right whenever both king squares put the threat numbering in the same MIRROR
HALF — that numbering is a function of the king's file alone — so the child is reachable by
swapping one half out and leaving the other in place:

```text
  a      = biases + HalfKA(this position, new king)      the new king square's cache
  parent = biases + HalfKA(ancestor, old king) + threats(ancestor)
  b      = biases + HalfKA(ancestor, old king)           the old king square's cache

  a + parent - b = biases + HalfKA(this position, new king) + threats(ancestor)
```

The biases cancel to exactly one copy, which is the property that makes the three-way sum an
accumulator rather than a difference; the chain's own threat records then turn
`threats(ancestor)` into `threats(this position)`. This is upstream's
`update_accumulator_hybrid` and ../mcfish's `nnue_acc_apply_hybrid_delta`.

**It needs a second cache, and that is the part that is a redesign rather than an edit.** The
king-piece terms have to exist WITHOUT threat rows mixed in, and the refresh cache cannot
supply them: its entries are whole accumulators and the two halves cannot be separated once
summed. `EvalScratch::ka_cache` is upstream's `AccumulatorCaches::Cache` — 64 slots per
perspective holding the biases plus the HalfKA rows, updated by board diff exactly as the
refresh cache is.

**What it buys is not the fold — it is that the hybrid builds NO FEATURE SET.** A refresh has
to materialise the whole threat and pawn-pair set through `active_sets` and diff it through
`collect_diff`, and those two are the reason a king move was the dearest thing an evaluation
did.

| | before | after |
|---|---|---|
| refreshes | 24,311 | **13,799** |
| taken as a hybrid | 0 | **10,512**, 1.85 hops each |
| search instructions, avx2 | 1,581,232,932 | **1,558,298,395** |

**Its cap is its OWN constant, and it is small.** A roll-forward materialises every ply it
steps through, so a long chain leaves work done; this path materialises only its last ply, so
a long chain is pure cost — the threat records concatenate without cancelling AND both cache
entries get dragged to a further board on every call, which thrashes them. Same workload:

| `HYBRID_HOP_CAP` | 1 | 2 | **3** | 4 | 5 | 6 | 8 | 12 |
|---|---|---|---|---|---|---|---|---|
| search Ir | 1,566M | 1,560M | **1,558M** | 1,559M | 1,563M | 1,569M | 1,583M | 1,596M |

Twelve — [`HOP_CAP`], the roll-forward's own value — is **38M worse** than three. The two
constants describe opposite economics and must not be shared.

**Asking only about the immediate parent is worth a 17% hit rate**, 4,219 of 24,311, and that
was the first version. A king moves once and every later ply then differs from every ancestor
before the move, so "the king moved on THIS hop" describes a small fraction of the plies that
cannot roll forward; "the nearest computed ancestor is on another square" describes nearly all
of them. Walking back is what took it to 10,512.

### Where the whole session landed

| | search Ir, avx2, 163,081 nodes |
|---|---|
| before this work | 1,743,381,846 |
| the refresh fold's indexed rows | 1,714,434,277 |
| the hop-by-hop roll-forward | 1,601,455,733 |
| `fold_psqt` and `fc_2` vectorised | 1,592,154,121 |
| the active set built per perspective | 1,581,232,932 |
| the king-move delta | 1,558,298,395 |
| **the sparse layer's row addressing** | **1,523,668,306** |

**12.60%**, bit-exact throughout, and against ../mcfish's own non-PGO build on the identical
tree the ratio is **1.28x** where this page's PGO table records 1.90x.

### The tier was the blind spot, and it was worth 160M at sse41

The campaign above is an avx2 record, and every constant in it was swept at avx2. One of them
is REGISTER-RESIDENT, and that makes its optimum a property of the register file rather than
of the kernel: [`TILE`] is the number of accumulator entries the fold carries across its row
loop, and 128 `i16` is eight `ymm` at avx2 and **sixteen `xmm` at sse41, which is the whole
file**. Disassembled, `transform` at sse41 spilled 58 vector stores and reloaded 45 against 10
and 7 at avx2, and the fold's row loop round-tripped one lane through the stack on EVERY
applied row.

Re-swept at HEAD, search instructions on `bench 16 1 8`, startup subtracted:

| TILE | 32 | 64 | 128 | 256 |
|---|---|---|---|---|
| sse41 | 2,594M | **2,261M** | 2,421M | — |
| avx2 | — | 1,688M | **1,505M** | 1,589M |

**64 is worth 160.4M at sse41 — 6.6% — where the previous sweep had measured it LOSING**, and
it costs 183M at avx2. The spills go with it: 58 stores and 45 reloads become 16 and **zero**.
../mcfish tiers the same constant the same way and calls it `ROW_TILE_WIDTH`, 64 / 128 / 256.

Two things made the old verdict stale, and only the second is about the code. The sweep that
set it predates [`FeatureTransformer::fold_hybrid`], which reads THREE source tiles where
[`FeatureTransformer::fold_into`] reads one — so the register context changed under it. And it
was taken at one tier and generalised to the other, which is the mistake this repository warns
about for `--arch` numbers and had not applied to its own constants.

**The avx512 rung is not taken.** Callgrind implements no AVX-512, so the deterministic
instrument stops at avx2 and nothing here can settle 256 there; it is ../mcfish's value and the
analogue by register count, and it stays unclaimed rather than guessed.

**A constant is a candidate for tiering exactly when it decides how much state is live across
a loop.** Sweep those at every tier that can be measured, not at the one the campaign is
written in.

### The pairwise product was walking three runtime-length slices

`Side::acc` is a `Vec<i16>` and `EvalScratch::transformed` an `Aligned<u8>`, so neither carries
its length past the borrow that splits them, and the pairwise product at the tail of
[`FeatureTransformer::transform`] walked all three through zipped iterators. The body was
already good — twenty instructions per thirty-two outputs — but LLVM wrapped it in a
runtime-trip loop, an eight-wide fixup loop and a scalar tail, emitted twice per evaluation.
`L1` is a constant and both halves are exactly half of it; one length test apiece says so.

**5.85M at avx2 and 2.41M at sse41**, bit-exact. Both tiers move the same way, which is what
separates removed work from moved code layout.

**The shape is `docs/08-idiomatic-rust.md` section 17.1, and the pattern sweep had reported it
as having zero hits in this tree** — its grep looked for a `base..base + N` range slice and
could not see `.iter().zip()` over `split_at` halves. A sweep is only as wide as its grep.

### The alignment was in the allocator and not in the type

`Aligned<T>` starts every weight table on a cache line, and upstream's own
`alignas(CacheLineSize)` is what it exists to reproduce. It was not enough, and the reason is
that the guarantee lives in the ALLOCATOR where the instruction needs it in the TYPE.

The fold reaches a row through an `as_chunks` view, so the row is `[i16; TILE]` and its
alignment is the element's two bytes. A legacy-SSE instruction folds a memory operand only
where the compiler can prove sixteen-byte alignment, so at sse41 the row loop emitted

```text
  movdqu (%r14,%rax,1),%xmm0
  psubw  %xmm0,%xmm15
```

where one `psubw` with a memory operand does the same work — **sixteen instructions per
(row, tile) instead of eight**. Storing the table as `Simd<i16, LANE>` puts the alignment in
the type and the load folds. ../mcfish carries the same note on its `_load_a` forms and calls
the alignment load-bearing; this is the same defect, found by reading that comment.

**−57.8M at sse41, −2.56%. avx2 is flat**, and that is not an accident: a VEX-encoded `vpsubw`
takes an unaligned memory operand, and the disassembly there showed the row already folded.
The i8 threat rows were already folded at BOTH tiers — `pmovsxbw` takes a 64-bit operand, and
operands under sixteen bytes are not alignment-checked. Only the i16 half was paying.

**The width is a separate variable from the alignment, and it is where this goes wrong.**
`LANE` is the REGISTER width, not a tuning knob: pinned at eight `i16` — enough for the
alignment, since sixteen bytes is all the fold needs — the avx2 fold ran on `xmm` where it had
run on `ymm`, and `bench 16 1 8` went 1,505,457,814 to **1,834,100,765, +328.6M, +21.8%**. LLVM
does not widen adjacent explicit vectors back up. Explicit vectors buy the alignment and cost
the autovectoriser's freedom to pick the width, so the width has to be named per tier.

### Falsified this round, with numbers

**Making the fold's bound an immediate is a wash.** The weight views are runtime-length slices,
so the bounds test under `index * ROWS_PER_FEATURE + t` reads its length off the stack —
`cmp 0x48(%rsp),%rax`, once per (row, tile). Fixed-size array references make it
`cmp $0x2bfff,%rax`, and the disassembly confirms the load is gone. It is worth **+1.93M at
avx2 and −0.88M at sse41**: the length load was already served from L1 and the per-call
`try_into` costs about what it saves. Section 14.2's win was a length load out of a SCALAR
walk; this one sits beside eight vector ops and disappears into them.

**Pre-scaling the fold's row indices is refuted at avx2 by the disassembly alone.** Section
11.4 sized it at ~6M on the grounds that `index * ROWS_PER_FEATURE` is recomputed per tile. At
avx2 that multiply is `lea (%r14,%rax,8),%rax` — the scale is an addressing mode and the
multiply is FREE. At sse41 `ROWS_PER_FEATURE` is 16, which no `lea` scale reaches, so it is one
`shl` per (row, tile), ~23M gross before the cost of scaling and of four more scratch buffers.
Re-derive it there before spending a session; do not quote the ~6M figure at either tier.

**There is no structural gap left in the accumulator's route mix.** ../mcfish's own counters,
built with `MCFISH_ACC_STATS=1` and run on the IDENTICAL `bench 16 1 8`, report 62,975
evaluations — the same count rfish makes — split as 49,784 shared walks, 13,191 split walks,
2,099 hybrids and **12,044 refreshes**, against rfish's 13,799. The refresh rate, which is the
expensive route, is within 15%. What ../mcfish has that rfish does not is the SHARED walk: one
pass advancing both perspectives, 79% of its evaluations. The feature indices are keyed on each
perspective's own king square and cannot be shared, so what it saves is the duplicated board
diff — worth single-digit millions here, not the hundreds the gap would need.

**Stating the affine layers' widths in their types costs 76.1M at sse41 and saves 6.4M at
avx2.** [`AffineLayer::propagate`], [`AffineLayer::propagate_sparse`] and
[`AffineLayer::propagate_one`] all reach a weight slice whose length comes from the file at run
time, for loops whose length the caller knows. Const-generic input widths plus one `try_into`
per layer make every trip count constant — and LLVM then FULLY UNROLLS loops it had kept
rolled, which at sse41 costs twice the registers and twice the code per iteration. Measured by
reverting one layer at a time: `propagate_one` +37.1M, `propagate_sparse` +31.7M, `propagate`
+7.3M, all at sse41. The typed signatures alone, without the constant-length views, are flat on
both tiers.

**This is the same lever as the pairwise product above, with the opposite sign**, and the
difference is what sits inside the loop: the pairwise body is two vector ops wide and unrolls
for free, an affine step is a whole widened weight row. Make the bound a constant where the
BODY is small; measure at the narrow tier before believing it where the body is not.

**Pairing two inputs per affine step is refuted on codegen, without a bench run.** The layer's
gap to upstream is that four `vpmaddwd` cover one input where `vpmaddubsw` covers four — but
`vpmaddwd` itself computes `a[2i]*b[2i] + a[2i+1]*b[2i+1]`, so interleaving the weight rows by
input PAIRS would fill the half of it that currently carries zeros, with no saturation question
and no byte-domain packing. LLVM has a combine for exactly that IR shape. It does not fire from
portable-simd's swizzles: compiled at haswell, the paired kernel is **52 instructions for two
inputs against 21 for one** — 26 per input against 21 — and keeps eight `vpshufb` while issuing
the same per-input `vpmovsxbd` and `vpmaddwd` count. Same wall as the four-way deinterleave this
page already priced at 651M. The probe is four lines of `simd_swizzle!` and one `objdump`; run
that before wiring a layout change through the loader.

Every row is a defect rather than a tuning gain: three of them are the same addressing shape
in three different kernels, two are loops LLVM would not vectorise because their output width
was small, and the two largest are walk-backs that threw away the work they had just done. Read with the 28.9M in the row below it, `bench 16 1 8` at avx2 went 1,743,381,846 to
1,601,455,733 — **8.14%**, bit-exact throughout, `cargo xtask parity` and
`cargo xtask arch-determinism` both exit 0.

**The fold's own cost is now at parity and the remaining excess is elsewhere.** Measured per
row over the whole bench, rfish applies 32.6 weight rows per evaluation at about 144
instructions each — which is what eight 128-lane tiles of `vpmovsxbw`/`vpsubw` cost, and what
upstream pays for the same row. What rfish still pays and upstream does not is
`collect_diff` and `active_sets`, and both fire only on a REFRESH: cutting the refresh rate is
therefore the lever on them too, and it has just moved 46.9% to 19.3%.

### Two kernels LLVM left scalar, and both were found by DISASSEMBLING

Neither is visible in the source and neither shows as a hot symbol — they are small
functions doing a few times more work than they need to. Both were found by reading
`objdump`, and ../mcfish's source records hitting the same two walls on the same two
kernels, which is what makes them worth naming as a SHAPE rather than as two incidents.

**`fold_psqt` accumulates eight `i32`, which is one AVX2 register exactly** — and it held 33
`mov`s and not one vector instruction. LLVM will not turn an eight-lane integer loop into a
`vpaddd` on its own. ../mcfish says so about its own `nnue_acc_apply_psqt_delta`: "the scalar
8-step loop these replaced stayed scalar (the toolchain does not auto-vectorize integer
loops)". Written as `Simd<i32, 8>` it is **3.3M**.

**`fc_2` is 128 -> 1, so the generic `propagate::<N>` instantiated at `Simd<i32, 1>`** — a
one-lane vector. What LLVM made of that is not the obvious failure, and it is the part worth
remembering:

```text
  vpmovzxbw / vpmovsxbw     8 inputs, 8 weights, widened to i16
  vpmaddwd                  4 partial sums
  vpshufd / vpaddd          \  reduce four lanes to one, EVERY iteration,
  vpshufd / vpaddd          /  because the accumulator in the source is a scalar
  vmovd / add
```

It widened the loop to `xmm` and then put a horizontal reduction inside it, so twelve
instructions per eight inputs sit on a serialised shuffle chain. A dedicated contiguous dot
carrying sixteen `i32` lanes and reducing once is **6.0M**. ../mcfish gives its one-output
layer its own `nnue_affine1_dot` for exactly this reason.

**The shape: a kernel whose output width is small is the one to disassemble.** Both of these
are tiny relative to the fold, and both were doing several times the necessary work precisely
because being small is what stopped the vectoriser caring.

### Falsified, with numbers

**A `Box<[i16; L1]>` accumulator is 6.1M WORSE.** `Side::acc` is a `Vec<i16>`, so the fold's
tile loop has a runtime chunk count where it is always exactly eight, every `src_rows[t]`
carries a bounds test, and each computation calls `resize(L1, 0)` to restate what the type
already says. Fixing all three at once moved `bench 16 1 8` at avx2 from 1,592.2M to 1,598.3M
at an identical node count. Fully unrolling a loop that was already eight tiles of
register-resident work buys nothing and costs code size, which is the counter this port is
already worst on. Do not re-derive it.

### Where the fold stands, per row

| | per (row, tile) |
|---|---|
| the arithmetic — eight `vpmovsxbw` + eight `vpsubw` over 128 lanes | 16 instructions |
| the ADDRESSING — `&tp_rows[index * ROWS_PER_FEATURE + t]` | **8 instructions** |

The row count itself is right: 5.3 rows per hop over 134,525 hops, which is a per-move delta's
worth and what upstream applies. What is left is that `index * ROWS_PER_FEATURE` is recomputed
once per TILE rather than once per row, because the row lists are walked inside the tile loop —
and that the bounds test upstream does not have costs two of the eight.

Pre-scaling the collected indices at their push sites would remove the multiply, and it is
worth about 6M against a 1,592M total. It is NOT taken: `collect_diff` shares those buffers
with a bitmap keyed on the RAW index, so the two would have to disagree about what an entry
means, across four files, for 0.4%. Recorded so the next attempt knows the size before
starting.

### The refresh fold had been left behind by the delta fold

`fold_into` was given fixed-width indexed weight rows and it was worth 19.3M. `fold_mirror` —
the same fold, on the refresh path — was still taking `weights[base..base + TILE]`, so every
row paid a range bounds test and then walked a `zip` iterator over the slice. The same edit is
worth **28.9M** there, because a refresh applies more rows than a one-hop delta does: 20.1
against 13.0, measured.

**When one half of a pair gets an optimisation, check the other half.** Both halves were in
the same file, forty lines apart, and the one that applies MORE rows is the one that was
missed.

### The affine layer is blocked on an instruction, and the density argument is why

Disassembled at avx2, `propagate_sparse` already emits the good shape: per non-zero input it
issues four `vpmovsxbd`, one `vpbroadcastd`, four `vpmaddwd` and four `vpaddd` — twelve vector
operations for thirty-two outputs. LLVM found `vpmaddwd` from `w.cast::<i32>() * splat(x)`
without being asked. There is nothing left to coax out of a per-input kernel.

Upstream and both siblings do the same thirty-two outputs for FOUR inputs in the same twelve
operations, and the instruction that buys it is `vpmaddubsw`. `../mcfish` reaches it with
`_mm256_maddubs_epi16` — it is **not** a portable-C build at this layer, whatever its
portability elsewhere; every x86 tier of `nnue_dot_step` in `src/engine/eval/nnue/simd.h` is
`immintrin.h`. `../zfish` reaches it through Zig's `@Vector`. rfish cannot: LLVM will not
synthesise `vpmaddubsw` from non-saturating IR, and the deinterleave that reaches it from
portable vectors costs more than the fold saves — measured, 2,759M against 2,108M.

**The density this page has always quoted was never measured, and it was nearly three times
too high.** Counted with an instrumented build: 9,551,893 non-zero inputs over 62,975
evaluations, which is **151.7 of 1024 — 14.8%**, against the ~40% this page claimed.

The conclusion survives the correction, and is worth restating with the right number. A group
of four is skippable only when all four are zero, so at 14.8% it survives `1 - 0.852^4` = 47%:
121 group visits replace 151.7 input visits, a fifth fewer, and each would have to do four
inputs' arithmetic. That is a win only with an instruction that dots four bytes at once. Quote
14.8%, and re-count it after any change to the activation feeding this layer.

**What the layer costs is now known exactly, because it was disassembled rather than
reasoned about.** The per-input loop is TWENTY instructions and every one of them is
arithmetic:

```text
  xor / tzcnt / blsr        walk the non-zero mask
  movzbl / vmovd / vpbroadcastd   the input byte, splatted
  shl                       the weight row address
  vpmovsxbd x4              32 weights, widened
  vpmaddwd x4 / vpaddd x4   32 outputs accumulated
  jne
```

No bounds test, no composite index, no spill — the addressing fix below took the last of it.
At 151.7 visits that is ~3,030 instructions per evaluation against upstream's ~1,940 for 121
group visits, so **the layer is ~69M above upstream and all of it is the four `vpmaddwd`
covering ONE input where `vpmaddubsw` covers four.** It is the instruction and not the shape,
and no reformulation in this file will reach it.

### The addressing was nine times the arithmetic

`propagate_sparse` reached its weight row as `blocks[c * SCAN + lane]`, where `blocks` takes
its length from the layer at run time. Line costs over the same bench:

| | Ir |
|---|---|
| `Simd::from_array(blocks[i])` | 66,863,251 |
| `let i = c * SCAN + lane` | 19,103,786 |
| `acc += w.cast::<i32>() * splat(x)` — the work | **9,551,893** |

Nine times the multiply-accumulate's cost, to address it. Chunking the weight rows the same
way the inputs already are makes the bound a CONSTANT — `rows` is `&[[i8; N]; SCAN]` and
`lane` came out of `trailing_zeros()` on a `u64`, so it cannot reach `SCAN` — and LLVM drops
the test. **34.6M.**

That is the third time this shape has paid on this page, after `fold_into` and `fold_mirror`,
and the first outside the transformer. Look for a runtime-length slice indexed by a composite
expression in any kernel that already vectorises.

**Two reshapes of the same kind that measured WORSE**, so the shape is not a rule:

- viewing the transformer's weight tables as whole feature rows and taking tile `t` inside
  one — `tp_rows[index].as_chunks::<TILE>().0[t]` against
  `tp_rows[index * ROWS_PER_FEATURE + t]` — is **9.3M worse**. The composite index wins there
  because the outer bound is data-dependent either way, so the second level buys no constant
  and costs a second address computation.
- `Box<[i16; L1]>` for the accumulator, which gives the fold's tile loop a constant trip
  count, is **6.1M worse** (see below).

### The 2024 refactor, and the four things that decided it

Built. **The per-move delta is now 38.4M cheaper than the rebuild**, at avx2 over the same
163,081 nodes with startup subtracted, bit-exact throughout and `cargo xtask parity` green.

| | search Ir | vs rebuild |
|---|---|---|
| rebuild, single slot (was kept) | 2,063,764,878 | — |
| delta, chains uncapped | 3,330,344,924 | +1,267M |
| the same, pawn pairs filtered | 2,318,621,552 | +255M |
| the same, hop cap 3 | 2,247,130,947 | +183M |
| the same, no-copy combined fold | 2,101,249,794 | +37M |
| **the same, `active_sets` hoisted, hop cap 2** | **2,025,331,895** | **−38.4M** |

The table above is a non-PGO build, which UNDERSTATES the change. Both sides rebuilt with
`cargo xtask pgo --tier avx2`, and the whole counter set beside it:

| | rebuild | delta | ratio |
|---|---|---|---|
| search Ir, PGO | 2,069,877,327 | **1,988,088,860** | **0.960** |
| search Ir, no PGO | 2,063,764,878 | 2,025,331,895 | 0.981 |
| Dr / Dw | 718,892,052 / 261,929,488 | 742,892,459 / 275,619,088 | 1.033 / 1.052 |
| D1mr / D1mw | 83,214,684 / 13,011,462 | 78,379,383 / 17,327,378 | **0.942** / 1.332 |
| I1mr | 11,866,704 | 18,179,235 | **1.532** |
| Bc / Bcm | 474,487,967 / 20,884,890 | 464,954,405 / 20,479,262 | 0.980 / **0.981** |
| Bi / Bim | 1,642,775 / 426,584 | 2,174,052 / 558,245 | 1.323 / 1.309 |

PGO doubles the instruction win, from 1.9% to 4.0%, and the reason is in the `I1mr` row: the
two-path dispatch and the wider fold cost 53% more instruction-cache misses, and code layout
is what PGO does. Against the pinned upstream oracle the NNUE axis moves from 1.667 to
**1.602**.

**Time does not resolve it on this box, and the run says so rather than hiding it.** Seven
interleaved rounds of `bench 16 1 12`, alternating which binary runs first: the median paired
ratio is 1.029 with a spread of 0.915 to 1.157. That straddles 1.000 in both directions, which
is the documented behaviour of this machine below ~10% — the callgrind number is the one to
read, and a wall-clock claim either way would be noise.

Every row after the first is the SAME architecture. What separated a 61% loss from a win was
four things, none of them the delta itself:

- **Chains must be bounded.** Records do not cancel before they are folded, so a piece that
  moved twice contributes four rows rather than two. Uncapped chains cost 843M. The cap is
  not a constant of the design: it sat at three with the copying fold and at two with the
  no-copy one, and it must be re-measured after any change to the fold.
- **`pawn_pair_delta` must not rebuild the set.** Recording the two pawn bitboards and taking
  the difference of their full pair sets is exact — cancellation handles it — and it cost
  96.5M over 144,412 walks. Only a pair with a pawn on a square the move CHANGED can differ.
- **The fold must not copy, and must sweep once.** Seeding the destination from the source
  and folding in place moves 1024 entries the fold is about to overwrite; applying the two
  feature kinds in separate passes sweeps the accumulator twice. Together, 146M.
- **A set must be built at most once per evaluation.** `active_sets` fills BOTH perspectives
  and `refresh` asked for it per perspective, so a double refresh built every set twice and a
  single one built the other perspective's for nothing. 75M — larger than the whole remaining
  margin.

**One bug, and no value gate could have caught it.** Ply zero is the only slot no `do_move`
stamps, so a root left by a PREVIOUS search read as self-consistent and was rolled forward
from. `nnue-check` passed throughout, because it evaluates at ply zero and always refreshes;
only the bench signature moved. The stack is dropped per search; the refresh cache survives,
being exact for any position that reaches it.

**So the conclusion this page reached twice was a measurement artefact, not a property of the
port.** The stack did not lose because a stack is wrong here; it lost because it was measured
with `active_sets` still running on the delta path, with a copying fold, and with the pawn
pairs rebuilt whole. The sentences below are kept as the record of how the wrong answer was
reached, and they are WITHDRAWN.

**The accumulator half must NOT be a per-ply stack, and that is already settled above.** A
delta needs the parent's accumulator, and the obvious way to guarantee one is a slot per ply —
which "One slot, not a stack" records losing twice, by 428M and then by 278M. It does not need
re-deriving a third time. It does not need to: that same section measured why the single slot
wins, and the reason is that a depth-first search evaluates a node and then its CHILD, so the
slot already holds the parent in the common case. The delta therefore rides on the slot that
exists — apply the recorded records when the slot holds this move's parent, fall back to the
rebuild when a subtree return has left a cousin there.

**And read the size before spending the effort.** The 827M excess is not mostly the delta: it
is the first affine layer (~210M) and the per-feature accumulator cost (~199M). ../zfish
reaches 1.001 with SAFE portable SIMD, so the affine half is not walled off by the `unsafe`
ban the way this page used to argue. Cost the delta at 72M and weigh it against those.

**The per-move delta is a REDESIGN, not an edit, and the reason is the refresh cache.**
Upstream can apply a delta because its accumulator is a stack that moves in lockstep with
`do_move`/`undo_move`, so the base is always the parent. rfish keys its accumulator on the
position instead, and diffs against whatever the live slot holds — which after a king move is
a *cache* slot for that king square, a different board, and a different board PER PERSPECTIVE.
A delta therefore cannot be shared between the two perspectives in exactly the case the cache
exists to serve, and the per-ply stack that would fix that has been measured twice and lost
twice (see the falsified table above). Anyone taking this on should cost the bookkeeping
first: the prize is the ~158M row, not the whole evaluation gap.

### The search spine is a SEPARATE axis, and it is at parity

Swap both sides to a material evaluation — `eval-material` here, and on the oracle the same
formula plus a stubbed threat scan, both patched in by `cargo xtask oracle --spine` — and what
is left is the spine: movegen, movepick, the histories, the TT, the pruning arithmetic. Same
tier, same toolchain, same PGO, identical trees:

| oracle | rfish | upstream | ratio |
|---|---|---|---|
| as upstream ships it | 1,445,638,904 | 1,518,970,470 | 0.952 |
| with the NNUE threat scan compiled out | 1,445,638,857 | 1,297,100,189 | 1.115 |
| the same, after the two dispatch fixes below | 1,405,589,511 | 1,297,103,102 | 1.084 |
| the same, re-measured at the `c5aef2bf1` pin | 1,421,995,718 | 1,301,230,180 | 1.093 |
| the same, with only the ORACLE's half of the stub | 1,424,139,177 | 1,301,234,036 | 1.094 |
| the same, once BOTH halves stub | 1,300,345,775 | 1,301,236,589 | 0.999 |
| **the same, after the picker outlining** | **1,294,519,943** | **1,301,236,589** | **0.995** |

The first three rows were measured at the previous pin, over 625,992 nodes; the rest are at
`c5aef2bf1`, over 657,500. Rows from different pins are different workloads and only their
RATIOS are comparable.

**The last two rows are the same tree measured against a harness that was half built.** The
row above the last is what the command printed while `patch_out_threat_scan` stubbed the
oracle's threat scan and nothing stubbed rfish's — which was true from the moment the
accumulator moved to a per-ply delta, seven hours after the stub landed. Re-running the
command in that state at HEAD reads **1.291**, not 1.094, because rfish's own recording had
grown to 380M by then. `SearchWorker::do_move` now carries the mirror under the same
`eval-material` feature and `the_spine_stub_has_its_mirror` fails if it is deleted. **A stub
on one side of a differential is not a measurement; it is a subsidy.**

**The first row is the trap, and this page published its ancestor for a long time.** Upstream
maintains the threat feature set inside `do_move`, writing a `DirtyThreats` that
`nnue/nnue_accumulator.cpp` reads and that NOTHING else reads. Under a material evaluation
nobody reads it at all — so leaving it in charges upstream for NNUE bookkeeping while rfish,
which recomputes threats inside its evaluation, is charged for none. Compiling it out leaves
both sides doing the same work, and the node count is unchanged either way, which is what
proves the scan was dead rather than load-bearing.

**That row was reachable from the harness until `patch_out_threat_scan` existed, and it was
reached.** `cargo xtask oracle --spine` patched only `Eval::evaluate`, so a spine oracle built
from the command alone left the scan in and read **0.910** — below even the 0.952 the trap
produced at the previous pin, because the port's own side had improved underneath it. The
compile-out is now part of the command: it stubs `Position::update_piece_threats`, whose six
call sites are all guarded by `if (dts)`. Rebuilt that way the oracle lands within 4k
instructions of the row above it, which is what establishes that the stub is the step that had
been missing rather than a new one. A number this page publishes has to come from a command,
not from a step someone remembers taking.

So the spine IS at parity on the instruction axis, and the "not closed" this section carried
for a fortnight was the missing half of the stub rather than the port. Paired time at depth 13
read 1.31x when the instruction ratio read 1.094, and that reading has not been re-taken
against the corrected pair — the spine may still have an IPC deficit, but no time number on
this page currently describes the same workload as the table above it.

What the corrected harness does show is that the deficit moved rather than vanished. Measured
the same way, all three engines at avx2 with PGO over the identical 657,500-node tree, split
into the spine and the threat recording that sits on `do_move`:

| | spine | recording |
|---|---:|---:|
| `../mcfish` | 1,200,951,357 | 248,777,576 |
| upstream | 1,301,236,589 | 263,435,937 |
| rfish | **1,294,519,943** | **380,280,100** |
| rfish ÷ upstream | **0.995** | **1.443** |
| rfish ÷ `../mcfish` | 1.078 | 1.528 |

The recording column was taken before the picker outlining and is carried rather than
re-taken: that change is in `movepick`, which is not on the `do_move` recording path. The two
columns are separate measurements and adding them is arithmetic, not a reading.

**Both remaining gaps are real and neither is the spine's pruning or its tables.** The
recording is priced in the section above it; the 1.083 against `../mcfish` is the move picker,
whose zone is 29.0% of rfish's spine against 16.6% of `../mcfish`'s.

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

### What cache-line alignment is worth, measured against itself

The alignment half of that pair does not show on the instruction axis at all — it **costs**
5.8M there, because `Aligned` derefs to a subslice and every site not hoisted pays for the
offset and its bounds test. Line splits are a hardware effect and callgrind counts the load
either way, so the instruction ledger is the wrong instrument and a ratio taken from it would
reject a real effect.

Measured against itself instead: one binary, a temporary switch forcing the old sixteen-byte
offset, so the code, the PGO layout and the instruction count are identical across both arms
and alignment is the only variable. Fifteen interleaved rounds of the depth-13 bench:

| | median misaligned/aligned | spread |
|---|---|---|
| all fifteen rounds | 1.0139 | 0.8977..1.0742 |
| the thirteen after the cold-start pair | 1.0153 | 11 of 13 favour aligned |

**So it is worth about 1.5%, and this box cannot establish it** — the spread straddles 1.000.
Two `perf` runs before the controlled arm existed appeared to show 6%, and that was box state:
rfish's absolute time fell 13% between runs while upstream's fell 6%, on a machine that had
just finished a PGO build. Build the controlled arm before believing a paired ratio taken
across two sessions.

It is kept anyway, for the reason upstream and both siblings keep it. The alignment that
exists without it is allocator luck: the accumulator survives at avx2 only because it happens
to land at `%32 == 0`, and a 64-byte AVX-512 load at `--tier avx512icl` would not forgive
that.

### The spine's IPC deficit is not a data-layout problem

The spine turns 1.094 instructions into 1.33x time, so something outside the instruction count
is costing it. The full counter set at HEAD, against the stubbed-scan oracle, over 657,500
nodes on both sides, says where it is NOT:

| event | rfish | upstream | ratio |
|---|---|---|---|
| Ir | 1,424,141,019 | 1,301,245,716 | 1.094 |
| Dr | 400,680,699 | 324,954,411 | **1.233** |
| Dw | 210,402,533 | 295,755,282 | 0.711 |
| D1mr | 6,837,759 | 6,722,550 | **1.017** |
| D1mw | 3,066,667 | 5,619,896 | 0.546 |
| DLmr | 246,186 | 414,112 | 0.594 |
| DLmw | 1,000,077 | 1,726,939 | 0.579 |
| I1mr | 7,206,111 | 3,520,520 | **2.047** |
| ILmr | 7,478 | 28,928 | 0.259 |
| Bc / Bcm | 192,435,064 / 12,764,700 | 196,421,694 / 9,810,082 | 0.980 / **1.301** |
| Bi / Bim | 2,296,457 / 931,683 | 2,041,830 / 925,797 | 1.125 / 1.006 |

**Alignment is the first thing to suspect and the counters rule it out — ON THIS AXIS.** A
misaligned hot table shows up as L1 data misses, and `D1mr` is 1.017 — parity — while every
last-level data counter is BELOW upstream's. Do not carry that conclusion to the NNUE axis: it
was carried once, and the NNUE weight tables turned out to be misaligned by sixteen bytes (see
above). The spine runs a material evaluation and never touches a weight table, so these
counters say nothing whatever about them. The structures agree: the TT `Cluster` is 32 bytes at
`repr(align(32))`, matching upstream's `static_assert(sizeof(Cluster) == 32)`; the history rows
carry `repr(align(64))` through `Line<T>` after the flat `Box<[i16]>` skew was found and fixed;
`CorrectionHistory`'s rows are sixteen bytes and so cannot straddle a line. Do not spend a
session re-aligning these.

What is left is code size and prediction. `I1mr` at 2.047 is the bill for the unrollings that
closed the indirect-branch gap — and `ILmr` at 0.259 says those misses are served from L2
rather than from memory, which is why the cost shows up as IPC rather than as stalls that would
dominate. `Bcm` at 1.301 over `Bc` at 0.980 is the other half: rfish retires fewer conditional
branches than upstream and mispredicts more of them. Read traffic at 1.233 is the one
data-shaped lead, and `StackEntry` at 72 bytes against upstream's 56 is part of it — but the
56-byte alternative was measured and was worse, so that row is a REDESIGN and not an edit.

