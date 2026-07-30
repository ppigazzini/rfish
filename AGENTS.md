# AGENTS.md

rfish is a **safe-Rust port of Stockfish**, built with `cargo xtask`. The goal is a
**bit-exact 1:1 clone** — same bench signature, NNUE, Syzygy, Lazy-SMP — in **100% safe
Rust**.

Read [docs/](docs/README.md) for the architecture and [CONTRIBUTING.md](CONTRIBUTING.md)
for the workflow. This file is only what an agent gets wrong before it has read either.

## The constraint that outranks everything

**No `unsafe`. Anywhere. Ever.**

`unsafe_code = "forbid"` is set for the whole workspace in `Cargo.toml`, so the compiler
rejects both an `unsafe` block and a local `#[allow]` that would re-enable one.
`cargo xtask unsafe-lint` asserts the same property from outside, because the manifest line
is one line and a reviewer can miss it being deleted.

This is not a style preference to be traded for a constant factor. It is the thing the port
exists to demonstrate. When a construct appears to need `unsafe`, the answer is a different
construct — see [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md), which records the
one that worked for every case so far. Do not:

- reach for `std::simd`, `stdarch` intrinsics, or any nightly feature (the toolchain is
  pinned to **stable** in `rust-toolchain.toml`, and the pin is deliberate);
- add a crates.io dependency to get an `unsafe` block written somewhere else — the engine
  crate has **zero** dependencies and that is a reviewed property, not an accident;
- "temporarily" allow it behind a `cfg`.

## The golden

`../Stockfish` is the **golden** — it defines correct behaviour. Where rfish and Stockfish
disagree, Stockfish wins.

## Known limitations

Do not document, gate, or optimise around the current shape as if it were the intended end
state. Check the state against the tree before acting on it:

- **NNUE** — **not ported.** `crates/rfish-engine/src/eval/nnue.rs` is the file format and
  the loader; the forward pass, the feature transformer and the incremental accumulator are
  not written. `crates/rfish-engine/src/eval/classical.rs` is **scaffolding with a
  scheduled deletion date**: do not tune it, do not extend it, and do not let it acquire
  callers NNUE will not satisfy. This is milestone M3.
- **Syzygy** — **discovery only.** `crates/rfish-engine/src/platform/syzygy.rs` resolves a
  `SyzygyPath`, recognises table names and reports a maximum cardinality. There is no
  prober. With no path set the cardinality is 0 and the search's tablebase step never
  enters, which is why the option surface is live without changing any search. M5.
- **Lazy-SMP** — **wired.** `Threads` builds a worker set and a `go` runs N workers over
  one root through `std::thread::scope`. There is no NUMA model and no network
  replication, because there is no network yet.
- **The option model** — the `uci` handshake advertises the full option set and the
  handshake golden pins it byte for byte. `UCI_LimitStrength`, `UCI_Elo` and `nodestime`
  are **declared but not acted on**; the rest are wired.

`tools/signature.golden` is **rfish's own number**, not upstream's, and it is measured at
the depth `cargo xtask signature` uses rather than upstream's 13 — the classical
scaffolding prunes far worse, and depth 13 over the full list takes hours. Raise the depth
in the same commit that lands NNUE. Read both facts from
`crates/xtask/src/gates.rs` and the golden's own header, never from prose.

## Setup

```sh
cargo xtask build            # binary is `stockfish`, at target/release/
cargo xtask help             # every step
cargo xtask parity           # the aggregate gate -- run before calling anything done
```

A new module must be declared in its zone's `mod.rs`. Rust will not compile a file nothing
declares, so unlike the sibling C port there is no "written but not in the build" state to
audit for — the compiler is the audit.

## Gates

**A behaviour-changing edit is not done until a gate says so.**

```sh
cargo xtask parity           # fmt, clippy, unsafe-lint, docs-lint, test, perft, golden,
                             # signature
cargo xtask signature        # just the anchor
cargo xtask test             # unit and property suite, under the gate profile
```

`parity` names any gate it skipped for a missing tool and exits 2 for it. A skipped gate
proves nothing — never report it as a pass.

