# Glossary

The words the rest of this set uses without stopping to define them, in tiers that must not
be confused:

- **Section 1 is Stockfish's vocabulary.** Upstream owns the word; the entry says which
  symbol carries it here. It does not teach the concept — a page in this set describes what
  this codebase does, and the domain reference is a link in
  [11-references.md](11-references.md).
- **Section 2 is this repository's vocabulary.** None of it appears in the Stockfish source,
  and upstream is not obliged to agree with any of it.
- **Section 3 is the words that mean two things here.** Each entry is a disambiguation
  rather than a definition.
- **Section 4 is the testing field's vocabulary.** Neither tree owns it; the literature
  does, which is what makes it worth using. A term there is searchable outside this
  repository, and a step name is not.

A reader who cannot tell which tier a word is in will grep the Stockfish source for `zone`
and not find it.

Audience: all contributors.

Every entry names the file, symbol or step that owns it, and none quotes a number a gate
computes. For how one search flows see [00-architecture.md](00-architecture.md); for what a
step proves see [10-tooling-ci.md](10-tooling-ci.md); for what a quantity denotes see
[09-type-design.md](09-type-design.md).

## 1. Upstream's word, and what carries it here

Grep the symbol if a citation misses; the owners move faster than the definitions do.

| term | what carries it here |
|---|---|
| **bench** | the fixed script the anchor is a fact about: `BENCH_ENTRIES` in [`../crates/rfish/src/bench.rs`](../crates/rfish/src/bench.rs), which is upstream's `Defaults` entry for entry. An entry is not always a FEN — a `setoption` line is executed and changes how the entries after it parse — and reordering the list moves the number as surely as changing the search does |
| **the bench signature** | the node total that run prints, asserted by `cargo xtask signature`. The *number* lives in `tools/signature.golden` and in no page; `docs-lint` refuses a page that quotes it |
| **node** | one execution of a node body — `SearchWorker::node`, generic over `NodeKind`, for alpha-beta, and `SearchWorker::qsearch`, generic over `QuiescentKind`, for quiescence. **Not** a NUMA node; see Section 3 |
| **depth** | a plain `i32`, deliberately. A depth-scaled product feeds six different codomains, so a newtype would need six output types and therefore needs none — [09-type-design.md](09-type-design.md) states the rule |
| **`Value`** | the search's score domain, and a real type rather than a typedef. Its algebra is a torsor: `Value - Value` is an `i32` **margin**, `Value ± i32` translates, and there is no `Value + Value`, so the three places that genuinely sum two scores each say `Value::new` at the line where that is decided |
| **key** | seven distinct spaces, one type each — `PosKey`, `TtKey`, `PawnKey`, `MaterialKey`, `MinorKey`, `NonPawnKey`, `MoveKey`. `PosKey::for_tt` is the only conversion between two of them and the only place the halfmove clock is mixed in, which is the "key identity" bug class no perft can see |
| **the root, PV, MultiPV** | the root move list is `RootMove` records in [`../crates/rfish-engine/src/state/mod.rs`](../crates/rfish-engine/src/state/mod.rs); the shell's `InfoSink` prints one `info` line per PV line, and `MultiPV` is how many it has |
| **currmove** | `info depth D currmove M currmovenumber N`, the root move now being searched. `InfoSink::current_move` is the hook, reached through the private `Announces` trait so that a non-root node carries a zero-sized reporter and pays nothing for it |
| **the transposition table** | [`../crates/rfish-engine/src/search/tt.rs`](../crates/rfish-engine/src/search/tt.rs), clusters of three entries packed into four `AtomicU64` words. Every field is `Relaxed`, so upstream's race survives and its undefined behaviour does not. `depth8 != 0` **is** the occupancy test, which is what makes the `DEPTH_NONE` bias the store applies load-bearing: an unbiased store at that depth is indistinguishable from an empty slot |
| **the history block** | one `Histories` value holding the main, low-ply, capture, continuation, continuation-correction, pawn and correction tables, plus the transposition-move-quality counter. A key selects a plane, and the plane index spaces are three separate types because the correction space is a *subrange* of the continuation space |
| **the accumulator** | the incremental half of the NNUE evaluation, `Accumulator` in [`../crates/rfish-engine/src/eval/nnue/transformer.rs`](../crates/rfish-engine/src/eval/nnue/transformer.rs). **One slot, not a stack**: it caches the last position evaluated anywhere and is brought forward by DIFFING recomputed feature sets rather than from a per-move delta. Upstream's per-ply stack loses here — a depth-first search evaluates a node and then its child, so the slot already holds the parent, and copying to make the subtree return cheap costs more than the subtree return does |
| **the feature transformer** | the first NNUE layer, over three concatenated feature sets — `halfka_index`, `threat_index` and `pawn_pair_index`. The first has its own weight table and its own index type `KaIndex`; the other two share a table and a `TpIndex`, whose constructor is the only thing that adds the pawn-pair base, so the concatenation is a property of the type rather than a coincidence a test asserts |
| **WDL, DTZ** | the two Syzygy probe results — win/draw/loss, and distance to zeroing. The prober is [`../crates/rfish-engine/src/platform/syzygy/`](../crates/rfish-engine/src/platform/syzygy/), and `cargo xtask tb` compares both against the oracle position by position |
| **cursed win, blessed loss** | `Wdl::CursedWin` and `Wdl::BlessedLoss`: a win or loss whose DTZ exceeds the fifty-move counter, so the result is a draw in play. Only a 5-man table reaches them and this repository ships the 3-man set, so those branches are blocked on table DATA rather than on code |
| **Lazy-SMP** | the threading model: N workers over one root, sharing the table. `ThreadPool` owns one `SearchWorker` per thread so the histories survive between searches, but there is no persistent *thread* — `std::thread::scope` lends each worker out for exactly one search, and `Threads 1` spawns nothing at all |

