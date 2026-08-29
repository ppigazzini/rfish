# The search zone

`crates/rfish-engine/src/search/` — the transposition table, the histories, the move
picker, the time manager, the score model, the strength limiter and the alpha-beta search.

Golden: `Stockfish/src/tt.cpp`, `history.h`, `movepick.cpp`, `timeman.cpp`, `score.cpp`,
`search.cpp`.

## `tt.rs` — the shared transposition table

Atomics, `Relaxed`, keeping upstream's race and dropping the undefined behaviour. The
32-byte cluster layout and the cluster-count arithmetic are upstream's to the byte, because
the count decides which positions collide. See
[08-idiomatic-rust.md](08-idiomatic-rust.md) §2.

`probe` returns a `TTProbe` that is **also the write handle**: it carries the cluster and
the slot the replacement policy already chose, so the policy runs once per position rather
than once per probe and once per store.

`value_to_tt` / `value_from_tt` convert a mate score between root-relative and node-relative
form. The `from` direction also carries upstream's fifty-move guard: a mate found under a
nearly expired halfmove clock is demoted, because the rule may draw the game before the
mate arrives.

## `history.rs` — the ordering tables

Every table is a **gravity** table. An update moves the stored value toward the bonus by an
amount proportional to how far it still is from the clamp, so a value saturates smoothly
instead of pinning. That single rule is upstream's `StatsEntry::operator<<`, and the whole
ordering rests on it: a table that simply added and clamped would lose the ordering
information between two moves that had both saturated.

| Table | Indexed by | Clamp |
|---|---|---|
| `ButterflyHistory` | side to move, `from_to` | 7183 |
| `LowPlyHistory` | ply, `from_to` | 7183 |
| `CaptureHistory` | moving piece, destination, victim type | 10692 |
| `ContinuationHistory` | (in check, capture) then parent's (piece, to) | 30000 |
| `ContinuationCorrectionHistory` | an ancestor's plane, then (piece, to) | 1024 |
| `PawnHistory` | pawn-key row, piece, destination | 8192 |
| `CorrectionHistory` (×4) | a key row, colour | 1024 |
| `tt_move` | nothing — one counter, not a table | 8192 |

The continuation planes start at **−40**, not zero: an untouched follow-up must sort below
a move that has merely never worked. A plane of zeros would make "unknown" look as good as
"neutral".

**The per-ply continuation bonus WRAPS, and the wrap is upstream's.**
`bonus * weight * multiplier / 131_072` is three `i32`s: `weight` reaches 1040 and
`multiplier` 126, so any `|bonus|` above ~16,400 leaves the type, and the fail-low caller
reaches it — `scaled_bonus * 263 / 16384` passes 30,000 once the histories saturate. Upstream
computes all three in `int`, a release build wraps, and the intended bonus is applied at
nearly full magnitude with the OPPOSITE sign. `SearchWorker::continuation_delta` spells the
wrap with `wrapping_mul` rather than inheriting it from the profile, because a release build
wrapped silently while the gate profile PANICKED on the same input — the split that profile
exists to catch. It is spelled at that one site and not in `Bonus`'s `Mul`, so every other
bonus formula keeps the gate profile's detection.

The two factors are REGROUPED, and only the outer product is left at run time. Both are
decided before the bonus is — the weight is a constant of the ply and the multiplier a
run-time index into a constant — so `CMHC_SCALED` tables `weight * multiplier` and
`continuation_delta` takes the product. Wrapping multiplication is associative, so the wrap
above and the sign flip it produces are unchanged; the inner product cannot itself wrap, at
1040 * 126 = 131,040. Worth −233,645 instructions at avx2 and −217,428 at sse41, bit-exact,
from ../Stockfish `refish` 56c6bfdd — whose other half, a shared counter loaded twice because
a relaxed atomic may not be folded, has no analogue: these tables are plain `i16`.

