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

## The extended PV, and the two defects it took to make it match

`syzygy_extend_pv` walks a won PV out to mate by repeatedly taking the top-ranked move. Its
output diverged from upstream's for the same position, at the same score and the same node
count, and it took two independent fixes to close.

**Generation order decided ties nobody else broke.** Dumping the ranks for the first
extension step of `8/8/8/8/8/4k3/8/4K2R w` after `h1h4` showed both black replies exactly
tied:

```text
TBRANK ply=1 mv=e3d3 pen=-17 rank=-262126
TBRANK ply=1 mv=e3f3 pen=-17 rank=-262126
```

Neither the DTZ rank nor the mobility tie-break chose between them, so whichever the legal
generator emitted first won. `generate_legal` was compacting stably where upstream filters by
moving the LAST element over the rejected one — see `board/movegen.rs`.

**`dtz_is_dtm` was applied at the root and nowhere else.** Upstream's OR lives INSIDE
`rank_root_moves`, so it reaches every caller; rfish had it at the root ranking only. When
mate is the only zeroing move DTZ *is* distance to mate, so ranking by it distinguishes a
shorter win from a longer one. Without it every win in step 1's walk was equally top-ranked,
the walk never truncated, and rfish displayed the SEARCH's continuation where upstream
displays the tables' shortest mate. That is why the two agreed at depth 1 and 2 and diverged
from depth 3 on — the shallow PVs were short enough that there was nothing left to truncate.

`tools/cases/tbpv.uci` pins both, and `cargo xtask golden-audit` adjudicates it against
upstream rather than against a previous run of rfish.

**The extension is capped at `MAX_PLY`, and under one reachable configuration that cap is
its only guard.** Step 2 walks the tables playing the shortest win until the line mates, and
its three exits are not the three they look like:

| exit | when it is dead |
|---|---|
| `while !(rule50 && pos.is_draw(..))` | `Syzygy50MoveRule false` reduces the whole guard to `while true` |
| `if timed_out(&start)` | `timed_out` is `uses_clock && ...`, so `go infinite` makes it constantly false |
| `if legal.is_empty()` — mate | the only one left, and it assumes the tables always rank a move that shortens the win |

That last assumption is about FILE DATA, which is the same class this prober refuses
everywhere else. A table ranking a repetition top loops here forever, pushing a move and a
board state per iteration — and a hang is the defect class a UCI host cannot tell from a slow
engine, arriving with unbounded memory attached because a `Vec` grows where upstream's
fixed-capacity `PVMoves` would hit an assert `-DNDEBUG` deletes. `MAX_PLY` costs nothing
where the loop terminates: it is the longest line the rest of the engine can represent, and a
won 3-man ending mates in a small fraction of it.

`../Stockfish`'s `refish` branch records this as a LATENT row — a real defect it could not
reproduce, having no tablebases on that host. This repository ships a 3-man set, so the
configuration is reachable here. The repetition check that row also names is deliberately NOT
added: it would change which line is reported, and a bound is not a behaviour change.

**The audit rewrites `SyzygyPath` for the oracle.** Every case runs from the engine's own
directory, so a relative path resolves against the ORACLE's source tree — `src/syzygy/`,
which holds C++ files and no tables. The oracle then finds nothing and the case reads as a
divergence that is really a rig with no tablebases in it. `rewrite_syzygy_path` makes a
relative value absolute against this repository's `resources/`, so both engines read the same
files. ../zfish records the identical trap.

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

**A drawn root move is drawn, whatever the table says.** Both root probes open by testing the
position the move reaches — `root_probe` as `(rule50 && is_draw) || is_repetition`,
`root_probe_wdl` as a bare `is_draw`. The WDL fallback was missing that test entirely, a
porting omission rather than a decision, so a move that draws by repetition or by the halfmove
clock was ranked and scored on what the table says about the material, for a game the rules
have already ended.

The restored test **ignores `Syzygy50MoveRule`**, which is upstream's shape and a defect there
rather than a choice: with the option OFF — the setting whose whole meaning is that the clock
does not end the game — every root move becomes a draw once the clock crosses 99, and the
ranking then switches the in-search probe off under a won position. It is inherited
deliberately, because this port is bit-exact to the pin; `../Stockfish refish` reports the same
finding against upstream, and the overrun it caused there — the PV walk past its array — is
already bounded here. No gate can see this: `tb` compares WDL and DTZ probe by probe and
nothing drives the root RANKING against the oracle, so the closest instrument is a unit test,
and the fallback is only reachable with WDL-only tables, which the shipped 3-man corpus is not.

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