### The other direction of this table is the `Golden:` line

Section 1 answers "upstream says X, what is it here?". The reverse question — "upstream's
`search.cpp`, where did it land?" — is answered by the `Golden:` line each module's header
carries, and by the pin in `tools/upstream/UPSTREAM_BASE` that says which upstream those
lines are read against.

## 2. This repository's vocabulary

None of these appear in the Stockfish source. Where a step owns the definition, the step
wins.

| term | what it is |
|---|---|
| **zone** | one of the five directories under [`../crates/rfish-engine/src/`](../crates/rfish-engine/src/) the dependency direction is stated over: `board/`, `state/`, `search/`, `eval/`, `platform/`. The engine-against-shell direction is a **crate** boundary instead, so `cargo build` checks it on every compile. Rust's module system does not enforce the direction *within* the crate, and that half used to be a property a reviewer maintained; `cargo xtask zone-check` gates it now, against a baseline of declared crossings that expires in both directions |
| **gate** | a `cargo xtask` step that **asserts**, and exits non-zero when the assertion breaks. A step that only builds, measures or re-derives is not one, and is listed with its reason in `meta::EXCUSED` so `lane-coverage` can tell the two apart. A gate whose tool is missing exits **2** and is SKIPPED, which is not a pass |
| **lane** | one independently driven run. Usually a CI job under [`../.github/workflows`](../.github/workflows) — `lane-coverage` holds every dispatched step to being in a workflow, in `parity`, or excused — and also one target inside a step that drives several, as `fuzz` drives a UCI harness, a search harness and a tablebase harness. A SIMD lane is a different word; see Section 3 |
| **the golden** | `../Stockfish`, the tree that defines correct behaviour. Also *a* golden: the upstream file a module was ported from, which is what the `Golden:` line in a module header names. A `tools/*.golden` file is a third thing entirely — Section 3 |
| **the anchor** | the bench node total, pinned in `tools/signature.golden`. It **equals** a pristine upstream build's `Bench:` at the pin, so a diff is a porting REGRESSION rather than a local snapshot moving, and there is no separate "finish line" step for the sibling ports' reason — a sync here is atomic, and one that cannot land bit-exact is a bug report rather than a sync |
| **the oracle** | a pristine upstream build at the pin, produced by `cargo xtask oracle` with clang at rustc's LLVM major, PGO on top of LTO, so the toolchain is held equal on both sides. **LOCAL**: a fresh checkout does not carry one, so every step that needs it reports SKIPPED. It is stamped with the SHA it was built from, and `find_oracle` refuses one whose stamp is not the pin — a leftover binary from the previous pin otherwise compares this engine against an upstream it is not a translation of, and reports a clean pass |
| **the pin** | `tools/upstream/UPSTREAM_BASE`, the commit rfish claims to match. `cargo xtask sync-status` reads it in both directions, which are not the same finding: **ahead** of the pin is normal and prints the re-port worklist, **behind** it is a defect in the workspace and goes red, because every oracle built from it then answers from source this tree has already ported past |
| **tier** | an ISA target the build selects, chosen by `cargo xtask build --arch` and named on every measurement. The tiers are ENUMERATED and `native` only SELECTS one of them, printing which — it never compiles `-C target-cpu=native`, because such a build carries tuning no tier label records and a number filed under it is reproducible nowhere. A measurement is a fact about **one** tier |
| **the spine** | the engine with the network removed: the board, movegen, move picker, histories, table and pruning arithmetic that remain when `--spine` builds rfish under the `eval-material` feature and patches the same material formula into the oracle. A **spine comparison** localises an effect to a zone by removing a component rather than attributing one, and both halves must carry the same formula or the trees differ |
| **fixture, property, witness** | the columns of `tools/fixture_properties.tsv`: the behaviour the engine branches on, the file that owns it, the case that presents it, and a literal substring that must still appear inside that case. `fixture-coverage` holds the table in both directions, so a case under no property is a failure and a property whose witness has vanished is too |
| **case** | a `.uci` script under `tools/cases/`, piped **RAW** into the engine — so a `#` line is a COMMAND, not a comment, and `fixture-coverage` refuses one. The `.fens` corpora in the same directory are read by a gate rather than piped, and do carry headers |
| **the perft table** | `tools/perft.table`, a file of facts about chess rather than about rfish. It is **not** a golden, no step regenerates it, and a mismatch is always a movegen bug |
| **the fingerprint** | `cargo xtask fingerprint`, which asks what no value gate can: not whether rfish computes upstream's numbers but whether it gets there by **calling what upstream calls, as often**. `tools/fingerprint_groups.tsv` names the groups, and records which symbols are deliberately NOT gated because upstream inlines them under LTO and a call count is inlining-immune only at the callee |
| **the budget** | `cargo xtask perf-budget`, which holds a tier's absolute retired-instruction count to a row in `tools/instr_budget.golden` — the cost regression `signature` cannot see, since the anchor pins the node count and not what a node costs. The golden is gitignored and per-machine. It **subtracts startup from the search figure and gates it as a second axis**, at 1% against the search's 0.005%, because one is paid per process and the other per move — so the net load and the magic build are measured rather than merely excluded. `--syzygy` swaps in a workload that reaches the tablebase reader, which the bench list never does; Section 3 for the other budget |
| **rig fault** | a verdict that the comparison **did not happen**: an empty corpus, both sides blank, a mutation whose `find` string has rotted, a timeout, a `go perft 1` that reads other than the start position's 20 root moves. It is neither a pass nor a failure, and it is reported instead of a verdict — crediting a gate for an experiment that never finished is worse than not running it. `runner::compared_something` is the refusal |
| **the ledger** | the measurement sections of [08-idiomatic-rust.md](08-idiomatic-rust.md) and [03-engine-eval.md](03-engine-eval.md), read as the record of what has already been measured — including the shapes that measured WORSE. [12-writing.md](12-writing.md) exempts them from the no-history rule for that reason: a measurement is a fact about the tree, so it is written as the rule a reader applies now |
| **sibling** | `../zfish` (Zig) and `../mcfish` (C23): peer ports of the same golden. **Neither is a source and neither is behind the other**; a finding in one is a hypothesis about this tree, to be probed here before it is fixed here, and a measurement never transfers because what a sibling's language pays for a construct says nothing about what Rust pays |
| **sweep** | driving one question across a whole class rather than fixing the instance in front of you — every named path, every include of a symbol, every sibling commit in a window. A **sibling sweep** is the special case, and its output includes the commits NOT taken with the reason, so the next sweep does not re-open them. A gate's own pass over its inputs is the word's other use; see Section 3 |
| **the internal surface** | the second documentation surface, which `.gitignore` excludes so a clone does not carry it. A shipped file must not name its LOCATION — the reference dangles for every reader but its author — and `docs-lint` sweeps the whole index for that, since an ignored path is exempt from the path check by design |
| **fleet** | several agents measuring in parallel, chartered onto **disjoint files** rather than disjoint metrics, delivering patches rather than commits. A worktree agent's findings travel in its report, because a gitignored local directory does not exist inside a worktree |
| **quiet box, the A/A floor** | an idle machine, and the noise floor obtained by A/B-ing a binary against a byte-identical copy of itself. This box is neither: NPS cannot settle a few per cent here, so anything under roughly ten per cent is argued on the deterministic instruction axis instead |

