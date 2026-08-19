# rfish developer documentation

This is the shipped documentation set: what the codebase does, for a contributor reading it
cold. It describes the tree **as it is**, not as it is intended to become — where a zone is
unfinished, its page says so and names the milestone.

Read in order. Each page owns a zone of the source and is the live claim about it.

| Page | Owns |
|---|---|
| [00-architecture.md](00-architecture.md) | the zone split, the crate boundary, the dependency direction |
| [01-engine-board.md](01-engine-board.md) | `crates/rfish-engine/src/board/` — types, bitboards, attacks, position, movegen |
| [02-engine-search.md](02-engine-search.md) | `crates/rfish-engine/src/search/` — TT, histories, move picker, search, time |
| [03-engine-eval.md](03-engine-eval.md) | `crates/rfish-engine/src/eval/` — the NNUE forward pass and the classical fallback |
| [04-multithreading.md](04-multithreading.md) | `crates/rfish-engine/src/platform/threads.rs` — Lazy-SMP without a pool |
| [05-tablebases.md](05-tablebases.md) | `crates/rfish-engine/src/platform/syzygy/` — the Syzygy prober |
| [06-platform.md](06-platform.md) | `crates/rfish-engine/src/platform/` — the worker set, the NUMA topology, the prober, and what is deliberately absent |
| [07-shell.md](07-shell.md) | `crates/rfish/src/` — UCI transport, options, benchmark |
| [08-idiomatic-rust.md](08-idiomatic-rust.md) | the pattern-by-pattern translation, and the measurement laws |
| [09-type-design.md](09-type-design.md) | the value domain: what each type means, why it has that shape, and what it does not promise |
| [10-tooling-ci.md](10-tooling-ci.md) | `cargo xtask`, the gates and CI — the instruments that say whether the engine is RIGHT |
| [11-performance.md](11-performance.md) | the six cost axes, what each proves and what it cannot see |
| [12-references.md](12-references.md) | upstream, Rust and UCI sources this port is checked against |
| [13-writing.md](13-writing.md) | how to write a comment and a doc page here |
| [14-glossary.md](14-glossary.md) | the words this set uses without stopping to define them |

## Docs are part of the change, not after it

Each page above is a live claim about code someone is about to touch. Change a zone,
re-read its page and fix it **in the same commit**: a doc is wrong from the moment the code
lands, and every false claim ever found in the sibling ports' docs got there that way.

`cargo xtask docs-lint` catches a dead link, a named path that does not exist, a number or a
list a gate computes, and a page that has stopped naming the gates that hold it. It
**cannot** tell you a sentence has become false. That part is yours.

**Each page carries a `## The gates` section**, naming what holds its claims. `owned by` reads
`this page` where the mechanics live there and names the page otherwise, so a gate is described
once and reachable from everywhere it matters — and the routing is checked in both directions,
because a pointer outlives its target exactly as a baseline outlives its edge.
[12-references.md](12-references.md) and [14-glossary.md](14-glossary.md) hold no gates and are
exempt by name.

## Two documentation surfaces

- **`docs/` plus `README.md` and `CONTRIBUTING.md` SHIP.** They describe the codebase for a
  contributor reading it cold.
- **The second surface is INTERNAL and gitignored**, so a clone does not carry it. It holds
  the engineering contract, the operator prompt, the port map and user-requested analyses.

Do not converge them. A shipped doc must not carry campaign history, and an internal note
must not be the only place a shipped fact lives. No shipped file may name the internal
surface's location either: `cargo xtask docs-lint` sweeps the index for that, since the
dangling reference it leaves is invisible to everyone except the author who wrote it.
