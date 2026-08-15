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

- reach for `stdarch` intrinsics, or for any crate that wraps them — every intrinsic in
  `std::arch` is an `unsafe fn`, which is the whole reason they are out;
- add a crates.io dependency to get an `unsafe` block written somewhere else — the engine
  crate has **zero** dependencies and that is a reviewed property, not an accident;
- "temporarily" allow it behind a `cfg`.

**`std::simd` is the ONE exception, and it is not an exception to the rule above.** It is
safe — it needs no `unsafe` block — so `forbid(unsafe_code)` is untouched and
`cargo xtask unsafe-lint` still asserts it. It costs a **nightly** toolchain, pinned to a
dated one in `rust-toolchain.toml`, and that pin was bought deliberately: the NNUE
evaluation sat at 1.8x a pristine upstream build while ../zfish, writing the same kernels
with Zig's `@Vector`, sat near 0.9x, and the disassembly said rfish's autovectorised loops
were already as good as that route gets. Use it in the NNUE kernels where a measurement
says it pays. Do NOT use it as a first resort elsewhere: five scalar reformulations of the
same kernels made things WORSE, and the reason is in
[docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) section 12.

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
- **The command surface** — **complete.** Every command upstream accepts is accepted here,
  `speedtest` included. It is not `bench` and shares no number with it: `bench` fixes a
  DEPTH over 51 positions and its node total is this repo's anchor, while `speedtest` fixes
  a TIME over five real games and reports throughput. Its report goes to standard error, as
  upstream's does, so no golden can hold it — the schedule that decides what it measures is
  a pure function with its own tests instead.
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

## Working here

The rest of this file is about the code. This section is about you.

**Deliver what was asked, at the scope intended.** Make the routine calls yourself and check
in only where two readings of the request would produce materially different work. If the ask
looks mistaken, say so in a sentence and build it anyway under a stated assumption — quietly
narrowing, widening or transforming it is the failure mode. Finish the whole task; if one part
is blocked, finish every other part and say plainly which one you left and why. Scaling the
work down is the user's call.

**The gates ARE the verification — do not invent a second one.** A behaviour-changing edit
runs `cargo xtask parity`, and a kernel edit runs `cargo xtask arch-determinism` on a box that
can execute the tiers: those are not optional, and the exit code is the only evidence anyone
reads. Re-running a gate that is already green, bolting a "final check" pass onto a finished
task, or having something review your own diff proves nothing the gate did not.

**Delegate only what is genuinely parallel and large.** A wide multi-file investigation, or a
perf fleet with disjoint charters, earns subagents; work you can finish in a handful of tool
calls does not, and nothing earns a subagent whose job is to check your work. If one agent can
do it, use one. Past two, the fleet rules below bind.

**Lead with the outcome.** One sentence before the first tool call saying what you are about
to do, then quiet until something changes the plan, then a first sentence that answers what
happened — the node count, the exit code, the ratio — with the detail after it for whoever
wants it. The full evidence goes in the commit body, which is the durable per-task record;
the reply is the summary of it.

**Correct only what changes a decision.** If an earlier statement would send a reader to the
wrong file or the wrong number, fix it in a sentence and carry on. For a slip that changes
nothing, fix it and say nothing — a running tally of your own mistakes buries the correction
that mattered.

**Match a document's length to what it must carry**, whether it is a page in
[docs/](docs/README.md) or a report in the reply. Cover the substance and stop: no restated
summary, no recap of what a gate prints, no next-steps list nobody asked for. Length is not
thoroughness; it is where rot hides ([docs/12-writing.md](docs/12-writing.md)).

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
cargo xtask parity           # fmt, clippy, unsafe-lint, docs-lint, lane-coverage,
                             # fixture-coverage, async-check, test, perft, golden,
                             # golden-audit, nnue-check, tb, signature
