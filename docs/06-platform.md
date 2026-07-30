# The platform layer

`crates/rfish-engine/src/platform/` — threads, and the Syzygy prober. That is all of it,
and the shortness is the interesting part.

## What upstream has here that rfish does not

Upstream's platform layer carries a large-page allocator, a NUMA topology model, a
memory-mapping wrapper and a thread-affinity binder. None of them appear in rfish, and each
absence is a decision rather than a gap:

**Large pages and aligned allocation.** The transposition table is a `Vec` of naturally
aligned atomic words. Rust's allocator already honours the type's alignment, so there is
nothing to hand-roll. Large pages would be a measurable win on a large `Hash`, and getting
them portably needs a platform call that is `unsafe` — so this is a real cost of the
constraint, and it is recorded as one rather than papered over. It has not been measured.

**Memory mapping.** A mapping is `unsafe` in Rust because the file can change under it. The
Syzygy prober reads with positioned file reads instead; see
[05-tablebases.md](05-tablebases.md).

**A thread pool.** `std::thread::scope` lends each worker a `&mut` for exactly the duration
of the search. There is no pool to own, no join to forget, and no lifetime to assert by
hand. See [04-multithreading.md](04-multithreading.md).

**NUMA.** There is nothing to replicate until the network lands, and binding threads to
nodes without replicated weights is worse than not binding them.

**Timing.** `std::time::Instant` is monotonic on every platform rfish targets, which is the
only property the time manager needs. There is no per-platform clock module.

## Portability

The engine crate uses `std` and nothing else, so the portable surface is whatever `std`
guarantees. The two places platform differences are visible:

- `Tablebases::discover` splits `SyzygyPath` on `;` under Windows and `:` elsewhere.
- The engine binary is `stockfish.exe` on Windows, which `crates/xtask/src/runner.rs` knows
  when it locates the binary for a gate.

Everything else compiles identically. There is no `#[cfg(target_os)]` in the engine's
search, evaluation or board zones, and there should not be one: a `cfg` in engine code is a
second engine that no gate runs.
