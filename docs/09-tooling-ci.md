# Tooling and CI

`cargo xtask <step>` is the single entry point for every build and every gate. There is no
Makefile and no shell driver: the gates are Rust, in the workspace, type-checked and
clippy-linted by the same CI lane that checks the engine — so a gate cannot rot in a way
the compiler would have caught.

`cargo xtask help` lists every step.

## Check the exit code, never a piped fragment

```sh
cargo xtask parity | tail -1     # WRONG -- reads 0 from tail while the gate is red
cargo xtask parity; echo $?      # right
cargo xtask parity > gate.log 2>&1 || echo FAILED
```

This has laundered red gates in both sibling ports, more than once. It is the single most
expensive mistake in their histories.

## Three outcomes, three exit codes

| Outcome | Exit | Means |
|---|---|---|
| pass | 0 | the gate ran and the property holds |
| fail | 1 | the gate ran and the property does not hold |
| **SKIPPED** | **2** | the gate could not run |

A skipped gate has proven **nothing**. `parity` names every gate it skipped, separately
from the passes, and never counts one as green.

## The gates

Listed in the order `parity` runs them. See [CONTRIBUTING.md](../CONTRIBUTING.md) for what
each asserts.

`fmt` → `clippy` → `unsafe-lint` → `docs-lint` → `test` → `perft` → `golden` → `nnue-check`
→ `tb` → `signature`

Cheap and structural first, so a formatting mistake is reported in seconds rather than
after a two-minute bench.

### `signature`

The anchor. `bench` must reproduce `tools/signature.golden`.

**The depth is upstream's 13**, which became affordable when the NNUE forward pass landed
and the tree stopped being enormous. Read it from `crates/xtask/src/gates.rs`, never from
prose. The COUNT is still rfish's own, because the search's pruning constants are not
upstream's yet — see CONTRIBUTING.md, "Two different numbers".

The gate has to run in well under a minute, because that is the property that decides
whether anyone runs it before pushing. If a change makes it slower, fix the change.

### `perft`

`tools/perft.table` is **not** a golden and no step regenerates it. Those counts are facts
about chess, reproduced here against a pristine upstream build, so a mismatch is always a
movegen bug.

The `chess960` flag is part of each row rather than a mode the runner is in, because the
same FEN means two different positions under the two castling dialects.

### `golden`

Each `tools/cases/*.uci` script is driven into the engine and its output compared with
`tools/<name>.golden`.

Lines whose content depends on the clock or the machine are filtered before comparison —
`info depth`, `Total time`, `Nodes/second`, the compiler banner. Without that filter every
golden would be a record of one machine's timing rather than of the engine's behaviour.

### `nnue-check`

The differential evaluation gate, and the one that says the NNUE port is a PORT rather than
an approximation. It drives rfish and a pristine upstream build over the same positions and
compares the RAW network output — the number upstream's `eval` prints as "internal units".

Comparing final evaluations would not do: the optimism blend and the fifty-move damping sit
on top and would mask a forward-pass error.

It needs two things a fresh clone does not have — the 90 MiB net (`cargo xtask net`) and an
upstream binary (`cd ../Stockfish/src && make -j build ARCH=x86-64-avx2`) — and reports
SKIPPED for either. **It is deliberately not a CI step**: a gate that can only skip in CI
teaches contributors to ignore a skip. `parity` names it when it could not run.

Positions in check are excluded, because upstream's `eval` refuses to score one — and so does
rfish's, for the same reason and so the two emit the same number of lines.

**Both differential gates drive ONE engine invocation for the whole battery.** A spawn per
position reloads the 90 MiB network every time; batching took `nnue-check` from minutes to
three seconds and `tb` from minutes to six. A gate that takes five minutes is a gate people
skip, which makes it worth no more than one that does not exist.

The batching also made a real difference visible that the per-position form had hidden: it
compares the two engines' line COUNTS, and rfish was answering two positions upstream
declined.

### `tb`

The differential tablebase gate: rfish's WDL verdict and DTZ distance must equal a pristine
upstream build's, position by position.

A tablebase answer is exact, so "close" is meaningless — an index computed one off reads a
different position's entry and returns a confident wrong verdict. A golden pinning rfish's
own output would pin whatever it currently does; only the comparison catches that.

It also checks the property that makes an unconfigured engine safe: with no path set nothing
is discovered, no probe fires, and the signature is unaffected.

SKIPs without `resources/syzygy/` or without an upstream build. Every entry in the battery is
LEGAL — an illegal position makes the oracle exit rather than answer, which reads to a gate
as a broken engine rather than as a position to skip.

### `docs-lint`

Every markdown link resolves, and every `crates/…` or `tools/…` path named in prose exists.

