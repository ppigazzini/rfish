# Idiomatic Rust: the translation, pattern by pattern

This is the page the port exists to write. Stockfish is C++ that reaches for a raw pointer
whenever the shape of the data calls for one; rfish forbids `unsafe` outright. Every entry
below is a place where that mattered, what replaced the pointer, and what the replacement
cost or saved.

**Read this before proposing an optimisation.** Half the ideas that look obvious here have
a reason they are not done.

---

## 1. The `StateInfo` chain: a `Vec`, not a linked list

**Upstream.** `Position::do_move` takes a `StateInfo&` the caller allocated — typically on
its own stack frame — and links it to the previous one through a `previous` pointer. The
repetition walk follows that chain backwards. A `Position` that outlives the frame holding
one of its `StateInfo`s is a dangling read, and the C++ avoids it by convention: the search
stack outlives the recursion, so the frames are still there.

**rfish.** `Position` owns `states: Vec<StateInfo>`. `do_move` pushes; `undo_move` pops;
`previous` is `len - 2`; the repetition walk is a backwards index scan over a slice.

**What it cost.** The `Vec` reallocates as the search deepens, once, and then never again —
`MAX_PLY` is 246 and the vector reaches that capacity in the first deep iteration.
`states.last()` is a bounds check the pointer deref was not, on a path taken several times
per node. Neither has shown up as measurable.

**What it bought.** The lifetime question does not arise. `Position` is `Clone` and can be
sent to another thread, which is what makes the Lazy-SMP design in
[04-multithreading.md](04-multithreading.md) express itself in `std::thread::scope`.

**The trap it introduced.** `st()` and `st_mut()` borrow `self`, so a method that reads the
state and then mutates the board cannot hold the reference across the mutation. The fix is
always to copy the field out first — never to reach for a raw pointer, and never to clone
the whole `StateInfo`.

---

## 2. The transposition table: keep the race, drop the UB

**Upstream.** The table is a raw allocation. Every thread writes 10-byte entries without
synchronisation, and a reader may see a half-written entry. The design absorbs that: the
stored `key16` is checked first, and a torn entry almost never matches.

That is a data race in both languages' formal terms. C++ gets away with it because nothing
checks; Rust would need `unsafe` to reproduce it literally.

**rfish.** Every field lives in an `AtomicU64`, read and written `Relaxed`. Two threads
still interleave, a reader may still see a half-updated cluster, and the key check is still
what catches it — the behaviour is upstream's. The difference is that the program has
defined semantics and ThreadSanitizer stays quiet.

**The layout is upstream's to the byte.** A cluster is 32 bytes holding three entries,
because the number of clusters a `Hash` setting buys is `mb * 1024 * 1024 / 32`, and *that
number decides which positions collide*. A larger cluster would change the collision
pattern and therefore the node count. Three 10-byte entries are packed into four `u64`
words with no field straddling a word:

```text
word[0..3]  entry i: key16 | move16 | value16 | eval16
word[3]     byte 2i = depth8, byte 2i+1 = gen_bound8, bytes 6..8 unused
```

**What it cost.** A `Relaxed` atomic load compiles to the same instruction as a plain load
on every architecture rfish targets. The measurable cost is that the compiler will not
merge or reorder them as freely — it cannot hoist a probe out of a loop, for instance. On
the probe path there is no loop to hoist out of.

**What it bought.** `&TranspositionTable` is shared across every thread with no lock and no
`&mut`, and the borrow checker proves it. No lifetime is asserted by hand.

---

## 3. The thread pool: `std::thread::scope`, no pool

**Upstream.** A persistent `ThreadPool` owns worker objects that hold raw pointers into
shared state. Rebuilding that wiring per search would be expensive and error-prone, so the
pool lives for the process.

**rfish.** `ThreadPool` owns `Vec<SearchWorker>` — the histories have to survive between
searches, which is most of why the second search of a game is faster than the first — but
there is no persistent *thread*. `search()` calls `std::thread::scope`, which lends each
helper a `&mut SearchWorker` for exactly the duration of the search and joins them all
before returning. The main thread searches on the caller's own thread, so a
single-threaded run involves no thread at all.

**What it cost.** One thread spawn per `go` per helper: microseconds against a search
measured in seconds, and zero for `Threads 1`.

**What it bought.** "A worker used after the search ended" is not a bug that can be written.
The scope's lifetime bound is what makes `&mut` to a worker and `&` to the table coexist
without a lock.

---

## 4. The move picker holds no borrows

**Upstream.** `MovePicker` stores `const Position&` and pointers to the history tables, and
the search makes and unmakes moves between calls to `next_move()`. That is fine in C++ and
impossible in Rust: the picker's `&Position` would conflict with the `&mut Position` that
`do_move` needs.

**rfish.** `MovePicker` stores no references. `next()` takes `&Position` and `&Histories`
at every call, and the continuation planes are passed as `ContKey` — plain `Copy` data
naming which plane to look up, not a reference to it.

This is the one place the borrow checker changed the *shape* of a type rather than its
implementation, and it is worth knowing that it is forced rather than chosen.

**What it cost.** Four extra arguments per call, all in registers. The plane lookup happens
per scoring pass instead of once per node, which is a handful of index operations.

---

## 5. Attack tables: `LazyLock`, not a startup hook

**Upstream.** `Bitboards::init()` fills the magic tables into file-scope arrays, and every
static initialiser has to be ordered around the fact that reading them before `init()` runs
is undefined.

**rfish.** `static SLIDERS: LazyLock<SliderTables>`. First access builds them.

The Zobrist tables go further and are **`const`-evaluated**: `static TABLES: Tables =
build();`. They exist before `main`, cost nothing at startup, and cannot be read half
built. The magic tables are not const-evaluated because the magic *search* is a loop over
88 772 table entries with a subset enumeration inside it, which is more const-eval than is
comfortable; the ray tables are `LazyLock` for symmetry with them.

