# The evaluation zone

`crates/rfish-engine/src/eval/` — the network loader, and the classical scaffolding that
stands in until the forward pass lands.

Golden: `Stockfish/src/evaluate.cpp`, `Stockfish/src/nnue/`.

## Status: NNUE is not ported

Upstream evaluates with an NNUE network and nothing else. rfish's forward pass is **not
written**. What exists:

- `nnue.rs` — the file format. Enough to find a net, read and validate its header, reject a
  file that is not one, and report its name to the UCI layer. Landing the loader first is
  deliberate: every piece of machinery around the net — the `EvalFile` option, the search
  path, the "where do I look for it" rules — is testable before a single weight is
  multiplied.
- `classical.rs` — **scaffolding with a scheduled deletion date.**

This is milestone M3. See `__DEV/PORTING.md`.

## The classical term is not a feature

Do not tune it. Do not extend it. Do not let it acquire callers NNUE will not satisfy.

It exists so the search has a score to order by while the network is being ported, and so
every gate above the evaluation — perft, the move picker's tests, the search's own
invariants — can be built and run before the network is. It is deliberately small and
deliberately untuned: material, a two-phase piece-square table, mobility, and a handful of
pawn-structure terms.

Making it stronger would be effort spent on code with a deletion date, and worse, would
make the engine's strength look like a property of this file.

Two properties it does have to hold, because the search depends on them:

- **Antisymmetry.** A position and its colour-flipped mirror must score the same from each
  mover's point of view. The tempo bonus is added **after** taking the side to move's point
  of view for exactly this reason; adding it before the negation makes the two differ by
  twice the bonus, and the search then sees a different game depending on which side it
  happens to be. A test pins it.
- **Material dominance.** An extra queen must outweigh every positional term combined, or
  the search trades pieces for shape.

## The net is a runtime input, never embedded

Upstream can embed a network into the binary. rfish does not, and will not: the bench
anchor is a property of a file fetched separately, and embedding it would make the anchor
look like a property of this repository instead.

`cargo xtask net` fetches it into `resources/`. `nnue::search_paths` looks in the working
directory, then `resources/`, then beside the executable — which is why every gate runs the
engine from `resources/`.

**A missing net is not an error.** rfish runs on the classical scaffolding and says so on
startup. That is what makes every gate above the evaluation runnable today.

## When the forward pass lands

The plan, so a later reader knows what was intended rather than guessing:

- The SIMD is **ordinary loops over fixed-size arrays**, autovectorised under
  `-C target-cpu`. No intrinsics (they are `unsafe`), no `std::simd` (it is nightly). See
  [08-idiomatic-rust.md](08-idiomatic-rust.md) §8, which is also where the first
  measurement against upstream's intrinsics goes.
- **The scalar path must be bit-identical to the vector path.** That is the property the
  whole evaluation rests on, and it gets its own gate.
- The accumulator's incremental update needs a per-move delta the board zone does not
  compute yet — see [01-engine-board.md](01-engine-board.md), "What is not here yet".
- The `optimism` argument already threads through `eval::evaluate` even though the
  scaffolding ignores it, so no call site changes when the NNUE path arrives.

**Gate:** `eval` output matches upstream to the last unit on every bench position, scalar
and vector paths agreeing.
