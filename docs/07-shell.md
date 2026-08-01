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
records but never acts on is worse than one it does not declare**, and none are in that
state: the handshake matches a pristine upstream build name for name and in order, and
every name on it changes what the engine does.

The values the SEARCH reads travel as one `SearchOptions` block rather than as loose
parameters. The engine crate cannot see this option model — that is the zone boundary
working — so the alternative is a growing argument list, and a declared struct at least
makes the set of options the search depends on something a reader can look up instead of
grep for.

**`export_net` writes the loaded network back out**, and the round trip is the point rather
than the file: reader and writer are mirrors, so a net that survives the trip proves they
agree. The shipped net comes back byte-identical, all 95 MB of it, which is the strongest
statement available that the format code is right — every LEB128 group, every split point,
every hash. The hashes are recomputed from the saving build's own constants rather than
copied from the file that was loaded, so a saved net asserts the architecture the binary
actually implements.

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

## A malformed command ENDS the process, as upstream's does

Upstream treats a command it cannot use as fatal, and rfish now matches it exactly:

```text
info string CRITICAL ERROR: Command `position fen this is not a fen at all` failed. Reason: Invalid FEN. Invalid piece: t
```

followed by a blank line and **exit 1**. The text is upstream's word for word, including
the backticks and the trailing blank line, because the string reaches a GUI and a paraphrase
is a divergence like any other. `FenError` carries upstream's twenty `PositionSetError`
messages rather than a summary of them.

Two things follow from it, and both had to move together:

- **`cargo xtask fuzz` accepts a reported CRITICAL ERROR as a valid outcome.** Random UCI
  text reaches the fatal path constantly, the engine is gone afterwards and answers no
  `isready`, and that is termination on purpose rather than a wedge. The harness is looking
  for hangs and crashes, and it still is: any OTHER non-zero status still fails it.
- **The gate driver accepts exit 1 and keeps the output.** It is the status upstream uses
  for this, so a run that ends in it produced real output to compare; a signal or an abort
  still fails.

## What is not here

- **No `help`, no `flip`, no `export_net`, no `tune`.** All declared out of scope until the
  zones they report on exist.
- **Reading and searching do not share a thread.** They cannot: the search runs where `go`
  was dispatched, so a loop that reads a line, dispatches it, and only then reads the next
  one cannot see a `stop` until the search it would stop has already ended. `go infinite`
  followed by `stop` hung forever, and that is the shape every analysis GUI uses. Stdin is
  drained by its own thread; `stop` and `ponderhit` act there, against the shared atomics
  they were built for, and everything else queues and dispatches in order.

  **A `quit` interrupts the search only when the search cannot end by itself.** Upstream
  aborts unconditionally, and this is a deliberate divergence: `go depth 13` followed by
  `quit` is how every gate and every measurement harness drives this binary, and aborting
  there would make a node count depend on scheduling. `go infinite` has no answer to wait
  for, so that one is stopped.
