# Performance

Every gate in `parity` answers a question about behaviour, and every one of them stays green
while the same tree gets slower: the anchor pins the NODE count and says nothing about what a
node costs. This page owns the instruments that can see cost, and the conditions under which
a ratio taken with one of them means anything at all.

There are six, and they are not substitutes for each other. The pages that use their output —
[03-engine-eval.md](03-engine-eval.md)'s ledger, [08-idiomatic-rust.md](08-idiomatic-rust.md)'s
shapes, [05-tablebases.md](05-tablebases.md)'s probing cost — quote figures produced here.

## Which axis answers which claim

Pick by what the change CLAIMS, not by what is cheap to run. The third column is the reason
the row above is not a substitute: an axis quoted without its blind spot is an axis being
over-trusted.

| the claim | the axis | what it cannot see |
|---|---|---|
| "it costs the same as the recorded row" | `perf-budget` | cache, branches and latency; a tier callgrind cannot execute; and without `--syzygy`, the tablebase reader entirely |
| "it costs the same as that commit" | `budget-ab` | the same, plus nothing at all on a clean checkout or a moved node count — it refuses both rather than reporting a comparison it did not make |
| "the compiler emitted what it emitted before" | `codegen-equiv` | anything that changes a SIGNATURE, because it matches bodies by name; and anything in DATA rather than in `.text` |
| "the hardware behaves the same" | `counters` | where the cost is per component, and the three AVX-512 tiers |
| "it is faster" | `perf` | anything under ~10% on this box; a spread that straddles 1.000 has established no direction |
| "it reaches its answer the same way" | `fingerprint` | a callee inlined INTO its caller, and any code the workload never reaches |
| "both sides searched the same tree" | `signature` | cost — which is what every row above exists for ([10-tooling-ci.md](10-tooling-ci.md)) |

Two of the six take `--syzygy`, and it is not a refinement: the bench list never enters the
prober, so without the flag a whole zone of the port sits outside every row here.

## Comparability: what must be held equal before a ratio means anything

A ledger entry is a ratio against an upstream build, so it is a statement about the two
engines only when everything that is not an engine has been held equal. Three things had
not been, and each moved the answer by more than the code changes the ledger tracks. All
three are now the job of `cargo xtask pgo`, `oracle` and `perf`.

- **The COMPILER, on both sides.** rfish is compiled by rustc, whose backend is LLVM. An
  oracle built by `g++` measures GCC against LLVM at least as much as it measures upstream
  against rfish — and it is not a small term: rebuilding the same upstream SHA with `clang++`
  instead of `g++` moved its instruction count by double-digit per cent here. `cargo xtask
  oracle` builds with `clang++`, and refuses to pretend the major version does not matter:
  match it to the LLVM rustc carries, which `rustc -vV` prints.
- **PROFILE-GUIDED OPTIMISATION, on both sides.** Upstream's own shipped recipe is
  `make profile-build` — PGO on top of LTO — and it is what a player runs and what fishtest
  measures. rfish had no PGO path at all, so every ledger row compared a profile-guided C++
  binary against a rustc build that never saw a profile. `cargo xtask pgo` adds the missing
  half: instrument, train on `bench`, merge, rebuild. Both sides train on the same workload
  because upstream's `profile-build` trains on its own `bench` too.
- **The TIER.** Already covered by `--arch`, and `--tier` now names the pair — rfish's
  `-C target-cpu` and upstream's `ARCH=` for the same machine — so one flag moves both sides.

**PGO cannot move the node count**, and `signature` against the PGO binary is what says so.
The profile steers block layout and inlining; a bench total that moves under it is a bug in
the engine, not a property of the profile.

## `perf-budget`

**The regression nothing else sees.** `signature` proves the same NODE count and says
nothing about what those nodes cost, so a change can shed no nodes, keep every gate in
`parity` green, and still run measurably slower.