cargo xtask signature        # just the anchor
cargo xtask test             # unit and property suite, under the gate profile
```

`parity` names any gate it skipped for a missing tool and exits 2 for it. A skipped gate
proves nothing — never report it as a pass.

**Check the gate's EXIT CODE, never a piped fragment.** `cargo xtask parity | tail -1`
reads 0 from `tail` while the gate is red; this has laundered red gates in both sibling
ports. Run it unpiped, or redirect to a log and test `$?`.

### Editing a GATE is the case where being wrong is silent

A broken engine reddens a gate. A broken gate reports success — which is what everyone was
hoping for, so nobody looks. These three ask the questions a green `parity` cannot:

```sh
cargo xtask negative-control  # break the engine on purpose; each named gate must go RED
cargo xtask lane-coverage     # does anything actually RUN this check?
cargo xtask sync-status       # is ../Stockfish still AT the pin every oracle assumes?
```

**A gate is done when it has been SEEN TO FAIL, by mutation rather than by argument** — not
when it passes. And every allowance a gate grants (a skip, an excuse, an exemption) needs an
owner that EXPIRES it: `lane-coverage` reports an excuse for a step that plainly runs, a unit
test refuses an excuse for a step that no longer exists, and `docs-lint`'s internal-area sweep
asserts each of its two exempt files still contains what it is exempt for.

Each of these found something on its first run that reading it had not — `arch-determinism`
was in no lane at all, six tracked files named the gitignored working area — and in two cases
the defect was in the gate BEING WRITTEN rather than in the code it was aimed at.

## Traps that cost real time

| trap | where |
|---|---|
| `signature-update` on a **red** gate launders a bug into the anchor. Fix the code, then re-derive. `golden-update` now REFUSES, because it drives rfish and writes a photograph of rfish; `golden-audit --write` drives the oracle. Do NOT reach for its override to get past a red gate — that is the one way around the refusal, and it deletes the finding. | [CONTRIBUTING.md](CONTRIBUTING.md) |
| A `.uci` fixture is piped **RAW**, so a `#` line is a COMMAND, not a comment. `fixture-coverage` refuses one; the `.fens` corpora are read by a gate rather than piped and do carry headers. | `tools/fixture_properties.tsv` |
| `tools/perft.table` is **not** a golden. Those counts are facts about chess; a mismatch is always a movegen bug. | [CONTRIBUTING.md](CONTRIBUTING.md) |
| The engine must run from `resources/` — it looks for its net relative to the working directory, and a run from the repo root silently finds none, falls back to the classical scaffolding, and reports a node count that looks entirely plausible. | [docs/10-tooling-ci.md](docs/10-tooling-ci.md) |
| A measurement without a net is a measurement of a different engine. Check for the `info string NNUE evaluation using …` line before believing any node count. | [docs/03-engine-eval.md](docs/03-engine-eval.md) |
| Release builds have `overflow-checks = false`; every intended wrap says `wrapping_*` in the source. A bare `+` that wraps in release and traps in `gate` is a bug the gate profile is there to catch. | [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) |
| The default build sets no `-C target-cpu`. `cargo xtask build --arch <tier>` does, and it changes what the NNUE loops vectorise to — so a perf number without its tier is not a number. Every tier is ENUMERATED and `native` only selects one of them; a number filed under host-specific codegen is reproducible nowhere. | [docs/10-tooling-ci.md](docs/10-tooling-ci.md) |
| `signature` builds at the DEFAULT arch, so it tests the portable arm and cannot see an ISA-gated divergence — and the NNUE kernels are `std::simd`, whose lowering the tier decides. `cargo xtask arch-determinism` is the gate that can. It blocks in CI now, but it only BENCHES the tiers its runner can execute — the hosted fleet is mixed, so the three AVX-512 rungs are usually built and not driven there. Run it locally on an AVX-512 box after touching a kernel; that is the only run that covers all five, and CI names the tiers it left unbenched rather than counting them checked. | [docs/10-tooling-ci.md](docs/10-tooling-ci.md) |
| A cost regression is invisible to every gate in `parity`: the anchor pins the NODE count, not what a node costs. `cargo xtask perf-budget` holds the instruction count to a recorded row, and `cargo xtask budget-ab` does it against a git ref with NO stored row — both sides built, so the toolchain cancels. Neither can say a refactor emitted the SAME CODE; `cargo xtask codegen-equiv` compares the disassembly symbol by symbol, which is the only gate that answers a "no functional change" claim. All three refuse a clean checkout or a moved node count rather than reporting a comparison they did not make. | [docs/10-tooling-ci.md](docs/10-tooling-ci.md) |
| **`perf-budget` SUBTRACTS startup, so it cannot see the net load or the magic tables.** Those were 1,281M instructions against a 1,524M search — nearly half a `bench` before it searches a node — and 17% of it was defect. Measure that axis with the `quit`-only profile the budget subtracts, and `/usr/bin/time -f "%M"` for peak RSS. A gate that subtracts a cost is a gate that hides it. | [docs/03-engine-eval.md](docs/03-engine-eval.md) |
| "Improving" on upstream. A cleaner formulation that moves a rounding boundary moves the node count. | [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) |
| **A defaulted trait method is a call site nobody has to write.** `InfoSink::current_move` shipped with a no-op default body, an EMPTY `if` block where the search should have called it, and a comment saying the shell did the reporting. No implementor existed, and a whole upstream `info` line was absent from the port. Grep for implementors, never for the declaration. | [docs/02-engine-search.md](docs/02-engine-search.md) |
| Comments are **imperative mood**; never pin a number a gate computes. | [docs/12-writing.md](docs/12-writing.md) |
| **Never exercise `Threads` near its declared maximum.** A worker is ~15.6 MB resident here (measured: 251 MB at `Threads 1`, 362 MB at `Threads 8`), so `Threads 1024` — the option's own max — is ~16 GB, and a harness running two engines is ~32 GB. That takes down a WSL2 VM and a CI runner, and it has: both sibling ports lost one. Cover the declared bounds with the `uci` listing diff, and keep every harness at 1, 2, 8 or 16. | this file |
| **An option value that becomes an allocation must be bounded where it is parsed.** `Hash`, `Threads` and `NumaPolicy` are the three. `NumaPolicy` accepts a processor list, and upstream bounds each range but not their sum — 735 bytes of input reached 2.8 GB here before the allocator gave up. `numa.rs` bounds the total; keep it bounded. | `platform/numa.rs` |
| A golden must pin the ENGINE, not the runner. `info string Available processors` is the host's core count, so `filter_volatile` drops it. Anything else host-dependent belongs there too. | [crates/xtask/src/gates.rs](crates/xtask/src/gates.rs) |
| **An oracle built by a different compiler, or without PGO, is not a comparable oracle.** rfish is rustc/LLVM; a `g++` oracle measures GCC against LLVM, and a non-PGO oracle measures upstream's own shipped recipe against something nobody runs. Build both sides with `cargo xtask oracle` / `cargo xtask pgo`, which use clang at rustc's LLVM major and train both profiles on the same `bench`. | [docs/10-tooling-ci.md](docs/10-tooling-ci.md) |