The width is reproduced, not repaired: widening to `i64` removes the sign flip and diverges
from the golden on every input that overflows. The bench never reaches the wrap, so no gated
number depends on it — a search on a real clock does, which is why no gate could have found
it. `../Stockfish refish` measured six exact repairs in C++ and every one cost ~0.08%, because
signed overflow is UB there: the compiler derives `|quotient| < 16384`, proves the clamp in
`operator<<` dead and deletes it, and any repair that widens the provable range brings it
back. Rust grants no such licence, so `apply_gravity` clamps either way and the repair that
costs them 0.08% costs this port nothing — `codegen-equiv` reports all 1030 symbols
byte-identical. The one divergence left cannot be closed: upstream's deleted clamp lets a
wrapped bonus reach its tables raw, where this port clamps it, and there is no defined
behaviour to be exact to.

The tables are large and per-worker, and are boxed so a `SearchWorker` stays movable and a
thread does not need a megabyte-deep stack.

## `movepick.rs` — the staged picker

Moves are yielded in the order the search wants to try them, and each stage is generated
only when the previous one runs out. Most nodes cut off on the transposition move or the
first good capture, so generating the quiet moves for those nodes would be most of the
generator's cost for nothing.

Main search: transposition move → good captures → killers and counter → quiets → bad
captures. In check: every evasion, captures first. Quiescence outside check: captures and
promotions only.

The picker **holds no borrows** — see [08-idiomatic-rust.md](08-idiomatic-rust.md) §4. That
is the one place the borrow checker changed the shape of a type rather than its
implementation.

`pick_best` is a selection sort, one element per call. A full sort would order moves the
search never reaches.

`skip_quiets` is read at **every** call, not fixed at construction: the search decides
mid-node that it has seen enough quiet moves, and the picker has to honour that from the
next call on.

## `timeman.rs` — the budget

Two numbers. The **optimum** is the point past which a new iteration should not be started —
stopping between iterations is free, because the last completed one already has a best
move. The **maximum** is the point past which the search stops wherever it is.

Four factors scale the optimum between iterations, and each answers a different question
about whether the current answer can be trusted. A **falling evaluation**, against the last
move and against the last four iterations, says the position is turning and buys time. A
best move that has been **stable** across iterations is unlikely to change now and sells it
back. **Best-move changes**, pooled across every thread, buy time. And a best move that
already accounts for most of the tree — its `effort` — has little left that could displace
it, so it sells time too. The product is clamped by the maximum, always.

The manager outlives one move, which is why it lives on the pool rather than in a worker.
Two of its fields are whole-game state: the `nodestime` node budget is spent across the
game, and the time-left factor is derived on the first move and reused. `ucinewgame` clears
both.

**Under `nodestime` the clock is not a clock.** The remaining time, the increment and the
move overhead are all multiplied into node counts, and the search is measured against nodes
searched. The `time` a GUI is told stays real milliseconds — it asked how long the engine
thought, not how the engine chose to count.

**A clock is bounded where it ENTERS, at `Limits::MAX_CLOCK_MS`.** UCI puts no range on
`wtime`, and upstream reads one into a signed 64-bit `TimePoint`, so
`go wtime 4e18 winc 4e18` reaches the horizon below

```text
time + inc * (mtg - 1) - move_overhead * (2 + mtg)
```

with `mtg` at 50, and `4e18 * 49` is twenty times what an `i64` holds. Upstream's arithmetic
is undefined there; a release build of this port would WRAP and budget the move from a
negative horizon, so what a user sees is a move played instantly rather than a crash, and the
gate profile's overflow checks turn the same line into a panic. The clamp is 1e12 ms —
thirty-one years, far above any real control and far below where the horizon, the `nodestime`
conversion at up to 10000 nodes per millisecond and the `Move Overhead` product can leave the
type. `../zfish` bounds the same input at the same value, so the two ports agree on what a
clock is.

It is **symmetric**, and that is not tidiness: a negative clock is a real state — a GUI whose
engine has overstepped sends one, and the manager budgets from it deliberately — so folding
it to zero would take the unmanaged path and search on. The horizon itself saturates as well,
so no CALLER can panic the engine and not merely no UCI line; saturating is identical to `+`
and `*` for every value in range, so no gated number moves.

