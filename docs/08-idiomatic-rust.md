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

**rfish.** `Position` owns the CURRENT state as a field, `st: StateInfo`, and every earlier
one in `prev: Vec<StateInfo>`, oldest first. `do_move` pushes the old state and updates `st`
in place; `undo_move` pops back into it; the repetition walk is a backwards index scan over
`prev`.

**Why the split, and where the boundary falls.** `prev.len()` is exactly the current state's
old index, so every backwards walk lands wholly inside `prev` and no caller needs a "which
half am I in" test. That is what makes the current state a field offset rather than a
`states.last()` — worth **12.8M instructions**, with the in-place update worth another 5.8M
on top. §15.4 has the measurement and the two traps that come with it.

**What it costs.** The `Vec` reallocates as the search deepens, once, and then never again —
`MAX_PLY` is 246 and the vector reaches that capacity in the first deep iteration.

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
| Upstream's **dual hyperbola quintessence**, its `USE_AVX2` slider path, replacing the magic bitboards | **BUILT, bit-exact at both tiers, and falsified on both axes.** +73.7M instructions and +2.05M I1 misses against a D1 read-miss saving of **805,579** — the cache win is real and roughly an order of magnitude too small to pay for the instructions. A magic lookup is a multiply, a shift and one indexed load; the four-lane HQ kernel is fifteen-odd vector ops. See §16.4. |
| Two independent accumulator chains in the sparse affine walk, mirroring mcfish's `AFFINE_CHAINS = 2` | **Falsified ON THE INSTRUCTION AXIS, at +3.08M — and that is the wrong axis for it.** Chains hide dependency latency; Ir counts retired instructions, so they can only add. Not re-attemptable until this repo has a trustworthy cycle harness. See §10. |
| Narrowing the pawn-pair `changed` set to zfish's per-board `after & !before` / `before & !after` masks | **Falsified, with a measurement**, at +41K. It is CORRECT — the signature confirms rfish's extra pairs cancel in the fold, which was worth establishing — but on the common pawn move the symmetric difference is already just `{from, to}`, so it saves a handful of emitted pairs and pays four extra `andnot`s on every call. |
| Upstream's hybrid same-half king-move accumulator path | **Not attempted, for a stated reason.** `halfka_delta` takes ONE king square and assumes base and target share it, so a hybrid path needs a new function re-indexing every piece from the old square to the new — roughly 64 rows. In upstream and zfish that buys skipping a full refresh; here rfish's refresh already starts from a per-king-square cache entry whose halfka diff is usually small, so the hybrid would ADD halfka work. Measure before assuming. |
| Fusing move scoring into generation, because "the siblings have no `generate_append`" | **The PREMISE was false.** mcfish's `score_list()` calls `generate_captures`/`generate_quiets`/`generate_evasions` to build the whole list and then walks it to fill `out[i].value` — exactly rfish's shape. It only looks fused because it is one static function that callgrind folds into `nextMove`. The picker-zone gap is inside the generation loops, not in the pass structure. See §15.6. |
| Making `Bitboard::iter` borrow instead of copy | **Rejected by design.** Iterating by value is what lets a loop mutate the board it came from, which is upstream's `while (b) pop_lsb(b)` over a local with the local made explicit. |
| Rewriting the feature transformer's fold and transform in `std::simd` | **Not worth attempting, from the disassembly.** Both already emit upstream's kernel shape — `vpaddw`/`vpsubw`/`vpmovsxbw` in the fold, `vpminsw`/`vpmullw`/`vpsrlw`/`vpackuswb` in the transform — over 136 and 156 `%ymm` operands. Explicit vectors would transcribe what LLVM emits. See §12 — and §17.1, where the ADDRESSING around those same loops was worth 11.6M without touching a kernel. |
| Upstream's per-move accumulator delta | **Falsified, with a measurement, after being BUILT.** Bit-exact — the anchor and `nnue-check` both held — and it loses: recording costs 85.7M on every `do_move` and the fast path fires on **11%** of evaluations, so it saves 0.47M. The board-zone half is kept and tested; see `docs/03-engine-eval.md`. |
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
  evaluation — signature reads 3454359 rather than the anchor. Two lanes read that as their own patch
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

---

## 16. What the sibling ports learned, and which of it crosses into Rust

`../mcfish/docs/08-idiomatic-c.md` and `../zfish/docs/08-idiomatic-zig.md` are the same
document as this one, written for C23 and for Zig. Reading both is worth an afternoon,
because the three ports hit different walls: **the walls are a property of the toolchain,
not of the engine.** What follows is only the part that crosses into Rust — with the ones
that do NOT cross marked, because assuming a sibling's lever applies here is how two of this
file's falsified entries were generated.

### 16.1 The autovectorisation split, and why it decides which port to copy

zfish's headline rule is *"vectorize integer hot loops by hand — the toolchain will not."*
Its NNUE carried a persistent deficit for exactly that reason, and closing it meant
hand-writing a vector form of every `u8 x i8 -> i32` dot.