## Performance work

Read [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) (porting patterns, measurement
discipline) and [docs/10-tooling-ci.md](docs/10-tooling-ci.md) (measurement tooling and its
blind spots) before proposing any optimisation. Four rules that outrank intuition:

- **Measure whole-binary, at a named `--arch` tier.** Instruction arithmetic over a diff is
  a guess, never a measurement, and the tier changes the answer.
- **Hold the TOOLCHAIN equal, not just the tier.** Both sides clang/LLVM at rustc's major,
  both sides PGO on top of LTO — `cargo xtask pgo` and `cargo xtask oracle`. This is not a
  detail: it is worth more than most rows in the eval ledger, and every ratio measured
  before it was in place understated the gap.
- **Subtract startup, by measurement.** A whole-process counter includes the net load and
  the magic-table build, and both are large next to a short bench.
- **Time is only comparable INTERLEAVED.** Alternate which binary runs first each round and
  report the median paired ratio with its spread; a batched best-of-N reads the thermal
  state, not the binaries.
- **Size an Elo run BEFORE starting it, at the RIGHT conversion.** 70 Elo per doubling is a
  long-time-control figure. Measured here against a PGO'd upstream, three cells
  (0.1+0.001 at two tiers, 1+0.01) all imply **138-152 Elo per doubling**. Use the figure
  that matches the clock, or the run is mis-sized before it starts.
- **A `Vec` on a per-node path is a defect, and no gate can see it.** Upstream allocates
  nothing per node. Measured here: three per-node allocations cost 100M instructions of
  malloc/free against upstream's 1.9M, at an identical node count. Hoist the collection to
  the worker and `clear()` it -- and note that the obvious fix, an inline array mirroring
  upstream's field, is measurably WORSE, because safe Rust must initialise it. See
  [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) section 9.
- **NPS cannot settle a few per cent on this box.** One unchanged binary read 240k-275k,
  and a cold run read 103k. Use callgrind for anything under ~10%.
- **A bounds check is not automatically the cost, and when it is, the cost is the LENGTH
  LOAD rather than the comparison.** Rust elides most of them. Find the check in the
  disassembly before reshaping code around it — and if reshaping is needed, it is still not
  a licence for `unsafe`; restructure so the bound is provable instead. Measured here:
  moving the search stack from `Vec<T>` to `Box<[T; N]>` was worth 16.0M instructions, while
  masking a square index to drop `piece_on`'s comparison COST 2.0M.
- **A guard proves nothing about a slice it is not stated against.** The magic search tested
  `idx >= size` and then wrote `table[offset + idx]`, whose length is not `size` — so the test
  bought nothing and the store paid a bounds check 11.5M times. Reslicing to exactly the length
  the guard names turns a runtime test into a compile-time one, for free: 93.5M there, 34.6M in
  the sparse affine layer, and part of 124.3M in the LEB128 decode. **Three zones, one defect**
  — look for a runtime-length slice reached by a composite index in any loop whose real work is
  arithmetic.
