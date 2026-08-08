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

The state chain is owned by the position rather than pointed at through caller stack frames:
the current state is a field, and every earlier one sits in a `Vec<StateInfo>` behind it. See
[08-idiomatic-rust.md](08-idiomatic-rust.md) §1 for what that costs and buys, and §15.4 for
why the split falls where it does.

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

### `see_ge` — static exchange evaluation

Answers one question and only one: **is the exchange sequence starting with this move worth at
least `threshold`?** Play the cheapest attacker each time and stop as soon as the side to move
can no longer beat the threshold. No king safety, no tactics away from the square.

Two things about it are easy to port wrong:

- **`swap` is a running MARGIN against the threshold, not a score.** It is the material the
  side to move is ahead by if the exchange stops here, which is why the value it holds is an
  `i32` difference rather than a `Value` — see [09-type-design.md](09-type-design.md) on what
  subtracting two of an affine quantity produces.
- **The special move types are answered by fiat, not by replay.** A promotion, an en-passant
  capture or a castling move does not have a material effect a one-square swap-off models, so
  each gets upstream's fixed answer. Replaying them instead is a divergence that costs
  strength without failing a gate.

The three early returns happen before any slider table is read, so the common cheap answers
never touch the magics.

## `cuckoo.rs` — the one move that would repeat

The repetition counter answers "have I been here before". The search also needs "can I get
**back** there in one move", to cut a line that the opponent can force into a draw, and
answering that by generating moves would cost a movegen at every node.

Marcel van Kervinck's construction makes it two table probes instead. Every reversible move's
key delta — `psq[pc][s1] ^ psq[pc][s2] ^ side` — is precomputed into a cuckoo hash, so the
question becomes a lookup on the XOR of two position keys. Two hash functions over disjoint
bit ranges, and an insert that lands on an occupied slot **evicts the sitting tenant to its
other slot** rather than chaining, which is what keeps the probe exact at two reads.

`MOVE_COUNT` is asserted, not assumed: upstream asserts the same total and so does the test
here. A miscount means the piece loop or the attack tables are wrong, and the symptom would
otherwise be a missed draw many plies down, in one game out of many.

The table is the one structure in this zone that **cannot** be `const`: it is keyed by slider
attacks, and those come from magics searched at first use. It is a `LazyLock` for that reason
and no other — see [00-architecture.md](00-architecture.md) on why nothing here has a startup
hook.

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

## Chess960 is data, not a mode

`UCI_Chess960` reaches the board as a flag on `Position::set`, and the design goal is that
almost nothing branches on it. Three things carry the variant instead:

- **The move encoding.** A castling move names the ROOK's square as `to`, in both dialects.
  That is what lets a 960 castling — where the king may move zero squares, or the rook may end
  up where the king started — fit the same 16 bits as every other move.
- **`castling_rook_square` and `castling_path`.** The rook origin per right and the squares
  that must be empty are precomputed per position, so the generator does a data lookup where a
  naive port writes a special case.

Only two places actually test the flag, and both are cases where the data cannot carry it:
**castling legality**, because in 960 the rook's departure can unmask a rank attack on the
king's destination and the standard-chess path check cannot see it, and **FEN output**, which
must name the rook's file in Shredder form rather than `KQkq`. Move notation is a third, and
it lives with the generators: standard chess names the king's destination, 960 names the
rook's square.

`perft` covers the 960 castling positions as its own battery, because a data lookup that is
subtly wrong generates a legal-looking move set.

## The threat recording is here, and nothing consumes it

`do_move_recording` is upstream's `do_move` with the bookkeeping attached: it records which
threat and pawn-pair features a move creates and destroys, into a caller-owned
`Vec<DirtyThreat>` and a `DirtyPawnPairs`. It is tested against the rebuild in
`board/threats.rs`, and the search calls it on every move.

**The evaluation does not use it**, and that is a measured decision rather than an unfinished
one: patching the accumulator from the delta fires on a minority of evaluations and loses to
diffing the recomputed feature SETS, which is correct by construction.
[03-engine-eval.md](03-engine-eval.md) carries the numbers and the four things that decided
it. Keep the recording working — the decision is about the accumulator, not about the board
zone — and read that page before proposing to consume it again.