mcfish's headline rule is the OPPOSITE, and it says why: *"clang auto-vectorizes integer hot
loops — so hand-write vectors for a reason."* It verified the dot product lowers to
`pmaddwd` at `-O3` and warns in as many words not to port zfish's per-loop vectorisation
slices on the assumption the compiler needs help.

**rfish is LLVM, so mcfish is right and zfish is inapplicable** — and this file has the
receipts on both sides: an explicit `std::simd` fold cost **+177M** (§11), while §15.1, §15.2
and §17.1 won by changing what the scalar loops were *fed*, not by vectorising them. The
practical rule: **when the two siblings disagree, copy mcfish.** It shares rfish's backend.

That rule has a positive receipt as well as the two negatives below: mcfish's
`ThreatIndexBlocks` colocation translated directly, at −7.7M (§17.2). Its recipes travel; its
verdicts on upstream's ISA-gated paths do not, because those are claims about FIDELITY (§16.4).

The one place zfish still transfers is where the exact lowering is load-bearing rather than
merely fast — mcfish makes the same carve-out for its `pmaddubsw` kernel, whose `i16`
intermediate SATURATES where the plain sum does not.

### 16.2 Re-take a falsified knob only when you can name what changed

zfish's AVX2 accumulator tile was measured and **rejected twice** before landing on the third
look, deliberately re-taken after other work had altered the register and traffic context the
earlier verdicts were measured in. Its rule: *"'try it again' is not a method; 'the context
that falsified it no longer holds' is."*

Applied here, and it is why `TILE`'s comment now carries two sweeps. The 32/64/128/256 sweep
behind `TILE = 128` was taken at **nehalem**, and the fold has since been rewritten twice.
Re-swept at avx2: 64/128/256 → **1,947M / 1,751M / 1,768M**. 128 is the peak on both tiers,
so the knob held — a negative result, and worth the two measurements it cost, because the
comment previously asserted the value might not travel and now says which tiers it was
checked on.

The same rule already produced a POSITIVE result in §14.2: `MoveBuf` as a boxed array
measured neutral (+28K), was reverted, and measured −2.4M later once the push loop was
inlined into a caller that could hold the count in a register.

### 16.3 The instruction axis versus the latency axis, settled across two ports

zfish tunes its affine chain count per ISA and measured **−16.7M at AVX2** for two chains.
A lane here measured the same change at **+3.08M** and rejected it. Both are correct: zfish
quotes cycles, this repo quotes instructions, and chains hide latency while adding
instructions (§10). This is the clearest example in either file of two ports appearing to
contradict each other while measuring different quantities — check the axis before believing
a cross-port comparison.

zfish's constraint on the shape is worth keeping if the chains are ever revisited on a cycle
harness: **the chain index must be a compile-time constant.** With a runtime counter the
accumulator array needs an address, spills, and round-trips per group, which costs more than
the chains win.

### 16.4 Ported logic is not ported code: upstream's paths are ISA-GATED

mcfish's largest single divergence, and it hid behind every behavioural gate for the port's
whole life: **upstream has one implementation per ISA tier selected by `#ifdef`, and a port
that transcribes only the portable path silently ships upstream's oldest algorithm at every
tier.** A different algorithm producing the same attack set produces the same tree, so no
signature or perft catches it.

Two of the instances it lists are NNUE — the `packus` transform body (`__AVX512BW__`) and
the NNZ index list that upstream never builds as a bitset at `USE_AVX512` — and both are
above rfish's avx2 measurement tier, so neither moves the numbers in this file. The third,
**dual hyperbola quintessence gated `__AVX2__`, replacing magic bitboards with 3 KiB of
L1-resident structs**, is at a tier rfish does build and is board-side rather than NNUE.
None of the three is ported here. **This is an unexplored class in rfish, not a closed one**,
and mcfish's method for finding them is a grep over upstream for its own gates rather than a
reading of the portable path.

The caveat mcfish attaches is load-bearing: **a divergence from upstream is a strong PRIOR,
not a proof.** It ported upstream's vectorised move splats in full, bit-exact and fully
gated, and measured them slower on three runs before reverting.

**That caveat is now this port's result too, on the first member of the class it tested.**
Dual hyperbola quintessence was built here in full — safe `std::simd` throughout, since every
intrinsic upstream uses has a portable spelling (`_mm256_sub_epi64` is `-`, and
`_mm256_shuffle_epi8` against a descending index vector is `swap_bytes` plus a two-lane
swizzle) — gated on `cfg(target_feature = "avx2")` exactly as upstream gates on `USE_AVX2`.

It is **bit-exact at both tiers**, which is the part worth keeping: the whole unit suite
passes under `-C target-cpu=haswell`, and an avx2 build's `bench 16 1 13` reproduces the
anchor, so the vector algorithm and the magic tables agree on every position in it. Building
it exposed a real hole — **`cargo xtask signature` builds at the DEFAULT arch, so it tests
the PORTABLE arm and cannot see an ISA-gated path at all** — and that hole now has a gate:
`cargo xtask arch-determinism` holds every enumerated tier to the anchor, so a future member
of this class is checked by a command rather than by hand.