**`mtg` is the term that had to be widened rather than bounded.** The two horizon products
convert it to `i64`, and the subtraction used to happen BEFORE the conversion — at `i32`,
where both edges of the type are reachable from one `go` line. `movestogo` arrives unbounded
and upstream accepts a negative one; and the sub-second taper above casts
`scaled_time * 0.05` to `i32`, which a negative clock inside `MAX_CLOCK_MS` drives past
`i32::MIN`, where Rust's float cast saturates and C++'s conversion is undefined. So the clock
ALONE reaches it, with no `movestogo` given. Forming both terms at `i64` is free — the
conversion happens either way — and exactly equal for every `i32`, which is why `movestogo` is
NOT clamped at the parser the way the clock and the mate count are: there is no arithmetic
left with no room for it, and a clamp would change what `go movestogo -5` searches, which
upstream defines and plays at depth 1.

**`mate` IS clamped at the parser**, at `Limits::MAX_MATE` = `i32::MAX / 2`, because there the
arithmetic is the stop condition itself: `go mate N` stops when
`VALUE_MATE - |score| <= 2 * N`, so the DOUBLE is what has to fit. Upstream's `2 * 2147483647`
wraps to `-2`, the condition can then never hold, and the search runs on past a mate it
already has. The clamp keeps the sign for `clamp_clock`'s reason — `2 * i32::MIN` leaves the
type as surely — and folding a negative count to zero would be the larger change, since zero
is how the field spells ABSENT. The comparison saturates behind it as well.

## `skill.rs` — playing below full strength

A weakened engine must not simply search less deeply. That produces an opponent which
blunders at random, which is no easier to plan against and no more fun to play. Upstream
searches at FULL strength, keeps four principal variations behind the GUI's back, and picks
among them with a bias that widens as the level drops — so every move it plays is one it
genuinely considered.

`UCI_Elo` wins over `Skill Level` when `UCI_LimitStrength` is set, because a GUI asking for
a rating has asked the more specific question. The polynomial mapping one to the other is a
fit against real games, so the ratings mean ratings rather than being a scale of the
engine's own invention.

The pick happens ONCE, at a depth set by the level, and later iterations do not revise it.
That is what keeps a weak level weak when it has time to spare. The thread vote is also
skipped while a handicap is active: it has chosen a weaker move on purpose, and a vote
would put the best one back.

Its generator is seeded from the clock — the one place in this engine where
reproducibility is the wrong property, because two games at the same level should not
follow the same script.

## `score.rs` — what a score means

The search works in units whose scale is a property of the network. Reporting them raw
would make `cp 200` mean something different after every net change, so the reported
centipawn is defined through a logistic fitted to real results, whose parameters depend on
the material left on the board. The same evaluation therefore reports lower in an endgame,
where there is less left to convert it with. `UCI_ShowWDL` reads the same model.

Three kinds of score stay distinct rather than being flattened into one number: a **mate**
is a distance, a **tablebase verdict** is a fact reported at a fixed magnitude above any
evaluation and below any mate, and everything else is an estimate. Only the estimate goes
through the model.

## `worker.rs` — the search

A `SearchWorker` owns its position, its histories and its stack. The only things it shares
are the transposition table and the stop/counter signals, both atomic.

`node::<PV>` is a const generic rather than a runtime flag, so the optimiser drops the
PV-only bookkeeping from the zero-window instantiation — which is the overwhelming majority
of nodes.

The pruning set, in the order the node applies it:

1. **Draw and ply-ceiling checks**, then **mate-distance pruning**.
2. **Transposition cutoff** — a stored score searched at least as deep, whose bound is on
   the right side of the window.
3. **Reverse futility** — already so far above beta that giving away material could not
   bring it below.
4. **Razoring** — far below alpha at low depth; verify with a quiescence search.
5. **Null-move pruning** — skipped in a pawn endgame, where zugzwang makes "pass" a
   genuinely bad option and the assumption fails.
6. **Internal iterative reduction** — a PV or cut node with no transposition move has no
   ordering to work with.
7. Per move: **singular extensions** and multi-cut, **late move pruning**, **futility**,
   **SEE pruning** for quiets and captures.
8. **Late move reductions**, with a re-search at full depth when the reduced search beats
   alpha.

**A mate hunt changes two of them.** `seek_mate` is one predicate asked of the ROOT — the
iteration is at depth 16 or more and the line being reported already scores past 2000, so the
search is chasing a mate rather than an advantage — and every node of that iteration reads
the same answer. While it holds, reverse futility prunes only below depth 6 instead of below
19, and the singular extension stands down entirely. Both readers exist so the tree collapses
onto the mating line instead of re-proving the moves around it; upstream states that the
futility cutoff is not a tuning knob, and the constants are its.

