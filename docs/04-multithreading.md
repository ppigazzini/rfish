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

- **No NUMA model and no network replication.** Upstream replicates the NNUE weights per
  NUMA node and binds threads to it. There is no network to replicate yet; when M3 lands,
  this section says what happened.
- **No thread voting.** Upstream lets the threads vote on the best move rather than taking
  thread 0's. rfish takes thread 0's. That is a strength difference, not a correctness one,
  and it is open work.
- **No ponder.** `Ponder` is declared and `ponderhit` is accepted; the search does not yet
  convert a pondering search into a real one.