**Check the gate's EXIT CODE, never a piped fragment.** `cargo xtask parity | tail -1`
reads 0 from `tail` while the gate is red; this has laundered red gates in both sibling
ports. Run it unpiped, or redirect to a log and test `$?`.

## Traps that cost real time

| trap | where |
|---|---|
| `signature-update` / `golden-update` on a **red** gate launders a bug into the anchor. Fix the code, then re-derive. | [CONTRIBUTING.md](CONTRIBUTING.md) |
| `tools/perft.table` is **not** a golden. Those counts are facts about chess; a mismatch is always a movegen bug. | [CONTRIBUTING.md](CONTRIBUTING.md) |
| The engine must run from `resources/` — it looks for its net relative to the working directory, and a run from the repo root silently finds none. | [docs/09-tooling-ci.md](docs/09-tooling-ci.md) |
| Release builds have `overflow-checks = false`; every intended wrap says `wrapping_*` in the source. A bare `+` that wraps in release and traps in `gate` is a bug the gate profile is there to catch. | [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) |
| The default build sets no `-C target-cpu`. `cargo xtask build --arch <tier>` does, and it changes what the NNUE loops vectorise to — so a perf number without its tier is not a number. | [docs/09-tooling-ci.md](docs/09-tooling-ci.md) |
| "Improving" on upstream. A cleaner formulation that moves a rounding boundary moves the node count. | [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) |
| Comments are **imperative mood**; never pin a number a gate computes. | [docs/11-writing.md](docs/11-writing.md) |

## Performance work

Read [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) (porting patterns, measurement
discipline) and [docs/09-tooling-ci.md](docs/09-tooling-ci.md) (measurement tooling and its
blind spots) before proposing any optimisation. Four rules that outrank intuition:

- **Measure whole-binary, at a named `--arch` tier.** Instruction arithmetic over a diff is
  a guess, never a measurement, and the tier changes the answer.
- **Subtract startup, by measurement.** A whole-process counter includes the net load and
  the magic-table build, and both are large next to a short bench.
- **Size an Elo run BEFORE starting it.** Speed converts at roughly 70 Elo per doubling, so
  a 6% change is about 6 Elo and needs ~10,000 games per cell to see. A 1000-game cell
  carries ±18 and returns a coin flip with a sign.
- **A bounds check is not automatically the cost.** Rust elides most of them, and the ones
  it does not are usually not on the hot path. Find the check in the disassembly before
  reshaping code around it — and if reshaping is needed, it is still not a licence for
  `unsafe`; restructure so the bound is provable instead.

## Fleets and subagents

Multi-agent work is a standing pattern here. Each rule below was paid for in a sibling
port:

- **Never `git stash`** — the stash is repo-wide across worktrees; pop only a stash you
  created, by index, immediately.
- **Check a gate's EXIT CODE, never a piped fragment** — see above; this is the single most
  expensive mistake in both siblings' histories.
- **`cargo xtask build` explicitly before every measurement** — a stale binary has produced
  false conclusions twice in the sibling ports.
- **Charter disjoint FILES, not just disjoint metrics** — two lanes converging on the same
  function from different charters produce patches the integrator must untangle.
- **Unique scratch filenames, and verify profile provenance** — concurrent agents sharing a
  scratchpad have clobbered each other's profiler output.
- **Worktree agents deliver patches, never commits** — and gitignored local note
  directories do not exist inside a worktree: findings travel in the final report.
- **A subagent is not re-woken by its own background jobs** — wait on a measurement with a
  foreground `until` loop, or the agent stalls silently.
- A worktree starts where its branch last was, not at your HEAD — reset it to the intended
  base and re-verify with `git log` before building a baseline.

## Commits

**One logical change per commit** — a commit that touches three modules cannot be bisected
when the node count moves.

Conventional subject ≤72 chars, blank line, body wrapped at 80 carrying the evidence: gate
output and exit code, not "should work". **Don't** `git push` — commit locally and stop
unless asked. **Don't** add co-author or generated-by trailers.
