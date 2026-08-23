# References

What this port is checked against. A claim in these docs is expected to be traceable to
something here or to the live tree.

## Upstream

- **Stockfish** — <https://github.com/official-stockfish/Stockfish>. The golden. The commit
  rfish targets is pinned in `tools/upstream/UPSTREAM_BASE`; read it there, never from
  prose. **The clone's checked-out branch is not the golden and does not have to be.** Every
  differential gate extracts the pinned commit with `git archive`, which reads the object
  store and touches neither that repository's working tree nor its index — so `../Stockfish`
  can sit on `refish` or on any other branch while `sync-status` and the oracle both still
  answer about the pin.
- **The Stockfish wiki** — <https://official-stockfish.github.io/docs/stockfish-wiki/>.
  Authoritative for UCI option semantics, `bench` conventions and the NNUE file format.
- **Fishtest** — <https://tests.stockfishchess.org>. Where a strength claim is settled, and
  where the networks are published.

## The sibling ports

Two complete, bit-exact ports of the same engine, and the source of most of the process
rules in [AGENTS.md](../AGENTS.md) and [CONTRIBUTING.md](../CONTRIBUTING.md):

- **zfish** (`../zfish`) — a pure-Zig port.
- **mcfish** (`../mcfish`) — a C23 port.

Both made the design decisions C++ templates, RAII and operator overloading force a port to
re-make, and both are proven against upstream. Where a structural question has an obvious
answer in one of them, that answer is worth reading before inventing another.

They are **not** goldens. The differential reference is always upstream.

A third sibling is not a port at all:

- **refish** (`../Stockfish`, branch `refish`) — a refactoring branch of upstream's own C++,
  with a working-notes area carrying a twenty-defect register of bugs found in upstream
  `master`, a speedup log and an issue list. It is the only sibling whose findings are about
  the GOLDEN rather than about a port, so a defect it records is one rfish may have inherited
  by being faithful — which is a different question from the one the two ports answer, and it
  is why its register is swept entry by entry rather than skimmed for ideas.

### How to sweep them

**A sibling's refactor is a hypothesis about what this tree never wired up** — ask what it
implies about rfish's *use* of the same thing, not only whether the same duplication is
here. The 2026-08-06 sweep took `mcfish ac7d02c7` and `zfish b09519f6`, which each gave the
`currmove` node threshold one owner after finding it declared twice. rfish already had one
owner and all three of upstream's call sites, so on the sibling's own terms there was
nothing to take. The site that announces the move called nothing at all, and the whole
`info depth N currmove X currmovenumber M` line was missing from the port.

**A measurement does not transfer, in either direction.** What a sibling's language pays for
a construct says nothing about what Rust pays. Take the idea, then price it here — the same
sweep priced three ways of routing that reporter and two of them were gates-red.

**A sibling's GATE is a hypothesis about what this tree does not INSTRUMENT**, which is a
different question from whether the same defect is here. Both siblings independently built a
gate diffing the root `currmove` line against the oracle, and rfish already printed that line
and had fixed the bug they were gating — so on the subject line there was nothing to take.
Asking the gate's question instead pointed at `async-check`: it drove `quit` into a search
already running and never drove the shape every gate, harness and piping GUI actually uses,
where the whole script arrives in one buffer. rfish hung on that input, exit 124, while both
siblings exited 0. **The instrument is the finding more often than the code is.**

**Probe against this tree, not against the subject line.** The commits worth recording are
as often the ones NOT taken, with the reason, so the next sweep does not re-open them. From
the 2026-08-08 window (`mcfish 75a76202..187ddec0`, `zfish 37da78fb..d0549833`):

| probed | verdict |
|---|---|
| `mcfish 233525f0`, comments naming files that no longer exist | **nothing here.** No comment names an absent `crates/**.rs`, and every upstream `*.cpp`/`*.h` cited in one exists at the pin |
| `mcfish cd3603cd`, includes a file already had | **no analogue.** Rust has no include graph to duplicate; a redundant `use` is a compiler warning `clippy` already refuses |
| `mcfish f757587d`, a `gives_check` argument `do_move` trusts | **no analogue.** rfish's prober calls `do_move`, which computes the checkers itself; `do_move_checked` is a separate entry point the tablebase path never takes, so there is no contract to pass `false` to |
| `mcfish 1cede41d`, a gap list describing shipped subsystems | **already current here.** All four of rfish's "What is not here" sections name what has come OFF the list as well as what is on it |
| `zfish 1f208d00`, docs-lint permitting a pinned anchor | **already stronger here.** `docs-lint` refuses ANY quoted signature rather than only a stale one |
| `zfish 88b2cc42`, a module no page names | **taken** — the sweep found `debug_log` |
| `zfish d0549833` / `mcfish 187ddec0`, a commit-format section | **already here.** All three sets had the same hole — a writing page deferring to the commit message twice without saying what one looks like — and all three closed it in this window |

**A sibling's DEFECT REGISTER is swept per SITE, not per entry.** The 2026-08-15 sweep of
`../Stockfish`'s `refish` branch — 199 commits, its twenty-defect register, its speedup log
and its issue list — reported "all twenty closed or not applicable here" and was wrong twice,
in the same shape both times: **the class was closed at the site the register names and open
at a second site in another zone.** The clock overflow was filed under the `speedtest` entry,
whose arithmetic rfish already saturated, so the clock itself was never tried — and it
panicked on the first attempt. The net-block length was filed under `read_header`, which rfish
bounds at `1 << 16`, while the LEB128 block header one function away believed a raw `u32` and
would reserve four gibibytes. Read a register entry as naming a CLASS and a place it was
found, then grep the class.

