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

`cargo xtask net-roundtrip` is what makes that a fact rather than a sentence, and it runs in
`parity`. It had to be built: the claim above shipped with the format and **nothing drove the
writer at all** — `export_net` appeared in no fixture, no test and no gate, so every `write`
in the NNUE zone was an output path no instrument read. That matters more than it sounds,
because the writer is not on the eval path: swap two of `FeatureTransformer::write`'s eight
operations and `cargo xtask signature` still matches the anchor and `cargo xtask nnue-check`
still matches upstream on every position — measured, not argued — while `export_net` emits a
net this build cannot read back. **A gate that reads only what the engine CONSUMES cannot see
a writer drift away from its reader.** That mutant is now a permanent row in
`negative-control`.

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

## `speedtest.rs` — "how fast is this box"

Upstream's `BenchmarkCommand`, and **not** a second `bench`. The two measure different
things and share no number: `bench` fixes a DEPTH over 51 positions and its node total is
this repository's bit-exactness anchor, so it must not move; `speedtest` fixes a TIME over
five real games — 258 positions taken from upstream byte for byte — and reports throughput,
so its numbers move with the machine and pin nothing. A change costing 3% of nodes per
second is invisible to one and is the entire subject of the other.

**The schedule is the part that had to be exact.** Each ply is worth `50000/(ply+15)` ms,
the 258 terms are summed, and the total is scaled to the requested duration — and upstream
sums into a `float` while the fit itself is `double`, then truncates the product to an
integer millisecond. Reproducing those widths is not pedantry: an `f64` accumulator
diverges at `speedtest N M 579`, where five positions come out a millisecond short and the
run measures different work. That duration is a test, because the defect is invisible at
the default one.

**The argument parse is C++ stream semantics, and its failure mode is observable.**
`is >> threads` sets failbit on a token that is not an integer, and every later extraction
on a failed stream fails too — so `speedtest x 8 5` takes all three defaults rather than
just the first, and echoes an empty user invocation. Parsing each argument independently
would accept the 8 and the 5, which is a different run. The report prints the typed
invocation and the filled one side by side precisely so that difference is visible.

**The first two arguments become engine OPTIONS, and are clamped into the ranges they feed.**
`threads` is emitted as `setoption name Threads` and `tt_size` as `setoption name Hash`, so
passing them through as typed has two consequences and the quiet one is worse:
`speedtest 4 99999999` asks for a hash outside `Hash`'s declared range, the `setoption` is
refused, and the run proceeds on whatever `Hash` was already set — then reports a throughput
as if it had been measured at the size the operator asked for. Upstream's own defect is that
one, reached through an overflow this port already saturated; the silent proceed survived the
overflow being fixed. A wrong number reported as a measurement is the defect here.

**The thread ceiling is the host's core count, NOT the option's maximum of 1024, and that
distinction is the fix.** Clamping into the DECLARED range is the obvious design and it is
wrong: it turns an instant refusal into a legal request for 1024 workers, about 16 GB
resident on this box — worse than the bug, because it succeeds. `../zfish` reached that
design, caught it before landing, and settled on the core count. A speedtest measures
throughput, and more workers than cores does not measure more throughput. A non-positive
duration, which would make every `go movetime` argument zero or negative and measure nothing,
is raised to one second.

The clamp applies AFTER the echo is built, so the report still prints what the operator
typed beside what actually ran — a report that silently rewrote the invocation would hide
the clamp, which is the whole point of printing both. And because `setup` is a pure function
with no `Options` to consult, it carries its own copy of `Hash`'s bounds; a unit test welds
the copy to the option table, because a copy with nothing holding it to the original goes
stale silently.

**Everything it prints goes to standard error**, as upstream does: a GUI reading standard
output must not be sent a report it cannot parse, and the progress counter overwrites its
own line with `\r`. That is also why no golden holds it — the gate is the schedule's own
tests, plus a field-for-field diff against the oracle with the values elided.

One row is renamed and it is the port-wide reason: upstream prints `Thread binding`, rfish
prints `Thread distribution`, because `sched_setaffinity` has no safe interface and this
engine assigns threads to nodes without pinning them. See [06-platform.md](06-platform.md).

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

## `debug_log.rs` — the transcript behind `Debug Log File`

When a GUI and an engine disagree about what was said, the only settlement is a record of
what crossed the pipe. Setting the option opens one, in upstream's format: every line the
engine READ prefixed `>> `, every line it WROTE prefixed `<< `, interleaved in the order
they happened.

**It is a file-scope global behind a mutex, and that is the smaller construct rather than a
shortcut.** The option can be switched on and off mid-session, by a `setoption` handled deep
inside the command dispatch, while the thing being logged is the output stream that same
dispatch is writing to. Threading a handle from the option down to the writer and back would
put a borrow across the whole shell for a feature that is off by default.

Every byte the engine writes passes through `TeeWriter`, which wraps the output stream in
`uci::run` and in the argv path both. **With logging off the whole body is behind one
`is_active()` test** — the line splitting, the lossy UTF-8 conversion and the file write all
sit inside it — so the cost on the path that prints every `info` line is that test alone.
It is a mutex acquisition rather than an atomic load, which is the price of letting the
option own the file handle directly; measure before assuming it is free on a hotter path
than this one.

Golden: `Stockfish/src/misc.cpp`, `Logger` and `Tie`.

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

- **No command.** Every command upstream accepts is accepted here, including the debugging
  surface it calls "custom non-UCI commands" — `d`, `eval`, `compiler`, `flip`,
  `export_net`, `speedtest`, and `help`/`license` with upstream's own text. See
  [`speedtest.rs`](#speedtestrs--how-fast-is-this-box) for the last one to land, and what
  pins it.
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

  **The reader latches the `quit`; `SharedState::set_searching_unbounded` decides it.** That
  split is the whole of the invariant, because the reader cannot answer the question: it sees
  `quit` in the same buffer as the `go` in front of it, before the main loop has dispatched
  that `go`, so a decision taken there asks whether a search is running and is told no. The
  search declares itself unbounded and answers the pending quit in the same call. Both races
  are gated — `async-check` invariant 4 sends the quit into a running search and invariant 5
  sends it ahead of one, and only the second fails if the decision moves back to the reader.
