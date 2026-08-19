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

`fmt` → `clippy` → `unsafe-lint` → `docs-lint` → `lane-coverage` → `zone-check` →
`fixture-coverage` → `async-check` → `repro-search` → `test` → `perft` → `golden` →
`golden-audit` → `nnue-check` → `net-roundtrip` → `tb` → `signature`

Cheap and structural first, so a formatting mistake is reported in seconds rather than
after a two-minute bench. `fmt-fix` is the same gate with the fix applied, and is not one
`parity` runs. Read the list from `gates::parity` when it matters: prose cannot be gated,
and a list that drifts by one entry reads exactly like one that has not — **which this list
did**, by three entries, between `zone-check` landing and anyone reading the sentence above
it. So the sentence is now a check: `docs-lint` reads this arrow run and compares it to
`gates::parity_steps`, which is the one list `parity` itself runs from. `negative-control`
carries the row, and dropping a gate from the prose turns the gate red with both orders
printed.

## Which gate answers which question

Two dozen steps, and choosing between them otherwise means reading the rest of this page.
The column that makes the table worth having is the last one, and every entry in it is that
gate's own stated limit from its section below rather than a summary written from memory.

| Gate | Answers | Cannot see | Lane |
|---|---|---|---|
| `signature` | does the search visit the same NODES as upstream | what a node COSTS; only the default arch, so no ISA-gated divergence | `rfish_parity` |
| `perft` | does movegen enumerate the right tree | anything above movegen — it never evaluates or searches | `rfish_parity`, `rfish_perft` |
| `golden` | does the shell still SAY what it said | whether what it says is what upstream says | `rfish_parity` |
| `golden-audit` | is each golden what UPSTREAM produces | paths no fixture drives | `rfish_upstream_check`, and via `parity` |
| `nnue-check` | is the network's raw output upstream's | how that output is USED once the search has it | `rfish_upstream_check`, and via `parity` |
| `tb` | are the WDL and DTZ answers upstream's | what the prober COSTS, and the root RANKING — neither is a probe | `rfish_upstream_check`, and via `parity` |
| `upstream-nodes` | does the search agree node-for-node off the bench list | positions no random draw reaches | `rfish_upstream_check` |
| `fingerprint` | does rfish CALL what upstream calls, as often | what happens between the calls | `rfish_upstream_check` |
| `net-roundtrip` | do the net reader and writer agree | whether either matches upstream's format — that is `nnue-check` | via `parity` |
| `async-check` | what an INTERRUPTED search leaves | values: an interrupted search ends wherever the clock got to | via `parity` |
| `repro-search` | what a COMPLETED search leaves for the next one | whether the node counts are RIGHT; and one thread only | via `parity` |
| `zone-check` | does any module name a zone at or above its own | a `use` in a block comment; whether an edge is behind `cfg(test)` | via `parity` |
| `lane-coverage` | does every step run somewhere, or say why not | whether the lane that runs it actually asserts anything | via `parity` |
| `fixture-coverage` | is every fixture classified and every property presented | whether the fixture exercises the property WELL | via `parity` |
| `docs-lint` | dead links, absent paths, a pinned number a gate computes | whether a claim is TRUE — only whether it is checkable | `rfish_parity` |
| `unsafe-lint` | is the workspace forbid still in place | nothing else; it is one property | `rfish_parity` |
| `arch-determinism` | does every tier reach the anchor | tiers this host cannot execute — it names them rather than counting them checked | `rfish_parity` |
| `tsan` | does a 4-thread search race | a race no 4-thread run happens to take | `rfish_parity` |
| `sync-status` | is the golden checkout AT the pin | whether the pin is the right pin | `rfish_upstream_check` |
| `negative-control` | do the gates FAIL when they should | a gate with no row | none — local, it mutates the tree |
| `perf-budget`, `budget-ab` | what a node costs, and what startup costs | cache, branches, latency; and without `--syzygy`, the tablebase reader entirely | none — local, per-machine golden |
| `codegen-equiv` | did the compiler emit what it emitted before | anything that changes a SIGNATURE: retyping a parameter renames the symbol, and it matches bodies by name | none — local, needs a working tree |
| `counters` | cache and branch behaviour against upstream | where the cost is, per component; and AVX-512 tiers | none — local, needs an oracle and a PGO build |
| `fuzz` | does hostile input reach a panic | anything the generator does not produce | `rfish_fuzz` |

**The stacked rows are the ones a reader gets wrong.** `signature` and `golden` both watch one
binary and only one of them reads what it SAID. `tb` and the probing budget both drive the
tablebases and only one of them is about cost. `async-check` and `repro-search` both ask what
a search leaves behind, and they differ on whether it finished.