And it loses:

| | magic | dual HQ | |
|---|---|---|---|
| Ir | 2,968,249,767 | 3,041,958,575 | **+73.7M** |
| D1mr | 76,787,392 | 75,981,813 | −805,579 |
| I1mr | 12,754,982 | 14,805,094 | **+2.05M** |
| DLmr | 6,335,821 | 6,402,239 | +66,418 |

The D1 saving is real — 841 KiB of randomly indexed tables really does become 64 structs of
48 bytes — and it is roughly an order of magnitude too small to pay for the instructions. A
magic lookup is a multiply, a shift and one indexed load; the four-lane kernel is fifteen-odd
vector operations plus a rank-table load.

**Note what mcfish does and does not claim.** This path appears in its ISA-gating table,
which is about FIDELITY, and NOT in its table of spellings that measured. Reading a measured
win into it was my inference, and it was wrong. The class stays open — its two NNUE members
sit above avx2 and are untested here — but its prior should be read as "upstream chose this
for a machine and a cost model; check that both still apply", not "upstream has it, so we are
missing a win".

### 16.5 Levers that do not cross, and why

- **`_Atomic` de-vectorising a bulk fill** was mcfish's single largest gap: its shared history
  tables filled ~4M entries one atomic store at a time, 183M instructions against upstream's
  67M, fixed to 14M by writing through a plain view during the provably exclusive phase.

  **Checked here properly, because the first version of this entry asserted immunity instead
  of measuring it.** The history half of the claim holds: rfish's tables are per-worker plain
  `i16` with `fill` taking `&mut self`, so there are no atomics to de-vectorise. But rfish
  DOES have a bulk fill over atomics — `TranspositionTable::clear` stores zero into every
  `AtomicU64` of every cluster, and it runs on `ucinewgame`, on `Clear Hash` and at the start
  of every bench. That is the same shape, so it was worth a measurement rather than an
  argument.

  One clear at `Hash 256`, isolated by differencing a `ucinewgame` session against one
  without it:

  | | Ir |
  |---|---|
  | the existing `store(0, Relaxed)` per word | **38,410,812** |
  | rewritten through `get_mut()` as plain `u64` | **270,146,121** |

  The "fix" is **seven times worse**, and the existing code is already near optimal: 33.5M
  words at **1.1 instructions per store** is an unrolled store loop, not the per-element
  scalar disaster mcfish found. (Whole-binary the rewrite also cost +14.5M on the bench, an
  inlining flip of the kind mcfish warns about near the transposition table — but the local
  number above is the one that settles it.)

  **The reason the analogue does not carry is the WIDTH, and it is worth stating because it
  bounds the whole class.** mcfish's tables are `_Atomic int16_t`. Its win came from merging
  many NARROW atomic stores into one wide vector store, which clang will not do through the
  atomic API. rfish's are `AtomicU64` — already the machine's store width, so there is
  nothing to merge and no vectorisation being suppressed. **Look for this bug only where the
  atomic element is narrower than a word.**
- **Returning large hot-path structs by value**, which cost zfish a per-node `memcpy` and cut
  its bench's memcpy share from 3.4% to 0.8% when fixed. rfish's whole libc/runtime zone is
  **21.4M against zfish's 91.1M and mcfish's 52.3M** — the best of the three — so there is
  nothing here to recover. Rust's move semantics and LLVM's RVO already do it.
- **Leaving a fully-written buffer `undefined`.** Not available: safe Rust must initialise.
  §9 and §14.2 record the shape that replaces it — initialise once per worker, reuse forever.

---

## 17. Four more shapes: the addressing, not the arithmetic

None of these changes an arithmetic operation. The loops retire the same products and sums in
the same order, and what moves is how the operands are REACHED — which is where this zone's
wins have been since the kernels themselves reached upstream's instruction shape (§12).
Measured as §14 and §15 are: avx2 tier, matched net, an identical 163 081-node `bench 16 1 8`,
startup subtracted, signature unchanged.

### 17.1 Index a fixed-width row; do not range-slice and zip it

**A range slice walked by an iterator costs twice over.** The slice tests a bound at BOTH
ends and cannot be hoisted out of the loop that produces `base`; the iterator pair then costs
a step and a pointer compare per element. Priced on the accumulator folds, where each feature
took `weights[base..base + WIDTH]` and zipped it against the tile: **94.5M instructions of
`core::slice::iter` and `core::ptr::non_null` inside `transform`**, and 13.7M of `fold_psqt`'s
29.1M — on a kernel whose whole body adds eight `i32`s.

Both folds now view their tables as fixed-width ROWS once per call through `as_chunks`, so a
feature's row is an index rather than a slice, and walk two `[T; WIDTH]` arrays by `0..WIDTH`,
which needs no check at either end. `L1` is a whole number of tiles and the PSQT head is
exactly one row, so both tables divide exactly — check that before reaching for this.

| | Ir |
|---|---|
| `fold_into`, all four accumulator loops | **−7.5M** |
| `fold_psqt`, with the head accumulated in a local | **−4.1M** |

