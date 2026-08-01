# Tablebases

`crates/rfish-engine/src/platform/syzygy/` — the Syzygy prober: discovery, the compressed
value stream, the index computation, and the WDL and DTZ probes.

Golden: `Stockfish/src/syzygy/tbprobe.cpp`.

## Status: ported, and verified against upstream

`cargo xtask tb` drives rfish and a pristine upstream build over the positions in
`tools/cases/tb.fens` and compares the **WDL verdict and the DTZ distance**, position by
position. Every one matches.

That is the only gate shape worth having here. A tablebase answer is EXACT, so "close" is
meaningless: an index computed one off reads a different position's entry and returns a
confident wrong verdict. A golden pinning rfish's own output would pin whatever it
currently does; only a differential comparison catches the off-by-one.

Not done: the 5-man cursed-win and blessed-loss branches, which need a 5-man table set to
exercise, and a block cache, without which a 7-man set would be gigabytes resident. Both
are blocked on table data rather than on code.

## What the files hold

Two per material configuration: `.rtbw` gives the win/draw/loss verdict, `.rtbz` the number
of plies to the next irreversible move. A file is identified by a **material key** — the
Zobrist hash of the piece counts with no square information — because that is what decides
which file answers a position.

Each table has **two** keys, one per side being the stronger. A file stores `KRvK` and never
`KvKR`, so a position where the lone king is White is mirrored before the lookup. That path
has its own entries in the battery, because nothing else reaches it.

## The index is where the compactness lives

A position becomes one integer by collapsing every symmetry it has:

- **Reflection.** The leading piece is folded into the left half; without pawns, also below
  the fifth rank, and then below the a1-h8 diagonal. Three folds, each taking the rest of
  the position with it.
- **Interchangeability.** Identical pieces are unordered, so a group of them contributes a
  binomial coefficient rather than a permutation count. Each later square is renumbered past
  the ones already used, which removes the impossible placements.
- **King legality.** The 4096 king placements collapse to the 462 that are legal and not
  mirrors of each other.
- **Pawns.** With pawns the file is split four ways by which file the leading pawn stands
  on, and the leading pawn is the one nearest an edge — among equal files, the one on the
  lowest rank.

`tables.rs` derives all of it rather than transcribing constants, because each table is a
consequence of the others.

## The payload is compressed three ways over

`pairs.rs` unwinds them:

1. **Recursive Pairing.** The most frequent adjacent pair of symbols is repeatedly replaced
   by a new one, so a single symbol expands into a whole run of values. The expansion is a
   binary descent: a symbol's two children are ADJACENT in the value sequence, so the offset
   picks a side.
2. **A canonical Huffman code** over those symbols. Every symbol of a given length is a
   consecutive integer, so the length is found by comparing the bit buffer against a table
   of lowest symbols rather than by walking a tree.
3. **A block index.** A sparse index gives a nearby block for every `span` values, and the
   per-block value counts walk from there to the exact one.

Two details that produce a plausible wrong number rather than an error if missed: the
payload is **big-endian** while everything around it is little-endian, and the symbol
expansion must be **iterative** — the pairing tree runs thousands deep and a recursive walk
overflows the thread stack.

## The probe is not a bare table read

`probe_wdl` searches the captures first, and `probe_dtz` every clock-zeroing move. That is
not an optimisation: a table stores nothing about **en-passant rights**, so a position with
them would be answered from a state the table does not model. Having searched every legal
move, the table is deliberately NOT consulted at all.

A DTZ file stores one side to move only. When the probe wants the other, the answer comes
from a one-ply search that minimises the distance — the `ChangeStm` path, which the battery
covers explicitly because no ordinary position reaches it.

## No memory mapping

Upstream maps each file into the address space. A mapping is `unsafe` in Rust for a real
reason: the file can be truncated under the map, and the program then reads unmapped memory.
Neither `memmap2` nor any other crate can make that sound, because the unsoundness is in the
operating system's contract rather than in the wrapper.

rfish reads each file into a `Vec<u8>` on first probe and stores **offsets** where upstream
stores pointers — the same information with the aliasing question removed.

**The cost is real and worth stating**: a table is resident in full rather than paged on
demand. For the 3-to-5 man sets that is megabytes and nobody notices. For a 7-man set it
would be gigabytes, and a block cache over positioned reads would be needed before that is
usable. That work is not done.

## OPEN: the extended PV picks a different line from upstream's

`syzygy_extend_pv` walks a won PV out to mate, and rfish's walk reaches mate by a DIFFERENT
route than upstream's. Same position, same score, same node count, same `tbhits`:

```text
rfish     pv h1h4 e3d3 e1f2 d3d2 h4h3 ...
upstream  pv h1h4 e3f3 e1d2 f3g3 h4a4 ...
```

Both lines win and both end in mate, so nothing about the VERDICT differs — `cargo xtask tb`
still matches upstream on all 264 WDL and DTZ probes. What differs is which of several
DTZ-equal moves is shown.

Three candidates were checked and are NOT the cause:

- the mobility tie-break, which is upstream's arithmetic — a reply that captures counts 100
  against the move and any other reply counts 1, and both engines sort ascending on that;
- the stability of the two sorts, which both rely on and both have;
- legal-move generation ORDER, which decides fully-tied moves and which `perft` already
  pins — 10 of 10 positions, and a spot check on the position in question agrees.

That leaves the DTZ ranks themselves: `rank_for_extension` here against upstream's
`TB::rank_root_moves` with `rankDTZ` set. Whoever picks this up should start by dumping both
rank vectors for the position above rather than reading the two functions side by side.

**Until it is closed there is no tablebase case in `tools/cases/`**, because
`cargo xtask golden-audit` adjudicates every case against upstream and would be red on this
one. A green audit over a known difference is the failure this repository has already had
once.

## How the search uses it

At a node below the piece limit and with a **zeroed halfmove clock**, the verdict replaces
the whole subtree. The clock condition is not optional: a tablebase knows the distance to
the next irreversible move, not to mate, so with a running clock its verdict and the
fifty-move rule can disagree.

`Syzygy50MoveRule` off turns a cursed win back into a win, because the caller has said the
rule does not apply. `SyzygyProbeDepth` and `SyzygyProbeLimit` bound where probing is worth
its cost — a probe reads a file, which is far more expensive than the search it replaces at
shallow depths.

**At the root the tables do more than answer a node.** Before the first iteration, a
position within the piece limit and with no castling rights is ranked move by move:
`root_probe` reads DTZ, falling back to `root_probe_wdl` when only WDL tables are present.
Better moves rank higher, certain wins rank equally — unless the caller asked for distances,
which is the case where mate is the only zeroing move and DTZ IS distance to mate. The root
moves are then sorted stably by rank, so moves the tables cannot separate keep their
generated order.

Ranking the root also switches the in-search probe **off** for the rest of the move. Every
move that survives the ranking preserves the result, so re-deriving from a file an answer
already held is pure cost. The one exception is the case the ranking cannot finish: WDL
answered, meaning no distance is known, and the root is not winning.

What the tables say and what the search estimates are kept in separate fields. A ranked
root move carries a `tb_score` alongside the search's `score`, and the reporter shows the
former — the tables know the result exactly, and a search score is an estimate of a fact
already established.

With no path set the registry is empty, no probe fires, no ranking happens, and the bench
signature is unaffected. That property has its own check in the gate.