The lane column is here and not repeated per section, so there is one copy to keep in step.
A gate whose lane is "none" is excused in `lane-coverage` with a reason, and the reason is
capability or cost — never that nobody got round to it.

**The last four rows are the cost axes, and their mechanics are
[11-performance.md](11-performance.md)'s.** They answer a different question from everything
else on this page — not whether the engine is right but what it costs — and the conditions a
ratio needs before it means anything are stated there once, beside the instruments that need
them.

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
rules [13-writing.md](13-writing.md) records as the most-broken — a pinned number and an
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

It settles the **mechanical** half of documentation rot, and [13-writing.md](13-writing.md)
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
  is different and is stated where it belongs: `perf-budget` measures startup separately and
  now gates it on its own tolerance, and the paired A/B reports its spread.

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

### `zone-check` — the direction `cargo` cannot check

```sh
cargo xtask zone-check
```

`rfish-engine`'s five zones have a declared dependency direction — `board` reads nothing,
`state` reads `board`, `eval` and `search` read both, `platform` reads all of them — and the
consequence that matters is that **perft is a complete test of the board zone**, because
nothing below it can influence it. The crate boundary is checked by the compiler; this graph is
inside ONE crate, where a cycle between modules builds fine.

So it was a property a reviewer maintained, and
[00-architecture.md](00-architecture.md) said so: it carried a hand-written inventory of what
crosses, with the note that a fourth edge would be noticed by nobody. **There was already a
fourth** — `search/worker.rs` names `platform::syzygy`'s types in five places, in every shipped
build. The gate found it on its first run, which is the second time in this repository that
writing the instrument was worth more than the finding it was aimed at.

The baseline **expires in both directions**, which is the half that makes a baseline worth
having: an undeclared crossing reddens the gate, and a declared crossing whose edge is gone
reddens it too. Both seen to fail, and `negative-control` carries the first:

```text
UNDECLARED board -> search in board/bitboard.rs: it names a zone at or above its own, ...
STALE search -> platform in search/harness.rs is in the baseline and the edge is gone. ...
```

Each entry's REASON is printed on every run, not merely stored. A baseline nobody reads stops
being questioned; printing it is what keeps each entry something a reader can disagree with.
`../Stockfish refish` keeps `depcheck.sh`'s baselines the same way and its `lanecheck.sh`
prints excuses for the same reason.

### `repro-search` — what a COMPLETED search leaves for the next one

Node counts repeat across `ucinewgame`, at twenty budgets.

```sh
cargo xtask repro-search
```

**Every other value gate reads the FIRST answer the process gives.** `signature` runs one
bench, `perft` counts a tree, `golden` pins a transcript — none of them asks whether a search
left anything behind. This runs the same two positions twice in one process with a
`ucinewgame` between the rounds and requires the second round to reproduce the first node for
node, so anything the reset misses shows as a divergence: a history table, a stack entry, a
correction bank, a root-move field, a time-manager carry-over.

It is **upstream's own `tests/reprosearch.sh`**, which this port had never taken, and the
budget progression is upstream's — `100 * 3^i / 2^i` for i in 1..=20. The budgets are not
round numbers at any step on purpose: each one stops the search at a different point.

What it cannot see: whether those node counts are the RIGHT ones, which is `signature`'s
question, and what a second thread would do to them. It runs at the default thread count, and
a Lazy-SMP search is not node-reproducible — no gate can make it so.

Upstream's version drives the engine through `expect` and, before that was repaired, a
missing interpreter left `grep` matching nothing, `awk` rejecting nothing, and the script
printing `reprosearch testing OK` having checked nothing at all. This one drives the binary
the way every other gate here does, so there is no interpreter to be absent and no pipeline
whose exit status belongs to its last stage — and a round that reports fewer than four
searches is the failure it looks like rather than a vacuous pass.

Seen to FAIL by mutation, and `negative-control` carries the row: with `ucinewgame` no longer
clearing the worker histories, 33 of 40 searches diverge and it names each one —

```text
differs 332525 nodes, `position startpos`: 332529 nodes before ucinewgame, 332595 after
repro-search: 7 of 40 searches reproduced across ucinewgame
```

The seven that still reproduce are the smallest budgets, where the histories have barely
moved — which is the honest shape of the result rather than a weakness in the row.

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

`cargo xtask build --arch <tier>` and `perf --tier <tier>` now resolve through **one** table.
`--arch` took its argument as a raw `-C target-cpu`, so the tier vocabulary every measurement
is quoted in — `sse41`, `avx2`, `native` — was the one vocabulary it did not accept, and
`--arch avx2` died inside rustc naming neither the flag nor the tier.

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

### The fuzz lane

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
