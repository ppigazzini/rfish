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

- **NNUE** — **ported, and bit-exact.** All three feature sets, the feature transformer,
  the PSQT head and the eight output stacks are in
  `crates/rfish-engine/src/eval/nnue/`, and `cargo xtask nnue-check` proves the raw network
  output equals a pristine upstream build's on every position in `tools/cases/eval.fens`.
  The accumulator is updated by **diffing the recomputed feature sets** rather than from a
  per-move delta — correct by construction, 1.48x the from-scratch cost it replaced, and
  measured rather than assumed. See `docs/03-engine-eval.md`; that table is the number any
  further attempt has to beat.
  `crates/rfish-engine/src/eval/classical.rs` is now only the fallback for a run with NO net
  on disk. It is **scaffolding with a scheduled deletion date**: do not tune it, do not
  extend it, and do not let it acquire callers NNUE will not satisfy.
- **Syzygy** — **ported and verified.** `crates/rfish-engine/src/platform/syzygy/` holds
  the whole prober: discovery, the Recursive-Pairing decoder, the index computation, and
  the WDL and DTZ probes, and the root ranking feeds the search: `root_probe` orders the
  root moves and switches the in-search probe off once the tables have settled the game.
  `cargo xtask tb` compares WDL and DTZ against a pristine upstream build position by
  position, and `syzygy_extend_pv` walks a won PV out to mate. Open, and both blocked on
  table data rather than on code: the 5-man cursed-win branches (they need a 5-man set to
  exercise; the repo ships 3-man) and a block cache for 7-man tables (which would be
  gigabytes resident without one).
- **Lazy-SMP** — **wired, with the best-move vote.** `Threads` builds a worker set, a `go`
  runs N workers over one root through `std::thread::scope`, and the move played is the one
  the pool agrees on.
- **NUMA** — **the topology, the policy and the reporting are ported; PINNING is not, and
  cannot be.** `crates/rfish-engine/src/platform/numa.rs` reads the real topology from
  `/sys` and the process affinity from `/proc/self/status`, implements all three of
  upstream's auto policies, parses and prints upstream's `NumaPolicy` syntax, and
  distributes a thread set across nodes with upstream's arithmetic — all from file reads,
  so no `unsafe` and no dependency. What is missing is `sched_setaffinity`: it has no
  filesystem equivalent, so rfish cannot pin a worker to the node it was assigned. The
  shell therefore reports "NUMA node thread **distribution**" where upstream reports
  "binding", and says plainly that the threads are not pinned. Network replication follows
  pinning and so is also absent — one shared copy, reported as upstream's
  `Network replica 1: Local memory.`. Do not "fix" this by adding a crate or an `unsafe`
  block; the honest report is the deliverable.
- **The option model** — **every declared option is acted on.** The `uci` handshake matches
  a pristine upstream build's name for name and in order, and the handshake golden pins it
  byte for byte. `Skill Level`, `UCI_LimitStrength` and `UCI_Elo` run upstream's `Skill`;
  `nodestime` converts the whole clock model into node counts; `Ponder` buys the current
  move a quarter more time and `ponderhit` honours a budget that ran out while pondering.
- **The command surface** — every command upstream accepts is accepted here, with one
  exception: `speedtest`, upstream's machine benchmark. `bench` is the command every gate
  and harness in this repo uses and it IS ported; `speedtest` is a separate "how fast is
  this box" tool with no consumer here. It is missing on purpose, not by oversight.
- **The search** — **ported, and bit-exact.** The pruning set, the constants, the reduction
  model, ProbCut, singular extensions, the correction histories and the move picker's
  staging are upstream's, and `crates/rfish-engine/src/search/` is a 1:1 translation rather
  than a reimplementation. What is NOT upstream's is the addressing: `Stack*` is an index,
  a `PieceToHistory*` is a plane index, and a null `ss->pv` is an explicit flag, because
  none of the three survives borrow checking while `&mut self` is live.
- **The score model** — **ported.** Reported centipawns go through upstream's fitted
  win-rate model rather than being the search's internal units, so the number means the same
  thing across net changes, and `UCI_ShowWDL` reports real chances. Mates, tablebase
  verdicts and estimates stay three distinct kinds of score.

`tools/signature.golden` **equals a pristine upstream build's `Bench:`** at the SHA in
`tools/upstream/UPSTREAM_BASE`, at upstream's own depth 13. Every one of the 51 bench
entries matches upstream node for node, and so does every `bestmove` and every `ponder`
move. A diff against upstream is therefore a porting REGRESSION, not a tuning difference —
which is what makes the anchor worth having. Read the depth from
`crates/xtask/src/gates.rs` and the number from the golden's own header, never from prose.

Four classes of bug cost the most in getting there, and all four are invisible to perft:

- **Integer semantics.** C++ converts a signed operand to unsigned when the other side is
  `u64`, so the division that follows FLOORS instead of truncating. Upstream relies on it
  in `update_all_stats` and in the root-move averages. Writing the "obviously equivalent"
  signed version is off by one, and the error propagates into every history table.
- **Generation ORDER.** Two generators that emit the same set in a different sequence
  search different trees, because the move picker's partial sort leaves equal-scored moves
  in generation order.
- **Key identity.** `Position::key()` mixes the halfmove clock in past move 14. Omitting
  that makes positions share table entries upstream keeps apart.
- **State updates that "obviously" belong.** A null move does NOT advance the halfmove
  clock, and an en-passant square is only set when the capture is actually LEGAL.

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
| The engine must run from `resources/` — it looks for its net relative to the working directory, and a run from the repo root silently finds none, falls back to the classical scaffolding, and reports a node count that looks entirely plausible. | [docs/09-tooling-ci.md](docs/09-tooling-ci.md) |
| A measurement without a net is a measurement of a different engine. Check for the `info string NNUE evaluation using …` line before believing any node count. | [docs/03-engine-eval.md](docs/03-engine-eval.md) |
| Release builds have `overflow-checks = false`; every intended wrap says `wrapping_*` in the source. A bare `+` that wraps in release and traps in `gate` is a bug the gate profile is there to catch. | [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) |
| The default build sets no `-C target-cpu`. `cargo xtask build --arch <tier>` does, and it changes what the NNUE loops vectorise to — so a perf number without its tier is not a number. | [docs/09-tooling-ci.md](docs/09-tooling-ci.md) |
| "Improving" on upstream. A cleaner formulation that moves a rounding boundary moves the node count. | [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) |
| Comments are **imperative mood**; never pin a number a gate computes. | [docs/11-writing.md](docs/11-writing.md) |
| **Never exercise `Threads` near its declared maximum.** A worker is ~15.6 MB resident here (measured: 251 MB at `Threads 1`, 362 MB at `Threads 8`), so `Threads 1024` — the option's own max — is ~16 GB, and a harness running two engines is ~32 GB. That takes down a WSL2 VM and a CI runner, and it has: both sibling ports lost one. Cover the declared bounds with the `uci` listing diff, and keep every harness at 1, 2, 8 or 16. | this file |
| **An option value that becomes an allocation must be bounded where it is parsed.** `Hash`, `Threads` and `NumaPolicy` are the three. `NumaPolicy` accepts a processor list, and upstream bounds each range but not their sum — 735 bytes of input reached 2.8 GB here before the allocator gave up. `numa.rs` bounds the total; keep it bounded. | `platform/numa.rs` |
| A golden must pin the ENGINE, not the runner. `info string Available processors` is the host's core count, so `filter_volatile` drops it. Anything else host-dependent belongs there too. | [crates/xtask/src/gates.rs](crates/xtask/src/gates.rs) |

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