## 3. Words that mean two things

| word | meaning A | meaning B |
|---|---|---|
| **golden** | `../Stockfish`: the tree that defines correct behaviour. Also *a* golden — the upstream file one module was ported from, which is what a `Golden:` header line names | a `tools/*.golden` file: a pinned transcript of what **rfish** printed. Nothing makes the two agree, which is why `golden-audit --write` re-derives from the oracle and `golden-update` REFUSES — it drives rfish, so it would write a photograph of rfish |
| **oracle** | the pristine upstream build the differentials drive | the testing-field term in Section 4: whatever decides that a result is correct |
| **node** | one search node body | one NUMA node: a set of CPUs a `NumaConfig` holds, addressed by `NumaIndex` |
| **lane** | one CI job, or one target inside a step that drives several | one SIMD lane. `LANE` in the feature transformer is the ISA REGISTER width, one rung per tier and not a tuning knob — pinning it at one width made the avx2 fold run on `xmm` where it had been running on `ymm` |
| **gate** | a `cargo xtask` step that asserts | the `gate` cargo **profile**: release codegen plus `debug_assert!` and overflow checks, which `cargo xtask test` builds at. "Under the gate" is ambiguous between the two; say which |
| **budget** | the per-tier instruction budget `perf-budget` holds | the search's own time budget, `TimeBudget`, resolved once per `go`. Under `nodestime` its bounds count NODES rather than milliseconds, which is why the unit lives inside the enum that carries them rather than in a flag beside them |
| **profile** | a cargo profile — `dev`, `test`, `gate`, `release`, `profiling` | a PGO profile: the instrumented run's data `cargo xtask pgo` trains on and rebuilds with. A candidate sized from a `--profile profiling` build is a ceiling and not an estimate, because that profile gives up inlining to keep symbols |
| **key** | `Key`, the bare `u64` a Zobrist hash lands in | one of the seven key TYPES over it. The alias is the representation; the type is the space, and only `for_tt` crosses between two of them |
| **worker** | one `SearchWorker`: the per-thread search state, which outlives the search because the histories have to | the OS thread `std::thread::scope` lends it to for exactly one search. Neither implies the other, and at `Threads 1` there is no spawned thread at all |
| **sweep** | a class swept across the tree, or a sibling's log across a window | a gate's own pass over its inputs — `docs-lint`'s pass over every tracked file for a reference into the internal area, or the accumulator's single pass applying a whole collected diff |
| **bench** | the UCI command | the entry list `BENCH_ENTRIES`, and the node total that run produces. "The bench moved" is ambiguous between all three; say which |