The static evaluation is **corrected** before any of it: five terms record how far the
evaluation of positions like this one has historically been from what the search found, and
the node starts from the corrected value. Four are keyed by a summary of the position — the
pawn structure, the minor-piece configuration, and each side's non-pawn material. The fifth
is keyed by the pair of moves that led here, and where there is no previous move to key on
it falls back to a large constant rather than to zero: that constant is what the sum looks
like when the other four have nothing to say.

The search **never prints**. It reports through the `InfoSink` trait, which the shell
implements as UCI `info` lines and a test implements as a no-op. That is what keeps the
engine crate free of the transport.

Three things are reported: a completed iteration (`depth_finished`), a root with no legal
moves (`no_moves`), and **the root move now being searched** (`current_move`). The third is
the one the port nearly lost. Upstream reaches its reporter from inside the node body
through `main_manager()->updates`, a global; safe Rust has none, so the reporter has to
travel down the recursion — and rfish had declared the hook, written the `nodes >
NODES_LIMIT_OUTPUT` guard at the root, and left the body of that guard EMPTY, under a
comment saying the shell would report it. Nothing did. The whole
`info depth N currmove X currmovenumber M` line was missing from the port, and no gate could
see it: the line is printed only past ten million nodes, which no golden, fixture or bench
reaches.

**How it travels is a measurement, not a taste.** Upstream's announcement sits under
`if (rootNode && ...)`, and `rootNode` is a template constant — so the block, the global it
names and the cost of reaching it are all deleted from every instantiation but the root's.
Carrying a live pointer down rfish's recursion instead is not free, and `perf-budget`
priced two ways of doing it against a `bench` the change cannot move by a single node:

| how the reporter reaches the root | instructions | delta |
|---|---|---|
| a `SearchCtx { tt, sink }` replacing the table parameter | 1,506,248,448 | +0.0281% |
| the same, with the table hoisted into a local | 1,506,439,589 | +0.0408% |
| `N::Announcer`: the sink at `Root`, `()` everywhere else | 1,505,856,945 | **+0.0021%** |

The tolerance is 0.005%, so the first two are gates-red and the third is not. `Announces`
is a second associated type beside `NodeKind::Quiescent`, and its zero-sized arm is the
whole point: Rust passes no register and no stack slot for `()`, and `Announce for ()` is
an empty body, so a non-root node emits exactly the code it emitted before. It is a second
trait rather than a field on `NodeKind` because `NodeKind` lives in the board zone and the
sink lives in the search zone — the search may name the board's types, and not the reverse.

Hoisting the table into a local measuring **worse** than reading it through the context is
worth keeping in mind before the next such reformulation: it is the same shape as the
`--profile profiling` trap, an argument about loads that the instrument did not agree with.

`qsearch` takes the table alone and no announcer. Quiescence has no root, so there is
nothing there to announce.

`check_limits` runs every 1024 nodes rather than every node: reading a clock is a syscall on
some platforms, and at a few million nodes per second the granularity is well under a
millisecond either way.

## Singular extensions, and the two bugs they introduced

If the transposition move is much better than every alternative, the node hinges on it and
it is searched a ply deeper. "Much better" is measured by re-searching the node with that
move **excluded**, at half depth and a zero window. Three outcomes:

- everything else falls short → **extend** by a ply;
- the node still fails high without it → **multi-cut**: more than one move works, so the
  whole subtree can be skipped;
- neither, at a node expected to fail high → **reduce** the move in favour of the others.

### What the two paths that do NOT extend leave behind

**Multi-cut also feeds the correction history.** The excluded-move search returning above
beta says the static evaluation was low, and outside check that difference is recorded the
way a completed search records one. Two details are upstream's and are load-bearing: the
bonus is weighted by the SINGULAR depth — the depth the evidence came from, not the depth of
the node handing it back — and it is clamped to a quarter of `CORRECTION_LIMIT` at both
ends, so one multi-cut cannot move a table entry as far as a real search would.
`multicut_correction_bonus` is a free function precisely so a test can pin both boundaries;
a node count moves when any term changes but cannot say which one did.