**What it cost — measured, after this page claimed otherwise.** This section used to say
that every access after the first is "one already-resolved branch the predictor gets for
free". It is not free. A `LazyLock` pays its check per **deref**, not once per program: the
acquire load of the `Once` state is a real load that LLVM will not always hoist across the
loads around it. On `bench 16 1 8` the profile put **17.7M instructions — 0.9% of the whole
run — inside `Once`**, reached from `rook_attacks` and `bishop_attacks`.
`update_piece_threats` alone paid 3.3M of it, because it derefs dozens of times per call.

`attacks::Sliders` is the fix: a borrow taken once and read many times, with the free
functions kept for callers that ask once. Converting the callers that ask **in a loop** —
the four movegen piece loops deref once per attacker — was worth **12.1M**.

Two rules came out of doing it, and both cost a measurement to learn:

- **Convert a caller that derefs in a LOOP; measure the rest rather than assuming it
  follows.** The same handle for the RAY tables was written and measured three times, at
  three different baselines, and lost every time: **+1.76M, +2.96M, +1.05M**. `between_bb`
  and `ray_pass_bb` are read once or twice per caller, LLVM already CSEs those, and holding
  the extra pointer live costs a register.
- **Do not take the borrow above an early return.** Taking it at the top of `gives_check`,
  `legal` and `see_ge` ran the initialisation check on every call including the ones that
  never read a slider — and those are the dominant paths. mcfish carries the same point in
  its own comment on `pos_gives_check`: *"the dominant paths … need none of them, and the
  hoisted loads do not sink past the early returns on their own."* Sinking them into the
  branches that consume them was worth 1.5M.

---

## 6. `MoveList`: a fixed array that panics rather than overflows

256 is upstream's `MAX_MOVES`, and it is a **proven bound**, not a guess: no reachable chess
position has more legal moves. `MoveList::push` asserts before writing.

The assertion is not defensive programming. It is the difference between "the generator has
a bug that produces a move twice" surfacing as a panic with a backtrace, and surfacing as a
corrupted stack frame three functions later.

---

## 7. Integer semantics: the classic trap, and how it is defused

Rust traps on overflow in debug and wraps in release. C++ has undefined behaviour where
Rust has neither. Upstream relies on wrapping in a handful of places — the Zobrist
generator's multiply, the magic multiply, the `adjust_key50` mix.

**Every one of those says `wrapping_mul` / `wrapping_add` / `wrapping_sub` in the rfish
source.** None inherits its behaviour from a profile.

That is what makes `overflow-checks = true` in the `dev`, `test` and `gate` profiles
useful: a bare `+` that wraps is a bug the gate profile catches, because the intended wraps
are all spelled out and will not trip it.

`release` sets `overflow-checks = false`, matching upstream's `-O3`. A change that only
works because release wraps a bare `+` will pass `cargo xtask signature` and fail
`cargo xtask test`. That ordering is deliberate.

---

## 8. SIMD: what the constraint actually cost

Upstream's NNUE kernels are hand-written intrinsics behind `#if` per instruction set.
`std::arch` intrinsics are `unsafe` in Rust; `std::simd` is nightly. Both are out.

What is in: **ordinary loops over fixed-size arrays**, which LLVM vectorises under
`-C target-cpu`. `cargo xtask build --arch <tier>` sets it; the default build sets nothing,
because the bench anchor has to be reproducible on a machine nobody here owns.

**The forward pass is bit-exact with upstream** — `cargo xtask nnue-check` proves it
position by position. So the arithmetic cost nothing; only the speed did.

For most of the port's life the speed gap was NOT the missing intrinsics. rfish recomputed
the accumulator from scratch per evaluation where upstream updates it incrementally, and
that term dominated everything else — an intrinsics-versus-autovectorisation comparison
would have been measuring a cost only one side paid. Diffing the feature sets removed it,
for a **1.48x** speedup measured as an A/B on the same machine under the same load
(31.34/31.58/30.80 s of user CPU before, 21.35/20.29/21.23 s after).

That ratio is the only NNUE speed number in this repository, and the restriction is
deliberate. A ratio between two arms measured under identical conditions survives a noisy
machine; an absolute nodes-per-second figure does not, and an earlier draft of this section
carried one taken while five cores were busy with something else. It was wrong by a large
factor and read as authoritative. Do not add an absolute throughput number here that was not
taken on a quiet box at a named `--arch` tier.

**So the honest state is: the constraint has not yet been shown to cost anything on this
axis.** The largest algorithmic difference is gone, so an intrinsics-versus-autovectorisation
comparison is now a fair one to run. It has not been run. Do not cite this section as
evidence either way until someone does.

---

## 9. Allocation: the pattern that cost the most, and the one that fixed it

`Vec` in a per-node path is the single largest defect this port has had. Upstream allocates
NOTHING per node — `ExtMove moves[MAX_MOVES]` and `ValueList<Move, 32>` are inline storage
in the object — and rfish was issuing three mallocs per node without anyone noticing,
because no gate can see it: the node count is identical either way.

Measured with callgrind on material-eval builds at an identical tree (881 762 nodes):

| | malloc/free `Ir` | total `Ir` |
|---|---:|---:|
| before | 100.2 M | 3.900 G |
| after | **3.53 M** | **3.8125 G** |
| Stockfish | 1.92 M | 3.952 G |

**The obvious translation is the wrong one, and it was measured before it was rejected.**
Giving the picker an inline `[ScoredMove; MAX_MOVES]` — the literal shape of upstream's
field — removes 97 M of allocator traffic and adds **473 M** of initialisation, because
safe Rust must initialise the array and C++ leaves it undefined. Net worse than doing
nothing. There is no safe way to have a large uninitialised buffer; the answer is not to
need one.

