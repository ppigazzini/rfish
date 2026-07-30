# The search zone

`crates/rfish-engine/src/search/` — the transposition table, the histories, the move
picker, the time manager and the alpha-beta search.

Golden: `Stockfish/src/tt.cpp`, `history.h`, `movepick.cpp`, `timeman.cpp`, `search.cpp`.

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
| `CaptureHistory` | moving piece, destination, victim type | 10692 |
| `ContinuationHistory` | (in check, capture) then parent's (piece, to) | 29952 |
| `PawnHistory` | pawn-key row, piece, destination | 8192 |
| `CorrectionHistory` (×4) | a key row, colour | 1024 |

The continuation planes start at **−40**, not zero: an untouched follow-up must sort below
a move that has merely never worked. A plane of zeros would make "unknown" look as good as
"neutral".

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

Instability scales the optimum: a root move that keeps changing is a sign the current
answer is not to be trusted, so the search buys more time rather than reporting it. The
scaling is clamped by the maximum, always.

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

The static evaluation is **corrected** before any of it: four tables record how far the
evaluation of positions sharing a pawn structure, a minor-piece configuration or a
non-pawn material count has historically been from what the search found, and the node
starts from the corrected value. Upstream also folds in a continuation-correction term keyed
by the last two moves; rfish does not have that table, and its weight is absent rather than
approximated.

The search **never prints**. It reports through the `InfoSink` trait, which the shell
implements as UCI `info` lines and a test implements as a no-op. That is what keeps the
engine crate free of the transport.

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

The pruning *set* is upstream's; the pruning *constants* are not. Several are tuned against
the NNUE evaluation, which now exists, so fitting them is finally a measurable exercise
rather than a guess. Until then the bench signature is rfish's own number — see
[CONTRIBUTING.md](../CONTRIBUTING.md), "Two different numbers". At upstream's own depth 13
rfish searches about 3.45 million nodes against upstream's 3.18 million, which is how close
the trees are.

Also open: the `UCI_Elo` strength limiter, and pondering.
