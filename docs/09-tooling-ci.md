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

**The depth is not upstream's 13.** rfish evaluates with the classical scaffolding, which
prunes far worse, so depth 13 over the full list takes hours rather than seconds. The gate
uses a lower depth — read it from `crates/xtask/src/gates.rs`, never from prose — chosen so
the gate runs in well under a minute, which is the property that decides whether anyone runs
it. **Raise it to 13 in the same commit that lands the NNUE forward pass.**

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
| `msrv` | the crate builds on the declared minimum Rust version |

`pre-commit` wires the fast half of the same set to run before a commit exists.

### Two things about CI that are easy to get wrong, and were

**`rust-toolchain.toml` beats whatever the workflow installs.** The msrv lane pins a
toolchain with `dtolnay/rust-toolchain@<version>`, and a bare `cargo` in that lane then uses
the toolchain FILE instead — silently testing stable while the lane reports green. The lane
runs `cargo +<version>` explicitly for that reason. A lane that cannot fail is worth less
than one that does not exist, because it is believed.

**The cross lane uses `cargo check`, not `cargo build`.** The runner has no cross linker for
either target, so a build fails at the link step having already proven everything the lane
exists to prove — that no `cfg` has crept into engine code.

**The declared MSRV is verified, not guessed.** `Cargo.toml` says 1.88 because 1.87 rejects
the `let` chains in the move picker, the network path search and the tablebase registry, and
1.88 builds clean. Raising it also changes what CLIPPY suggests: the lint set is MSRV-aware,
so an API stabilised after the declared version is suppressed until the version moves. Bump
the MSRV and expect new findings.