```sh
cargo xtask perf-budget --tier avx2          # hold the count to the recorded row
cargo xtask perf-budget-update --tier avx2   # re-record it
```

It builds at the tier into `target/budget/<tier>` — never `target/release`, which
`signature` rebuilds at the default arch — profiles `bench 16 1 8` under callgrind twice, and
holds the median to `tools/instr_budget.golden` on **two axes**: the search within **0.005%**
and **startup within 1%**.

Five properties, each of which cost a sibling port a wrong verdict before it was fixed:

- **Startup is GATED, not merely subtracted.** It used to be measured, subtracted, printed and
  then not judged — so the net load and the magic-table build could regress by any amount
  behind a green exit. They are not small: 1,053,602,750 instructions here against a
  1,512,297,444 search, **41% of the whole bench**, and the last time the axis was read by hand
  17% of it was defect. A gate that subtracts a cost is a gate that hides it. The two axes
  carry **different tolerances on purpose**: startup is paid once per process and the search is
  paid for minutes, so a tenth of a percent in the loader is invisible to a player and the same
  tenth in the search is what the search tolerance exists to refuse. Both verdicts are computed
  and both are printed before either exits, so a run cannot report one axis clean while staying
  silent about the other. Seen to FAIL by mutation: a startup budget moved to 1,032,000,000
  gives `startup REGRESSED by 2.0933%, outside 1.0000%`, exit 1, with the search axis still
  reading +0.0000%. Taken from `../Stockfish refish`'s `d657cfae`, which found the same hole in
  the same instrument.

- **The tolerance is set by MUTATION, not by feel.** Forcing `Position::adjust_key50` out of
  line — the per-node class this gate owns — costs **+0.0541%** here with `signature` still
  green. ../zfish shipped 0.20% and ../mcfish 0.5%; both watched that regression sail
  through. Measured spread on this box is **ten instructions in 1.7e9 across a from-scratch
  rebuild**, so 0.005% is ~8000x the noise and ~11x under the mutation. Verified both
  directions at the shipped value: clean → exit 0 at +0.0000%, mutated → exit 1 at +0.0541%.
- **Every row is keyed by an ENUMERATED tier**, and `native` resolves to one before a row is
  ever written — so a row names a build any host of that tier can reproduce. That is what
  removed the hazard ../zfish 7d4de85f refused by hand and ../mcfish 71c3fae3 first tried to
  key around; see [10-tooling-ci.md](10-tooling-ci.md)'s tier section. The key carries the
  tier AND its `target-cpu`, so a row written before a tier-table change is named rather than
  matched. A tier callgrind
  cannot execute — the three AVX-512 rungs — is refused for that reason and no other.
- **The node count is in the row.** A count taken over a different tree is not comparable,
  and the gate says so instead of reporting the difference as cost.
- **A missing row SKIPS at exit 2**, never passes. "Could not measure" must not read as "did
  not regress". A row in the **pre-startup four-column format** is REFUSED rather than read
  with the missing column defaulted, because a startup budget of zero passes every
  measurement — and the refusal is decided AFTER the tier is matched, so a stale row for a
  tier nobody is measuring cannot block the tier that is.

The golden is **gitignored and per-machine**: the count is a property of the toolchain and
the libc as well as of the code. Record your own, and re-record after a toolchain bump, a
net change or a deliberate perf commit — a budget raised to fit the tree gates nothing.

## The probing axis — `--syzygy`, the workload the bench list cannot reach

**Every position in the bench list has more men than the shipped three-man corpus covers.**
`TbTable::new`, `do_probe_table` and `decompress_pairs` are therefore absent from every figure
on this page above this line: a zone of the port with its own decoder, its own index
arithmetic and its own untrusted parser sat outside every cost gate in the tree. `tb` proves
the prober's ANSWERS against upstream and says nothing about what they cost.

```sh
cargo xtask perf-budget --tier avx2 --syzygy          # against a recorded row
cargo xtask budget-ab   --tier avx2 --syzygy          # against a git ref
cargo xtask counters    --tier avx2 --syzygy          # cache and branches, against upstream
```

