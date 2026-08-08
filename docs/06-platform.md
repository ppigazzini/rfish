# The platform layer

`crates/rfish-engine/src/platform/` — the runtime that **hosts** the engine, rather than a
layer beneath it. That direction is why this zone may read the board, the search and the
evaluation while none of them may read it.

## What is here

Three modules, two of which have their own page because their subject is bigger than the
platform question they answer:

| Module | Owns | Detail |
|---|---|---|
| `threads.rs` | the worker set, the scoped search, the best-move vote | [04-multithreading.md](04-multithreading.md) |
| `numa.rs` | the topology, upstream's three auto policies, the `NumaPolicy` syntax, the distribution | below |
| `syzygy/` | discovery, the pairs decoder, the index computation, the WDL and DTZ probes | [05-tablebases.md](05-tablebases.md) |

**`numa.rs` reads the machine out of the filesystem, and that is the whole trick.** Upstream
discovers the same facts through `sched_getaffinity` and `GetNumaProcessorNodeEx`, which are
FFI and therefore closed here; Linux publishes every one of them as text that is already in
the format upstream parses — the process affinity in `/proc/self/status`, the online nodes
and each node's `cpulist` under `/sys/devices/system/node/`, and the L3 sharing list under
each CPU. Reading a file is safe, so the topology model costs neither an `unsafe` block nor a
dependency. What it cannot do is the next step, and that is the subject of the rest of this
page.

**An option value that becomes an allocation is bounded where it is parsed, and `NumaPolicy`
is the case that proves it matters.** It accepts a processor list; upstream bounds each range
and not their sum, and a few hundred bytes of input reached gigabytes of resident memory here
before the allocator gave up. `numa.rs` bounds the total. `Hash` and `Threads` are the other
two of the three, and [07-shell.md](07-shell.md) owns them.

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

**NUMA.** Upstream keeps one copy of the network per NUMA node and binds each thread to
the node holding the copy it reads. rfish keeps one `Arc<Network>` for the whole pool.

`platform/numa.rs` owns everything up to that last step: `NumaConfig::from_system` reads the
real topology and the process affinity, `AutoPolicy` implements upstream's three, `from_string`
parses its `NumaPolicy` syntax, and the distribution across nodes uses upstream's arithmetic.
All of it from file reads, so no `unsafe` and no dependency. What is blocked is the binding,
and it is the first case in this port where the no-`unsafe` constraint stops a feature
outright rather than redirecting it:

- Topology discovery is **fine** — `/sys/devices/system/node/*/cpulist` is a text file, and
  reading it needs nothing but `std::fs`. It is done, not merely possible.
- Pinning a thread to a node is **not possible in safe Rust**. `std` exposes no affinity
  API at all; `sched_setaffinity` and `SetThreadAffinityMask` are FFI, which means `unsafe`
  or a dependency, and the engine crate has neither.
- Placing an allocation on a node is **not possible either**. `mbind`/`set_mempolicy` are
  FFI for the same reason, and the only alternative is first-touch — which delivers locality
  only if the touching thread is pinned, and it cannot be.

Replicating without placement control would cost 112 MiB per replica to buy locality that
nothing guarantees. That is not a trade to make speculatively, and it cannot be measured
here: this machine reports a single node, so every arm of the experiment would return the
same number. **The item is blocked, not pending.** Reopen it if `std` ever grows an affinity
API, or if the zero-dependency rule is revisited on purpose rather than by accident.

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