Two rules generalise out of it:

- **Take the row view once per CALL, not once per iteration.** §12 already records
  `chunks_exact(N)` beating `w[i * N..i * N + N]` by 20M; this is the same lever pushed one
  step further, and the iterator that survived that fix was worth 11.6M more.
- **Accumulate into a LOCAL, not through the caller's `&mut`.** The compiler must assume the
  weight loads alias the output and reloads it every iteration — §12's `&mut [T]` entry, seen
  from the loop's end rather than the vectoriser's. The same rule is why `Aligned` is coerced
  to a slice above a loop rather than indexed through inside it.

Read the pair against §11's `std::simd` row: an explicit vector rewrite of this exact tile
costs **+177M**. rustc already autovectorises these loops into the kernel upstream writes by
hand. **Before hand-vectorising a kernel, check what the kernel is being FED.**

### 17.2 Colocate what one lookup reads behind one base

**A lookup that combines two tables under one key is one table.** A threat index adds a class
base to a slot number; held in separately based tables it computes two bases and touches two
allocations. `ThreatBlock` carries both for one attacker, so the lookup is one base and two
loads: **−7.7M**.

Both siblings found half of this. Sum the entries at build time where they are constants of
each other — `slot` is the pre-summed `Offsets[from] + IndexLut2[from][to]`, after ../zfish
662d82ef — and colocate them behind one base where they are not, which is ../mcfish's
`ThreatIndexBlocks` and `nnue_full_make_index`, measured there at −1.16%. Taking the second
half after the first is why this reads smaller here than there.

The block is `#[repr(C, align(64))]` and a `const` assertion holds its size to a whole number
of cache lines, because **a stride that is not a multiple of the line carries the alignment to
the first attacker's rows and no further** — a silent half-fix. mcfish's own `static_assert`
makes the same claim for the same reason.

### 17.3 Walk what CHANGED, not the container

**Where a move touches one or two elements, iterate the changed set and reject nothing.**
`pawn_pairs_touching` walks `pawns & changed` and draws each partner from
`PAWN_PAIR_BB[from] & (unchanged | bb)`, which is ../zfish's `ppGenerate`; scanning all
sixteen pawns and testing each against `changed` rejects fifteen of them per call.

Emission ORDER changes under this, and that is safe **here and only here**: the feature lists
are deliberately unsorted and membership-tested. A move list is the opposite — generation
order decides the tree, and `AGENTS.md` names it as one of the four bugs perft cannot see.
The correctness argument for the walk is that each unordered pair is still emitted exactly
once, because a partner comes from the unchanged pawns plus the changed ones not yet popped,
so a pair whose two pawns both moved is seen only from whichever pops first, and the index is
symmetric in its two pawns.

Three changes were measured together at **−15.7M** and are not individually attributed:

- the walk above;
- `PAWN_PAIR_BB` as a `const` rather than a `LazyLock`, because it is read once per pawn
  inside that walk and a lazy static pays a `Once` load and a branch per deref (§5). `THREATS`
  cannot follow — its build needs the magic tables — so its deref is hoisted instead;
- the orientation and the perspective bit hoisted out of the record loops, invariant for a
  whole list (§14.4).

### 17.4 A generic sink blocks every hoist across a push

**A generic `&mut S` sink may alias the position, so LLVM hoists NOTHING across a push.** No
board accessor called inside a generation loop is common-subexpression-eliminated, however
obviously loop-invariant it reads — and `occupied()` in the per-attacker slider loop is the
generator's innermost read. The generator therefore takes the occupancy, own, enemy and
checker sets once at the top and threads them down, at **−0.9M**.
`clippy::too_many_arguments` is suppressed rather than restructured; a struct of the four
sets would read better and has not been measured.

**A comparable rewrite of the activations was worth about the same, and the smallness is the
finding.** Narrowing `clipped_relu` and `sqr_clipped_relu` sixteen `i32` at a step — two AVX2
registers in, one byte store out — is **−0.9M**, because LLVM was already vectorising both
competently. The `i64` square is not the defect it looks like: squaring in `i32` after
saturating into `i16` agrees for every input this network produces, and the reason is the CAP
rather than the arithmetic (an operand outside `i16` squares to at least 2^30, which is above
127 after any shift a caller passes). Where a kernel already vectorises, expect a number this
size and budget the session accordingly.

## 18. The data structure, not the loop: six shapes and the three that inverted

§17 moved how operands are REACHED. This section moves what they are STORED IN, which is the
next layer down and the one where the falsified attempts outnumber the wins. Every row is the
same instrument as §14, §15 and §17: avx2 tier, matched net, an identical 163 081-node
`bench 16 1 8`, startup subtracted, signature unchanged at every step.

**Read the three that measured WORSE first.** Each is the obvious application of a rule that
had just paid somewhere else, which is exactly why they are here.

### 18.1 Make the bound a CONSTANT the index cannot reach