The workload is `DIFF_BENCH`'s own hash, threads and depth over a different corpus — `bench 16
1 8` on `tools/cases/tb.fens` with `SyzygyPath` set — so the only variable between the two axes
is which code the positions reach. It is **313,744 nodes and 14,080 tbhits**, and it costs
9.28 G instructions against the bench workload's 1.51 G: 29,600 instructions per node against
8,750, because on this corpus the prober IS the workload. Depth 12 buys a third more probing
for four times the callgrind time, which is a gate nobody runs.

The corpus is stripped of its `#` header into `target/probe/` first, and both binaries are
pointed at that: this port's `bench` skips comment lines and **upstream's does not** — it
answers `CRITICAL ERROR: Invalid FEN. Invalid piece: #`. A differential axis whose workload
only one side can run is not a differential axis.

Three properties it needs to be worth having:

- **A probing run that loaded no tables is REFUSED**, not reported. It reads as a plausible
  short bench and the prober never runs — the same failure as measuring an engine that fell
  back to the classical evaluation, and it is refused on the same terms.
- **A run that did not read the CORPUS is refused too**, and that check exists because the
  first version of this axis needed it: a wrong path left `bench` falling back to its built-in
  list, with the tables still loaded, and produced a complete and entirely wrong measurement
  that the tables check could not see. The gate now counts the positions searched against the
  corpus's own line count.
- **The row is keyed `<tier>+syzygy`.** A probing row and a bench row describe one binary
  answering two different questions, and a row that answered one must never be read as an
  answer to the other.

**What it is for, demonstrated rather than argued.** With `#[inline(never)]` on the tablebase
decoder — behaviour-neutral, and the bench never calls it:

| gate | verdict |
|---|---|
| `signature` | the anchor, unmoved — **PASS** |
| `perf-budget --tier avx2` | +0.0000%, **PASS** |
| `perf-budget --tier avx2 --syzygy` | **+0.9048%, FAIL, exit 1** |

Two gates green and one red on the same tree is the whole argument for the axis. Taken from
`../Stockfish refish`, which files the same gap as `T5` and closed it on both halves — the
instruction axis and the counter axis — which is why `counters` takes the flag as well.

**The first probing counters run, against upstream at the pin**, both PGO, 313,744 nodes on
both sides:

| | ratio |
|---|---:|
| instructions | 1.206 |
| data reads | 1.467 |
| data writes | 0.719 |
| D1 read misses | 1.071 |
| L1 icache misses | 1.641 |
| conditional branches | 1.115 |
| **conditional mispredicts** | **0.399** |
| indirect mispredicts | 0.672 |

rfish retires a fifth more instructions in this zone and mispredicts **sixty per cent fewer
branches**, which is the bucket length table in [05-tablebases.md](05-tablebases.md) showing
up on the axis it was expected to: upstream
still walks. This is the first cache-and-branch reading this port has of the tablebase zone,
and it is the axis `refish`'s own performance page calls its open question.

**It also found a hole in this gate.** `verify_oracle` proved the upstream half was built at
the pin and nothing proved rfish's own half was built from the tree in front of you — a PGO
build is expensive, so it is naturally kept and reused. A stale one searched 316,793 nodes on
the probing corpus where the current tree and upstream both search 313,744, and the differential
caught it only because that workload made the divergence visible; on the bench workload it
would have matched and every ratio would have described code nobody was looking at. `counters`
now refuses a PGO build older than the newest source file.

## `budget-ab` — the same budget, with no stored golden

`perf-budget`'s golden is per-machine, so it binds on the box that derived it and nowhere
else: a fresh clone has no row, and CI has none it could trust. `budget-ab` builds **both
sides** instead, so the toolchain, the tier, the net and the workload cancel by construction
and there is nothing to store.