| probed | verdict |
|---|---|
| the twenty-defect register, entries 3, 4, 11, 15 | **nothing here.** `setoption`/`export_net` during `go infinite` exit 0 with one bestmove, a 12-byte net is refused, `speedtest` already saturated |
| entries 5, 8, 9, 16–20, the tablebase parser | **nothing here**, and measured rather than read: eight of the register's fixtures were rebuilt from its own byte lists against the identical 3-man corpus and all eight exit 0 with a legal bestmove |
| entry 12 `hash_bytes`, entry 7 `shm`, entries 1/2/10 | **no analogue.** rfish reads `u8`, has no shared memory, and cannot read uninitialised memory or write to a failed allocation |
| entry 13, `nodestime` making `movetime` mean nodes | **deliberately not taken.** `worker.rs` reproduces upstream's unit crossing under a comment saying so; the goldens depend on it |
| entry 14, every worker clearing the shared continuation history | **no analogue.** `Histories` is per-worker here, which the pin's own sync commit records as a standing divergence |
| entry 6, a mismatched-colour clock | **open, and a behaviour question rather than a defect.** Upstream searches unbounded off indeterminate memory; rfish is defined-but-different and returns at once. There is no correct answer to be bit-exact to |
| its LATENT row on `syzygy_extend_pv` | **taken.** It could not reproduce it for want of tablebases; rfish ships a 3-man set, `timed_out` is dead without a clock and the draw guard is dead under `Syzygy50MoveRule false`, so mate was the only exit |
| `f3057516` fill a history bank by the run, `P1` an atomic forbidding a bulk fill | **already done.** The innermost `fill` is a wide store and carries a comment saying why; rfish's histories are plain `i16`, so P1's prohibition does not exist |
| `b43a60c9` / `E10`, clang vectorising move scoring into gathers | **does not reproduce.** 15 `vpgather` at `avx512icl`, all in `TableRegistry::file_counts` and `Skill::pick_best`; none on a per-node path. `../zfish` censused 272 and reached the same verdict |
| `006eb707` / `E12`, `putPiece` as a template parameter | **already done.** `update_piece_threats` is `#[inline(always)]` with no surviving symbol, so refish's own tell — the callee is still a real symbol — fails here |
| `P4`, an `int` index forcing a sign extension per use | **not attempted**, on the rule refish declines it by and this tree shares: a type is free while a value is carried and costs where many are live in one large function |
| its whole rig-hardening series — `2fc949a3`, `b94001a3`, `2e8d17f7`, `86f63764`, `1ae7a170`, `a115486e`, `34c265b7` | **already stronger here.** `compared_something` is applied across the suite, a blank golden is refused in BOTH modes, a tier that fails to build propagates, and the deadline polls `try_wait` with the reader on its own thread |
| `15f06b6a` + `c57c57b5`, the tablebase decode's length walk and per-length tables | ~~**real, and unmeasurable here.**~~ **RETRACTED 2026-08-18, and `15f06b6a` is TAKEN.** The verdict was true of the INSTRUMENT and false of the finding: `decompress` reads 8.8M Ir on a workload that barely probes, and on one built to probe the same walk is **1,648,117,166 Ir, 15.9%** of it. Replacing it with a verified bucket table is −12.05% at avx2 and −10.03% at sse41, +0.0000% on the bench axis. `c57c57b5` remains open. **A "too small to measure" verdict is a claim about the instrument, and this file now carries the rule: build the axis before filing one** |
| `90d8dcb9`, decoding a weight from an eight-byte window | **REFUTED by measurement, twice.** See [08-idiomatic-rust.md](08-idiomatic-rust.md) §11 |
| its `malformed.sh` fixture set, as a deterministic test here | **built, and DROPPED.** It passed and could not be made to fail: five mutations, including removing a bounded read outright, all stayed green, because rfish refuses those files structurally a layer or two before the bounds they target. A test with no detection power is the defect the meta-gates exist to refuse. `../zfish` derived its fixture offsets from its own loader instead, which is the part worth retrying |
| `d4324d9e` + `f1318daa` codegen equivalence, `9c26b4d6` a budget with no stored golden | **taken**, as `codegen-equiv` and `budget-ab` — see [10-tooling-ci.md](10-tooling-ci.md) |
| `perfbudget.sh --syzygy` and `perfcounters.sh --syzygy`, its `T5` — no cost gate can run a probing workload | **taken on BOTH halves**, and it is the entry that retired the row above. Every bench position has more men than the shipped corpus covers, so the decoder, the index arithmetic and the parser sat outside every cost gate here. `#[inline(never)]` on the decoder is +0.90% on the new axis, +0.0000% on the bench one and invisible to `signature` — two gates green and one red on the same tree |

**A register swept once is not a register swept.** The 2026-08-15 sweep read `refish`'s
twenty-defect register — its FIRST campaign. A second campaign of fourteen was landed in the
twelve commits after it, and eighty commits arrived in the three days that followed, so the
2026-08-18 sweep found six live defects in a tree the previous one had reported clean. Five of
the six are in the second campaign; the sixth is not in any register, because it is a
divergence from the GOLDEN that only appeared when upstream's own text was read beside the
sibling's fix. **Read the sibling's fix, then read what it is a fix TO** — the omission was in
the four lines above the line refish changed.