**The reduction is a single -3.** It used to be two arms — 3 when the transposition move was
assumed to fail high over beta, 2 when the node was merely a cut node. Upstream collapsed
them, so either condition now reduces by 3 and the -2 arm no longer exists.

Two bugs came out of adding it, and both are worth knowing because neither was caught by
anything except running the thing:

- **The extension must not clamp the child's depth UP.** Writing `new_depth.clamp(1, depth)`
  made a depth-1 node recurse into a depth-1 child forever. Zero is what drops into
  quiescence, and clamping it away removed the search's base case. Every test was a search,
  so every test hung.
- **The child's principal variation must be cleared before each move.** A child that ends in
  quiescence writes no PV, and without the clear the parent spliced in the line a DIFFERENT
  sibling left behind — producing a reported PV whose moves were not legal from the root.
  That one a test did catch.

## What is not faithful yet

The pruning set, the constants and the reduction model are all upstream's now, and the bench
signature is upstream's own number rather than rfish's — see [CONTRIBUTING.md](../CONTRIBUTING.md),
"One number, and what a diff against it means". Every entry matches node for node, and so does
every `bestmove` and `ponder` move, which is what makes a diff a porting regression rather
than a tuning difference.

The time manager, the strength limiter and pondering are no longer on that list: the clock
model is `timeman.cpp` line for line, `Skill` is upstream's, and `ponderhit` converts a
ponder into a real search while honouring a budget that ran out on the opponent's clock.
None of them touch the signature, because a fixed-depth bench never consults a clock.

## The gates

| gate | what it proves here | owned by |
|---|---|---|
| `repro-search` | what a COMPLETED search leaves for the next one: node counts repeat across `ucinewgame`, at twenty budgets | this page |
| `signature` | the search visits upstream's nodes over the bench list | [10-tooling-ci.md](10-tooling-ci.md) |
| `upstream-nodes` | the same, node for node, on positions no fixed list contains | [10-tooling-ci.md](10-tooling-ci.md) |
| `async-check` | an INTERRUPTED search still answers `stop`, `ponderhit` and `quit` | [07-shell.md](07-shell.md) |
| `tsan` | a four-thread search does not race on the table or the counters | [10-tooling-ci.md](10-tooling-ci.md) |

### `repro-search` — what a COMPLETED search leaves for the next one

Node counts repeat across `ucinewgame`, at twenty budgets.

```sh
cargo xtask repro-search
```

**Every other value gate reads the FIRST answer the process gives.** `signature` runs one
bench, `perft` counts a tree, `golden` pins a transcript — none of them asks whether a search
left anything behind. This runs the same two positions twice in one process with a
`ucinewgame` between the rounds and requires the second round to reproduce the first node for
node, so anything the reset misses shows as a divergence: a history table, a stack entry, a
correction bank, a root-move field, a time-manager carry-over.

It is **upstream's own `tests/reprosearch.sh`**, which this port had never taken, and the
budget progression is upstream's — `100 * 3^i / 2^i` for i in 1..=20. The budgets are not
round numbers at any step on purpose: each one stops the search at a different point.

What it cannot see: whether those node counts are the RIGHT ones, which is `signature`'s
question, and what a second thread would do to them. It runs at the default thread count, and
a Lazy-SMP search is not node-reproducible — no gate can make it so.

Upstream's version drives the engine through `expect` and, before that was repaired, a
missing interpreter left `grep` matching nothing, `awk` rejecting nothing, and the script
printing `reprosearch testing OK` having checked nothing at all. This one drives the binary
the way every other gate here does, so there is no interpreter to be absent and no pipeline
whose exit status belongs to its last stage — and a round that reports fewer than four
searches is the failure it looks like rather than a vacuous pass.

Seen to FAIL by mutation, and `negative-control` carries the row: with `ucinewgame` no longer
clearing the worker histories, 33 of 40 searches diverge and it names each one —

```text
differs 332525 nodes, `position startpos`: 332529 nodes before ucinewgame, 332595 after
repro-search: 7 of 40 searches reproduced across ucinewgame
```

The seven that still reproduce are the smallest budgets, where the histories have barely
moved — which is the honest shape of the result rather than a weakness in the row.