```sh
cargo xtask budget-ab --base HEAD~1 --tier avx2     # this change, against its parent
```

It builds `--base` from a **detached worktree** under `target/budget-ab/` — never by moving
this checkout, which would risk the work it was asked to judge — counts both sides the way
`perf-budget` does, and holds the delta to the same two tolerances — 0.005% on the search,
1% on startup — printing both axes as one table before either decides.

Two refusals rather than a number:

- **A clean checkout is refused, at exit 2.** The step compares the WORKING TREE against a
  ref, so with nothing changed both sides are one build and the delta is zero by
  construction. A zero that was never in doubt reads exactly like a change that cost nothing.
- **Unequal node counts are refused.** A smaller count is a smaller workload; dividing one by
  the other would report a different search as a cheaper one.

Interleaving is deliberately absent and buys nothing here: callgrind counts instructions
RETIRED and is deterministic, so the two sides do not compete for a thermal state the way
`perf`'s wall-clock A/B does.

Verified both directions on this box, `--rounds 1` at `avx2`, 172,793 nodes on both sides:

| | search Ir | |
|---|---|---|
| A/A floor — only `crates/xtask` changed, which cannot reach the engine binary | 1,512,267,902 → 1,512,267,911 | **+9, +0.0000%** |
| `Position::key` forced out of line | 1,512,267,844 → 1,513,680,447 | **+1,412,603, +0.0934%**, exit 1 |

The floor is nine instructions in 1.5e9, and the mutation is nineteen times the tolerance —
the same shape `perf-budget`'s own calibration has, without a row to keep.

## `codegen-equiv` — the gate for a "no functional change" claim

Every other instrument here answers a question about BEHAVIOUR, and all of them stay green
when a rewrite keeps the behaviour and costs instructions. `perf-budget` and `budget-ab` see
that, but only above a tolerance and only on a tier callgrind can execute. Neither says the
thing a refactor actually claims, which is that **the compiler emitted what it emitted
before**.

```sh
cargo xtask codegen-equiv --base HEAD~1 --tier avx2
```

It builds both sides at `--profile profiling` — release codegen with the symbol table kept —
disassembles `.text`, and compares symbol by symbol. A genuine rewording reports every symbol
identical; anything that moved a bound, changed an inlining decision or handed the register
allocator a different problem names the symbols it moved.

**It cannot settle a SIGNATURE change, and the reason generalises.** A parameter's type is
part of a Rust symbol's mangled name, so retyping one renames the function — and this gate
matches bodies BY NAME, so a renamed symbol reads as one removal and one addition with no
body compared at all. Only a change that keeps every signature can use this gate as proof.
Retyping the tablebase root probe's two flags is the worked example: the callee was inlined,
so nothing was added or removed and three CALLERS moved by one to three instructions each —
enough for the gate to refuse the claim, which is the right answer, and the commit says so
rather than calling itself a pure refactor.

Four normalisations, and each is a place where identical code prints differently once
something before it changes size:

- **the crate disambiguator**, `Cs<hash>_` in a v0-mangled name. It is derived from crate
  metadata including the manifest PATH, and the baseline builds in a worktree at a different
  path — left alone, every Rust symbol reads as renamed;
- **branch and call targets**, which print an absolute address beside the symbol. The symbol
  is the content; the address is where it landed;
- **rip-relative operands**, which print a displacement plus a resolved comment. Again the
  comment names the datum and the displacement is where the datum landed;
- **trailing alignment padding**, which is not codegen. A function whose body is untouched
  still ends on a different run of `nop`s once anything before it changes length. This is
  `f1318daa`'s correction to the original, and without it every symbol downstream of a real
  change reports as changed — which is every symbol.

A small immediate is deliberately NOT collapsed: five hex digits is the floor, because a
changed constant is exactly the kind of change this gate exists to catch.

Verified both directions at `avx2`, over **1029 symbols**, with the two sides built in
different directories:

| | result |
|---|---|
| only `crates/xtask` changed | 1029 of 1029 byte-identical, exit 0 |
| the capture-scoring weight `7` → `8` in `MovePicker` | one symbol named — `MovePicker::init_captures`, 115 → 114 instructions — exit 1 |

The positive control is the load-bearing one: 1029 symbols matching across two build
directories is what proves the normalisations have no false positives.

**What it cannot say.** Identical code is not identical behaviour when the change is in
DATA — a different constant in a table, a different net, a different `static` — none of
which is in `.text`. `signature` remains the gate that proves the engine.

Both steps are **local and excused from `parity`** for the same reason: inside `parity`, where
a clean checkout makes the two sides one tree, they have nothing to compare.

## The call-count fingerprint

`cargo xtask fingerprint` asks the question none of the value differentials can — not
"does rfish compute upstream's numbers" but **"does it get there by calling what upstream
calls, as often"**. Every other differential in this tree compares VALUES — the bench
anchor, the goldens, `nnue-check`, `upstream-nodes` — and every one of them passes over a
state divergence that happens not to move a number on the positions it drives.

It found two on its first run, both in how a move is rendered for output, and both invisible
to every value gate:

- the PV reporter walked a **cloned position**, playing each PV move to name the next, and
  truncated the line at the first move that failed a legality check. Upstream renders against
  the root — `UCIEngine::move(m, pos.is_chess960())` — and prints the stored line whole.
- `bestmove … ponder X` named the ponder move in the position **after** the best move, cloning
  and playing it first. Upstream names it against the root too.

Together: 1492 position clones and 1492 `do_move`/`undo_move` pairs per `bench 16 1 8` that
upstream never performs, and a PV that could come out shorter than upstream prints it. The
node count, all 51 `bestmove` lines, all 51 ponder moves and all 394 `info` lines were
identical throughout — which is exactly why nothing else could see it.

**Why call counts and not cost.** A call count is inlining-immune at the callee: it does not
care how the callee was reached, only that it was. That is what lets a rustc tree be compared
against a clang one at all, where a cost claim has to argue attribution first. callgrind also
simulates rather than samples, so the answer is deterministic — which matters on a box where
NPS cannot settle a few per cent.

It is **not** immune at the caller, and that limit is load-bearing: upstream inlines
`legal`, `gives_check`, `see_ge` and `undo_move` into its search templates under LTO, so its
symbol counts omit every inlined site. Those four are therefore NOT gated, and
`tools/fingerprint_groups.tsv` records why with the numbers. The proof needs no second
measurement: upstream reports `do_move` 163345 and `undo_move` 68736 over the same bench, and
every do_move in a search is undone, so 94609 of its undo_moves were inlined away. Gating
them would assert that two compilers inline identically, which is not a fact about this port.

**The same limit cuts on rfish's side, where a refactor trips it exactly like a compiler
change would.** Two rows named entry points that stopped surviving as symbols, and the weekly
lane reported both as MISS on 2026-08-10: `do_move_checked` became a one-line wrapper over
`do_move_recording` when threat recording landed — and the search calls the recording form
directly — while `eval::evaluate` inlines into `SearchWorker::evaluate`, which is itself
inlined at three of its six call sites and so counts 35771 against upstream's 62975. Both rows
now name a callee that survives whole: `do_move_recording` at 163345 and
`LayerStack::propagate` at 62975, each EXACT. Prefer the callee when a row has to move, and
re-derive the count before believing a wrapper — a caller symbol can undercount without ever
reaching zero, and that is the one shape this gate cannot tell from a real divergence.

A group whose pattern matches nothing on one side is a **MISS and fails**, never a zero — a
symbol the compiler inlined away would otherwise read as agreement at zero-versus-zero
forever. Both failure modes were proved rather than assumed: reinstating the ponder clone
turns `do_move` red at exactly +49, and a pattern matching nothing reports MISS; the step
exits 1 naming both.