## What the prober COSTS, and the gate that can see it

**The symbol length is ONE LOAD, not a walk, and that was 12% of the prober.** The decode loop
found each symbol's length by walking up `base64` until the padded bitstream word stopped being
smaller — 1,648,117,166 instructions on the probing workload, **15.9% of it and the largest
single line in the reader**, and a data-dependent loop, so it cost the branch predictor as well
as the pipeline.

A code no longer than K bits owns a whole number of buckets of the stream's top K bits, because
`base64` is right-padded to 64, so a byte per bucket answers exactly what the walk searched
for. K is the table's own `max_sym_len`, capped at 12: uncapped it would want `1 << 63` entries,
and sizing it to the TABLE rather than to the cap keeps a small alphabet's table small and its
cache lines few.

**Every entry is VERIFIED rather than derived**, and that is this port's departure from the
branch it came from. The alignment argument above is sound for a table a real writer produced;
these files come from a mirror. So each bucket is walked at BOTH ends and keeps its answer only
when the two agree — a bucket that straddles a length boundary, on any table however malformed,
stores `NO_FAST_LEN` and reaches the walk. The decode is exact by construction rather than by
proof, and a corrupt header costs speed instead of correctness.

| | avx2 | sse41 |
|---|---:|---:|
| probing workload | **−1,119,491,009 (−12.05%)** | **−996,752,167 (−10.03%)** |
| bench workload | +69 (+0.0000%) | — |

**The cap never binds on the corpus this repository ships**, which is worth knowing before
anyone tunes it: the three-man tables measure `max_sym_len` 7 and 11, so their length tables
are 128 and 2,048 bytes and **not one bucket escapes to the walk**. Twelve is sized for the
tables the prober supports and the repository does not ship — on a five-man corpus, where a
typical table is `min_sym_len` 5 and `max_sym_len` 18, `refish` measures 22% of buckets
escaping at 12 bits, 41% at 10 and 72% at 8, and taking the cap to 8 cost them 2.5x the
mispredicts and 12% more instructions to save 39% of the reader's L1 read misses. The knee is
real and it is on a corpus no gate here runs, so the constant carries their numbers as
*theirs* rather than as a measurement of this tree.

313,744 nodes on both sides of every run, and `tb` reads 264 of 264 probes matching upstream
before and after. The bench figure is the control: that workload never enters this zone, which
is the whole reason the axis had to exist before the change could be judged.


**No bench position enters this zone.** Every one of them has more men than the shipped
three-man corpus covers, so the decoder, the index arithmetic and the parser are absent from
`signature`, from `perf-budget` and from every other cost figure in the tree — a zone proven
for its answers by `tb` and unmeasured for its cost.

`cargo xtask perf-budget --syzygy` and `cargo xtask budget-ab --syzygy` bench
`tools/cases/tb.fens` with `SyzygyPath` set instead: 313,744 nodes, 14,080 tbhits, and 29,600
instructions per node against the bench workload's 8,750, because on this corpus the prober is
the workload rather than a term in it. A run that loaded no tables is refused rather than
reported. See [10-tooling-ci.md](10-tooling-ci.md).

This is what the 2026-08-15 sweep's "real, and unmeasurable here" verdict on refish's
length-walk commits was missing: the instrument, not the finding. Re-opening that question now
needs a measurement rather than an argument.

## The parse is untrusted input, and is fuzzed as such

A table file is a **binary blob from a mirror**, and every offset the decoder walks is
derived from the file's own header. That makes it the most exposed surface in the port: the
UCI shell reads text a user typed, the search reads positions the engine itself produced,
and this reads bytes nobody in the process vouches for.

What can go wrong here is a **panic**, not memory corruption. `forbid(unsafe_code)` means a
bad index is a bounds check, and a bounds check is a process that exits — a denial of service
reachable from a downloaded file. Upstream indexes the same places unchecked, where the
same corrupt byte is undefined behaviour instead; neither is acceptable, and only one of them
is fixable from inside safe Rust.

`platform::syzygy::fuzz` mutates a **real** table's bytes, writes them to a `.rtbw`/`.rtbz`,
and probes through discovery, the magic and size checks, the group set-up, the index
arithmetic and both the WDL and DTZ decoders — the engine's own path, not a stand-in for it.
Seeding from a table that parses is the point: a random blob dies at the magic number and
never reaches a decoder. It found **six** distinct panics on its first afternoon, in an
already-verified prober that matches upstream on all 264 differential probes:

