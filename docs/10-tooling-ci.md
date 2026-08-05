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

`fmt` → `clippy` → `unsafe-lint` → `docs-lint` → `lane-coverage` → `fixture-coverage` →
`async-check` → `test` → `perft` → `golden` → `golden-audit` → `nnue-check` → `tb` →
`signature`

Cheap and structural first, so a formatting mistake is reported in seconds rather than
after a two-minute bench. `fmt-fix` is the same gate with the fix applied, and is not one
`parity` runs. Read the list from `gates::parity` when it matters: prose cannot be gated,
and a list that drifts by one entry reads exactly like one that has not.

### `signature`

The anchor. `bench` must reproduce `tools/signature.golden`.

**The depth is upstream's**, which became affordable when the NNUE forward pass landed and
the tree stopped being enormous. Read it from `crates/xtask/src/gates.rs`, never from prose.
The COUNT is upstream's too: the golden equals a pristine upstream build's `Bench:` at the
pin, so a diff is a porting REGRESSION rather than a tuning difference — see
[CONTRIBUTING.md](../CONTRIBUTING.md), "One number, and what a diff against it means".

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

Five checks: every markdown link resolves, every `crates/…` or `tools/…` path named in prose
exists, no page quotes the current bench anchor, no `xtask` step goes unnamed by every
shipped page, and no tracked file names the internal working area. The middle two hold the
rules [12-writing.md](12-writing.md) records as the most-broken — a pinned number and an
undiscoverable step — and both read their subject from its owner (`tools/signature.golden`,
the dispatch table in `crates/xtask/src/main.rs`) rather than from a second list here.

**The last check sweeps the whole INDEX, not the markdown set**, and it exists because the
path check above cannot see its class. That check exempts a path `.gitignore` names, on the
grounds that an ignored path is one the repository decided not to carry and a doc naming it
is usually documenting the tool that writes it. The internal area is ignored, so every
reference into it landed in that exemption and reported clean — six tracked files were doing
exactly that, two of them engine sources. A source comment dangles for a reader precisely as
a doc line does, which is why the subject is every tracked file rather than every page.

Both sibling ports wrote this rule against a hand-written list of directories and both were
bitten by the same shape it guards: ../zfish's read eight paths, so its whole build package
and all of `.github/` were blind, and a file landed there four commits later; ../mcfish
established the rule, verified it by hand, and had it broken twice within days by commits
that had no way to know. `crates/xtask/src/devsweep.rs` carries the needles and `.gitignore`
declares the directory — those two files are the only ones allowed to name it, and the
exemption is asserted rather than assumed.

It settles the **mechanical** half of documentation rot, and [12-writing.md](12-writing.md)
names the three classes it cannot: a real symbol attributed to the wrong file, a list with
the wrong count or order, and a behaviour described as absent from a build that has it. It
cannot tell you a sentence has become false, and in the sibling ports every false claim ever
found got there by a commit that changed the code and not the page. That half is yours.

### `unsafe-lint`

No `unsafe` keyword, no `allow(unsafe_code)`, and `unsafe_code = "forbid"` still present in
the workspace manifest.

The compiler already rejects the first two. The gate exists because the manifest line is one
line and a reviewer can miss it being deleted — the property is asserted from **outside**
the mechanism that enforces it.

It scans the shipped crates only. `xtask` is a build tool that never enters the binary and
necessarily names the patterns it looks for; scanning it would make the gate report itself.
It is still covered by the workspace forbid, which the manifest check asserts.

### `lane-coverage`

**A lane that is in no gate is not a lane.** Every step the dispatch table answers to must
run in a workflow, run inside `parity`, or appear in `meta::EXCUSED` with a reason. A new
step joins one of the three or this goes red.

The rule was held by somebody remembering it, and ../mcfish's first run of the mechanical
version found four differentials that had quietly stopped being lanes — `upstream-parity`,
the finish line of that whole port, among them. rfish's first run found one:
**`arch-determinism` ran nowhere**, which is the gate AGENTS.md tells you to run after
touching an NNUE kernel and the only one that can see an ISA-gated divergence, since
`signature` builds the portable arm alone. It is now a blocking job in
`rfish_parity.yml`, where both siblings run their equivalent — 90s locally over all five
tiers is what makes that affordable.

