# rfish

**rfish** is a [Rust][rust] port of the [Stockfish][stockfish] chess engine, written in
**100% safe Rust** — `unsafe_code = "forbid"` for the whole workspace, and no `unsafe` block
anywhere. Like Stockfish, it is a UCI engine, not a GUI.

The goal is a **bit-exact 1:1 clone**: the same `bench` node signature, the same bestmove,
NNUE evaluation, Syzygy tablebases and Lazy-SMP threading. `../Stockfish` is the **golden**
— where rfish and Stockfish disagree, Stockfish wins.

The board, the search, the transposition table, the threads, the UCI shell, the **NNUE
evaluation** and the **Syzygy prober** are written and gated: `cargo xtask nnue-check`
proves the network's output is identical to a pristine upstream build's position by
position, `cargo xtask tb` does the same for the WDL verdict and the DTZ distance, and
`cargo xtask signature` holds the bench node total to upstream's own. What is missing is
thread PINNING, which `std` exposes no API for — [docs/06-platform.md](docs/06-platform.md)
says why that is blocked rather than pending. Run `cargo xtask parity` for the current state
of every gate, and see [docs/](docs/README.md) for what each zone does.

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
| SIMD intrinsics in the NNUE kernels | `std::simd`, which needs no `unsafe` block — **bit-exact with upstream** |

None of those are workarounds. Each removes a class of bug the C++ has to avoid by
convention — a dangling `StateInfo`, a worker outliving its data, a table truncated under
its own mapping — and the cost is measured rather than assumed. See
[docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md).

## Build

Requires the **dated nightly** pinned in `rust-toolchain.toml`, which `rustup` installs by
itself — the engine enables one nightly feature, `portable_simd`, and no stable channel
accepts it. `std::simd` needs no `unsafe` block, so the workspace forbid is untouched. No
other tooling is needed, and the engine itself has **no crates.io dependencies at all**.

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