**A fourth one survived all of that, in the evaluation rather than the search.**
`Network::evaluate` opened with `let mut transformed = vec![0u8; L1]` — a malloc, a 1 KiB
zero-fill and a free per EVALUATION, 61,341 of them over `bench 16 1 8` — for a buffer
`transform` overwrites in full before anything reads it, so it never needed to be fresh.
`EvalScratch` already existed as the per-worker home for exactly this and said so in its own
doc comment. Hoisting it there is **38.4 M** on the NNUE axis.

Two things to take from its having lasted so long. The audit that found the first three swept
the SEARCH and stopped there, so "per-node" was read as "per node of the tree" and an
allocation one call deeper went unexamined — grep the evaluation too. And a struct whose
documentation says it exists to prevent per-node allocation is not evidence that it does;
`EvalScratch` carried that sentence the whole time.

What works is the *workhorse collection* from the Rust Performance Book: hoist the buffer
out of the hot path, keep it alive, and `clear()` it — capacity survives, so after the
first visit there is neither an allocation nor an initialisation. rfish keys those buffers
by `(ply, kind)`, and the `kind` is load-bearing rather than defensive: the search re-enters
a ply twice over — a singular search re-runs the same ply with a move excluded while the
outer node is still walking its own list, and razoring drops into quiescence at the same ply
from inside that singular search. One buffer per ply corrupts the outer list mid-iteration.
Neither re-entry can nest further, because a singular search requires no excluded move, so
three slots per ply are sufficient *and provably so*.

The general rule this leaves behind: **a hot-path collection belongs to the worker, not to
the object that uses it**, and it is lent per call the same way the position and the
histories already are. That also keeps the picker free of borrows (§4).

Where a small fixed bound really is the answer — the searched-move lists, capped at 32 by
upstream — a plain array is right, because 64 bytes of initialisation is cheaper than a
malloc. The crossover is size, and it is worth measuring rather than guessing.

---

## 10. Measurement laws

Every one of these was paid for in a sibling port. They are not Rust-specific.

- **Measure whole-binary, at a named tier.** Instruction arithmetic over a diff is a guess.
  The specialised node bodies swing under register-allocation changes and a small edit in
  the transposition table flips LTO inlining decisions elsewhere.