Giving a local gate its first lane is where it meets a host it had never met. This one had
only ever run on AVX-512 boxes, so nobody had asked whether a runner could EXECUTE the tier
it builds; the first CI run drew an AMD runner and died on SIGILL at the third tier. See
[`arch-determinism`](#arch-determinism) for what the gate now derives from the host.

The excused list is the hole, so it expires in its own direction: a step excused that DOES
run somewhere is reported as a stale excuse, and a unit test refuses an excuse naming a step
the dispatch table no longer has. Two extraction bugs the sibling ports paid for are held
here too — a step named in a workflow **comment** is not a step the workflow runs, and a
word boundary that accepts a hyphen lets `xtask net-fetch` satisfy `xtask net`.

### `fixture-coverage`

**A test's input domain is not the arguments it passes — it is every property the code
branches on**, and a fixture set is only as good as the list of properties it was partitioned
over. That list was in nobody's head twice over: "does the golden corpus cover Shredder
castling notation, or an en-passant capture that exposes the king along the rank?" could only
be answered by reading three directories. `tools/fixture_properties.tsv` writes it down — 60
rows of `<property> <owner> <fixture> <witness>` — and this gate holds it to the tree in both
directions.

Direction 1: every row is still true. The owner exists, the fixture exists, and the witness
still appears in it, so a case that stops presenting its property reddens — the option line
deleted, the position rewritten, the file renamed. The witness is a literal **substring**,
not a pattern, with `\n` as the one escape, so it cannot silently match more than it says.

Direction 2: every file in `tools/cases/` appears in some row. The fixture universe is
globbed from the tree rather than listed in the table, because a second list rots exactly
like the first, and this is the direction that catches a case arriving with nobody having
answered "a representative of *what*?".

It also refuses a `#` line in a `.uci` fixture. **A `.uci` file is engine input**, piped raw,
so a line that looks like a comment is a command the engine answers `Unknown command` to and
the case diverges for a reason unrelated to what it tests. ../mcfish lost a milestone to
exactly that.

**What it cannot do** is prove that presenting a property exercises the owner's branch. That
needs coverage data this tree does not collect, and a green run says only that the fixtures
still present what the table claims.

### `async-check`

**No byte-golden can reach the interrupted-search path.** Every case in `tools/cases/` is
driven by writing all its lines and closing the pipe, so a `stop` there is read after the
search has already ended — and a stop that lands inside a *running* search ends it wherever
the clock got to, which moves the final `info` line's node count run to run. There is nothing
to pin.

So this gate asserts **invariants** rather than values, which needs no reference at all. They
are not rfish-authored expectations of upstream's output; they are properties of the UCI
contract:

1. a `stop` inside a running search yields exactly one `bestmove`, it is legal, and the
   engine still answers `isready`;
2. a bare `stop` with no search running answers nothing and stays up — an engine that replied
   here would be inventing a move;
3. `ponderhit` converts a pondering search and it still ends with exactly one `bestmove`;
4. `quit` during a running search exits. **The timeout is the assertion**: before `go` ran off
   the UCI thread this would have hung, and a hang in CI reads as an infrastructure flake
   rather than as the engine ignoring `quit`.

The legal move list comes from the engine's own `go perft 1` rather than being written down
here, so the gate carries no expectation of its own — and reading anything but 20 root moves
from the start position is a rig fault, not a verdict. 4 of 4, in 7s.

`negative-control` covers it: with `quit` no longer stopping an unbounded search, the gate
goes red in 59s. The mutant is bounded by the GATE rather than by the engine — `async-check`
caps its own wait at 30s and reports a broken invariant instead of hanging the run.

### Two framework checks with no subject here

The sibling ports carry two more meta-gates. Neither is ported, and the reason is the same in
both cases — **a gate with no subject is a gate that compares nothing**, which is the failure
the section below refuses:

- **`tools-smoke`** asserts that a tool no lane invokes still runs. ../mcfish had four such
  tools in `tools/`, one of which had rotted exactly as predicted. rfish has none: every tool
  here IS an `xtask` step, so `lane-coverage` covers the whole set and a broken one fails to
  compile in the same CI lane that checks the engine.
- **`counter-validate`** validates a `perf_event_open` counter against two workloads with
  known bottlenecks, because an instrument is a hypothesis until something confirms it. rfish
  measures with **callgrind**, which counts retired instructions deterministically rather than
  sampling a hardware counter, so there is no counter to validate. What that leaves unchecked
  is different and is stated where it belongs: `perf-budget` subtracts startup by measurement,
  and the paired A/B reports its spread.

If either acquires a subject — a shell tool in `tools/`, a sampling profiler in the loop —
it acquires a gate at the same time.

### A gate that compared nothing must not pass

`no mismatches` is true of an empty corpus, so every gate above whose verdict is a
`failures.is_empty()` can report "0 of 0 match" and exit 0. That is worse than a bare zero:
it does not merely pass having compared nothing, it publishes a comparison it never made.
`runner::compared_something` is the refusal, and `perft`, `golden`, `golden-audit`,
`nnue-check`, `tb` and both halves of `fuzz` are wired to it.

The distinction it draws is deliberate: a **missing** corpus is `Skipped` at exit 2 — the
gate could not run — while a corpus that is present and yields nothing is a rig fault and
goes RED. A filter that matches no test is the same shape one step further out: `cargo test`
calls "0 passed" a success, so `fuzz` asserts its soak actually ran rather than reading the
exit status.

../mcfish 01e0b71c found this in a transcript gate that fed both engines an unmatched glob
and counted their identical nothing as agreement; ../zfish 108e7af6 found a step checker
reporting OK while reading 6% of its subject.

**A non-empty corpus does not settle it, because two blank sides compare equal.** Every way
a comparison can fail blanks it — an oracle that dies before its banner, a filter that eats
the output, a golden re-derived against a dead engine — and `""` == `""` scores an agreement.
`golden-audit` refuses a case where both sides are blank, before it tallies either way, and
`golden` refuses a case whose engine printed nothing in BOTH modes: the two failures compose,
since an update writes the blank golden and the check then passes it against the next dead
run. Every case here ends by printing something, so a blank side is a dead engine and never a
behaviour. ../zfish a4f0b6e9 is the same equality one gate over.

## The local gates, which `parity` does not run

### `negative-control`

**A gate is done when it has been SEEN TO FAIL, not when it passes.** Every gate's power to
detect a defect is an assumption until something breaks the engine on purpose and watches the
gate go red, and a gate that has quietly stopped being able to fail is invisible — it reports
success, which is what everyone was hoping for. ../mcfish found two in one month: an empty
transcript corpus scored as agreement, and a docs check that read no subject.

Four rows, one representative mutant per gate. Each applies one behavioural mutation,
requires the named gate to exit non-zero, restores the file, and the run ends by rebuilding
and running `signature` green — the tree is proven clean by running a gate, not by asserting
it. Measured here:

```
negative-control: signature   -- futility margin base 45 -> 46        ok, red (1)
negative-control: golden      -- the board display omits `Checkers:`  ok, red (1)
negative-control: perft       -- no knight under-promotion            ok, red (1)
negative-control: nnue-check  -- network output scale 16 -> 17        ok, red (1)
4 of 4 gate(s) detected their mutation, tree restored     EXIT=0, 96s
```

**Perturb the value, do not remove the bound.** A mutant aimed at an evaluation must leave
the search a ceiling, or the experiment cannot end — ../mcfish's first NNUE mutant inverted an
activation clamp, which handed the search an evaluation with no ceiling, and the gate ran past
900s twice without returning a verdict. The row here scales the output instead: the engine
stays a sane engine searching a different tree, and the gate answers in seconds. A timeout is
therefore a **rig fault**, never a detection — crediting a gate for an experiment that never
finished is worse than not running it.

Three ways the rig can be wrong, and all three refuse rather than return a verdict: a `find`
string that has rotted (the tree is never mutated, the gate greens, and that reads as "the
gate failed to detect it"), a mutation the compiler rejects (not behavioural), and a selector
naming no row (mutated nothing, proved nothing). The restore runs from a `Drop` guard, so an
error or a panic anywhere in the run still puts the sources back.

It is **local and in no lane**: it edits tracked sources and rebuilds per row, so it cannot
share a checkout with anything. Run it when a gate is edited.

### `arch-determinism`

Every enumerated tier must bench the anchor. It builds the engine once per tier into
`target/arch/<tier>` and drives `bench` at the signature depth.

**`signature` cannot stand in for it.** That gate builds at the default arch, so it exercises
the portable arm and nothing else, while the NNUE kernels are `std::simd` and the tier decides
how each lane operation lowers — a saturation or a narrowing that behaves differently at 512
bits produces a different tree with every other gate green. Run it after touching a kernel,
and before adding a tier: that is what makes a new rung a checked change.

Outside `parity` because it is five release builds, and a blocking job of its own in
`rfish_parity.yml`. All five reproduce the anchor today, including the three AVX-512 rungs.

**A tier BUILDS anywhere and BENCHES only where the host can execute it.** A build at
`-C target-cpu=skylake-avx512` emits AVX-512 whatever the machine doing the building is, so
driving that binary on a box without AVX-512 raises SIGILL before the first node — a fact
about the host, not a verdict on the anchor. The gate therefore derives its executable set
from the host's own `target_feature` list, the same one `native` resolves through, builds
every tier regardless (a tier that stops compiling is still caught), and NAMES the tiers it
left unbenched:

```
  avx512 (skylake-avx512): BUILT, NOT benched — this host lacks avx512f, avx512bw, …
```

A host short of the top tier reports SKIPPED and exits 2, because the anchor is unasserted
for the tiers it could not drive. `--host-tiers` accepts that reduced coverage and passes,
still printing the hole. **The hosted CI fleet is mixed** — an AMD runner has no AVX-512 —
so the lane passes the flag, and the lane's first run had already died at the third tier
without it. The flag expires by itself: a runner that gains AVX-512 benches all five and it
stops excusing anything. Full five-tier coverage is a LOCAL run on an AVX-512 box, which is
the run to make after touching a kernel.

### `perf-budget`

**The regression nothing else sees.** `signature` proves the same NODE count and says
nothing about what those nodes cost, so a change can shed no nodes, keep every gate in
`parity` green, and still run measurably slower.

```sh
cargo xtask perf-budget --tier avx2          # hold the count to the recorded row
cargo xtask perf-budget-update --tier avx2   # re-record it
```

It builds at the tier into `target/budget/<tier>` — never `target/release`, which
`signature` rebuilds at the default arch — profiles `bench 16 1 8` under callgrind twice,
subtracts startup, and holds the median to `tools/instr_budget.golden` within
**0.005%**.

Four properties, each of which cost a sibling port a wrong verdict before it was fixed:

- **The tolerance is set by MUTATION, not by feel.** Forcing `Position::adjust_key50` out of
  line — the per-node class this gate owns — costs **+0.0541%** here with `signature` still
  green. ../zfish shipped 0.20% and ../mcfish 0.5%; both watched that regression sail
  through. Measured spread on this box is **ten instructions in 1.7e9 across a from-scratch
  rebuild**, so 0.005% is ~8000x the noise and ~11x under the mutation. Verified both
  directions at the shipped value: clean → exit 0 at +0.0000%, mutated → exit 1 at +0.0541%.
- **Every row is keyed by an ENUMERATED tier**, and `native` resolves to one before a row is
  ever written — so a row names a build any host of that tier can reproduce. That is what
  removed the hazard ../zfish 7d4de85f refused by hand and ../mcfish 71c3fae3 first tried to
  key around; see the tier section above. The key carries the tier AND its `target-cpu`, so a
  row written before a tier-table change is named rather than matched. A tier callgrind
  cannot execute — the three AVX-512 rungs — is refused for that reason and no other.
- **The node count is in the row.** A count taken over a different tree is not comparable,
  and the gate says so instead of reporting the difference as cost.
- **A missing row SKIPS at exit 2**, never passes. "Could not measure" must not read as "did
  not regress".

The golden is **gitignored and per-machine**: the count is a property of the toolchain and
the libc as well as of the code. Record your own, and re-record after a toolchain bump, a
net change or a deliberate perf commit — a budget raised to fit the tree gates nothing.

## Running the engine

**Always from `resources/`.** The engine looks for its net relative to the working
directory, so a run from the repository root silently finds none and produces an unrelated
number — one that looks entirely plausible. `runner::drive` sets the working directory for
every gate; a hand-run measurement must do the same. `perf-budget` asserts the
`NNUE evaluation using …` line for the same reason: a budget measured without a net is a
budget for the classical fallback.

## Build tiers

The **default** build sets no `-C target-cpu`, because the bench anchor has to be
reproducible on a machine nobody here owns, and `target-cpu=native` silently changes which
vector width the NNUE loops autovectorise to.

`cargo xtask build --arch <tier>` sets it per tier, through the child process's environment
rather than by mutating this process's — so a later gate in the same `parity` run cannot
inherit it.

**A perf number without its tier is not a number.**

### The tiers are enumerated, and `native` only SELECTS one

| tier | `-C target-cpu` | upstream `ARCH=` | callgrind |
|---|---|---|---|
| `sse41` | `nehalem` | `x86-64-sse41-popcnt` | yes |
| `avx2` (default) | `haswell` | `x86-64-avx2` | yes |
| `avx512` | `skylake-avx512` | `x86-64-avx512` | no |
| `vnni512` | `cascadelake` | `x86-64-vnni512` | no |
| `avx512icl` | `icelake-server` | `x86-64-avx512icl` | no |

`--arch native` / `--tier native` reads the host's `target_feature` set from rustc and names
the highest tier the box can run, printing which. **It never compiles
`-C target-cpu=native`**, and that is the whole point: such a build carries tuning and ISA
extensions no tier label records, so two hosts reporting the same label ship different
binaries and every per-tier number — budget rows, instruction ratios against the oracle, an
Elo standing — is a comparison across builds that cannot be reproduced anywhere.

The cost is real and belongs next to the benefit: a fixed-flag tier gives up whatever the
host-tuned build was worth, and buys a number that can be reproduced on any host of that
tier. ../mcfish 3b9fc8ae measured **+1.38%** for the same trade, and its history is the
argument for taking the cause rather than the symptom — it first keyed its budget rows by
the resolved host CPU, which records host-specific codegen instead of removing it. ../zfish
resolved `native` this way from the start.

**A tier is not free to add: `signature` cannot see an ISA divergence.** It builds at the
DEFAULT arch, so it tests the portable arm and nothing else, while rfish's NNUE kernels are
`std::simd` whose lowering the tier decides. `cargo xtask arch-determinism` builds every
enumerated tier and holds each to the anchor — that is what makes adding a tier a checked
change rather than a hopeful one. All five reproduce the anchor today.

**Add the rung on a host that can EXECUTE it.** The gate benches only the tiers the host
can run, and passes in CI over the reduced set the runner allows, so a new AVX-512 rung
added and checked on a runner without AVX-512 is a rung nothing benched. The gate names
what it left unbenched for exactly this reason — read that line, do not read the exit code
alone.

## Resyncing to a newer upstream

### `sync-status` — the pin, read in both directions

`tools/upstream/UPSTREAM_BASE` names the commit rfish claims to match, and everything
differential here is built from `../Stockfish`. The pin is therefore only meaningful while
that checkout is actually **at** it, and nothing local asserted that: CI checked how far
upstream's *master* had moved, which is a different question.

**The two directions are not the same finding.** A checkout **ahead** of the pin is normal —
upstream moved, the port has not followed — and prints the commit list, which is the re-port
worklist. A checkout **behind** the pin is a defect in the workspace and goes RED: it is the
golden, so every oracle built from it, and every grep of it, answers from source this tree has
already ported past. Counting only the first direction reports that state as "in sync", which
is worse than silence — it asserts the thing a reader would otherwise go and verify. A pin the
golden does not contain is red for the same reason: "0 commits behind" for a SHA nobody can
resolve is a drift report over nothing.

It runs in the weekly `rfish_upstream_check.yml`, immediately after that lane checks the
golden out at the pin — `--detach <pin>` can only be wrong quietly.

**rfish has no `UPSTREAM_TARGET`, deliberately.** ../mcfish carries a second pin because it is
mid-catch-up and needs to name the commit it is aiming at while the base says what it matches
today. A sync here is atomic: the base and `tools/signature.golden` advance in the same
commit, and a sync that cannot land bit-exact is a bug report rather than a sync. There is no
catch-up state for a second pin to describe, and a file with no role is scaffolding this tree
deletes rather than adds.

### Four things to get right when the pin moves

Each of these would otherwise produce a green gate over a wrong engine:

- **Land one upstream commit per commit, and check each against upstream built at THAT
  commit.** Not against the old pin, and not only at the end. Two search changes landed
  together cannot be attributed when the node count is wrong, and building the oracle once
  at the end hides which change moved what.
- **A commit with no counterpart is a RESULT, not a gap to skip silently.** Of the five
  commits in the last sync, two had no counterpart here — LoongArch intrinsics and a shared
  memory implementation rfish cannot have — and one was already equivalent. Record all three
  in the commit body; a reader who cannot tell "ported" from "did not apply" has to redo the
  analysis.
- **Rebuild the oracle before trusting a differential gate, and CHECK ITS STAMP.** A binary
  left over from the previous pin compares the new engine against the old upstream while
  reporting a clean pass. Writing that down was not enough: one commit later the same trap
  caught this repository anyway, because `../Stockfish/src` also held a **`stockfish-new`**
  from the old pin, that name sorted first, and every differential gate quietly adjudicated
  against the wrong commit. `find_oracle` now reads the short SHA upstream stamps into
  `id name` and refuses anything that is not `UPSTREAM_BASE`. Trust the stamp, never the
  filename or the timestamp.
- **A golden cannot see a divergence from upstream, so ADJUDICATE it.** `cargo xtask
  golden-audit` drives upstream through the same cases and diffs. It found three real
  differences the goldens had been recording as correct for as long as they existed.
- **A golden pins THIS engine, so a golden alone cannot see a divergence from upstream.**
  The last sync's `search.golden` had recorded a `bestmove` with no ponder move for years,
  green every run, while upstream printed one — `extract_ponder_from_tt` had never been
  ported. Re-deriving a golden records whatever the engine now does. Before accepting one,
  drive the same case through the ORACLE and diff.

**Driving the oracle needs a driver that waits.** Upstream runs `go` on a separate thread and
treats end-of-input as `quit`, so a script that writes every line at once and closes stdin
truncates the search and collects a `bestmove` from a search that never finished — which
reads as a divergence that is not there. Send a line, and after a `go` read until `bestmove`
before sending the next.

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

## CI

`.github/workflows/rfish_parity.yml` runs the same gates this page describes, on Linux, macOS
and Windows. Nothing in CI is a gate that does not exist locally, and nothing local is
weaker than CI — a contributor who runs `cargo xtask parity` and sees green should not be
surprised by the merge gate.

The four workflow files are named for the port and then for what they gate, as `../zfish`'s
are: **the file name and the displayed name are read in a list of four repositories' runs**,
and `CI` says only that a repository has some. Job names carry the platform for the same
reason — a red `test` says nothing a reader can act on, where `macOS aarch64 test` names the
one architecture no other lane covers. A workflow with ONE lane names it for what the lane
proves instead, because there is no sibling lane to tell it apart from: `Deep perft against
the reference counts`, `Detect upstream drift`, `Fidelity against the pinned oracle`. The
fuzz matrix names the HARNESS for the reason the `test` matrix names the platform, which is
the one the fuzz section below turns on.

Job **ids** follow the same rule one level down, and where all three ports run the same lane
they now spell it the same way: `parity`, `tsan-race` and `valgrind` are identical in
`rfish_parity.yml`, `zfish_parity.yml` and `mcfish_parity.yml`. That is worth the churn only
because these three files get read side by side — a session that fixes a lane in one port
almost always has to check the other two, and `gates` against `parity` is a rename to hold in
your head for nothing. Where the lanes genuinely DIFFER the names still do: `lint` runs four
gates rather than the siblings' single formatter, and mcfish annotates the compiler
(`parity (clang)`, `parity (gcc)`) because it gates two. `fidelity` keeps its own name for
the opposite reason — mcfish's `upstream-nodes` is the nearest thing either sibling runs and
zfish has no equivalent, so there is no two-of-three spelling to converge on.

**Renaming a lane is cheap HERE, and check both halves before assuming it is cheap again.**
A job id is named by a `needs:` edge, from inside the same file; a displayed name is named by
a REQUIRED STATUS CHECK, from the branch protection rules, and orphaning one leaves the
branch waiting forever on a check that will never report. No workflow here has a `needs:` edge
and `main` is unprotected — `gh api repos/:owner/:repo/branches/main/protection` answers
`404 Branch not protected` — so both renames touched nothing but the rows of this page.

Every lane, which is what the file has rather than the ones worth mentioning:

| Lane (job id) | Displayed as | Runs |
|---|---|---|
| `lint` | Format, clippy and the unsafe gate | `fmt`, `clippy`, `unsafe-lint`, `docs-lint` |
| `test` | Linux x86-64 / macOS aarch64 / Windows x86-64 test | `cargo xtask test` on three platforms |
| `parity` | Linux x86-64 parity | `net`, `tb-fetch`, `perft`, `golden`, `signature` on Linux |
| `tsan-race` | Linux TSan race gate | `net`, then a four-thread search under ThreadSanitizer |
| `valgrind` | Linux valgrind memcheck | `net`, `build`, then the binary under memcheck |
| `cross-build` | Cross-build (`<target>`) | `cargo check` for `aarch64-unknown-linux-gnu` and `x86_64-pc-windows-gnu` |

`cross-build` **checks rather than builds**, and the reason is worth keeping: the runner has
no cross LINKER for either target, so a build fails at the link step having already proven
everything the lane is for. What it is for is catching a `cfg` sneaking into engine code —
the engine's only real platform differences are the `SyzygyPath` separator and the binary's
extension. Every lane reads its toolchain from `rust-toolchain.toml` rather than naming a
channel, because that file overrides whatever the action installs: a hard-coded channel
installs one toolchain and silently runs another, without the target's `std` in this lane.

`cargo xtask tb-fetch` downloads the 3-man Syzygy set — ~26 KiB, ten files — and CI caches
it keyed on the fetcher, which is how both sibling ports carry theirs. It verifies each
file's MAGIC NUMBER rather than the HTTP status alone: a mirror that answers a missing file
with a 200 and an HTML error page would otherwise be stored as a table and fail much later
inside the decoder, reported as a corrupt file rather than as a bad download.

Without it the tablebase-dependent golden case skips in CI, and a case that can only skip
there is a case nobody notices breaking. `tb` itself stays out of CI, because it needs a
pristine upstream BUILD as well as the data — that half has not changed.

The push lanes also carry `tsan-race` and `valgrind`, which both sibling ports gate on and
this one did not. **`forbid(unsafe_code)` is not an answer to either.** It rules out the
pointer mistakes a C++ port has to fear and rules out nothing about ATOMICS: the shared
table, the stop flag and the node counters are `Relaxed` by design, and an ordering that is
too weak is a logic bug the type system is happy with. `cargo xtask tsan` runs a four-thread
search under ThreadSanitizer — one thread would instrument the same code and observe nothing.

`-Zbuild-std=std,panic_abort` is not optional there. This toolchain refuses to link an
instrumented crate against an uninstrumented `std`, and `panic_abort` has to be named because
the release profile sets `panic = "abort"` — without it the build fails on a duplicate lang
item rather than on anything to do with sanitizers.

`rfish_perft.yml` is a nightly gate: the merge lane's perft table stops at depths that keep a
push fast, and a movegen bug past those depths would sail through every one.

`rfish_upstream_check.yml` is a third workflow, weekly rather than on push. It fetches
upstream's master and reports how many commits the pin in `tools/upstream/UPSTREAM_BASE` is
behind, with the subjects of what is missing. It is DETECTION only and never gates: a red
merge button for upstream having moved would block work that has nothing to do with it.

It exists because the gap is invisible between sessions. This port sat five commits behind,
across a search retune and an NNUE change, until someone compared by hand -- and two of the
five moved the bench signature. Both sibling ports carry the same workflow; this one was
added after that lesson rather than before it.

A pin upstream's history does not contain fails the lane loudly, because "0 commits behind"
and "that SHA does not exist" are indistinguishable in the count alone.

Its second job asks a DIFFERENT question — not "has upstream moved" but "is rfish still
faithful to the pin it already claims to match" — and runs the four gates that need an
upstream BUILD and therefore cannot live in a push lane: `nnue-check`, `tb`, `golden-audit`
and `upstream-nodes`. The last is the strongest fidelity probe here, because `signature` pins
one fixed list, the goldens pin fixed scripts and `nnue-check` pins a fixed FEN file, while a
position reached by random legal play appears in none of them. Until this job existed all four
ran only when someone remembered to run them.

The oracle for that job is built AT THE PIN, in `../Stockfish/src`, because that is where the
gates look and because `find_oracle` checks the SHA — a build of master is rejected rather
than silently compared against.

### The call-count fingerprint

`cargo xtask fingerprint` asks the question none of the four above can: not "does rfish
compute upstream's numbers" but **"does it get there by calling what upstream calls, as
often"**. Every other differential in this tree compares VALUES — the bench anchor, the
goldens, `nnue-check`, `upstream-nodes` — and every one of them passes over a state divergence
that happens not to move a number on the positions it drives.

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

A group whose pattern matches nothing on one side is a **MISS and fails**, never a zero — a
symbol the compiler inlined away would otherwise read as agreement at zero-versus-zero
forever. Both failure modes were proved rather than assumed: reinstating the ponder clone
turns `do_move` red at exactly +49, and a pattern matching nothing reports MISS; the step
exits 1 naming both.

It is ~50x slower than the bench it profiles, so it stays out of `parity` and runs in the
weekly lane, which already builds an oracle.

### An oracle must be stamped, and the pair must share a net

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
search, and a mismatch fails in the direction that looks like a porting bug.

`cargo xtask build --arch <tier>` and `perf --tier <tier>` now resolve through **one** table.
`--arch` took its argument as a raw `-C target-cpu`, so the tier vocabulary every measurement
is quoted in — `sse41`, `avx2`, `native` — was the one vocabulary it did not accept, and
`--arch avx2` died inside rustc naming neither the flag nor the tier.

`rfish_fuzz.yml` is a second workflow, on a nightly schedule rather than on push. It runs
`cargo xtask fuzz`, which drives three harnesses: mutated UCI text at the shipped binary,
random legal positions through the real search in-process, and mutated table bytes at the
Syzygy decoder. Three harnesses because they fail differently. Subprocess fuzzing spends
a mutation's budget on the PARSER and never reaches the search behind it, which is the lesson
../mcfish records in `2b8eaad7` when it added its own in-process harness beside its UCI one;
and neither of those two ever reaches a **file**, which is the only input here that no part of
the process vouches for. Both sibling ports fuzz the table parse — ../mcfish with a dedicated
lane, ../zfish with its own targets — and rfish lacked it until the decoder had already
shipped as verified. It found six panics on its first afternoon, and a seventh on the nightly
lane three days later — the residue of one of the six, which had been bounded against the
wrong half of the field it validated. The list, and the rule that decided between refusing
and clamping each one, are in [docs/05-tablebases.md](05-tablebases.md). **That seventh is
the argument for the schedule**: it was reachable from a corrupt file on the day the lane went
green, and no gate in `parity` looks at a file nobody wrote a case for.

The workflow runs them as **three jobs from a matrix, each with the whole budget**, which is
the shape ../mcfish's fuzz workflow uses and for its reason: the three run at throughputs
orders of magnitude apart, so one shared budget is really a budget for the fastest of them.
Separate jobs also mean a red UCI run still lets the other two spend their time, and the job
name says which harness failed without opening a log. `cargo xtask fuzz <seconds> all` is the
local single-process form, and there the budget IS divided three ways.

The tablebase job fetches the 3-man set with `cargo xtask tb-fetch`, cached, and
`continue-on-error` — a mirror outage must not fail a fuzz job. Without tables the soak SKIPS
and prints that it did, which is a visibly weaker run rather than a false green.

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

**How the step delivers its stops, and why it is not a knob.** One `stop` per line of the
burst, each after a pause. A burst can start SEVERAL unbounded searches — `go infinite`,
`go mate 1`, or a bare `go` with no limit — and the commands queued behind the first are not
dispatched until it returns, so each needs a stop that arrives while IT is the one running.
Writing them all at once puts every one in the buffer before the first search starts, where
they collapse into the single flag that search consumes, and the second unbounded `go` then
runs forever. A pristine upstream build does exactly the same on exactly the same input —
verified, both hang — so this is the shape of the protocol rather than a defect in either
engine.

Worth stating because the unpaced form LOOKS fine: it stays green for fifty-odd scripts and
wedges somewhere past three hundred. A fuzz step is only ever run to its budget in the
nightly job, so a harness bug that needs 300 scripts to show up is one a developer running it
for thirty seconds will never see. Run it at the scheduled budget before believing it.

**Found and FIXED: a buffered `stop` could not end an unbounded search.**

  printf 'position startpos\ngo mate 1\nstop\nisready\nquit\n' | ./stockfish

Upstream answers; rfish does not. The cause is structural rather than a parse bug. Upstream
reads and dispatches on ONE thread, so its `go` is fully dispatched before the next line is
looked at. rfish reads ahead on a reader thread -- which is what lets a `stop` reach a search
already running -- and that same read-ahead means the reader requests the stop BEFORE the
main loop has dispatched the `go`, whose `SharedState::reset` then clears it. A `stop` that
arrives after the search starts works correctly; only the buffered ordering loses.

The fix is a split, and it took two attempts. `SharedState::reset` no longer touches the stop
flag; `clear_stop` owns it, and is called at the earliest point that OBSERVES a search
command. There are two such points, which is what the first attempt got wrong:

| entry | who clears |
|---|---|
| the shipped binary | the input reader, when it reads a `go` or `bench` line |
| a direct `Engine::handle` — tests, golden harnesses | `cmd_go` / `cmd_bench`, gated on `reader_owns_stop` |

Doing it in both would race the reader and undo a real `stop`; doing it in neither leaves a
stale one to truncate the next search. Both mistakes were made before the flag existed — the
first attempt cleared only in the reader and broke the unit suite and the `search` golden.

A third clear closes the loop: `ThreadPool::search` drops the stop it raised to bring its own
helpers home, at the END of the search. Without it, a `go` following a `go` inherits the
previous one's stop and returns at depth zero.

Verified against a pristine upstream build across six orderings — buffered `stop` after
`go mate 1` and after `go infinite`, a delayed `stop`, a plain bounded `go`, an idle `stop`
BEFORE a `go` (which upstream ignores, and so must this), and two `go`s back to back. All six
agree.

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