- **A kernel whose OUTPUT WIDTH is small is the one to disassemble.** Being small is what
  stops the vectoriser caring. `fold_psqt` accumulates eight `i32` — one AVX2 register — and
  emitted 33 `mov`s and no vector instruction; `fc_2` is 128->1, so a generic
  `propagate::<N>` instantiated at `Simd<i32, 1>` and LLVM widened it to `xmm` and then put a
  HORIZONTAL REDUCTION inside the loop. 9.3M between them, invisible in the source and
  invisible in the profile's symbol list. Both siblings record the same two traps on the same
  two kernels.
- **When one half of a pair gets an optimisation, check the other half.** `fold_into` was
  given indexed fixed-width weight rows and `fold_mirror` — the same fold, forty lines away,
  on the refresh path — was not. It was worth 28.9M there, MORE than on the half that got it,
  because the refresh applies more rows.
- **Two paths that both walk back can need OPPOSITE caps.** The roll-forward materialises
  every ply it steps through, so a long chain leaves work done and its cap is 12; the
  king-move delta materialises only its last ply, so a long chain is pure cost and its cap is
  3. Sharing the constant costs 38M. Measure a cap per PATH, not per file.
- **A walk-back that writes only its last step is a walk-back you will take again.** rfish's
  roll-forward concatenated a chain of plies into one fold and stamped only the destination,
  so `HOP_CAP` had to sit at two to contain it — and that cap then caused 92% of all
  accumulator refreshes. Materialising every ply on the way forward, as upstream's
  `AccumulatorStack::evaluate` does, inverts the cap's whole curve and was worth 113.0M.
  Measure the cap AFTER changing what a hop costs, never before.
- **The largest wins are constructs that read as free.** A `LazyLock` deref, an `Option` no
  caller can make `None`, a `Vec` whose length never changes, a loop that will not unroll
  because of a `break`, a horizontal reduction, a compute-then-copy pair, a range slice walked
  by an iterator, two tables read under one key, a container scanned where only the CHANGED
  elements matter. [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) sections 14, 15 and
  17 record thirteen such shapes, what each was worth, and the ten of the same shape that
  measured WORSE — read them before hand-optimising anything, in either zone.
- **A cold body inlined into a hot caller costs that caller its frame on every call, even
  though it never runs.** `MovePicker::next_move` is entered 1,268,056 times a bench and its
  three stage-setup arms run once per picker; `#[inline(never)]` on those three was worth
  −12.37M at avx2 and −12.17M at sse41, bit-exact. The cost never appears as a line — it lands
  on the caller's prologue, which reads as overhead nobody wrote. The test is whether the body
  runs on a DIFFERENT SCHEDULE from its caller, not whether it is large.
- **A candidate sized from a `--profile profiling` build is a ceiling, not an estimate.** That
  profile gives up inlining to keep symbols, so a `Vec::push` the release build hoists out of a
  loop entirely still shows there, line by line, in exactly the shape the defect table tells
  you to grep for. One such candidate was sized at 112M and measured a wash. Confirm against
  `perf-budget`, on BOTH tiers, before spending a session — a change that improves one tier and
  regresses the other moved code layout rather than removing work.
- **In a kernel that already vectorises, attack the ADDRESSING before the arithmetic.** The
  NNUE folds emit upstream's own instruction shape, and an explicit `std::simd` rewrite of one
  cost +177M — while indexing fixed-width weight rows instead of range-slicing and zipping
  them, and colocating a lookup's two tables behind one base, were worth 19.3M between them.
  Section 17 of the same file has both.
- **An instruction count cannot see a latency win.** Callgrind counts instructions RETIRED,
  so multiple accumulator chains, software pipelining and unrolling-for-ILP can only ADD to
  it however much wall clock they buy. Decide which quantity a change is meant to move before
  measuring it.
- **Comparing one function's cost across two ports is void when they inline differently.**
  Compare zone totals, or match CALL COUNTS first and derive a per-call cost — the counts are
  identical across the three ports, which is what makes them the reliable instrument.

## Fleets and subagents

Multi-agent work is a standing pattern here. Each rule below was paid for in a sibling
port:

- **Charter a fleet only above the bar in *Working here*** — independent, sizeable tracks.
  Below it one agent working end to end beats three coordinating, and a fleet spawned to
  double-check a finished change buys nothing.
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

## Before you reply

Keep it short and lead with the outcome: what moved, what the gate said, what is left. The
long form belongs in the commit body.
