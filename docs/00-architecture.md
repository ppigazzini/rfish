# Architecture

rfish is three crates and five zones. This page is the map; each zone's own page is the
detail.

```
rfish/
  crates/
    rfish-engine/     the ENGINE: no I/O, no UCI, no stdin, no stdout
      src/board/      the value domain, bitboards, attacks, position, movegen
      src/search/     transposition table, histories, move picker, search, time
      src/eval/       the network loader, and the classical scaffolding
      src/state/      the blocks the search and the shell share
      src/platform/   threads, and the Syzygy prober
    rfish/            the SHELL: UCI transport, option model, benchmark. Binary `stockfish`
    xtask/            the build driver and the gate battery
```

## The crate boundary is the zone check

The engine crate reads no standard input, writes no standard output and parses no UCI. It
reports through an [`InfoSink`] trait the shell implements.

Upstream needs a link-time check for that property, and the sibling C port runs one as a
gate (`zone-check`: link `engine/` and `platform/` with no `shell/` object and see whether
it resolves). Here it is a crate boundary: `rfish-engine` does not depend on `rfish`, so
the property is checked by `cargo build` on every compile, and a violation is a compile
error rather than a gate someone has to remember to run.

That is the single clearest structural win of the Rust port, and it is worth stating
plainly because it is invisible: **the gate that does not exist is the point**.

## Dependency direction

```
        shell (rfish)
             |
             v
      +-- engine (rfish-engine) --+
      |            |              |
   search        eval         platform
      |            |              |
      +----> state |              |
      |            |              |
      +------------+--> board <---+
```

Every arrow runs one way. `board` reads nothing; `state` reads `board`; `eval` and `search`
read both; `platform` reads all of them; the shell reads the engine. There is no cycle, and
Rust's module system does not enforce that — a cycle between modules of one crate compiles
fine — so it is a property a reviewer maintains, not one the compiler does.

The consequence that matters: **perft is a complete test of the board zone**, because
nothing below it can influence it.

## Zero dependencies

`rfish-engine` has no crates.io dependencies. Not few — none. `rfish` depends only on
`rfish-engine`; `xtask` depends on nothing.

That is deliberate and reviewed. A chess engine that reproduces upstream's node count byte
for byte must own every line that can move it, and a transitive dependency bump is a
behaviour change nobody reviewed. It also means `cargo build` on a fresh clone compiles
exactly the code in this repository and nothing else.

## No `unsafe`, and no nightly

`unsafe_code = "forbid"` is set once, for the whole workspace, in `Cargo.toml`. `forbid`
rather than `deny` so no module can re-enable it locally.

The toolchain is pinned to **stable**. rfish uses no nightly feature: not `portable_simd`,
not `allocator_api`, not `stdarch` intrinsics. Reaching for nightly would trade the port's
central property — it builds on the compiler everyone has — for a constant factor nobody
has measured.

[docs/08-idiomatic-rust.md](08-idiomatic-rust.md) is the pattern-by-pattern account of what
that costs and what it buys.

## Profiles

| Profile | For | Distinguishing setting |
|---|---|---|
| `dev` | editing | `overflow-checks = true`, `debug = 1` |
| `test` | `cargo test` | `overflow-checks = true` |
| `gate` | `cargo xtask test` | release codegen **plus** debug assertions and overflow checks |
| `release` | shipping and measuring | `lto = "fat"`, one codegen unit, `panic = "abort"` |
| `profiling` | `perf`, callgrind | release plus symbols, `panic = "unwind"` |

The `gate` profile exists because the search states its invariants as `debug_assert!`, and
a release build that skips them turns a violated invariant into a plausible wrong number
rather than a failure. Running the suite under release codegen is also the only way to
catch a bug that only appears optimised.

`release` has `overflow-checks = false` on purpose: upstream relies on wrapping in a handful
of places, and each of those says `wrapping_*` in the rfish source rather than inheriting
the behaviour from a profile. A bare `+` that wraps is therefore a bug, and `gate` is what
catches it.

## What is not here

- **No build system beyond cargo.** `cargo xtask <step>` is the driver; there is no
  Makefile and no shell script. The gates are Rust in the workspace, so they are
  type-checked and clippy-linted by the same CI lane that checks the engine.
- **No embedded network.** The net is a runtime input, fetched by `cargo xtask net` into
  `resources/`. Embedding it would make the bench anchor look like a property of this
  repository rather than of a file downloaded separately.
- **No large-page allocator and no memory mapping**, and **NUMA without PINNING**: the
  topology, the policies and the reporting are in `crates/rfish-engine/src/platform/numa.rs`,
  read from `/sys` and `/proc`; what `std` exposes no API for is binding a thread to a node.
  See [06-platform.md](06-platform.md) for why each gap is blocked rather than pending.
