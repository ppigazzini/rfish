# The shell

`crates/rfish/src/` — the UCI transport, the option model and the benchmark. The binary is
named `stockfish`.

Golden: `Stockfish/src/uci.cpp`, `ucioption.cpp`, `engine.cpp`, `benchmark.cpp`.

## This is the only place with I/O

The engine crate reads no standard input and writes no standard output. The shell
implements `InfoSink` and turns the search's reports into `info` lines. The split is a
crate boundary, so it is checked by `cargo build` — see
[00-architecture.md](00-architecture.md).

`Engine::handle` takes the output stream as an argument rather than writing to `stdout`
directly, which is what lets the whole UCI surface be tested by driving a script into a
`Vec<u8>`.

## `options.rs` — the option model

The option table is the **engine's public surface**: the `uci` handshake prints it verbatim,
a GUI configures the engine through it, and `tools/handshake.golden` pins it byte for byte.
Adding, renaming or reordering an entry is a protocol change, not a refactor.

**Declaration order is handshake order.** The map is keyed by name, so each entry carries
the index it was declared at and `iter_declared` sorts by that — a plain `BTreeMap` walk
would print alphabetically, and the golden pins upstream's order instead.

Two behaviours worth knowing:

- A `spin` out of range is **clamped, not rejected**. A GUI that asks for a 64 GiB hash on a
  machine that cannot provide one should get the largest table the engine allows.
- An empty string is sent as `<empty>`, and `<empty>` sets it back to empty. A bare
  `default` with nothing after it is what several GUIs choke on.

Setting an option is not just recording it: `Hash` reallocates the table, `Threads` resizes
the pool, `EvalFile` reloads the network, `SyzygyPath` rescans. **An option the engine
records but never acts on is worse than one it does not declare** — `UCI_LimitStrength`,
`UCI_Elo` and `nodestime` are currently in that state, and
[AGENTS.md](../AGENTS.md) says so.

## `bench.rs` — the benchmark

The total node count `bench` prints is the engine's **signature**. Four facts decide it,
and every one is load-bearing: the entry list, the depth, the `Threads` and `Hash`
settings, and the fact that the table is cleared **once** before the whole run rather than
once per position — the table and the history block carry across positions.

**An entry is not always a FEN.** Upstream's list is verbatim here, and it contains:

- `setoption name X value Y` lines, executed as commands. `UCI_Chess960` is toggled around
  the 960 block this way, and dropping those lines would make the 960 positions parse under
  the wrong castling dialect — they would be rejected, and the node total would silently
  shrink.
- FENs with a trailing `moves <uci>...`, where the searched position is not the one written.

Treating every entry as a bare FEN silently drops both. A test asserts every entry parses
and every trailing move is legal.

## `uci.rs` — the transport

Unknown commands are reported and ignored, never fatal: a GUI sends what it likes, and an
engine that exits on an unrecognised word is unusable.

`setoption name <words...> value <words...>` splits on the **keywords**, not on position:
both halves can contain spaces (`Move Overhead`, `Clear Hash`, a Windows `SyzygyPath`).

Castling notation depends on the dialect and on the position: standard chess names the
king's destination (`e1g1`), Chess960 names the rook's square (`e1h1`), because g1 may
already hold a piece. `move_to_uci` takes the position for that reason, and the PV is
rendered by walking a copy so each move is named in the position it is played in.

`go perft N` is answered here and never reaches the search: it is a movegen command.

## What is not here

- **No `help`, no `flip`, no `export_net`, no `tune`.** All declared out of scope until the
  zones they report on exist.
- **No `stop` during a running search from the same input loop.** The stop flag is shared
  and atomic, so an external caller can set it, but the loop reads one line at a time and
  the search runs on that thread. A GUI that sends `stop` mid-search is served by the flag
  once the search's own limit check reaches it. Making the input loop concurrent with the
  search is open work.