| where | what a corrupt byte did |
|---|---|
| the four byte readers | read past the end of the mapping |
| `set_sizes`, block and span | a shift width taken from a byte, so `1 << 200` |
| `set_sizes`, `base64` | an unsigned subtraction upstream lets wrap, trapped by the gate profile |
| `set_sizes`, the padding shift | a symbol length of zero, so a shift as wide as the type |
| `compute_symlen` | a btree child naming a symbol the table does not have |
| `decompress`, the bit window | a symbol wider than 64 bits, so a shift past the type |
| the probe's lead pawn | a pawnful table whose first piece is not a pawn |

Two different fixes, chosen by **where the error can still be reported**:

- **At parse time, refuse.** `set_sizes` returns `Option`, so a table with an impossible
  shift, an inverted symbol-length range or a btree that does not close is rejected and the
  probe answers as if the file were absent. That is a correct answer.
- **At decode time, clamp.** `decompress` returns a plain `i32` with nowhere to report a
  failure, so the block walk, the code-length search, the bit window and the tree descent are
  bounded and a corrupt file yields a wrong verdict rather than a dead engine.

The order matters: a wrong verdict is worse than a refusal, so anything that *can* be caught
at parse time is refused there, and clamping is only what remains. **None of it changes a
valid table**: every bound is slack for a file upstream's own writer produced, `cargo xtask
tb` still matches the oracle on all 264 probes, and the bench signature is untouched.

There is a third place, and it is the one both bounds above walk past: the **domain of the
value** they so carefully deliver. Every row of the table bounds an *index* into the file.
None of them bounds what comes back out. `decompress` returns `min_sym_len` verbatim on the
single-value path — a raw header byte — so a stored 255 leaves `map_score` as a WDL score of
253, and no index was ever out of range on the way. A WDL file holds five outcomes, so
[`Wdl::from_stored`](../crates/rfish-engine/src/platform/syzygy/probe.rs) refuses anything
else and the probe answers `Fail`, exactly as it does for a table that is not there.

- **At conversion time, make the domain unrepresentable.** `Wdl` is a five-valued enum with
  one fallible constructor and no other way in, so the check cannot be forgotten by a
  consumer added later. Both sibling ports close this hole with a range test at the probe
  (`../mcfish` `f08ee9ef`, `../zfish` `741f8ffc`); neither language can stop a caller from
  simply not writing one, which is why they had to bound it where it is born and rfish can
  bound it in the type.

**A total conversion over an untrusted value is a laundering step, not a convenience.** The
constructor here had a catch-all arm reading `_ => Wdl::Draw`, and a unit test that *pinned*
that behaviour as intended — so an invented byte became a confident draw, and `root_probe_wdl`
ranked and scored it as a real tablebase verdict. In C and Zig the same hole ends as an
out-of-bounds index into a five-entry map, which is loud; in safe Rust the enum makes the
index impossible and the wrong answer silent. **The safer language moved the defect from a
crash to a lie**, and only the missing domain check was common to all three.

The last row of that table was refused only **half** way, and the nightly lane found the other
half three days later. A pawnful file's `pieces[0]` was held to being a PAWN, which still
accepts the pawn of the colour that has **none** — and `do_probe_table` picks the leading
pawns' *colour* off that same nibble (`tbprobe.cpp:1174`, where upstream asserts the type and
trusts the colour). Name the wrong colour and the collection is empty: the probe sorts
`squares[1..0]` and indexes `lead_pawn_idx[0]` with a square the loop never wrote. One flipped
nibble in a downloaded `KPvK.rtbw` reaches it — byte 6 is `0x11`, and `0x99` is the same pawn
the other way round. The check now names the exact piece code, and the colour it expects comes
from the **material** — the enumeration the filename implies, which is where `pawn_count` has
always taken its ordering from — rather than from another byte of the same file. A file that
disagrees is refused at load, where the answer can still be "no table".

**A bound stated against the wrong quantity is not a weaker bound, it is an absent one.** The
type check read as if it covered `pieces[0]`, and covered one bit of it; the fuzz lane had been
green over the remaining hole for three nights. `../zfish` fixed the same half-hole two days
earlier (`3883af90`), from this port's own harness finding — the two ports keep re-finding each
other's residue, and the shape to look for is a validated field whose *consumers* read more out
of it than the validation checked.
