# The board zone

`crates/rfish-engine/src/board/` — the value domain, the bitboards, the attack tables, the
position and the move generator. Nothing here knows about search, evaluation, UCI or
threads, and the dependency runs one way. That is what makes **perft a complete test of
this zone**.

Golden: `Stockfish/src/types.h`, `bitboard.cpp`, `attacks.cpp`, `position.cpp`,
`movegen.cpp`.

## `types.rs` — the value domain

Newtypes over fixed-width integers, each with a named total range. The width is
load-bearing: `Piece` packs into `Position::board[64]` and `Square` indexes every attack
table.

- `Color` and `PieceType` are `repr(u8)` enums; `Piece` is `colour << 3 | type`, leaving 7
  and 15 unused so the colour bit sits at a fixed position and `Piece::color` is a shift.
- `Square` is 0..=63 plus `Square::NONE` at **64**. That sentinel is a contract with the
  NNUE feature indexer, which tests raw square bytes against 64. A unit test asserts it.
- `Move` is 16 bits: `type << 14 | (promo - Knight) << 12 | from << 6 | to`. `Move::NONE`
  and `Move::NULL` are the two encodings with `from == to`, which no legal move produces.
- **A castling move encodes the ROOK's square as `to`**, not the king's destination. That
  is upstream's king-takes-rook convention, and it is what makes Chess960 castling fit the
  same 16 bits as every other move.

## `bitboard.rs` — geometry

`Bitboard` is a newtype over `u64` with the set operations spelled as operators, so a
ported expression reads the same on both sides.

`Bitboard::iter` iterates **by value**: the iterator owns a copy, so a loop can freely
mutate the board it came from. That is upstream's `while (b) pop_lsb(b)` over a local, with
the local made explicit.

Every table here is `const`-evaluated — knight and king steps, pawn attacks, the
square-distance matrix. They exist before `main` and cannot be read half built, which is a
class of bug the C++ orders its static initialisers around.

`lsb`, `msb` and `pop_lsb` **panic on the empty set**, matching upstream's assertion. A
silent zero would read as square a1.

## `attacks.rs` — magic bitboards

One multiply, one shift, one indexed load. The tables are built on first use by
`LazyLock` — not by a startup hook a caller has to remember, and not by a `static mut` a
second thread could observe half written.

The magic *search* is here rather than a hardcoded table: a xorshift64\* seeded per rank
with upstream's seeds, so the search takes the same path on every build. A candidate is
accepted when it is injective over the reference set — collisions are allowed only when
both occupancies produce the **same** attack set, which is why the check compares attacks
rather than indices.

Upstream reaches its table through a raw pointer per square. Here the table is one owned
array and a `Magic` carries an offset into it, so every lookup is a bounds-checked slice
index.

`between_bb(a, b)` includes **b** and excludes **a**. That is upstream's convention and it
is load-bearing: the set a checked king may block or capture on must contain the checking
piece's own square, and it makes the knight-check case fall out without a special path.

## `position.rs` — the board state

`Position` holds what a move rewrites in place; `StateInfo` holds what a move cannot
recompute cheaply on the way back. Undo restores by popping a `StateInfo`, never by
recomputing — so **any field added to `StateInfo` must be written by `do_move` before the
recursion**, or unmake restores a stale value.

The state chain is a `Vec<StateInfo>` the position owns rather than a pointer chain through
caller stack frames. See [08-idiomatic-rust.md](08-idiomatic-rust.md) §1 for what that cost
and bought.

`StateInfo`'s fields divide into two groups, exactly as upstream's do:

- **Carried forward and updated incrementally**: the material, pawn, minor-piece and
  non-pawn keys, the non-pawn material totals, `rule50`, `plies_from_null`, the en-passant
  square and the castling rights.
- **Recomputed, never copied**: the position key, the checkers, the pin and blocker sets,
  the per-type check squares, the captured piece and the repetition distance.

The key semantics are upstream's and are easy to get subtly wrong: `pawn_key` is pawns
**only**, seeded with a constant so an empty pawn structure still has a distinct key;
`non_pawn_key` includes **kings**; `minor_piece_key` excludes them. Putting a king in or
out of the wrong one mis-indexes a history table, which costs strength without ever failing
a gate.

`repetition` encodes distance **and** a flag in its sign: positive for a first repetition,
negative when the earlier occurrence was itself a repetition (so this is the threefold),
zero when never repeated.

`Position::state_is_consistent()` recomputes every incrementally maintained field from the
board alone and compares. It is a diagnostic for gates and tests, not something the search
calls.

### En passant is only recorded when the capture exists

A double push sets `ep_square` only when some enemy pawn can actually take. Upstream does
the same, and the position **key** depends on it — a position that is functionally identical
must hash identically.

## `movegen.rs` — the generators

Every generator is PSEUDO-legal except `generate_legal`. `Position::legal` is the filter,
and the search applies it lazily so the cost is paid only for moves it actually searches.

`legal` only checks the three cases that can go wrong, exactly as upstream does: an
en-passant capture that unmasks a rank attack (both pawns leave at once, so neither is a
blocker on its own), a king move onto an attacked square (with the king itself removed from
the occupancy, or it "blocks" the ray it is fleeing), and a pinned piece leaving the pin
line.

**The generated ORDER is part of the contract.** Two engines that generate the same set in a
different order search different trees once move ordering is only partially deterministic.

`perft` and `perft_divide` live here. `tools/perft.table` is the reference battery and is
deliberately **not** a golden: those counts are facts about chess, so a mismatch is always
a bug here.

## What is not here yet

- **Threat deltas.** Upstream's `do_move` records which threat and pawn-pair features a move
  creates and destroys, so the NNUE accumulator can be patched from the move's geometry.
  rfish does not, and no longer needs to: [03-engine-eval.md](03-engine-eval.md) reaches the
  same accumulator by diffing the recomputed feature SETS, which is correct by construction
  and measured at 1.48× the from-scratch cost it replaced.

  A per-move delta would still be faster, because it would remove the set recomputation
  entirely. It is now an optimisation with a number to beat rather than a blocker.