- **Subtract startup, by measurement, and only on the instruction axis.** A whole-process
  counter includes the magic-table build and the net load. On the cycles axis the
  subtraction removes a term whose error rivals the effect — time the search directly
  instead (the bench's own total contains no startup by construction).
- **Isolate the component instead of attributing it.** `go perft` is the board zone alone.
  Attribution across two differently-inlined binaries is void by construction.
- **A bounds check is not automatically the cost — and when it is, it is the LENGTH LOAD,
  not the comparison.** Rust elides most of them. Find the check in the disassembly before
  reshaping code around it, and if reshaping is needed it is still not a licence for
  `unsafe`: restructure so the bound is *provable* instead. Two measurements on the same
  bench separate the two halves. Moving the search stack from `Vec<StackEntry>` to
  `Box<[StackEntry; STACK_SIZE]>` — which turns the length into an immediate and lets LLVM
  fold the checks of a node's several accesses together — was worth **−16.0M**. Masking
  `Square::index()` to `& 63` at `piece_on`, which removes the comparison but leaves the
  load, cost **+2.0M**: the check folds into the addressing, the AND is a dependent
  instruction in the address chain. See §14.
- **Gate on the clock, and validate any counter before believing it.** A change can be
  instruction-neutral, cache-better and branch-level and still cost cycles.
- **Size an Elo run before starting it.** The 70-Elo-per-doubling figure is a LONG time
  control number and does not hold at the fast ones. Three measured cells against a PGO'd
  upstream — 0.1+0.001 at two tiers and 1+0.01 — all imply **138–152 Elo per doubling**.
  Use the figure that matches the clock being run, or the run is mis-sized before it starts.
- **An instruction count cannot see a latency win, and it is not neutral about one.**
  Callgrind counts instructions RETIRED. Anything whose whole purpose is to hide dependency
  latency — multiple accumulator chains, software pipelining, unrolling for ILP — can only
  ADD instructions, so it reads as a regression on this axis no matter how much wall clock
  it buys. Measured here: mcfish's two-chain affine accumulator cost **+3.08M Ir**. That
  does NOT mean it is a bad idea; it means Ir is the wrong instrument for it, and this repo
  has no trustworthy cycle harness (see the NPS entry below). Before optimising, decide
  which quantity the change is supposed to move, and do not chase a latency idea against an
  instruction target.
- **NPS on this class of box cannot settle a few percent.** Readings ranged 240k–275k for
  one unchanged binary, and a cold first run read 103k. Use callgrind, which is
  deterministic to 0.01%, for anything under ~10%; use NPS only for a headline ratio.

---

## 11. Falsified, or not attempted for a stated reason

Keep this list current. Re-deriving a dead idea costs a session.

| Idea | Status |
|---|---|
| `std::simd` / `stdarch` intrinsics for the NNUE kernels | **Rejected by constraint**, not by measurement — nightly and `unsafe` respectively. Not a measurement result; do not cite it as one. |
| Comparing autovectorised NNUE kernels against upstream's intrinsics | **Now worth doing.** The accumulator no longer dominates the way it did; see `docs/03-engine-eval.md` for the current split. |
| Recomputing the NNUE accumulator per evaluation | **Superseded, with a measurement.** Diffing the recomputed feature sets is 1.48x faster at a bit-identical node count. Do not go back without beating 21.2 s of CPU on the depth-11 bench. |
| Optimising the classical evaluation | **Pointless.** It runs only when no net is on disk, and it has a deletion date. |
| `memmap2` for the Syzygy tables | **Rejected.** The crate's soundness contract cannot be met (a table file can be truncated under the map), and positioned reads behind a block cache give what the mapping actually provides. |
| Const-evaluating the magic tables | **Not attempted.** 88 772 entries with a subset enumeration each is far more const-eval than the Zobrist tables' ~800 draws; `LazyLock` costs one predicted branch per lookup. Measure before changing. |
| An inline `[ScoredMove; MAX_MOVES]` in the move picker, mirroring upstream's field | **Falsified, with a measurement.** Removes 97 M of allocator traffic, adds 473 M of per-node initialisation — worse than the `Vec` it replaced. Safe Rust cannot leave a buffer uninitialised; reuse one instead (§9). This is about a PICKER-owned buffer built per node. A WORKER-owned `Box<[ScoredMove; MAX_MOVES]>` initialised once is a different thing and it wins — see §14. |
| A fixed `[DirtyThreat; 96]` for the threat records, mirroring upstream's `ValueList<DirtyThreat, 96>` | **Falsified, with a measurement**, though the bound is upstream's own proven one. Inline in `PlySlot` it cost **+2.83M** — 388 bytes widened the stride of an array of 256 that is indexed per node — and boxed it still cost **+1.96M**. The list is short, so the `Vec`'s capacity check was already cheap. Copying an oracle's data structure is not automatically right; see §14. |
| Const-generic `GenType` on the move generator, mirroring upstream's `template<GenType Type>` | **Falsified, with a measurement.** Every picker call site passes a literal kind, and the generator tests it ten times, so the fold looked certain. It cost **+2.14M**, rising to **+14.4M** with the pawn generator forced inline alongside it: four specialised copies of the generator execute MORE instructions than one shared body. |
| A borrow handle for the RAY tables, mirroring the one that works for the sliders | **Falsified three times, at three baselines** (+1.76M, +2.96M, +1.05M). See §5. |
| Two independent accumulator chains in the sparse affine walk, mirroring mcfish's `AFFINE_CHAINS = 2` | **Falsified ON THE INSTRUCTION AXIS, at +3.08M — and that is the wrong axis for it.** Chains hide dependency latency; Ir counts retired instructions, so they can only add. Not re-attemptable until this repo has a trustworthy cycle harness. See §10. |
| Narrowing the pawn-pair `changed` set to zfish's per-board `after & !before` / `before & !after` masks | **Falsified, with a measurement**, at +41K. It is CORRECT — the signature confirms rfish's extra pairs cancel in the fold, which was worth establishing — but on the common pawn move the symmetric difference is already just `{from, to}`, so it saves a handful of emitted pairs and pays four extra `andnot`s on every call. |
| Upstream's hybrid same-half king-move accumulator path | **Not attempted, for a stated reason.** `halfka_delta` takes ONE king square and assumes base and target share it, so a hybrid path needs a new function re-indexing every piece from the old square to the new — roughly 64 rows. In upstream and zfish that buys skipping a full refresh; here rfish's refresh already starts from a per-king-square cache entry whose halfka diff is usually small, so the hybrid would ADD halfka work. Measure before assuming. |
| Fusing move scoring into generation, because "the siblings have no `generate_append`" | **The PREMISE was false.** mcfish's `score_list()` calls `generate_captures`/`generate_quiets`/`generate_evasions` to build the whole list and then walks it to fill `out[i].value` — exactly rfish's shape. It only looks fused because it is one static function that callgrind folds into `nextMove`. The picker-zone gap is inside the generation loops, not in the pass structure. See §15.6. |
| Making `Bitboard::iter` borrow instead of copy | **Rejected by design.** Iterating by value is what lets a loop mutate the board it came from, which is upstream's `while (b) pop_lsb(b)` over a local with the local made explicit. |
| Rewriting the feature transformer's fold and transform in `std::simd` | **Not worth attempting, from the disassembly.** Both already emit upstream's kernel shape — `vpaddw`/`vpsubw`/`vpmovsxbw` in the fold, `vpminsw`/`vpmullw`/`vpsrlw`/`vpackuswb` in the transform — over 136 and 156 `%ymm` operands. Explicit vectors would transcribe what LLVM emits. See §12. |
| Upstream's per-move accumulator delta | **Falsified, with a measurement, after being BUILT.** Bit-exact (signature 2508687, nnue-check 109/109) and it loses: recording costs 85.7M on every `do_move` and the fast path fires on **11%** of evaluations, so it saves 0.47M. The board-zone half is kept and tested; see `docs/03-engine-eval.md`. |
| Making `diff_apply`'s membership tests branchless | **Falsified, with a measurement.** Storing every element and advancing the count by the predicate removes 2.11M conditional mispredicts (2.187 → **1.896** of upstream's) and costs **+62.7M instructions** and **+37.4M data writes**. The branchy form stores only the rare kept element; the branchless one stores every element of BOTH sets. See §13 for why the premise was wrong. |

---

## 12. External references

Consulted July 2026. Listed with what each is actually good for here, because a link with
no verdict gets re-read from scratch every session.