| probed, campaign 2 (entries 21–34) | verdict |
|---|---|
| `69c52a88` / entry 21, castling legality gated on the `UCI_Chess960` option | **taken.** Live here in the same shape, and `go perft 1` on `4k3/8/8/8/8/8/8/qR2K3 w Q` generated `e1c1` — nine moves where there are eight. See [01-engine-board.md](01-engine-board.md) |
| `64e65bee` / entry 32, a duplicate castling right leaving a stale mask | **taken.** Live here; `w AB` then `a1a2` destroyed the right the b1 rook owns |
| `6a9dd5c7` / entry 31, `movestogo` and `mate` reaching arithmetic with no room | **taken, and SPLIT.** `mate` is bounded at the parser because the stop condition doubles it; `movestogo` is not, because widening the horizon's subtraction to `i64` is free, exact and keeps upstream's behaviour. Their fix bounds the count and leaves the CAST that produces the same `mtg` from a negative clock — an arm live on both trees, and found here rather than read |
| entry 27, the continuation-history bonus overflowing `int` | **taken, and it is the case where the port pays LESS than the branch.** Six exact repairs each cost them ~0.08%, because the UB is load-bearing for the codegen — see [02-engine-search.md](02-engine-search.md). Rust grants no such licence, so spelling the wrap is byte-identical codegen |
| `07b8535a` / entry 24, `root_probe_wdl` ignoring `Syzygy50MoveRule` | **NOT taken, and it found something larger.** Upstream's own line — `if (pos.is_draw(1))` — was MISSING here altogether, so the fidelity gap was the whole test rather than the flag on it. Restored in upstream's exact form; their correction of the flag is an improvement on the golden and is inherited as a defect instead |
| `9b163d49` / entry 23, a failed net load half-overwriting the live network | **no analogue.** `Network::load` returns a whole `Network` or an error, so there is no live object to read into and nothing to half-overwrite |
| `96697e8a` / entry 29, a mis-sized table ending the process | **no analogue.** Nothing here maps a table or exits on one: `TbTable::new` returns an `Option` and every read is bounds-checked, so a truncated file is refused structurally |
| `9aa8e560` / entry 34, DTZ files counted before the WDL gate | **already correct by construction.** `add` returns before it looks at the `.rtbz` when the `.rtbw` does not open, so the DTZ count is behind the gate already |
| `6ba0a756` / entry 33, a numa policy that does not parse whole | **divergent, and neither theirs nor upstream's.** `str_to_size_t` accepts `0,1 2,3` as `{0,1,3}`; this port's `parse::<usize>()` refuses the piece and yields `{0,3}`; refish refuses the string. Left alone: it is a third answer on an input no operator sends, and changing it is a behaviour choice rather than a defect fix |
| `060b4146` / entry 26, an unbounded cpu index, and `6d08acbf` / entry 28, an unbindable policy | **no analogue**, as the 2026-08-15 sweep found for the same class: there is no `CPU_ALLOC` here, the total is bounded, and there is no pinning to fail |
| `46944a92` / entry 30, `std::exit` under a live tablebase probe | **no analogue.** Nothing unmaps a table, and the one `exit` on the shell's failure path is not reachable with workers inside a probe |
| entry 22, a crafted LEB128 header hanging the loader | **already closed** at `c996043`, by the 2026-08-15 sweep's own second look |
| `d657cfae`, gating the startup the budget only printed | **taken** — see [10-tooling-ci.md](10-tooling-ci.md). It closes a hole AGENTS.md carried as a trap row telling the reader to measure it by hand |
| `9b6ebbab` a liveness gate, `ccdd41c1` thread scaling, `e5476414` shellcheck, `662c6675`/`3bb90d38` CI pins and citations, `5c00ff7d`/`6b36496a` include and friend direction | **not taken.** The first two are wall-clock instruments this repository already refuses to put in `parity`; the rest are C++-tree properties — an include graph, a `friend`, a bash suite — that `cargo` and `clippy` make unrepresentable here, and `sync-status` already answers the citation question for the one SHA that matters |
**A sibling's GATE SUITE is swept the same way, and the useful half is the gate with no
analogue rather than the gate that is better.** The 2026-08-18 sweep mapped all 28 of refish's
shell gates, 7 Python harnesses and 5 C++ harnesses against this tree's 39 `xtask` steps.

