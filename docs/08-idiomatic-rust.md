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

**rfish.** `static SLIDERS: LazyLock<SliderTables>`. First access builds them; every access
after is one already-resolved branch the predictor gets for free.

The Zobrist tables go further and are **`const`-evaluated**: `static TABLES: Tables =
build();`. They exist before `main`, cost nothing at startup, and cannot be read half
built. The magic tables are not const-evaluated because the magic *search* is a loop over
88 772 table entries with a subset enumeration inside it, which is more const-eval than is
comfortable; the ray tables are `LazyLock` for symmetry with them.

**What it cost.** `&*SLIDERS` per lookup. Hoisting it out of a generation loop — `let t =
&*SLIDERS;` once, then index — is available if a measurement ever asks for it.

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
- **A bounds check is not automatically the cost.** Rust elides most of them. Find the
  check in the disassembly before reshaping code around it — and if reshaping is needed, it
  is still not a licence for `unsafe`: restructure so the bound is *provable* (iterate a
  slice, use a fixed-size array, bind a subslice once) instead.
- **Gate on the clock, and validate any counter before believing it.** A change can be
  instruction-neutral, cache-better and branch-level and still cost cycles.
- **Size an Elo run before starting it.** The 70-Elo-per-doubling figure is a LONG time
  control number and does not hold at the fast ones. Three measured cells against a PGO'd
  upstream — 0.1+0.001 at two tiers and 1+0.01 — all imply **138–152 Elo per doubling**.
  Use the figure that matches the clock being run, or the run is mis-sized before it starts.
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
| An inline `[ScoredMove; MAX_MOVES]` in the move picker, mirroring upstream's field | **Falsified, with a measurement.** Removes 97 M of allocator traffic, adds 473 M of per-node initialisation — worse than the `Vec` it replaced. Safe Rust cannot leave a buffer uninitialised; reuse one instead (§9). |
| Making `Bitboard::iter` borrow instead of copy | **Rejected by design.** Iterating by value is what lets a loop mutate the board it came from, which is upstream's `while (b) pop_lsb(b)` over a local with the local made explicit. |

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
and 15% at the very best**, and only in number-crunching code. That is the published
figure, and it matches this port's own evidence — rfish retires 1.7–1.9x more branches than
upstream while *missing fewer* of them, which is what a wall of perfectly-predicted checks
looks like, and the spine gap is nonetheless dominated by structural defects like the
allocator rather than by the checks. Chase the structure first; the checks are the last few
per cent, not the first twenty.

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

Upstream's first affine layer is one `vpdpbusd` per four input bytes. There is no way to
reach that instruction from this project's constraints, and three separate attempts to
recover its arithmetic in scalar form all lost (above). This is not a limit of Rust the
language — it is a limit of the intersection this port has chosen:

| route to explicit SIMD | safe? | stable? | usable here |
|---|---|---|---|
| `std::arch` intrinsics | **no** — every intrinsic is an `unsafe fn` | yes | no, `unsafe_code = "forbid"` |
| `std::simd` (`portable_simd`) | **yes** — no `unsafe` needed | **no** — nightly only | no, `rust-toolchain.toml` pins stable |
| autovectorised scalar loops | yes | yes | **this is what rfish uses** |

The sibling ports are not doing something cleverer with the same tools. ../zfish writes
upstream's kernels directly — 173 `@Vector` uses across ten NNUE files, plus per-ISA files
like `nnue_affine_vnni.zig` — because Zig has portable SIMD vectors in the *safe, stable*
language. Rust's equivalent exists and needs no `unsafe`, but is nightly. That single
difference, not code quality, is why a Zig port sits near upstream on the evaluation while
this one sits at 1.8x. Anyone comparing the two engines' NNUE throughput should read that
table first.

**What is deliberately NOT on this list:** `smallvec`, `arrayvec`, `bumpalo` and every other
allocation crate. They are the standard answer to §9 and rfish cannot use any of them — the
engine crate has zero dependencies, and that is a reviewed property. The workhorse pattern
is the dependency-free equivalent and measured as good.