| Source | Use it for |
|---|---|
| [The Rust Performance Book — Heap Allocations](https://nnethercote.github.io/perf-book/heap-allocations.html) | The workhorse-collection pattern §9 is built on, and the general rule that a `Vec` in a loop body should be hoisted and `clear()`ed. The canonical reference; check here before inventing an allocation strategy. |
| [The Rust Performance Book — Bounds Checks](https://nnethercote.github.io/perf-book/bounds-checks.html) | Why the answer is almost never `get_unchecked`. |
| [How to avoid bounds checks in Rust without `unsafe`](https://shnatsel.medium.com/how-to-avoid-bounds-checks-in-rust-without-unsafe-f65e618b4c1e) | The four safe techniques, in the order to try them: bind a subslice and index by *its* `len()`; iterate instead of indexing; `assert!` the length in front of the hot loop so the optimiser can use it; `#[inline(always)]` so the length fact crosses the function boundary. This is the playbook for the "restructure so the bound is provable" rule in §10. |

**The measured claim worth remembering:** removing bounds checks is worth **1–3% typically
and 15% at the very best**, and only in number-crunching code. Chase the structure first;
the checks are the last few per cent, not the first twenty.

**What this port's own counters say has changed, and the old reading was wrong.** This
paragraph used to claim rfish retires 1.7–1.9x more branches than upstream while *missing
fewer* of them — a wall of perfectly-predicted checks. Re-measured with both sides on one
toolchain (clang/LLVM at rustc's major, PGO on top of LTO, avx2, startup subtracted), the
branch picture is the opposite of that:

| | conditional branches | mispredicted | L1 icache misses |
|---|---|---|---|
| NNUE axis | 1.34x | **2.57x** | 2.10x |
| spine axis | 0.98x | **1.41x** | 2.00x |

rfish retires roughly as many branches as upstream and misses **1.4–2.6x** as many, and
takes twice the instruction-cache misses on both axes. Indirect branches are worse again
(2.3x on the spine, and 2.2x of them mispredicted). So the surviving gap is NOT a wall of
cheap predicted bounds checks; it is prediction and instruction-fetch behaviour, which is
where a reader arriving here for "why is safe Rust slower" should look first. The
bounds-check figure above stands as published guidance; it is simply not what this port's
remaining gap is made of.

### Vectorisation: what actually governs it here

The links above are about allocations and bounds checks. Those were the spine's problem and
they are solved. They are **not** the evaluation's problem, and a reader who arrives here
looking for why the NNUE is slower than upstream will not find it above. What follows is
measured on this codebase rather than cited.

**First: look at the disassembly before theorising.** The assumption that safe Rust "cannot
vectorise" is false here and cost time. Built at `-C target-cpu=native` on a Zen 4 box, the
accumulator fold emits exactly what a hand-written kernel would:

```
vmovdqu64 (%rcx),%zmm3 ... vpaddw 0x40(%rsi,%rdi,2),%zmm2,%zmm2
```

four 512-bit registers carrying a 128-entry tile, one `vpaddw` per 32 entries. `objdump -d`
and count `%zmm` / `%ymm` / `%xmm` in the hot symbol; do that first, every time.

**What was measured to BLOCK widening:**

- **Loop-carried state.** Pairing non-zero inputs through an `Option<(usize, i16)>` carried
  between iterations cost **+625M instructions** — the arithmetic was strictly cheaper and
  the loop stopped vectorising. Anything that makes iteration *n* depend on iteration *n-1*
  ends the discussion.
- **A `&mut [T]` output.** The compiler must assume stores through it alias the weights
  being read, and spills the accumulators every iteration. A `&mut [T; N]` with `N` a const
  generic does not have that problem: **-269M**.
- **Indexing where a chunk would do.** `w[i * N..i * N + N]` costs a multiply and a bounds
  check per iteration; `chunks_exact(N)` zipped against the driver gives the same rows with
  neither: **-20M**.

**What was measured NOT to help,** all of it reshaping code to look more like upstream's
kernels: multiplying in the 16-bit domain (+84M), folding two weight kinds in one sweep
(+201M), zero-skipping in groups of four as `vpdpbusd` does (+1015M). **The shapes upstream
chose are the ones its instructions reward, not the ones the autovectoriser rewards.** Port
upstream's *algorithm*; do not port the register-level shape it wrote for an instruction
this toolchain will not emit.

**Tile widths are measured, never reasoned.** 32 / 64 / 128 / 256 gave 4,093M / 3,673M /
3,598M / 4,037M — not monotonic in either direction, and the peak is tier-specific.

### The ceiling, and where it comes from

**Upstream's first affine layer is NOT `vpdpbusd` at avx2 — this section had it wrong.**
Disassembled, the avx2 oracle holds zero `vpdpbusd`; that instruction is VNNI and appears only
at `x86-64-vnni512`. At avx2 upstream runs 72 `vpmaddubsw` and 76 `vpmaddwd`, and rfish runs
42 `vpmaddwd` and no `vpmaddubsw`. Neither needs `unsafe`. What rfish is missing is the
`u8`x`i8` form: its kernel widens the weights to `i16` first, so it moves half the lanes per
instruction and pays the widening. See `docs/03-engine-eval.md`. The paragraph below is kept
because its measurements stand, but read its premise as false at the tier measured here.

Upstream's first affine layer is one `vpdpbusd` per four input bytes. There is no way to
reach that instruction from this project's constraints, and three separate attempts to
recover its arithmetic in scalar form all lost (above). This is not a limit of Rust the
language — it is a limit of the intersection this port has chosen:

| route to explicit SIMD | safe? | stable? | usable here |
|---|---|---|---|
| `std::arch` intrinsics | **no** — every intrinsic is an `unsafe fn` | yes | no, `unsafe_code = "forbid"` |
| `std::simd` (`portable_simd`) | **yes** — no `unsafe` needed | **no** — nightly only | **yes**, at the cost of the dated nightly `rust-toolchain.toml` pins |
| autovectorised scalar loops | yes | yes | yes, and it is still what most of the NNUE uses |

The `std::simd` row said "no, `rust-toolchain.toml` pins stable" for longer than it was true.
The pin was bought precisely to lift that restriction, `eval/nnue/layers.rs` has used it since,
and the row survived as the reason not to look anywhere else. **It is not the reason the
transformer is scalar.** The reason is measured, and it is below.

The sibling ports are not doing something cleverer with the same tools. ../zfish writes
upstream's kernels directly — 173 `@Vector` uses across ten NNUE files, plus per-ISA files
like `nnue_affine_vnni.zig` — because Zig has portable SIMD vectors in the *safe, stable*
language. Rust's equivalent needs no `unsafe` either and is now in use here; what it costs is
the nightly channel rather than the constraint. What remains out of reach is `vpdpbusd`, and
at avx2 that is IRRELEVANT -- the oracle holds none. What remains out of reach is `vpdpbusd`
specifically, which `std::simd` has no operation for.

**Do not read that as the reason this port is slow.** Measured head to head at the same pin,
same box, same net and an identical 163,081-node tree, ../zfish retires 1.001 of upstream's
instructions and ../mcfish 0.877, against rfish's 1.667 — and ../zfish is a SAFE-SIMD port
with no PGO. A constraint both ports also satisfy cannot be what costs this one 827M
instructions. What separates them is that both siblings implement upstream's per-move
accumulator delta and rfish recomputes the feature sets every evaluation. See
[03-engine-eval.md](03-engine-eval.md) for the four-way table and the source evidence.

**Explicit `std::simd` in the feature transformer is NOT an open lead.** It is the largest
search-time block in the engine — `fold_changed` at 190M and `transform` at 148M over
`bench 16 1 8` — and it is scalar source, so it reads like the obvious next target. The
disassembly of the PGO build says it is already there: `fold_changed` emits 136 `%ymm`
operands, 16 `vpaddw`, 16 `vpsubw` and 16 `vpmovsxbw`, and `transform` emits the clamp,
pairwise multiply, shift and pack as 16 `vpminsw` / 16 `vpmaxsw` / 8 `vpmullw` / 8 `vpsrlw` /
8 `vpackuswb` — upstream's own kernel shape, reached from `as_chunks_mut` and a `zip`. Writing
those two functions in `std::simd` would be a transcription of what LLVM already emits. Count
the vector operands in the symbol before spending a session on this.

**What is deliberately NOT on this list:** `smallvec`, `arrayvec`, `bumpalo` and every other
allocation crate. They are the standard answer to §9 and rfish cannot use any of them — the
engine crate has zero dependencies, and that is a reviewed property. The workhorse pattern
is the dependency-free equivalent and measured as good.

---

## 13. Branch misprediction: where it is, and the trade that does not pay

Once the NNUE memory layout was fixed (§9), **misprediction became the worst counter on that
axis** — 2.187x upstream's, where every cache row is at or below 1.16. It is worth knowing
where it actually is before reformulating anything, because the obvious fix loses.

Per-line, under `--branch-sim=yes` on the PGO build, `bench 16 1 8`:

| line | mispredicts |
|---|---|
| `diff_apply`, `if mark[w] & b == 0` | 861,228 |
| `diff_apply`, `if mark[w] & b != 0` | 862,252 |
| `Bitboard::iter`, `if self.0.is_empty()` | 1,687,451 |
| `NetReader::leb128_i16` | 5,138,376 — the net LOAD, not the search |

Two things this settles. The `leb128_i16` row is startup and must come out before any ratio is
quoted; it is 25% of the whole-process total on its own. And **the search spine is not the
problem** — `movepick::next_move` mispredicts 911k against upstream's `MovePicker::next_move`
at 1,380k, and `worker::node` 855k against upstream's `search` at 1,030k. rfish predicts the
SEARCH better than upstream does. The excess is all in the evaluation.

**The `Bitboard::iter` row is the design, not the loop.** `while bb != 0 { pop_lsb }` is what
upstream writes too; rfish simply runs it far more, because it rebuilds the threat set every
evaluation where upstream delta-updates. That row moves when the per-move delta lands and not
before — see `docs/03-engine-eval.md`.

**The `diff_apply` rows look like the textbook case for branchless code, and going branchless
loses badly.** Writing the element unconditionally and advancing the count by the predicate —
`buf[n] = f; n += usize::from(!hit)` — is bit-exact and does remove the mispredicts:

| | instructions | mispredicts | data writes |
|---|---|---|---|
| branchy (kept) | 2,067,496,922 | 15,891,946 | 156,211,846 |
| branchless | 2,130,153,965 | **13,778,891** | 193,619,507 |
| | **+62.7M** | −2.11M | **+37.4M** |

−2.11M mispredicts is roughly 36M cycles; +62.7M instructions is more than that on its own,
and the write traffic is the reason: **the branchy form stores only the rare kept element,
the branchless one stores every element of both sets.**

The premise was wrong, and the mistake is worth naming. A test that mispredicts 861k times
looks like a coin flip; this one is heavily BIASED — most features are unchanged from one
evaluation to the next, so nearly every iteration takes the same arm and the misses are the
minority transitions. Branchless trades a rare store for a certain one, which only pays when
the predicate really is balanced. **Measure the taken/not-taken RATIO, not the mispredict
count, before removing a branch:** `Bc` against `Bcm` per line is what distinguishes a coin
flip from a biased test, and only the first is worth the unconditional write.

---

## 14. Cost that is not in the source: four shapes, and what each was worth

Everything in this section came out of one campaign: closing the spine-and-search gap
against `../zfish` and `../mcfish` at a matched tier, a matched net and an identical 163 081
nodes, with startup subtracted. It moved that zone from **1.31x zfish** to **1.14x**, and the
whole bench from 2 019 M instructions to 1 909 M, at a bit-identical signature throughout.

What makes these worth writing down is that **none of them is visible in the source as
work**. Each is a construct that reads as free and is not. Grouped by the lever, largest
first within each.

### 14.1 Fold the constants the caller already has

`#[inline(always)]` pays where it **specialises**, and costs where it only removes a call.
The test is whether the caller passes something the callee branches on.

| change | why it folds | Ir |
|---|---|---|
| `update_piece_threats` | all six call sites pass `put` and `compute_ray` as literals | **−17.3M** |
| `process_sliders` | its two call sites pass `add_direct` as literal `true` and `false` | **−9.8M** |
| `correction_value`, the `do_move` wrapper, TT probe and store | the caller already holds the stack index, the state, the cluster | **−9.5M** |
| `attackers_to_occ` | was `#[inline]`, which the compiler declined; zfish and mcfish both fold `attackersTo` into `legal`, `gives_check` and `see_ge` | **−1.1M** |
| `generate_into` | `GenType` is a RUNTIME value at both call sites — nothing folds | +235K |
| `slider_blockers` | already inlined | −47 |
| `update_continuation_histories` | nothing to specialise | +219K |

### 14.2 Make the length an immediate

A `Vec` carries its length and capacity in memory, so every index LOADS the length before
comparing, and LLVM can fold nothing across a function's several accesses because the length
is, as far as it knows, free to change between them. `Box<[T; N]>` makes it an immediate.

| change | Ir |
|---|---|
| the search stack | **−16.0M** |
| the NNUE ply stack and the move-buffer pool | **−8.3M** |
| `MoveBuf` to a boxed array and a count (21 calls to `RawVec::grow_one` sat in `generate_append` alone, for a buffer that cannot grow) | **−2.4M** |
| the reduction table | **−0.6M** |

Not everything of this shape qualifies. The transposition table's cluster array is sized by
the `Hash` option and `Position::states` grows with the game; both stay vectors. And the
threat-record list measured WORSE either way (§11) — it is short, so the capacity check was
already cheap, and inline it widened a struct that is indexed per node.

**One of these reversed itself inside a single session.** The `MoveBuf` array measured
**+28K — neutral** early on, was reverted as churn, and measured **−2.4M** later. Same diff,
different answer: it only pays once the push loop is inlined into a caller that can hold the
count in a register. A neutral result is a result *at that baseline*, not a permanent one.

### 14.3 Delete the case the call graph excludes

A type that carries a state the callers cannot produce costs a branch at every use.

- **`ContKeys` was `[Option<ContKey>; 6]`**, so `score_quiets` tested five `Option`s per
  quiet move on the picker's hottest line — 17.2M instructions sit at that read. The branch
  could never be taken from there: the main search fills all six with `Some`, and the only
  constructors that pass `None` are quiescence and `ProbCut`, which reach `score_evasions` at
  most and never `QuietInit`. Replacing them with a named `UNREAD_PLANE` index: **−11.3M**.
- **`EvalScratch::grow_to`** ran twice per `do_move` and again per `transform`, and could not
  resize after the first descent to a given depth. Sizing the vector to `PLY_SLOTS` once:
  **−9.8M**.
- **`PieceType::from_index`'s out-of-range arm** sat on every `piece_type()` call, although
  the low three bits of a `Piece` are 0..=6 by construction. A table indexed by a value the
  mask proves is in range is what mcfish's `type_of_piece` and zfish's `pc & 7` cost:
  **−0.5M**.
- **`EvalScratch::new_search` cleared the ply vector**, dropping three heap buffers per slot
  so the next descent re-allocated all of them on the way down. Resetting the validity fields
  instead says exactly the same thing: **−2.5M**.

### 14.4 Resolve once what does not vary, and not one line earlier

Two opposite failure modes, and the boundary between them is an early return.

**Resolve once per list.** `score_quiets` re-derived the butterfly row, the pawn plane, the
five continuation planes and the low-ply row for every move, although the colour, the pawn
key, the ply and the parent moves are fixed for the whole list. Upstream's
`ss->continuationHistory` is a POINTER settled at the node, and zfish's `pawnHistoryBlock`
says it outright — *"resolve it ONCE per move list"*. Worth **−2.9M**, and the disassembly
confirms the per-move read became `shl $0x7; add base; movswl (%rax,%r12,2)` with no bounds
check at all.

**But not above an early return.** The same instinct applied to the slider-table borrow in
`gives_check`, `legal` and `see_ge` put an initialisation check on every call, including the
majority that never read a slider. Sinking them into the consuming branches: **−1.5M**. §5
has the rule.

**And unroll where the index has to be a constant.** `update_continuation_histories` cost
262.8 instructions per call against mcfish's 117.7, at an identical 58 435 calls. Its loop
over the six `(ply, weight)` pairs did not unroll — the `break` and `continue` kept the ply a
runtime value — so the frame lookup stayed a bounds-checked slice index worth 33 instructions
per call on its own, where both siblings reach the frame by constant pointer arithmetic.
Written out per ply with literal offsets: **−4.8M**.

### 14.5 How to find these

The disassembly and the per-function profile, not intuition. Three checks earned their keep:

- **Compare CALL COUNTS before comparing costs.** `next_move` is called 380 931 times in all
  three ports; `see_ge` 202 943. With the counts matched, a cost difference is a per-call
  difference and nothing else — which is what turned "history is at parity" into "history is
  1.79x mcfish".
- **Count the calls a hot function still makes.** `objdump` over `generate_append` showed 21
  calls to `RawVec::grow_one` for a buffer that cannot grow. That is what pointed at §14.2.
- **Attribute std's cost back to the caller.** A per-function, per-file split put 15.7M of
  `slice::last` and 6.5M of `Once` inside the spine and search zones. Neither appears in
  rfish's own source as anything.

**The item this campaign named as outstanding has since been done — see §15.4.** It was
`Position::st()` as `states.last().expect(..)`, worth 15.7M, and the obstacle recorded here
was that `set_repetition`, `upcoming_repetition` and `has_repeated` index arbitrary positions
in the chain. That obstacle turned out to be illusory, and the reason is worth keeping: split
into `st: StateInfo` plus `prev: Vec<StateInfo>`, **`prev.len()` is exactly the current
state's old index**, so every backwards walk lands wholly inside `prev` and none of the three
needs a "which half am I in" test. No dispatch helper was required. A refactor that looks like
it needs a branch everywhere sometimes only needs the right split point.

---

## 15. Five more shapes, from a two-wave agent fleet

Ten chartered lanes, each on a disjoint file, each with a twenty-minute budget, all measured
the same way: avx2 tier, matched net, an identical 163 081-node bench, startup subtracted.
Two waves took the whole bench from **1 909 M to 1 763 M instructions** at a bit-identical
signature. The shapes below are new; §14's four still hold and are not repeated.

The headline is that **eight of the ten largest wins in this file are in the NNUE zone, and
none of them needed an intrinsic.** ../mcfish is plain C and beats rfish on the affine layer;
that is what said the gap was ordinary program structure rather than the `unsafe` ban.

### 15.1 Delete the horizontal reduction — walk by column, not by row

`AffineLayer::propagate` walked weights a row at a time: one scalar accumulator per output,
the input re-read for each of the N rows, and a **horizontal reduction per row**. LLVM
vectorises the inner dot product happily and then spends log2(N) shuffle-and-add pairs to
extract one lane — N times over.

Walking by COLUMN over the transposed copy the sparse path already maintains gives one
`Simd<i32, N>` accumulator seeded from the biases, the input read once, and **no horizontal
reduction at all**: each product lands in the lane it belongs to. Bit-exact by construction,
because it is the same integer products summed in a different order.

**−41.9M**, and no new memory — it reuses a copy that was already resident.

Generalise it as: *a reduction whose result is one lane of a vector you are about to build
anyway is pure waste.* Look for `sum` accumulated across a loop and then stored to
`out[i]` — that is the signature of a row walk that wants to be a column walk.

### 15.2 An unconditional early-exit scan loses to one whole-array compare

`halfka_delta` compared two 64-square boards eight `[Piece; 8]` words at a time, opening only
the words that differed. That reads as the careful version — and the scan ran on **every**
call regardless of how few squares a move touched, twice per accumulator update.

One `Simd<u8, 64>` compare, `simd_ne().to_bitmask()`, then a `trailing_zeros` /
`differs &= differs - 1` walk over the two to four set bits: **−47.2M**, the largest single
win in this file.

The objection that killed the idea for a long time was that `was.map(Piece::raw)` pays a
64-byte copy to build the vector. **It does not.** `map` over a transparent newtype folds
into the two 32-byte loads the compare already needs; LLVM never materialises the copy. The
lesson is narrower than "use SIMD": *an early-exit scan only pays when the exit is actually
taken early, and a fixed-size compare has no loop to exit.*

Tier caveat, stated because it is untested: `to_bitmask()` on a 64-lane mask lowers to two
`vpmovmskb` and a shift-or at avx2. A tier with no 32-byte compare may lower it to four
16-byte compares.

### 15.3 Write both destinations while the value is still in registers

`refresh` folded into the ply slot and then `clone_from`'d the result into the cache slot — a
second full read and write of the 2 KiB row it had just written. Mirroring each tile to both
destinations while it is live (`fold_mirror`) makes every entry written twice and read back
never: **−19.6M**.

The shape to look for is a compute-then-copy pair where the copy's source was produced
locally. The copy is only free if the compiler can prove the value is still live, and across
a 2 KiB buffer it cannot.

### 15.4 Carry state in place; a swap costs a copy a push does not

Two commits, and the second is the interesting half. Holding the current `StateInfo` as a
FIELD instead of at the end of a `Vec` removed `slice::last` from every state access
(**−12.8M**) — but building a local and `mem::replace`-ing it in made a do/undo pair pay two
~200-byte copies where the old `push`/`pop` paid one, because `pop()`'s discarded result had
compiled to a length decrement. Pushing the old state and then mutating the field **in
place** recovers it (**−5.8M**).

Two notes worth keeping. The borrow checker does not fight the in-place form provided each
field is written as its own statement, so no `&mut` outlives a single statement — §1's trap.
And carrying in place means the recomputed group is no longer zeroed, so **every** field must
be written before return; that list belongs in a comment at the push, not in a reviewer's
head.

### 15.5 A store a later assignment overwrites is a store

`MoveBuf::push_move` wrote `score: 0` alongside the move, and all three scoring passes
*assign* over exactly the appended range rather than accumulating into it. The store was
dead. Same wave: `partial_insertion_sort` specialised for the `i32::MIN` limit its four
unconditional callers pass, where `sorted_end` tracks `p` exactly and the limit compare and a
self-assignment both vanish. **−1.5M** together.

### 15.6 What the fleet got WRONG, and it was my premise

Two lanes were chartered on the claim that the siblings fuse move scoring into generation and
rfish's separate `generate_append` was therefore the picker-zone gap. **That is false.**
mcfish's `score_list()` builds the whole list and then walks it to fill `out[i].value` —
exactly rfish's shape. It only *looks* fused because it is one static function, and callgrind
folds it into `nextMove`'s attribution.

The general trap, and it has now bitten twice in this file: **function-level cost comparisons
across differently-inlined binaries are void by construction** (§10 already says this for
attribution; it applies to inlining boundaries too). rfish's `next_move` at 123.5M "beating"
zfish's `nextMove` at 127.7M was never like-for-like either — zfish's number already contained
its generation. **Compare zone totals, or compare call counts and derive a per-call cost.**
Call counts are the reliable instrument: `next_move` is called 380 931 times in all three
ports and `see_ge` 202 943, so once the counts match, a cost difference is a per-call
difference and nothing else.

### 15.7 Running a fleet on this repo

Four traps cost the first wave most of its budget. All four are environmental, none is in any
doc, and every one produces a plausible number rather than an error:

- **A worktree has no net.** `resources/` is gitignored, so a worktree checkout has no
  `.nnue`. Callgrind runs and `cargo xtask signature` then silently use the CLASSICAL
  evaluation — signature reads 3454359, not 2508687. Two lanes read that as their own patch
  breaking bit-exactness. Fix before anything else:
  `mkdir -p resources && ln -sf <main>/resources/nn-*.nnue resources/`
- **`cargo xtask signature` rebuilds `target/release/stockfish` at the DEFAULT arch**, wiping
  an `--arch avx2` binary. Copy the binary out before running any gate.
- **`cargo xtask nnue-check` and the `tb` gate need `../Stockfish`**, resolved from the
  workspace root, which does not exist from inside a worktree. They SKIP with exit 2. A
  skipped gate proves nothing; run them at integration.
- **A worktree starts where its branch last was, not at the integrator's HEAD.** Three of ten
  lanes arrived on a stale commit. Every lane must `git log --oneline -1`, reset, and
  re-verify before building.

What worked: disjoint FILE charters (ten patches, zero merge conflicts, and the five in one
wave composed to within 0.1M of the sum of their individual measurements); handing each lane
the falsified list for its own file; and requiring a measured number for every rejected
variant, which is where three of the results above came from.