## 4. The testing field's vocabulary

No file in either tree defines these. They are worth learning as names rather than
descriptions: each is the handle for a known failure mode, and the last two describe checks
that are worse than absent.

| term | what it means | what it is here |
|---|---|---|
| **oracle** | whatever decides that an observed result is correct | the pristine upstream build, `tools/perft.table`, a sanitizer report, or nothing at all |
| **differential testing** | drive two implementations with one input and diff | `golden-audit` over the cases, `nnue-check` over the eval corpus, `tb` over the tablebase corpus, `upstream-nodes` over random legal positions, and `fingerprint` over call counts |
| **characterization test** | pins *current* behaviour, and is explicitly not a correctness claim | every `tools/*.golden`. One re-derived from rfish proves only that rfish still agrees with itself — which is exactly how a `bestmove` printed with no ponder move stayed green for as long as the golden existed |
| **metamorphic relation** | a property relating two runs, rather than a pinned value | `net-roundtrip`: write the resident net out, and require the bytes back, so the format reader and writer are one order stated twice. `arch-determinism`: five tiers, one anchor. `repro-search`: the same two positions twice in one process across a `ucinewgame`, which pins nothing and catches anything the reset misses |
| **implicit oracle** | needs no reference, because some outcomes are wrong on their face | the `gate` profile's `debug_assert!` and overflow checks, the TSan and valgrind lanes, and all three fuzz harnesses, where the finding is the panic |
| **mutation testing** | inject the defect, and require the check to go red | `cargo xtask negative-control` is the automated form, one representative mutant per gate; the by-hand one is step 4 of "Adding a type" in [09-type-design.md](09-type-design.md), and the "seen to fail" evidence a gate's commit body carries |
| **lost test** | a test that exists and is in no suite the build runs | a case in no `fixture_properties.tsv` row, and an `xtask` step in no lane. A source FILE cannot be lost here: Rust does not compile what no `mod` declares, so the compiler is that audit and the sibling ports' wired/unwired distinction has no counterpart |
| **false pass** | a run that passed because it compared **less**, not because more was right | `runner::compared_something`, the blank-side refusals in `golden` and `golden-audit`, and `fuzz` asserting its soak actually ran rather than reading an exit status `cargo test` calls success over zero tests |
| **negative control** | a run against the **defective** tree that must show the defect, proving the check can fail at all | `cargo xtask negative-control`, which mutates the engine once per gate, requires that gate to exit non-zero, restores from a `Drop` guard, and ends by running `signature` green. A gate that has never fired is not a gate |