| probed, the gate suite | verdict |
|---|---|
| `reprosearch.sh` | **taken**, as `repro-search` — and it is UPSTREAM's own test, which this port had never ported. It asks the one question no gate here asked: what a COMPLETED search leaves for the next one. See [10-tooling-ci.md](10-tooling-ci.md) |
| `perfbudget.sh`'s startup tolerance | **taken** at the previous sweep, and its first run found 28.7M — see the slider-ray commit |
| `actionpins.sh`, an action pin that has gone stale | **little to add, and it needs the network.** Every action in the four workflows here is already pinned to a full SHA, so what the gate adds is staleness against the tag's current tip — which is a question only GitHub can answer, and refish spent two commits stopping a rate limit reading as a verdict. Recorded rather than taken |
| `devcite.sh`, every cited SHA is an ANCESTOR of HEAD | **not taken, and the reason is that the shape does not transfer.** 48 SHAs are cited across `docs/` and `AGENTS.md` and **47 of them are SIBLING commits** — `../Stockfish`, `../zfish`, `../mcfish` — which resolve in another clone or in none. A reachability test against THIS repository would report 47 findings that are not defects, and a test against the sibling would skip whenever the sibling is absent, which is every CI run. What transfers is refish's own conclusion rather than its gate: **the durable form is a subject beside every SHA**, because the subject survives a rebase and is greppable. The pages here already write citations that way in the register above |
| `zones.sh`, `depcheck.sh`, `linkcheck.sh`, `iwyu.sh`, `buildcoverage.sh` | **four have no analogue and the fifth is TAKEN.** Four police an include graph and a build list; Rust has neither, a module absent from `mod.rs` does not compile, and the crate split makes `engine -> shell` unrepresentable. The part that IS representable — layering WITHIN the engine crate — was named as this row's residue on 2026-08-18 and gated the same day as `zone-check`, against a baseline that expires in both directions. **Its first run found a fourth crossing edge the architecture page did not carry**, on the page that says in as many words that nothing would notice one |
| `shellcheck.sh` | **no analogue.** The gates here are Rust, so `clippy` in `parity` is the same lane and a broken gate fails to COMPILE |
| `optiondefaults.sh` | **already stronger here.** The `uci` handshake golden pins every option's name, order, type, default and range byte for byte, where refish's gate compares a list |
| `textequal.sh` | **already stronger here.** It compares the binary's text for a pure-motion claim; `codegen-equiv` compares the disassembly symbol by symbol and names which symbol moved |
| `liveness.sh` | **mostly covered.** `async-check` already drives `stop`, `ponderhit` and `quit` into a RUNNING search and asserts the engine answers afterwards. What refish's version adds is a deadline on the answer, which `async-check` already carries at 30s |
| `instrumented.py`, `match.sh`, `npsab.sh`, `npsthreads.sh`, `perfcounters.sh`, `perfdecomp.sh` | **covered or deliberately absent.** `tsan` and `fuzz` cover the sanitizer half; `perf` and `counters` cover the differential and the cache/branch axes; `match.sh` needs cutechess and a games budget this repository does not spend |
| `malformed.sh`, `leb128.sh` | **built and DROPPED** at the 2026-08-15 sweep, with the reason recorded above: the fixtures could not be made to fail here |

### Three findings from that sweep that are NOT changes

**The transposition table's clear is instruction-cheap and latency-expensive, and no instrument
here can judge the trade.** Upstream clears across every worker thread, each `memset`-ing a
contiguous region, deliberately so the pages land on the right NUMA node. `clear` here is one
thread storing `AtomicU64`s one at a time. The naive port is backwards: `rep stosb` retires
about one instruction per BYTE, while a 64-bit store loop retires about one per eight, so
adopting upstream's shape would make `perf-budget` RED while making the wall clock better —
and at `Hash 1024` the current shape is a visible stall at `ucinewgame`. Rust can remove the
atomic prohibition without weakening any ordering, because `AtomicU64::get_mut` proves
exclusivity and `clear` has an owner at every call site; what it cannot do is decide the trade
without a wall-clock instrument. **Left alone, and written down instead** — this is the
"an instruction count cannot see a latency win" rule with its sign the other way round.

**refish's `b04f3bec`, the worker histories cleared twice, is real here and measured too
small.** `SearchWorker::new` ends in `clear()` and every caller then calls `pool.clear()`, so
the tables are filled twice — and once more before that, because `Box::default()` zeroes what
`clear` immediately overwrites. The whole of `Histories::clear`, summed over every call in a
bench, is **2.7M Ir**; a redundant pass is at most half of that, against a change that would
have to split the pool's construct-and-reset contract. Not taken, with the number rather than
an argument.

**98.4M instructions of the bench process are `__memset_avx2_unaligned_erms` — 3.8% — and what
allocates them is NOT established.** It is not the histories (above) and it is not the net,
whose large buffers are `alloc_zeroed` and take their zeroes from fresh mmap pages. The entry
stops here rather than guessing, exactly as refish's own P6 entry stops at the 110 MiB it could
not attribute. Size the allocations before writing anything.

**The 2026-08-19 window was eleven commits and almost all of them were TYPE DESIGN**, which
is the one subject where this port has a page of its own and the branch has none. Swept per
commit:

| probed, `refish` `2e484fdb..c39bdf1c` | verdict |
|---|---|
| `db74904f`, typing `root_probe`'s two adjacent bools | **taken.** The identical hazard: two `bool`s in argument position, both inversions silent, and one of them decides whether a won position is ranked as won. `Rule50`/`RankDtz`, broken on purpose to see the compiler refuse both orders |
| `2e484fdb`, what `LenTabMaxBits` costs on either side of 12 | **taken, and re-measured rather than quoted.** Their 22%-at-12-bits is a FIVE-man figure; the corpus here is three-man, `max_sym_len` 7 and 11, and **no bucket escapes at all** — the cap never binds on what this repository ships. Both numbers are recorded, theirs marked as theirs |
| `43e74015`, compile-probe rows proving each new type refuses its transposition | **partly taken, and the gap is named.** The refusals were confirmed by hand, which is this tree's existing convention for the table in [09-type-design.md](09-type-design.md). What is NOT here is a GATE: `negative-control` requires a mutant to compile, so a row that must FAIL to compile needs a mode it does not have. Seven rows of that table rest on "was broken on purpose" with nothing re-running it |
| `c39bdf1c`, what `textequal.sh` cannot prove about a signature change | **taken as documentation.** The same limit binds `codegen-equiv`: a parameter's type is part of the mangled name, so retyping one renames the symbol and the gate compares bodies by name. Recorded in [10-tooling-ci.md](10-tooling-ci.md) |
| `057eb35a`, the cpu-add result made nodiscard | **taken**, as `#[must_use]` with a one-line reason at each of the four sites that drop it |
| `a0234971`, naming `reduction`'s parameters | **not taken as typing, and the reason is this tree's own rule.** `reduction(improving, d, mn, delta)` carries three adjacent `i32`s and the swap is silent, but it is called from `search`, the largest function in the engine, where [09-type-design.md](09-type-design.md) says a type costs where many values are live. That is refish's own `P4` verdict on the same function, reached independently |
| `29be37a0` bestmove with its ponder move, `f301f771` a history share, `e56eabec` bank index types, `dd4024ed` quadrant subscripts, `b17449b5` two magic layouts, `3683267f` explicit constructors | **no analogue or already held.** Four are shapes this port never had — its history block is per-worker and its quadrant is reached through named methods, not two bool subscripts — and `explicit` is a C++ conversion rule with no Rust equivalent |

| probed, `SPEEDUP.md`'s deterministic-win patterns | verdict |
|---|---|
| **P7**, a runtime parameter that is a literal at every call site | **taken, and it was the sweep's largest result.** `build_magics` took its four ray directions as an argument, so one non-generic instantiation served both sliders and every ray step loaded a direction that could have been a constant. −28.7M at `avx2` and −27.9M at `sse41`, 2.7–2.9% of every startup, search flat on both. refish's tell is what found it: the callee survives in the profile as ONE symbol however literal the call sites look |
| **P1**, an atomic forbidding a bulk operation | **one site left, and it is the TT clear above.** The histories are plain `i16` here, so the prohibition refish removed does not exist |
| **P6**, zeroing a later full write makes dead | **not reachable in safe Rust.** Skipping an initialisation needs `Box::new_zeroed` and `assume_init`, and `unsafe` is the constraint this port exists to keep. The one place it would pay is the 98.4M above, which is unattributed |
| **P2** aliasing barriers, **P3** a vectoriser gather, **P5** discarded work | **nothing found**, and P3 was censused at 272 gather sites by `../zfish` with the same verdict |
| **P4**, an index whose type forces a sign extension | **still not attempted**, on the rule recorded at the previous sweep |

| its `perf(...)` series on the accumulator and the tablebase decoder — `81254ddb`, `acef91aa`, `9769f5a2`, `a25d8fb4`, `1c384d46` | **not applicable.** Every one is a gcc pragma, a `restrict` scope or a pointer pinned for a compiler this port does not use. The two that are language-neutral — one load per pairing-tree step, resuming the length walk — are the same site the 2026-08-15 sweep measured at **0.11%** of a probing bench and below every instrument here |


**A sibling that REWRITES ITS HISTORY needs a content-based boundary, not a subject line.**
`../Stockfish`'s `refish` ran `filter-branch` over the whole branch between the 2026-08-19
sweep and this one, so every SHA the register above cites is now an unreachable object with a
living twin under a different hash and often a different message. Matching the last swept
commit by SUBJECT found the right diff and the wrong boundary: the rebase had also reordered,
and two commits the previous sweep had already recorded sat ABOVE it. `git patch-id --stable`
over both sides is what settles it — the diff survives an amend and a reorder, and the set
difference is the window. The last swept commit `c39bdf1c` is `67e36c0d` on the branch today.

**The 2026-08-23 window was 99 commits, and its perf half is a DIVIDE CAMPAIGN.** Nine of the
twenty perf commits remove or reciprocate an integer division in the search. Swept per commit,
and every verdict below is a measurement on this tree rather than a reading of theirs:

| probed, `refish` `67e36c0d..6c1274a3` | verdict |
|---|---|
| `8c233e73`, the reduction's window term carried across the move loop | **taken.** The identical shape: `root_delta` is fixed for the search and `beta` does not move inside `search`, so the quotient changes only where `alpha` is raised, and at a non-PV node it cannot be raised at all. −0.0255% at avx2 and −0.0180% at sse41, bit-exact |
| `5219b2be`, the reductions table retyped so `/ 512` is a shift | **taken as the IDEA and REFUSED as the change, and the split is the finding.** Retyping the table to `u16` measured **+0.0191%** here: `reduction` reads it twice per move and the two `movzwl` cost more than the correction they remove. Dividing the improving TERM at `u32` proves the same fact where it is used rather than where it is stored, and reads −0.0474% — within 0.0003% of their own −0.0477%. **The proof goes on the DIVIDEND, not on the container**, because a C++ branch pays nothing to narrow a table it indexes directly and this port pays a zero-extension per read |
| `23174aae`, the lmr divisor by reciprocal multiply | **NOT taken, and it is the first entry this repository can PRICE ON BOTH REGIMES.** Built, proved bit-exact, checked by exhaustion over \|h\| ≤ 2^21 for all sixteen divisors, and it costs instructions on both axes here: **+0.0323% cold, +0.0406% warm.** refish reads +0.0636% on their bench axis and −0.112% on their warm PGO game, so the sign flip that justifies it there does not occur here — rustc pays more for the widening multiply than clang under PGO does. The quantity the change is FOR is divider latency, and this box has no cycle instrument that can resolve it: the same shape as the transposition-table clear below, with the sign the other way round |
| `d0e85ffc` the fail-high average's divisor, `1a602df2` the all-node reduction, `477b61fe` the root window | **not attempted, on the entry above.** All three are the same trade against the same non-pipelined divider, and the one that was built measured the cost and could not measure the benefit. Building three more of them would produce three more numbers of the same kind |
| `c6633ad3`, asserting a reciprocal table cannot round the wrong way | **taken WITH the entry above and reverted with it**, and it is the part worth keeping in view: the `+ (h < 0)` correction is uniform only while the magic's error term is strictly positive, which fails exactly where a divisor divides the shift. It is a property of four tuned numbers, so a tuner moving one onto a power of two gets a silently different search. Whoever revisits the reciprocals owes that assert |
| `f4b00b55`, the good-quiet walk stopped at the first move below the bar | **taken.** Live here in the same shape, including the depth-4 crossing where the sort's limit falls to the threshold. −0.0276% cold and **−0.0849% warm**, bit-exact |
| `530b1f12`, the eval bucket shifted instead of divided | **no analogue.** `(pos.piece_total() as usize - 1) / 4` is already unsigned here, so there is no round-toward-zero fixup to remove |
| `295b1905` the root position copied instead of round-tripped through a fen, `8844cee0` every worker's root installed from the calling thread | **no analogue, and neither was ever built.** `ThreadPool::search` passes `&Position` straight into each worker through `std::thread::scope`; there is no `fen()` serialise-and-reparse on the move-latency path and no condition-variable dispatch to pay two context switches for |
| `a4398e64`, option names folded in ascii instead of through the locale | **no analogue.** Nothing here calls `setlocale` and the option map compares with `eq_ignore_ascii_case`, so the latent Turkish-locale defect their commit also closes does not exist |
| `87a23d54`, the three list walks taken out of the state machine's table | **open, and NOT attempted.** Their whole result is a clang layout effect (−1.61% under clang PGO, +0.07% under gcc), and what it is a claim about is indirect mispredicts, which is a sampled column here. The warm axis could now size the instruction half of it |
| `78115f0b`, the quiet score's flat history index and shift-form bit tests | **partly already here and partly open.** The six planes are already resolved once per list rather than per move, which is the larger half of their finding; the flat `[pc][to]` element index is not, and the low-ply term still divides by a loop-INVARIANT inside the loop |
| its NNUE series — `8068c80f` two dot-product chains, `97bd8d68` and `9929070f` clamping after the narrowing, `f4a7835a` a memory broadcast, `fb484a13` an unroll pragma, `c415d021` a pinned broadcast | **not applicable as written, and two are language-neutral.** Four are a gcc pragma, an opaque-pointer barrier or a `restrict` scope for a compiler this port does not use. The two that are not — clamping after the narrowing, and splitting the output layer into two accumulator chains — are expressible in `std::simd`; the first is a real instruction saving and the second is explicitly a LATENCY claim its author says an instruction count cannot see, which is this tree's own rule |
| `663f5976`, an axis that measures a warm game at a long clock's depth | **taken**, as `warm-ab` — see [11-performance.md](11-performance.md). It is the sweep's largest result and it changed a verdict on its second use |
| `92b9656f`, `d4abba59`, `288179ee`, `345791bb`, `6c1274a3`, what each axis can resolve | **taken as documentation**, and re-measured rather than quoted: the startup column of seven runs over five independently built binaries spans 0.0004% here, which is what makes an instruction ratio a fact about the code |
| `2e804530` a match ledger, `3300f1bf` naming a match's two sides, `4d6ee5a4` a CI fallback base, `5cf8ed8a`/`04885601` negative-control anchors, `6870e8d2` distinct net-mutation offsets, `d5c44fc7` a CI table gate, `6afaba9c` retrying a download | **not taken.** `match.sh` has no analogue here — it needs cutechess and a games budget this repository does not spend — and the rest are properties of a bash gate suite and four GitHub workflows whose rfish equivalents are Rust, where `clippy` refuses a broken gate at compile time |
| its 08-19 docs reorganisation — `bd75ed8d` through `fa8b64ab` giving each page the gates that hold it, `775a279c` splitting the axes onto their own page, `e8340484` renumbering | **already here, and gated.** `docs-lint` has held every page's `## The gates` section to the tree in both directions since 2026-08-19, which is the property that block was building toward |
| `c349f888`, an idiomatic-C++ page the sibling ports all carry | **already here** as [08-idiomatic-rust.md](08-idiomatic-rust.md), which is the page their commit says it is copying |

## Rust

- **The Rust Reference** — <https://doc.rust-lang.org/reference/>. Particularly the
  behaviour-considered-undefined section, which is what `forbid(unsafe_code)` makes
  unreachable.
- **The Rustonomicon** — <https://doc.rust-lang.org/nomicon/>. Read to understand what the
  constraint is buying, not to look for a way around it.
