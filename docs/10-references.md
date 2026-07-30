# References

What this port is checked against. A claim in these docs is expected to be traceable to
something here or to the live tree.

## Upstream

- **Stockfish** — <https://github.com/official-stockfish/Stockfish>. The golden. The commit
  rfish targets is pinned in `tools/upstream/UPSTREAM_BASE`; read it there, never from
  prose.
- **The Stockfish wiki** — <https://official-stockfish.github.io/docs/stockfish-wiki/>.
  Authoritative for UCI option semantics, `bench` conventions and the NNUE file format.
- **Fishtest** — <https://tests.stockfishchess.org>. Where a strength claim is settled, and
  where the networks are published.

## The sibling ports

Two complete, bit-exact ports of the same engine, and the source of most of the process
rules in [AGENTS.md](../AGENTS.md) and [CONTRIBUTING.md](../CONTRIBUTING.md):

- **zfish** (`../zfish`) — a pure-Zig port.
- **mcfish** (`../mcfish`) — a C23 port.

Both made the design decisions C++ templates, RAII and operator overloading force a port to
re-make, and both are proven against upstream. Where a structural question has an obvious
answer in one of them, that answer is worth reading before inventing another.

They are **not** goldens. The differential reference is always upstream.

## Rust

- **The Rust Reference** — <https://doc.rust-lang.org/reference/>. Particularly the
  behaviour-considered-undefined section, which is what `forbid(unsafe_code)` makes
  unreachable.
- **The Rustonomicon** — <https://doc.rust-lang.org/nomicon/>. Read to understand what the
  constraint is buying, not to look for a way around it.
- **`std::sync::atomic`** — <https://doc.rust-lang.org/std/sync/atomic/>. The memory
  ordering the transposition table relies on.
- **`std::thread::scope`** — <https://doc.rust-lang.org/std/thread/fn.scope.html>.
- **The Cargo book, on profiles** — <https://doc.rust-lang.org/cargo/reference/profiles.html>.
- **`cargo xtask`** — <https://github.com/matklad/cargo-xtask>. The pattern, not a
  dependency.

## Protocol

- **UCI** — the protocol description as published by Stefan Meyer-Kahlen. The engine's
  handshake, option syntax and `info` line format follow it, and `tools/handshake.golden`
  pins the result.

## Chess facts

- **Perft results** — the reference node counts in `tools/perft.table` are published in
  several places and were reproduced here against a pristine upstream build. They are facts
  about chess, not about any engine.
- **Syzygy tablebases** — <https://github.com/syzygy1/tb>. The table format and the naming
  convention the prober in `crates/rfish-engine/src/platform/syzygy/` recognises.
