# What the types prevent, and what they do not

rfish carries twenty domain newtypes. A reader who counts them will over-trust them, so this
page states the boundary: what a compile error will stop, and what it will not.

`cargo xtask docs-lint` checks that the paths named here exist. It does not check that a
sentence is true — the list below is maintained by hand, and a type added without an entry
here is a page that is quietly wrong.

## The types

| type | owns | file |
| --- | --- | --- |
| `Color`, `PieceType`, `Piece` | the side to move, a kind, a coloured piece | `board/types.rs` |
| `File`, `Rank`, `Square` | the two coordinates and the square they make | `board/types.rs` |
| `SquareOrNone` | a square, or nothing — the en-passant target, a castling rook | `board/types.rs` |
| `CastlingRights` | the rights nibble | `board/types.rs` |
| `Move`, `MoveType` | a packed move and its kind | `board/types.rs` |
| `Value` | a score, in the search's internal units | `board/types.rs` |
| `Ply`, `GamePly` | distance from the root; plies since the game began | `board/types.rs` |
| `Bound`, `NodeType` | a transposition bound kind, a node kind | `board/types.rs` |
| `Bitboard` | a set of squares | `board/bitboard.rs` |
| `DirtyThreat` | one threat feature a placement changes | `board/threats.rs` |
| `StackIx` | a slot in the search stack | `state/mod.rs` |
| `ContKey`, `CorrKey`, `PawnRow` | three flat plane indices into three history tables | `search/history.rs` |
| `Bonus` | a history-table update | `search/history.rs` |
| `Score` | what a score means to the protocol: mate, tablebase, or estimate | `search/score.rs` |
| `KaIndex`, `TpIndex` | the two NNUE feature index spaces | `eval/nnue/features.rs` |
| `TbFile` | which of a pawnful Syzygy table's four sub-tables | `platform/syzygy/tables.rs` |

## What a compile error stops

Each of these was a shape the code accepted before the type existed, and each is now
rejected. The list is the mutation set: every one has been made to fail on purpose.

- **A ply where a game ply belongs**, and the reverse. `Limits.ply` counts from the start of
  the game and feeds the time manager; `Position::is_draw` takes the distance from the root.
- **A margin where a score belongs.** `Value - Value` is an `i32`; there is no `Add<Value>`
  and no `From<i32>`, so a raw number entering the score domain is a visible `Value::new`.
- **A history bonus where a score belongs**, and any other `i32` in scope reaching a history
  update.
- **A continuation plane index where a correction plane index belongs.** The correction space
  is a *subrange* of the continuation space, so this used to read a real plane of the wrong
  table.
- **A ply where a stack slot belongs.** The stack index is rfish's own addressing — upstream
  walks a pointer — so no upstream diff covers an error in it.
- **A board file where a Syzygy sub-table index belongs.** The conversion is
  `TbFile::for_board_file` and there is no other route between the two spaces.
- **A threat feature index where a king-piece feature index belongs**, at the fold boundary.
- **An off-board square.** A `Square` is 0..=63; a step that may leave the board is
  `try_shift`, which returns `Option`.
- **A tablebase verdict blended like an estimate.** `Score` keeps mates, tablebase results
  and evaluations three distinct variants all the way to the protocol.
- **A widened domain type.** `board/types.rs` closes with a `const` block asserting each
  width against the bound that implies it — `PIECE_NB == COLOR_NB * 8` because a piece is
  `colour << 3 | type`, `SQUARE_NB == u64::BITS` because a bitboard is one bit per square.

## What a compile error does NOT stop

This half is the reason the page exists.

**A wrong index that is in range.** Every one of the index types above is a newtype over an
integer, not a refinement over a range. `ContKey` stops a correction plane reaching the
continuation table; it does not stop the *wrong* continuation plane reaching it. The Syzygy
prober is the sharpest case: an index computed one off there returns a confident wrong
verdict, and `TbFile` narrows which space the index lives in, not which entry.

**A depth used as anything.** There is no `Depth` type, deliberately. A depth-scaled product
feeds at least six different codomains — a bonus, two kinds of score margin, a move count, a
history threshold and a reduction denominator — and a `Mul` impl has one output type. See
`__DEV`'s typing notes for the measurement; the short version is that a newtype needing six
output types is a newtype that needs none.

**The four bug classes that cost this port the most.** None of them is a typing problem, and
all four are invisible to perft:

- **Integer semantics.** C++ converts a signed operand to unsigned beside a `u64`, so the
  division floors instead of truncating. A newtype does not change which arithmetic you
  wrote.
- **Generation ORDER.** Two generators emitting the same set in a different sequence search
  different trees, because the move picker's partial sort leaves equal-scored moves in
  generation order.
- **Key identity.** `Position::key()` mixes the halfmove clock in past move 14.
- **State updates that "obviously" belong.** A null move does not advance the halfmove clock;
  an en-passant square is only set when the capture is actually legal.

**Anything inside a fold.** `fold_psqt` indexes two weight tables that have the same element
type, so a `TpIndex` used against the king-piece rows still compiles *within* that function.
The protection is at the call boundary, where the two slice pairs are arguments.

**A niche.** `Option<Square>` is two bytes, not one. Giving `Square` a niche needs either a
64-variant enum — whose construction sits on `Bitboard::pop_lsb`, the hottest line in the
engine — or an unstable feature; `NonZero`'s mechanism requires `unsafe` to construct, which
`forbid(unsafe_code)` closes.

**Performance.** A type is not free here. Six of the newtypes measured zero; four cost
between 0.006% and 0.95% of a bench at one tier while gaining at the other, and the cost is
register allocation inside large inlined functions rather than any instruction the type
added. `cargo xtask perf-budget` is the gate that sees it; `signature` cannot.

## The rule for adding one

A newtype that is **carried** — produced, stored, indexed with — is free. A newtype whose
instances are **live at once inside one large function** perturbs register allocation there,
costs instructions with no attributable symbol, and is not addressable by an inline
attribute. Measure with `perf-budget` at both tiers before believing otherwise, and add a row
to this page either way.