- **`std::sync::atomic`** — <https://doc.rust-lang.org/std/sync/atomic/>. The memory
  ordering the transposition table relies on.
- **`std::thread::scope`** — <https://doc.rust-lang.org/std/thread/fn.scope.html>.
- **The Cargo book, on profiles** — <https://doc.rust-lang.org/cargo/reference/profiles.html>.
- **`cargo xtask`** — <https://github.com/matklad/cargo-xtask>. The pattern, not a
  dependency.

## Type theory and type design

What [09-type-design.md](09-type-design.md) rests on. Each entry says what it is *for* here;
none of them is implemented as an algorithm, and where a paper describes machinery this port
does not run, that is said.

### The bound in the type

- **Xi & Pfenning, "Eliminating Array Bound Checking Through Dependent Types", PLDI 1998** —
  <https://www.cs.cmu.edu/~fp/papers/pldi98dml.pdf>. Index and array types carrying a static
  size let a checker discharge the bound and emit no check. This is the mechanism behind
  `Box<[T; N]>` and the const-generic layer widths, with LLVM's value-range propagation
  standing in for the dependent typechecker. The effect is therefore *not* guaranteed:
  [08-idiomatic-rust.md](08-idiomatic-rust.md) records both where it paid and where the same
  move cost instructions.
- **Xi & Pfenning, "Dependent Types in Practical Programming", POPL 1999** —
  <https://doi.org/10.1145/292540.292560>. The language (Dependent ML) the above became.
- **Rust RFC 2000, const generics** — <https://rust-lang.github.io/rfcs/2000-const-generics.html>,
  and the 2026 project goal for the full feature —
  <https://rust-lang.github.io/rust-project-goals/2026/const-generics.html>.

### Affine quantities: why a score is a torsor

- **Baez, "Torsors Made Easy"** — <https://math.ucr.edu/home/baez/torsors.html>. The standard
  informal account of quantities that have differences but no origin. `Value`, `Ply` and
  `GamePly` are three instances; `Value - Value` yields a margin for exactly this reason.
- **Baker, "Torsors as proportion spaces", 2023** —
  <https://mattbaker.blog/2023/09/18/torsors-as-proportion-spaces/>. A readable modern
  restatement.
- **`std::time::Instant`** — <https://doc.rust-lang.org/std/time/struct.Instant.html>. The
  reference instance in the standard library: `Instant - Instant` is a `Duration`,
  `Instant + Duration` is an `Instant`, and `Instant + Instant` does not exist.

### Units of measure

- **Kennedy, "Types for Units-of-Measure: Theory and Practice", CEFP 2009** —
  <https://link.springer.com/chapter/10.1007/978-3-642-17685-2_8>. Why dimensioned quantities
  want distinct types, and why the discipline is cheap. Shipped in F#.
- **Kennedy, "Relational Parametricity and Units of Measure", POPL 1997** —
  <https://people.mpi-sws.org/~dreyer/tor/papers/kennedy.pdf>. The theorem that a function
  polymorphic in its unit cannot inspect it.
- **`uom`** — <https://github.com/iliekturtles/uom> — and **`dimensioned`** —
  <https://github.com/paholg/dimensioned>. The Rust realisations, named as the reference
  implementations of the discipline and **not** as candidates: the engine crate has zero
  dependencies and that is a reviewed property. Unit *polymorphism* is what they have and
  rfish cannot express, which is why there is no `Depth`.

### Index newtypes

- **`rustc_index::newtype_index!`** —
  <https://doc.rust-lang.org/nightly/nightly-rustc/rustc_index/macro.newtype_index.html>. The
  same pattern at compiler scale: conversions marked `inline(always)` so they vanish even in
  debug, and an explicit maximum so the type keeps a niche.
- **matklad, "Newtype Index Pattern", 2018** —
  <https://matklad.github.io/2018/06/04/newtype-index-pattern.html>. The short practical
  statement of it.
- **Rust Design Patterns, "Newtype"** —
  <https://rust-unofficial.github.io/patterns/patterns/behavioural/newtype.html>.

### Making illegal states unrepresentable

- **Minsky, "Make illegal states unrepresentable"** — the maxim, from a 2010 lecture on
  OCaml. `Score`'s three variants and the `Square`/`SquareOrNone` split are the two places it
  is applied here.
- **Wlaschin, "Designing with types: making illegal states unrepresentable"** —
  <https://fsharpforfunandprofit.com/posts/designing-with-types-making-illegal-states-unrepresentable/>.
  The canonical worked write-up.
- **King, "Parse, Don't Validate", 2019** —
  <https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/>. Push the check to the
  boundary and let the result type carry that it happened. rfish parses at its edges — FEN,
  UCI, the net file, a Syzygy table — and validates nowhere.

### Proofs carried by values

- **Noonan, "Ghosts of Departed Proofs", Haskell Symposium 2018** —
  <https://kataskeue.com/gdp.pdf>. Preconditions encoded as proofs inhabiting phantom type
  parameters, at no runtime cost. The diagnosis this port uses, not the machinery: the failure
  mode it names — a proof discharged in a comment and represented in the code as a bare
  integer — is what every index newtype here is fixing.
- **Kiselyov & Shan, "Lightweight Static Capabilities", 2007**. The earlier statement: a
  capability type witnesses a checked property so downstream code need not re-check it.
