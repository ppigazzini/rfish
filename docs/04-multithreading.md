# Multithreading

`crates/rfish-engine/src/platform/threads.rs` — Lazy-SMP, expressed in
`std::thread::scope`.

Golden: `Stockfish/src/thread.cpp`.

## Lazy-SMP does not partition the tree

Every thread searches the same root. They diverge because they hit the shared transposition
table in a different order. The gain comes from the table, not from a split — which is why
the table's design (see [02-engine-search.md](02-engine-search.md)) is the load-bearing
part of the threading story and the thread management is not.

## There is no persistent thread pool

`ThreadPool` owns `Vec<SearchWorker>` because the **histories** have to survive between
searches; that is most of why the second search of a game is faster than the first. But
there is no persistent *thread*.

`search()` calls `std::thread::scope`, which lends each helper a `&mut SearchWorker` for
exactly the duration of the search and joins them all before returning. The main thread
searches on the caller's own thread, so `Threads 1` involves no spawned thread at all.

The cost is one spawn per helper per `go` — microseconds against a search measured in
seconds. What it buys is that "a worker used after the search ended" is not a bug that can
be written: the scope's lifetime bound is what lets `&mut SearchWorker` and
`&TranspositionTable` coexist without a lock.

See [08-idiomatic-rust.md](08-idiomatic-rust.md) §3.

## What is shared, and how

| State | Sharing | Why it is safe |
|---|---|---|
| `TranspositionTable` | `&`, all fields atomic | no `&mut`, so no exclusivity to violate |
| `SharedState` (stop flag, node and tbhit counters) | `Arc`, atomics | a stop from the UCI thread reaches a searching thread without a lock and without either side blocking |
| `SearchWorker` (position, histories, stack) | `&mut`, one per thread | the scope proves each is exclusive |
| `Position`, `Limits` | `&`, read-only for the search's duration | cloned into each worker at the start |

Nothing else crosses a thread boundary. There is no lock in the engine crate.

## The threads vote

The move played is the one the pool AGREES on, not the one thread 0 happened to end on:
each thread contributes `(score - worst + 14) * depth` to its choice, summed across the
threads that picked it.

Two parts of that are load-bearing. Offsetting by the worst score keeps every weight
non-negative, so a deeply searched losing move cannot outvote a shallow winning one by sign
alone. And the `+ 14` floor is what makes agreement count at all — without it, threads whose
score equals the worst contribute nothing, and a lone slightly-higher score outvotes two
threads that agree.

With one thread there is nothing to vote on and the result is thread 0's, which is what
keeps `Threads 1` deterministic. `MultiPV` is excluded, because the caller asked for a
ranked list rather than a single answer.

## Determinism

`Threads 1` is fully deterministic: same position, same limits, same node count, same move.
That is what makes the bench signature an anchor at all.

More than one thread is **not** deterministic, and cannot be: the divergence is the
mechanism. The properties that must hold, and that the tests assert, are that a
multi-threaded run returns a **legal** move, joins cleanly, and honours a stop request.

## `resize` keeps thread 0

A `Threads` change mid-game truncates or extends the worker vector and keeps thread 0's
histories. Thread 0's history is the one the next search benefits most from, so throwing it
away for a configuration change would cost strength for no reason.

## What is not here

- **No thread PINNING, and therefore no network replication.** The NUMA model itself is
  here — `platform/numa.rs` reads the real topology and the process affinity, implements
  upstream's three auto policies, parses and prints its `NumaPolicy` syntax, and distributes
  a worker set across nodes with upstream's arithmetic. What is missing is the last step:
  `std` exposes no affinity API, and both `sched_setaffinity` and node-local allocation are
  FFI the engine crate cannot reach without `unsafe` or a dependency. Replication follows
  pinning, so it is **blocked** rather than pending, and the shell reports thread
  *distribution* where upstream reports *binding* rather than implying a guarantee it cannot
  make. [06-platform.md](06-platform.md) has the full reasoning.

Pondering IS here. `Ponder` buys the current move a quarter more time; a budget that runs
out while pondering sets a flag rather than stopping, because only the GUI can end a ponder;
and `ponderhit` converts the search into a real one, stopping immediately if that flag was
already set. The thinking done on the opponent's clock counts.

## The gates

**`bench` is single-threaded**, so every gate that reads a node count — the anchor included —
stays green while a race is live and while contention gets worse. These are the ones that are
not blind to it.

| gate | what it proves here | owned by |
|---|---|---|
| `tsan` | a four-thread search under ThreadSanitizer: the table, the counters and the vote, on the paths a real search takes | [10-tooling-ci.md](10-tooling-ci.md) |
| `repro-search` | what a completed search leaves for the next one — at ONE thread, which is the limit of the row | [02-engine-search.md](02-engine-search.md) |
| `test` | the vote over a constructed candidate set, and that a resized pool keeps at least the main thread and still searches — both in-process, at 1 and 3 workers | [10-tooling-ci.md](10-tooling-ci.md) |

A worker is ~15.6 MB resident, so a harness must never drive `Threads` near the option's
declared maximum — that is `Threads 1024`, ~16 GB, and it has taken this box down. Keep every
harness at 1, 2, 8 or 16 and test the bounds as a pure function; [07-shell.md](07-shell.md)
owns the rule and the option surface it applies to.