**A runtime-length slice indexed by a composite expression pays a length load, a compare and
a scaled address on every visit.** Priced on the sparse affine layer, which reached its
weight row as `blocks[c * SCAN + lane]`:

| line | Ir |
|---|---|
| `Simd::from_array(blocks[i])` | 66,863,251 |
| `let i = c * SCAN + lane` | 19,103,786 |
| `acc += w.cast::<i32>() * splat(x)` — the arithmetic | **9,551,893** |

**Nine times the multiply-accumulate's cost to address it.** The fix is not to remove the
check but to make it FOLD: chunk the weight rows the way the inputs already are, so the row
view is `&[[i8; N]; SCAN]` and the index is `lane` alone — and `lane` came out of
`trailing_zeros()` on a `u64`, so LLVM knows it is below 64 and `SCAN` is 64. The test
disappears and the row is one displacement off a base the loop already holds. **−34.6M**, and
the loop is then twenty instructions of which every one is arithmetic.

**The condition is narrow, and the counter-case cost 9.3M.** The same reshape applied to the
accumulator fold — `tp_rows[index].as_chunks::<TILE>().0[t]` in place of
`tp_rows[index * ROWS_PER_FEATURE + t]` — is **+9.3M WORSE**. The difference is that a
feature `index` is data-dependent and unbounded by anything the compiler can see, so no level
of the nest becomes a constant; the second level buys nothing and costs a second address
computation. **The lever is a compile-time bound the index provably cannot reach, not
nesting.** If the outer index stays data-dependent, leave the composite alone.

**It has now paid in THREE zones, and the enabling condition is the same in each.** The
index has to be provably below a bound the compiler can see — and the proof is usually
already in the source, one line above:

| zone | the index | what made the bound provable | worth |
|---|---|---|---|
| `layers.rs`, the sparse affine layer | `blocks[c * SCAN + lane]` | chunk the rows as the inputs are chunked; `lane` comes from `trailing_zeros()` on a `u64` and `SCAN` is 64 | 34.6M |
| `common.rs`, the LEB128 decode | `out[i]`, with `if i == out.len()` per BYTE | walk `out.iter_mut()` and pull bytes from an iterator | part of 124.3M |
| `attacks.rs`, the magic search | `table[offset + idx]`, `epoch[idx]` | slice both to `size` once per square — the loop ALREADY tests `idx >= size` | 93.5M |

The third is the clearest statement of the rule. The guard was there the whole time:

```rust
if idx >= size { continue 'search; }     // the proof
...
table[offset + idx] = references[i];     // against table.len(), which is not size
```

**A guard proves nothing about a slice it is not stated against.** Reslicing to exactly the
length the guard names is what turns a runtime test into a compile-time one, and it is free.

### 18.2 `Box<[T; N]>` is not automatically better than `Vec<T>`

The search stack moved from `Vec<T>` to `Box<[T; N]>` for **−16.0M** (§14.2) and the ply stack
followed it. Applying the same to the accumulator — `Side::acc` as `Box<[i16; L1]>`, which
gives the fold's tile loop a trip count of eight at compile time, removes a bounds test per
tile and deletes a `resize` that restates the type — is **+6.1M WORSE**.

**What separates them is whether the loop body was already register-resident.** §14.2's win is
a length LOAD removed from a scalar walk. The fold's tile loop is eight iterations of
register-held vector work; making the count constant lets LLVM unroll it fully, and the code
size costs more than the eight compares saved. Instruction-cache pressure is the counter this
port is already worst on (`docs/03-engine-eval.md`), so an unroll has to be measured against
it rather than assumed free.

### 18.3 Store the halves your consumers need separately

**A sum forecloses every path that needs one of its terms.** The accumulator refresh cache
held one whole accumulator per king square — the biases, the king-piece rows and the threat
rows, added together — plus the feature set that produced it. That is everything the refresh
path needs and it made the cheaper path impossible to write: a king move can be taken as a
delta from `HalfKA(new) + parent − HalfKA(old)`, and neither `HalfKA` term can be recovered
from a slot that only kept the sum.

A SECOND cache holding the king-piece half alone — upstream's `AccumulatorCaches::Cache`,
which is exactly this and not the other one — makes the delta expressible: **−22.9M**, and
what it buys is not the arithmetic but that the delta path builds no feature set at all where
a refresh had to materialise one and diff it.

The cost side is worth stating because it is what such a decision usually trades: 64 slots per
perspective at 2 KiB each, ~256 KiB per worker, against ~15.6 MiB a worker already carries.
**When one consumer wants `a + b` and another wants `a`, storing `a + b` alone is a decision,
not a default.** Ask which terms a future path could need before summing at rest.

### 18.4 Two structures that both walk back can need OPPOSITE caps

`HOP_CAP` bounds the accumulator roll-forward and sits at **12**; `HYBRID_HOP_CAP` bounds the
king-move delta and sits at **3**. Sharing one constant costs **38M**, and the reason is a
property of what each walk LEAVES BEHIND, not of the chain length:

| | a longer chain |
|---|---|
| roll-forward — materialises every ply it steps through | leaves work done; the next evaluation starts one hop away |
| king-move delta — materialises only its last ply | is pure cost, and drags two cache entries to a further board on every call, which thrashes them |

The second row's curve is the readable one: 1,566M / 1,560M / **1,558M** / 1,559M / 1,563M /
1,569M / 1,583M / 1,596M at caps 1 through 12. **Sweep a cap per PATH.** A constant named for
the structure rather than for the traversal invites exactly this.

### 18.5 A walk-back that writes only its last step is one you will walk again

**The largest single row of the campaign, and it reads as a loop shape rather than a data
one.** The roll-forward concatenated every hop's records into one fold and wrote only the
destination ply, so a chain walked once was walked again by the next evaluation — and because
records do not cancel before they are applied, a longer chain folded MORE rows, which is why
its cap had been pinned at two. That cap then caused 92% of all accumulator refreshes.

Materialising every ply on the way forward, as upstream's `AccumulatorStack::evaluate` does,
inverts the cap's whole curve and is **−113.0M**. Instrumented, the walk was never blocked on
a broken chain: of 59,054 failures the cause was the cap 54,527 times and a hop with no
records **zero** times.

**Measure the cap AFTER changing what a hop costs, never before** — and when a structure is
written once per traversal rather than once per element, ask what the next traversal will
find there.

### 18.6 Fill only what a consumer will read

`active_sets` fills both perspectives' feature sets, and the comment defending it is about the
SCAN: which square attacks which is a fact about the position, so scanning twice recomputes
every attack set twice. True, and it stays. What is not shared is the INDEX — every feature is
numbered against its own perspective's king square — and a set nobody reads costs exactly what
one that will costs.

That did not matter while the hop cap caused most refreshes, because the cap hit both
perspectives at once; it started mattering the moment a king move became the dominant cause,
because a king move invalidates ONE side. Measured: **15,453 of 19,882 calls needed one
side**, 78%. Passing which perspectives are wanted is **−10.9M**.

**A shared producer is not a reason to produce both outputs.** Split the shared part from the
per-consumer part and gate the second.

### 18.7 A small output width is what defeats the vectoriser

Two kernels emitted no vector instructions at all, both found by `objdump` rather than by
reading, and neither is large enough to appear in a profile's symbol list:

- **`fold_psqt` accumulates eight `i32`** — one AVX2 register exactly — and held 33 `mov`s and
  not one vector instruction. LLVM will not turn an eight-lane integer loop into a `vpaddd` on
  its own. Written as `Simd<i32, 8>`: **−3.3M**.
- **`fc_2` is 128 → 1**, so a generic `propagate::<N>` instantiated at `Simd<i32, 1>`. LLVM
  widened it to `xmm` and then put a HORIZONTAL REDUCTION inside the loop — `vpshufd`/`vpaddd`
  twice per iteration, because the accumulator in the source is a scalar — for twelve
  instructions per eight inputs on a serialised chain. A dedicated dot carrying sixteen `i32`
  lanes and reducing once: **−6.0M**.

Both siblings record the same two traps on the same two kernels
(`nnue_acc_apply_psqt_delta`, `nnue_affine1_dot`). **Being small is what stops the vectoriser
caring** — disassemble the narrow kernels, not the wide ones.

### 18.8 Measure the constant the argument rests on

The sparse layer's doc comment and `docs/03-engine-eval.md` both said "roughly 40% of the
inputs non-zero", and the entire case against a group-of-four kernel rests on it. Counted with
an instrumented build: **9,551,893 non-zero inputs over 62,975 evaluations — 151.7 of 1024,
14.8%.** Nearly three times too high, and never measured.

The conclusion survived (a group of four survives the zero test 47% of the time rather than
87%, which still kills it), but nobody could have known that without counting. **A figure that
decides a design and was never produced by a command is a figure to re-derive before you
trust the design.** It costs one `AtomicU64` and one bench run.

### 18.9 Decode into the width it lands in, not through a wider temporary

`leb128_i16` decoded into a `vec![0i32; out.len()]` and narrowed afterwards. On the NNUE main
weight block that temporary is 23,068,672 entries — **92 MiB allocated and page-faulted in to
be read once and dropped**, and it was most of what made a `quit`-only run peak at 253 MiB.

The encoding is defined over `i32`, so the decode has to compute in `i32`; nothing requires it
to STORE in `i32`. A private trait with one method per destination width gives both blocks one
decode and no temporary. Read with §18.1's row: the same commit moved the length test off the
per-byte path and removed the store's bounds test, and the three together are **124.3M of
startup and 63 MiB of peak RSS**.

**Ask what the intermediate width is for.** If it exists only because the algorithm's
arithmetic is wider than its output, it belongs in a register, not in a buffer the size of the
output.

### 18.10 A transient allocation is not automatically a peak-RSS problem

The same file's `i8s`, `i16s` and `i32s` each allocate a `Vec` the size of the whole block and
convert out of it — **61 MiB for the threat weights alone**. Converting out of a fixed 8 KiB
buffer instead measured **+5.3M instructions and moved peak RSS not at all.**