It is ~50x slower than the bench it profiles, so it stays out of `parity` and runs in the
weekly lane, which already builds an oracle.

## An oracle must be stamped, and the pair must share a net

`cargo xtask oracle` writes the upstream SHA it extracted into `.rfish-oracle-base` beside the
tree, and `perf` and `fingerprint` refuse an oracle whose stamp is not `UPSTREAM_BASE`.

**This caught a real one.** The oracle directory is built once, reused, and lives OUTSIDE the
repository, so advancing the pin leaves it untouched and nothing about its filename changes.
After the `c5aef2bf1` sync the avx2 oracle here was still the `23cf5d82` tree — it benched
3184328 where the pin benches the anchor — and every measurement taken against it compared this
port to an upstream it is not a translation of. The instruction differential would eventually
have noticed, because it compares node counts before quoting a ratio; eventually is the
problem, since that check only runs when callgrind does, so at `--tier native` or on a box
without valgrind the wall-clock A/B ran against the wrong binary and reported a ratio with no
warning at all.

Both commands also refuse a pair that loaded **different nets**, for the same reason both
sibling ports added that check: a node count is a property of the net as much as of the

## Measurement, and each instrument's blind spots

- **`perf stat` / `cachegrind` count the whole process**, which includes the magic-table
  build and the net load. Both are large next to a short bench. Subtract startup by
  measurement, and only on the instruction axis — on the cycles axis the subtraction removes
  a term whose error rivals the effect. Time the search directly instead: the bench's own
  total contains no startup by construction.
- **The box these numbers came from is NOT quiet, and pairing does not fix that.** A laptop
  part under WSL2, measured in sessions that were also compiling and profiling. The paired
  protocol below removes the order bias and the drift between batches; it cannot remove the
  variance within a round, and the published spreads — a median of 1.53 over a 1.33..1.65
  range — are what remains. Quote a time ratio to one significant figure and no further. The
  instruction axis carries no such caveat: callgrind counts are deterministic, so an Ir ratio
  here is reproducible to the digit and is the axis any claim under ~10% must be argued on.
- **A batched best-of-N wall-clock reading is thermally void.** Running A five times and then
  B five times measures the order as much as the binaries: the second batch runs on a hotter
  core. Time is only comparable INTERLEAVED, with the order alternating each round and the
  MEDIAN of the paired ratios reported alongside its spread — a spread that straddles 1.000
  has established no direction and must be reported as establishing none. `cargo xtask perf`
  runs that protocol; it is the one the sibling C port drives its nps A/B with, whose header
  records that the rule was paid for by publishing "the spine is at parity" over a real
  deficit.
- **callgrind has a TIER CEILING.** It implements no AVX-512 and SIGILLs on the first
  instruction it does not know, so the instruction axis tops out below the tier a player
  actually builds. Measure instructions at `avx2` and time at `native`, and never quote one
  tier's instruction ratio beside another tier's time ratio as though they agreed.
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

## The gates

None of these runs in `parity`, and each is excused there with the reason `lane-coverage`
prints: a per-machine golden, a working tree to compare against, or an oracle and a PGO build
that a push lane cannot afford.

| gate | what it proves here | owned by |
|---|---|---|
| `perf-budget` | retired instructions against a recorded row, on two axes — the search at 0.005% and startup at 1% | this page |
| `budget-ab` | the same two axes against a git ref, with both sides built and nothing stored | this page |
| `codegen-equiv` | per-symbol machine-code identity between the working tree and a ref | this page |
| `counters` | what the hardware did: reads, writes, D1 and icache misses, branches and their mispredicts | this page |
| `perf` | the interleaved paired wall clock, reported as a median ratio with its spread | this page |
| `fingerprint` | rfish still reaches its answer by CALLING what upstream calls, as often | this page |
| `signature` | that both sides searched the same tree, without which every figure above is void | [10-tooling-ci.md](10-tooling-ci.md) |