- **Yanovski, Dang, Jung & Dreyer, "GhostCell", ICFP 2021** —
  <https://plv.mpi-sws.org/rustbelt/ghostcell/paper.pdf>. Branded types in Rust
  specifically, mechanised in RustBelt.
- **bluss, `indexing`** — <https://github.com/bluss/indexing>. Sound unchecked indexing via
  generativity. Listed to record that it is **out**: its payoff is `get_unchecked`, which
  `forbid(unsafe_code)` closes, and it is a dependency the engine may not take. What crosses
  is the discipline — state the guard against the slice you index — which
  [08-idiomatic-rust.md](08-idiomatic-rust.md) shows is worth tens of millions of instructions
  without any of the machinery.

### Typestate

- **Strom & Yemini, "Typestate", IEEE TSE 12(1), 1986** —
  <https://www.cs.cmu.edu/~aldrich/papers/classic/tse12-typestate.pdf>. The origin.
- **Aldrich, Sunshine, Saini & Sparks, "Typestate-Oriented Programming", Onward! 2009** —
  <https://www.cs.cmu.edu/~aldrich/papers/onward2009-state.pdf>. The observation that without
  language support, typestate is encoded by flag fields and design patterns. `Aligned<T>`'s
  missing resize API is the one instance in this tree: an invariant held by what the type does
  not let you do.

### Representation as a type property

- **The Rust Reference, "Type Layout"** —
  <https://doc.rust-lang.org/reference/type-layout.html>. `repr(transparent)`,
  `repr(align(N))`, `repr(u8)` — the attributes that make width and alignment properties of
  the type rather than hopes about the allocator.
- **"Niches for integer types in Rust"** —
  <https://deterministic.space/niche-int-types-in-rust.html> — and `core`'s own
  `niche_types.rs` —
  <https://doc.rust-lang.org/beta/src/core/num/niche_types.rs.html>. Why `Option<Square>` is
  two bytes here and what it would take to make it one.
- **The pattern-types RFC draft** —
  <https://gist.github.com/joboet/0cecbce925ee2ad1ee3e5520cec81e30>. The route that would give
  a range-restricted integer a niche without `unsafe`. Unstable; tracked, not adopted.

### Refinement types — the direction, not the practice

Everything above puts a *set* in a type. These put a *predicate* there, and would statically
discharge the in-range arguments this port currently makes in doc-comments. None is used.

- **Lehmann, Geller, Vazou & Jhala, "Flux: Liquid Types for Rust", PLDI 2023** —
  <https://dl.acm.org/doi/10.1145/3591283>.
- **Jhala & Vazou, "Refinement Types: A Tutorial", 2021** —
  <https://doi.org/10.1561/2500000032>.
- **Aebi & Furia, "Practical Range Refinement Types with Inference", 2026** —
  <https://arxiv.org/pdf/2607.00824>. The closest published match to the narrow case here:
  inferred integer *range* types, aimed at verifying index manipulation and in-bounds access.
- **Lattuada et al., "Verus", OOPSLA 2023** — <https://dl.acm.org/doi/10.1145/3586037> — and
  **Creusot** — <https://creusot.rs/>. Static verification, no runtime checks added. Named
  because the alternative escalation, "add an `unsafe` block and assert the bound", is
  permanently closed here, so the only direction that exists is more proof rather than less.

### The counterweight

- **"When Zero Cost Abstractions Aren't Zero Cost", 2021** —
  <https://blog.polybdenum.com/2021/08/09/when-zero-cost-abstractions-aren-t-zero-cost.html>.
  Read alongside every claim above. A newtype is free in *layout* and is not always free in
  *codegen*, and this port has the measurements to prove it — see the cost rule in
  [09-type-design.md](09-type-design.md).

## Protocol

- **UCI** — the protocol description as published by Stefan Meyer-Kahlen. The engine's
  handshake, option syntax and `info` line format follow it, and `tools/handshake.golden`
  pins the result.

## Chess facts

- **Perft results** — the reference node counts in `tools/perft.table` are published in
  several places and were reproduced here against a pristine upstream build. They are facts
  about chess, not about any engine.
- **Syzygy tablebases** — <https://github.com/syzygy1/tb>. The table format and the naming
  convention the prober in `crates/rfish-engine/src/platform/syzygy/` recognises.

## Terms

Here because a page of external sources is where a reader looks for them, and because two of
the three artifacts this repository handles are not covered by the same licence as its code.
[README.md](../README.md) carries the statement; this is the index into it.

- **GNU GPL v3** — <https://www.gnu.org/licenses/gpl-3.0.html>. rfish is a derivative of
  Stockfish and inherits it. The text is [Copying.txt](../Copying.txt) and the attribution is
  [AUTHORS](../AUTHORS), whose list is upstream's unmodified under a header naming the
  derivation — a port that reproduces upstream's output line for line does not get to reword
  its attribution.
- **ODbL** — <https://opendatacommons.org/licenses/odbl/odbl-10.txt>. The terms on the Leela
  Chess Zero data the networks are trained on. It reaches this repository through the file
  `cargo xtask net` downloads, not through any source file, which is why nothing in
  `crates/` names it.
- **The network itself is not in the tree.** It is fetched at build time and gitignored, so a
  clone carries no weights and the licence question travels with the download rather than with
  the checkout. [03-engine-eval.md](03-engine-eval.md) covers why it is a runtime input.