That 61 MiB is allocated and FREED before the peak is reached, so it was never what set the
peak; the LEB128 path was. **Check WHEN an allocation lives, not just how big it is** — and
measure peak RSS directly (`/usr/bin/time -f "%M"`) rather than inferring it from the largest
number in the source. Not kept.

### 18.11 Startup is an axis the instruction budget SUBTRACTS

`cargo xtask perf-budget` subtracts a `quit`-only profile, which is exactly the net load and
the table build. **Nothing in §14–18 above could see either**, and together they were 1,281M
instructions against a 1,524M search — a `bench` spent nearly half its instructions before it
searched a node.

Measure that axis with the profile the budget subtracts:

```sh
cd resources && echo quit | valgrind --tool=callgrind --callgrind-out-file=/dev/null \
    --cache-sim=no ../target/release/stockfish 2>&1 | grep "I *refs"
/usr/bin/time -f "%e s  %M KB peak" sh -c 'echo quit | ./stockfish > /dev/null'
```

Two blocks own it, and both turned out to be §18.1's defect: the LEB128 decode and the magic
search. **A gate that subtracts a cost is a gate that hides it.** State what each instrument
excludes before trusting a zero from it.

### 18.12 Outline what runs once per STAGE out of what runs once per MOVE

`MovePicker::next_move` is called **1,268,056 times** on a `bench 16 1 8`. The three `*Init`
arms of its stage machine run **once per picker**. Inlined into it, the generator, the scoring
loop and both insertion sorts sat inside the body every one of those calls entered, and the
bill was frame size rather than work: a 30-instruction prologue, a 21-instruction dispatch and
an epilogue, on a call that usually only advances a cursor and returns.

`#[inline(never)]` on the three init bodies:

| | before | after | delta |
|---|---:|---:|---:|
| avx2 | 1,523,668,306 | 1,511,299,226 | **−12.37M, −0.81%** |
| sse41 | 2,435,849,501 | 2,423,676,659 | **−12.17M, −0.50%** |

Bit-exact — this is code motion, not a reformulation — and `arch-determinism` benched the
anchor on all five tiers.

**The shape: a cold body inlined into a hot caller is not free even when it never runs.** It
costs the caller its register allocation and its frame on every call, and neither shows up as
a line in the profile — the cost lands on the caller's prologue, which reads as overhead
nobody wrote. Look for it wherever a state machine's setup arm and its steady-state arm live
in one function.