It settles the **mechanical** half of documentation rot. It cannot tell you a sentence has
become false, and in the sibling ports every false claim ever found got there by a commit
that changed the code and not the page. That half is yours.

### `unsafe-lint`

No `unsafe` keyword, no `allow(unsafe_code)`, and `unsafe_code = "forbid"` still present in
the workspace manifest.

The compiler already rejects the first two. The gate exists because the manifest line is one
line and a reviewer can miss it being deleted — the property is asserted from **outside**
the mechanism that enforces it.

It scans the shipped crates only. `xtask` is a build tool that never enters the binary and
necessarily names the patterns it looks for; scanning it would make the gate report itself.
It is still covered by the workspace forbid, which the manifest check asserts.

## Running the engine

**Always from `resources/`.** The engine looks for its net relative to the working
directory, so a run from the repository root silently finds none and produces an unrelated
number — one that looks entirely plausible. `runner::drive` sets the working directory for
every gate; a hand-run measurement must do the same.

## Build tiers

The **default** build sets no `-C target-cpu`, because the bench anchor has to be
reproducible on a machine nobody here owns, and `target-cpu=native` silently changes which
vector width the NNUE loops autovectorise to.

`cargo xtask build --arch <tier>` sets it per tier, through the child process's environment
rather than by mutating this process's — so a later gate in the same `parity` run cannot
inherit it.

**A perf number without its tier is not a number.**

## Measurement, and each instrument's blind spots

- **`perf stat` / `cachegrind` count the whole process**, which includes the magic-table
  build and the net load. Both are large next to a short bench. Subtract startup by
  measurement, and only on the instruction axis — on the cycles axis the subtraction removes
  a term whose error rivals the effect. Time the search directly instead: the bench's own
  total contains no startup by construction.
- **callgrind is blind to software prefetch**, on both engines. No callgrind bar can certify
  a prefetch change.
- **A serial cycle A/B has a run-to-run floor** of around a percent on a typical desktop,
  plus an A/A bias. A sub-1% single-tier cycle claim is unmeasurable; adjudicate on the
  deterministic instruction axis or with a properly sized Elo run.
- **A symbol-group regex is a hypothesis.** Inlining differs per side, so verify per-symbol
  before trusting any component ratio.
- **`cargo bench` is not used.** The engine is measured by driving the binary, because that
  is what a harness measures and what ships. A microbenchmark of a function LTO would have
  inlined away is a measurement of a program nobody runs.

## CI

`.github/workflows/rfish_ci.yml` runs the same gates this page describes, on Linux, macOS
and Windows. Nothing in CI is a gate that does not exist locally, and nothing local is
weaker than CI — a contributor who runs `cargo xtask parity` and sees green should not be
surprised by the merge gate.

The lanes:

| Lane | Runs |
|---|---|
| `lint` | `fmt`, `clippy`, `unsafe-lint`, `docs-lint` |
| `test` | `cargo xtask test` on three platforms |
| `gates` | `net`, `perft`, `golden`, `signature` on Linux |

`rfish_fuzz.yml` is a second workflow, on a nightly schedule rather than on push. It runs
`cargo xtask fuzz`, which spends half its budget throwing mutated UCI text at the shipped
binary and half walking random legal positions through the real search in-process. Two
harnesses because they fail differently: subprocess fuzzing spends a mutation's budget on the
PARSER and never reaches the search behind it, which is the lesson ../mcfish records in
`2b8eaad7` when it added its own in-process harness beside its UCI one.

It is NOT a merge gate. A clean run means "nothing failed inside that budget", never "there
is nothing to find", and that is not a statement a merge should block on. The step prints the
seed it used, and the workflow takes a seed as a dispatch input, because the value of a fuzz
run is a reproducible failure.

**The search half is not libFuzzer, and cannot be.** `libfuzzer-sys` is a dependency where
the engine crate has none, and its `fuzz_target!` expands to a `#[unsafe(no_mangle)]` export
that `forbid(unsafe_code)` rejects. What replaces it is a seeded PRNG walk, which loses
coverage guidance and keeps the part that finds bugs here: real positions off every golden
and bench list. It runs under the `gate` profile, so `debug_assert!` and `overflow-checks`
are both on — this port's equivalent of the sanitiser build the sibling fuzzes under.

**What it checks, beyond "did not crash".** ../zfish carries eight fuzz targets in
`src/engine/board/fuzz_targets.zig` and the interesting ones are not the search — they are
board invariants a crash-only fuzzer would walk straight past. Ported here:

| invariant | why a crash-only run misses it |
|---|---|
| make then unmake restores the key, the board, the ep square, the clock and the checkers | nothing crashes; the position is just quietly wrong afterwards |
| a whole line unwinds to the position it started from | a key that desyncs and RESYNCS passes every per-ply check |
| the legal list holds no null move and no duplicate | a duplicate is not a crash and not a wrong perft count — it is a move searched twice |
| the move count is the same after making and unmaking every move | catches a state restore that is close but not exact |
| a nonsense FEN is rejected or accepted, never a panic | the parser is untrusted input; a GUI can send anything |

The make/unmake checks run per MOVE, not per line, so a category-specific fault — castling,
en passant, a promotion — is attributed to the move that caused it. AGENTS.md lists key
identity among the four bug classes that cost this port the most, and notes that all four are
invisible to perft: perft counts leaves, so a key that desyncs and resyncs is exactly what it
cannot see.

**The harness was checked against an injected defect**, not just observed to pass. Making
`undo_move` leave the fifty-move counter one too high fails
`make_and_unmake_restore_the_position_exactly` and names the move: `ply 0: the fifty-move
counter moved over a2a3`. A green fuzz target that has never been shown to go red is a
decoration.

**Found and FIXED: `go` with a malformed argument.** Seed 999,
`go value Hash binc isready SyzygyPath`. rfish accepted the bad value silently and then
searched unbounded; upstream rejects the whole command. Every key in a `go` takes a value,
and the values are parsed as `i64` because that is what upstream parses at and both edges are
observable -- `movestogo -5` is accepted there, `nodes 99999999999999999999` is rejected for
overflow. The error line is byte-identical to a pristine upstream build's, diffed rather than
eyeballed.

**Found and OPEN: a buffered `stop` cannot end an unbounded search.**

  printf 'position startpos\ngo mate 1\nstop\nisready\nquit\n' | ./stockfish

Upstream answers; rfish does not. The cause is structural rather than a parse bug. Upstream
reads and dispatches on ONE thread, so its `go` is fully dispatched before the next line is
looked at. rfish reads ahead on a reader thread -- which is what lets a `stop` reach a search
already running -- and that same read-ahead means the reader requests the stop BEFORE the
main loop has dispatched the `go`, whose `SharedState::reset` then clears it. A `stop` that
arrives after the search starts works correctly; only the buffered ordering loses.

Fixing it by moving the clear out of `reset` and into the reader was tried and REVERTED: the
unit suite and the `search` golden both drive `Engine::handle` directly, bypassing the reader,
so nothing cleared the flag for them and a stale stop truncated the next search. The fix
needs to survive both entry points and is a concurrency change, not a parser one.

The fuzz step therefore delivers its stops the way a GUI does -- after a pause, one per line
of the burst, because a burst can start several unbounded searches and the commands queued
behind the first are not dispatched until it returns. That reproduces real timing instead of
re-finding this every night, and the reproduction above is one line if anyone wants it back.

There is no `msrv` lane. It ran `cargo +<rust-version> build` and cannot pass while the
engine enables `portable_simd`, which no stable channel accepts.

`pre-commit` wires the fast half of the same set to run before a commit exists.

### Two things about CI that are easy to get wrong, and were

**`rust-toolchain.toml` beats whatever the workflow installs**, and that is now the whole
design rather than a trap to route around. Every lane reads the channel OUT of that file and
hands it to `dtolnay/rust-toolchain`, so the toolchain it installs is the toolchain that
runs. Hard-coding a channel in the workflow instead would install one and silently run
another — with that other one's components, and in the cross lane without the target's std,
which is exactly how a `check --target` lane passes while proving nothing.

That trap used to be worked around in the msrv lane with an explicit `cargo +<version>`.
The lane is gone (above), and with it the only place the two could disagree.

**The toolchain is read with `sed`, not `grep -oP`.** `-P` is a GNU extension. macOS ships
BSD grep, which rejects the flag outright, and Git Bash on Windows is frequently built
without PCRE. The `msrv` lane got away with `grep -oP` for as long as it existed because it
ran only on `ubuntu-latest`; the step that replaced it runs in the three-OS `test` matrix,
where the same line breaks two of the three. A shell one-liner that has only ever run on
Linux is not portable, it is untested.

**The cross lane uses `cargo check`, not `cargo build`.** The runner has no cross linker for
either target, so a build fails at the link step having already proven everything the lane
exists to prove — that no `cfg` has crept into engine code.

**The declared MSRV is now documentation, not a gated property.** `Cargo.toml` says 1.88
because 1.87 rejects the `let` chains in the move picker, the network path search and the
tablebase registry. It does NOT mean a 1.88 toolchain can build this — none can, because of
`portable_simd`. The number is kept for two reasons: clippy's lint set is MSRV-aware, so an
API stabilised after it stays suppressed until it moves, and it still records which stable
features the source leans on. Nothing verifies it any more; treat it accordingly. The real
requirement is the dated nightly in `rust-toolchain.toml`.
