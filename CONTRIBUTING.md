# Contributing to rfish

rfish is a **safe-Rust port of [Stockfish][stockfish]** aiming at a bit-exact 1:1 clone.
`../Stockfish` is the **golden**; where anything disagrees with Stockfish, Stockfish wins.

## Building

See the [README](README.md#build): install stable Rust and run `cargo xtask build`. There
are no other dependencies.

## The one rule that outranks the rest

**rfish is 100% safe Rust.** `unsafe_code = "forbid"` is set for the whole workspace in
`Cargo.toml`, and `cargo xtask unsafe-lint` asserts it from outside the mechanism that
enforces it. A patch that adds `unsafe`, or that adds `#[allow(unsafe_code)]` to work
around the forbid, is not accepted — not for performance, not "just for this one kernel".

If a construct seems to need it, the answer is a different construct. Every such case so
far has had one, and [docs/08-idiomatic-rust.md](docs/08-idiomatic-rust.md) records what it
was.

## Faithfulness

**One logical change per commit.** A commit that touches three modules cannot be bisected
when the node count moves.

**Do not "improve" on upstream.** A cleaner formulation that moves a rounding boundary
moves the node count. Port faithfully first; a better idea is a separate change with its
own evidence.

**Integer semantics are the classic trap.** Rust traps on overflow in debug and wraps in
release, and C++ has undefined behaviour where Rust has neither. Upstream relies on
wrapping in places, and every such place in rfish says `wrapping_*` in the source rather
than inheriting the behaviour from a profile. The `dev`, `test` and `gate` profiles all set
`overflow-checks = true` so an unintended wrap fails a gate instead of silently matching.

## The gates

Any change that touches engine behaviour must keep the whole battery green:

```
cargo xtask parity
```

**Check its EXIT CODE, not a piped fragment.** `cargo xtask parity | tail -1` reads `0`
from `tail` while the gate is red; that has laundered red gates in both sibling ports. Run
it unpiped, or redirect and test `$?`.

In the order `parity` runs them:

| Gate | Asserts |
|---|---|
| `fmt` | `cargo fmt --check` is clean |
| `clippy` | `cargo clippy --all-targets -- -D warnings` is clean |
| `unsafe-lint` | no `unsafe`, no `allow(unsafe_code)`, and the workspace forbid is in place |
| `docs-lint` | no dead doc links, no named `crates/` or `tools/` path that does not exist |
| `test` | the unit and property suite, under the `gate` profile's debug assertions |
| `perft` | the reference counts in `tools/perft.table` match |
| `golden` | the UCI case outputs match `tools/*.golden` |
| `golden-audit` | every golden is what UPSTREAM produces, not merely what rfish produces |
| `nnue-check` | the network's raw output equals a pristine upstream build's, position by position |
| `tb` | Syzygy discovery finds the tables, and an empty path changes nothing |
| `signature` | `bench` reproduces `tools/signature.golden` |

A gate whose **tool** is missing reports SKIPPED and exits **2**, never 0. A skipped gate
proves nothing — install the tool before relying on the run.

## Two different numbers

Do not confuse them:

- **`tools/signature.golden`** — rfish's node count *today*, at the depth
  `cargo xtask signature` uses. It exists so a refactor cannot silently change behaviour
  mid-port. It is not the target.
- **Upstream's `Bench:` at `tools/upstream/UPSTREAM_BASE`** — the finish line.

Neither number is written into prose anywhere. Quote the gate.

## Regenerating a golden

`signature-update` and `golden-update` exist, and both are dangerous.

**Regenerating a golden on a red gate pins the defect.** The gate then passes forever with
the bug baked into the expectation. Before running either, establish that the behaviour
change is *intended*, and say in the commit body what moved and why.

`tools/perft.table` is deliberately **not** a `.golden` and no step regenerates it. Those
node counts are facts about chess, identical for every correct engine, so a mismatch is
always a bug in rfish.

## Code style

Formatting is `cargo fmt`; lints are `cargo clippy -D warnings`. Both are gates, so follow
their output rather than restating their rules here.

Beyond what they check:

- **Comments state the invariant the code cannot show**, in imperative mood. Never pin a
  number a gate computes — the anchor, a table size, a module count.
- **A doc comment naming an upstream file is a claim**; carry `Golden:` lines across when
  porting a module, and keep them pointing at the file that actually owns the behaviour.
- **`#[must_use]` on every pure query.** A silently discarded `see_ge` or `legal` is a
  behaviour bug that compiles.

For git blame, ignore the formatting-only revisions:

```
git config blame.ignoreRevsFile .git-blame-ignore-revs
```

## Commits

One module per commit, as above.

Conventional subject, 72 characters or fewer; blank line; body wrapped at 80 carrying the
evidence — gate output and exit code, not "should work". A change that moves the bench
signature must say what moved it.

**No trailers.** The body ends with the evidence and nothing after it:

- **no `Co-Authored-By:`** — not for a tool, not for an assistant, not automatically. A
  trailer naming a non-author is a false claim about who wrote the change, and
  `git log --format='%an'`, `git shortlog -sn` and every blame view repeat it forever.
- **no `Generated with …`**, and no tool advertisement of any kind.

This applies whoever or whatever is driving the commit. Tooling that appends a trailer by
default must be configured not to, rather than having it stripped afterwards — the fix
belongs before the commit, not in a later rewrite.

Commit locally and stop. Do not `git push` unless asked.

## License

By contributing you agree that your contributions are licensed under the **GNU General
Public License v3** — see [Copying.txt](Copying.txt) — the same license as Stockfish, of
which rfish is a derivative.

[stockfish]: https://github.com/official-stockfish/Stockfish