**And the counter-rule from the same session: `#[inline(never)]` is a claim about CALL
FREQUENCY, not about size.** [§18.2](#182-boxtn-is-not-automatically-better-than-vect) is the
mirror image, where forcing a shape on a loop that was already register-resident cost 6.1M.
The test is whether the outlined body runs on a different schedule from its caller, and here
it is once per stage against once per move.

### 18.13 Software prefetch is out of reach, and a discarded load is not a substitute

Upstream issues `prefetch(tt.first_entry(posKey))` inside `do_move`, so the child's cluster is
resident by the time it probes (`search.cpp:642`); `../mcfish` restored the same placement as a
fidelity fix. **rfish cannot emit that instruction.** Every `std::arch` intrinsic is an
`unsafe fn`, and so is every crate that wraps one — the same wall as `mmap` in
[§18.11](#1811-startup-is-an-axis-the-instruction-budget-subtracts).

The obvious surrogate is safe: read one word of the cluster and throw it away through
`black_box`. A `Cluster` is 32 bytes at `repr(align(32))`, so it never straddles a line and one
word brings the whole thing in. It was built, wired at upstream's own point in the make, and
measured. It is bit-exact: `cargo xtask signature` passed with it in.

| axis | reading |
|---|---|
| instructions, avx2 | 1,511,299,226 → 1,512,637,391, **+1.34M** |
| instructions, sse41 | 2,423,676,659 → 2,425,270,734, **+1.59M** |
| paired clock, `bench 256 1 13`, 15 warm interleaved rounds | median **0.9992**, spread 0.9171..1.0397, 9 of 15 favouring |

**A certain cost and no measurable return, so it is not in the tree.**

Two things to carry forward rather than re-derive:

- **A load is not a prefetch, and the difference is retirement.** A `prefetcht0` retires
  immediately; a load cannot retire until its data arrives, so on the DRAM miss this was meant
  to hide it can block the reorder buffer instead of hiding anything. The construct that looks
  equivalent in the source is the opposite of equivalent in the pipeline.
- **The first run of a paired A/B on this box is worth nothing, and so are the next two.** The
  same pair read median 0.9700 with the spread 0.7010..2.4915 before a warm-up, and 0.9992 with
  a tenth of the spread after — with the absolute times drifting 2868 ms to 2356 ms ACROSS the
  warm run as the machine settled. A three-per-cent "win" is what a warming box looks like.
  Discard until the absolutes stop falling, then alternate.

### 18.14 Upstream's structure is a strong prior, not a proof

`generate_into` takes the generation kind at RUNTIME. Upstream instantiates
`generate<Type>` and `generate_all<Us, Type>`, so each of its call sites compiles only the
masks and pawn blocks that kind needs, and every `match gt` here is a branch upstream does not
execute. That reads like a defect with upstream's own fix attached.

`#[inline]` on `generate_into` and `generate_pawn_moves` is the whole change — the picker's
three callers pass a literal and are themselves `#[inline(never)]`
([§18.12](#1812-outline-what-runs-once-per-stage-out-of-what-runs-once-per-move)), so each gets
a folded copy without growing the once-per-move path. Bit-exact.

| | before | after | delta |
|---|---:|---:|---:|
| avx2 | 1,511,299,226 | 1,511,413,430 | +0.11M |
| sse41 | 2,423,676,659 | 2,423,951,951 | +0.28M |

**Flat to slightly negative on both tiers**: the duplicated bodies cost what the folded matches
save. Reverted. `../mcfish` reached the same verdict porting upstream's dense `Move*` generator
interface — bit-exact, fully gated, and slower on every reading — and wrote the rule this
repeats: **port upstream's structural divergences ONE AT A TIME and let the clock rule on each.**

The branch-prediction half of the claim is a separate question and is **not settled**. One body
serving four generation kinds makes a single branch site carry four interleaved histories, which
is the defect class `../mcfish` measured at 1.382 of upstream's branch misses; rfish's own is
1.301. Nothing here can resolve it — the instruction effect is 0.007% and this box's paired
clock has a noise floor near ±4% ([§18.13](#1813-software-prefetch-is-out-of-reach-and-a-discarded-load-is-not-a-substitute)).
It needs hardware counters on a quiet machine, and until someone has them the honest statement
is that the cost is unknown rather than zero.

### 18.15 A register-resident width is a TIER constant, not a kernel constant

The feature transformer's fold carries `TILE` accumulator entries across its row loop, so the
value that fits is decided by the REGISTER FILE and not by the arithmetic. 128 `i16` is eight
`ymm` at avx2 and **sixteen `xmm` at sse41 — the whole file**. Disassembled at sse41,
`transform` spilled 58 vector stores and reloaded 45 against 10 and 7 at avx2, and the row loop
round-tripped one lane through `0x20(%rsp)` on every applied row.

| TILE | 32 | 64 | 128 | 256 |
|---|---:|---:|---:|---:|
| sse41 | 2,594M | **2,261M** | 2,421M | — |
| avx2 | — | 1,688M | **1,505M** | 1,589M |

**64 is worth 160.4M at sse41, where the previous sweep had measured it losing**, and it costs
183M at avx2. Spills go 58/45 to 16/**zero**. `../mcfish` tiers the same constant the same way
(`ROW_TILE_WIDTH`, 64 / 128 / 256), which is a strong prior — [§18.14](#1814-upstreams-structure-is-a-strong-prior-not-a-proof)
— and here the measurement agreed with it.

Two things had made the old verdict stale. The sweep predates a third consumer of the same
tile that holds THREE source rows where the original holds one, so the register context moved
under it. And it was taken at one tier and generalised to the other — which is the mistake this
repository warns about for every `--arch` number and had never applied to its own constants.

**A constant is a candidate for tiering exactly when it decides how much state is live across a
loop.** Sweep those at every tier an instrument can reach, and say so where it cannot: callgrind
implements no AVX-512, so the third rung here is left at its avx2 value rather than guessed.

### 18.16 A constant trip count is a WIN where the body is small and a LOSS where it is not

[§18.1](#181-make-the-bound-a-constant-the-index-cannot-reach) says to make the bound a constant
the index cannot reach. It is right, and it is not unconditional — because what LLVM does with a
constant trip count is UNROLL, and an unrolled iteration costs registers and code in proportion
to its body.

Both halves were measured in one session, on the same tree, on both tiers.

**The win.** The pairwise product at the tail of the transformer walked three runtime-length
slices through zipped iterators, so a body that was already twenty instructions per thirty-two
outputs was wrapped in a runtime-trip loop, an eight-wide fixup loop and a scalar tail, twice per
evaluation. One `try_into` per walk states the length in the type: **−5.85M avx2, −2.41M sse41.**

**The loss.** The three affine layers reach a weight slice whose length comes from the file at
run time, for loops whose length the caller knows. The same fix — const-generic input widths and
one `try_into` each — is **−6.4M at avx2 and +76.1M at sse41**. Measured a layer at a time, at
sse41: the 128→1 dot +37.1M, the sparse 1024→32 +31.7M, the dense 64→32 +7.3M. The typed
signatures without the constant-length views are flat on both tiers, so the widths are not what
cost it; the unrolling is.

The difference is the body. Two vector ops per iteration unroll for free; a widened weight row
does not, and at the narrower tier every iteration takes twice the registers and twice the code.

**Rule: make the bound a constant where the body is small, and MEASURE AT THE NARROW TIER before
believing it where the body is not.** A change that improves avx2 and regresses sse41 is not
always code layout — here it was a real transformation, correctly applied, that the wide tier
could afford and the narrow one could not.
