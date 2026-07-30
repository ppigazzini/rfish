# rfish

**rfish** is a [Rust][rust] port of the [Stockfish][stockfish] chess engine, written in
**100% safe Rust** — `#![forbid(unsafe_code)]` for the whole workspace, no `unsafe` block
anywhere, and no nightly feature. Like Stockfish, it is a UCI engine, not a GUI.

The goal is a **bit-exact 1:1 clone**: the same `bench` node signature, the same bestmove,
NNUE evaluation, Syzygy tablebases and Lazy-SMP threading. `../Stockfish` is the **golden**
— where rfish and Stockfish disagree, Stockfish wins.

**The port is in progress.** The board, the search, the transposition table, the threads,
the UCI shell and the **NNUE evaluation** are written and gated — `cargo xtask nnue-check`
proves the network's output is identical to a pristine upstream build's, position by
position. The Syzygy prober is not written, and the NNUE accumulator is recomputed rather
than updated incrementally, which costs about an order of magnitude in speed. Run
`cargo xtask parity` for the current state of every gate, and see [docs/](docs/README.md)
for what each zone does and does not do yet.

## Why safe Rust

The interesting question a port answers is not "can this be translated" but "what does the
translation cost". Every place the C++ reaches for a raw pointer, rfish has to express the
same idea another way:

| Upstream | rfish |
|---|---|
| `StateInfo*` chained through caller stack frames | a `Vec<StateInfo>` the position owns |
| an unsynchronised 10-byte transposition entry | atomic words, `Relaxed` — the same race, defined |
| a persistent thread pool holding raw worker pointers | `std::thread::scope`, one lend per search |
| `mmap` of each Syzygy table | positioned file reads |
| SIMD intrinsics in the NNUE kernels | loops over fixed-size arrays, autovectorised — **bit-exact with upstream** |

None of those are workarounds. Each removes a class of bug the C++ has to avoid by
convention — a dangling `StateInfo`, a worker outliving its data, a table truncated under
its own mapping — and the cost is measured rather than assumed. See
[docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md).

## Build

Requires **stable Rust**, edition 2024. No other dependencies, and the engine itself has
**no crates.io dependencies at all**.

```
cargo xtask build            # build the engine -> target/release/stockfish
cargo xtask net              # fetch the NNUE network into resources/
cargo xtask bench            # run the benchmark and print the node signature
cargo xtask parity           # the full gate battery -- run before calling anything done
cargo xtask help             # every step
```

The binary is named `stockfish`, not `rfish`: every GUI and every test harness invokes a
UCI engine by that name.

The NNUE network is a runtime input, not embedded and not committed. Run the engine from
`resources/`, or use `cargo xtask bench`.

## Documentation

- [docs/](docs/README.md) — the architecture, each subsystem, the Rust patterns.
- [CONTRIBUTING.md](CONTRIBUTING.md) — the gates and the workflow.
- [AGENTS.md](AGENTS.md) — what an automated contributor gets wrong before reading either.

## License

rfish is a derivative of Stockfish and is distributed under the **GNU General Public
License v3** — see [Copying.txt](Copying.txt). All chess strength and the NNUE networks
come from the [Stockfish project][stockfish]; see [AUTHORS](AUTHORS). The networks are
trained on [Leela Chess Zero data][lc0-data] under the [ODbL][odbl].

[rust]:       https://www.rust-lang.org
[stockfish]:  https://github.com/official-stockfish/Stockfish
[lc0-data]:   https://storage.lczero.org/files/training_data
[odbl]:       https://opendatacommons.org/licenses/odbl/odbl-10.txt
