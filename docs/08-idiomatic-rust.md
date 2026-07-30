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

And most of the speed gap is NOT the missing intrinsics. rfish recomputes the accumulator
from scratch per evaluation where upstream updates it incrementally, and that alone accounts
for the bulk of an eleven-fold nodes-per-second difference. Until the incremental path
lands, an intrinsics-versus-autovectorisation comparison would be measuring the wrong thing:
both sides would be dominated by a term only one of them pays.

**So the honest state is: the constraint has not yet been shown to cost anything on this
axis, because a larger algorithmic difference is in the way.** Do not cite this section as
evidence either direction until the accumulator is incremental.

---

## 9. Measurement laws

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
- **Size an Elo run before starting it.** Speed converts at roughly 70 Elo per doubling, so
  a 6% change is about 6 Elo and needs ~10 000 games per cell to resolve. A 1000-game cell
  carries ±18 and returns a coin flip with a sign; two such cells must never be compared to
  each other.

---

## 10. Falsified, or not attempted for a stated reason

Keep this list current. Re-deriving a dead idea costs a session.

| Idea | Status |
|---|---|
| `std::simd` / `stdarch` intrinsics for the NNUE kernels | **Rejected by constraint**, not by measurement — nightly and `unsafe` respectively. Not a measurement result; do not cite it as one. |
| Comparing autovectorised NNUE kernels against upstream's intrinsics | **Premature.** The from-scratch accumulator dominates, so the comparison would measure that instead. Do it after the incremental path lands. |
| Optimising the classical evaluation | **Pointless.** It runs only when no net is on disk, and it has a deletion date. |
| `memmap2` for the Syzygy tables | **Rejected.** The crate's soundness contract cannot be met (a table file can be truncated under the map), and positioned reads behind a block cache give what the mapping actually provides. |
| Const-evaluating the magic tables | **Not attempted.** 88 772 entries with a subset enumeration each is far more const-eval than the Zobrist tables' ~800 draws; `LazyLock` costs one predicted branch per lookup. Measure before changing. |
| Making `Bitboard::iter` borrow instead of copy | **Rejected by design.** Iterating by value is what lets a loop mutate the board it came from, which is upstream's `while (b) pop_lsb(b)` over a local with the local made explicit. |
