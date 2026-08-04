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
| `docs-lint` | no dead doc links, no named `crates/` or `tools/` path that does not exist, and no tracked file naming the internal working area |
| `lane-coverage` | every `xtask` step runs in a workflow, runs in `parity`, or is excused with a reason |
| `fixture-coverage` | `tools/fixture_properties.tsv` still holds: every property has a fixture that presents it, and every fixture is classified |
| `async-check` | `stop`, `ponderhit` and `quit` on a RUNNING search leave one legal `bestmove` and a live engine |
| `test` | the unit and property suite, under the `gate` profile's debug assertions |
| `perft` | the reference counts in `tools/perft.table` match |
| `golden` | the UCI case outputs match `tools/*.golden` |
| `golden-audit` | every golden is what UPSTREAM produces, not merely what rfish produces |
| `nnue-check` | the network's raw output equals a pristine upstream build's, position by position |
| `tb` | Syzygy discovery finds the tables, and an empty path changes nothing |
| `signature` | `bench` reproduces `tools/signature.golden` |

A gate whose **tool** is missing reports SKIPPED and exits **2**, never 0. So does a step
that REFUSES to run. A skipped gate proves nothing — it did not run.

### Editing a gate is the case where being wrong is silent

A broken engine reddens a gate. A broken **gate** reports success, so the bar is different:

```sh
cargo xtask negative-control        # break the engine on purpose; each gate must go RED
cargo xtask lane-coverage           # does anything actually run this check?
cargo xtask sync-status             # is ../Stockfish still AT the pin every oracle assumes?
```

A gate is done when it has been **seen to fail**, by mutation rather than by argument — not
when it passes. And every allowance a gate grants (a skip, an excuse, an exemption) needs an
owner that **expires** it: `lane-coverage` reports an excuse for a step that plainly runs, a
unit test refuses an excuse for a step that no longer exists, and the internal-area sweep
asserts that each of its two exempt files still contains what it is exempt for.

This is not theory. Each of these gates found something on its own first run that reading it
had not: `arch-determinism` was in no lane at all, six tracked files named the gitignored
working area, and in two cases the defect was in the gate **being written** rather than in the
code it was aimed at.

## Two different numbers

Do not confuse them:

- **`tools/signature.golden`** — rfish's node count *today*, at the depth
  `cargo xtask signature` uses. It exists so a refactor cannot silently change behaviour
  mid-port. It is not the target.
- **Upstream's `Bench:` at `tools/upstream/UPSTREAM_BASE`** — the finish line.

Neither number is written into prose anywhere. Quote the gate.

## Regenerating a golden

**`golden-update` REFUSES.** It drives rfish, so everything it writes is a photograph of
whatever this binary does today — including a bug, after which the gate is green forever.
That is not hypothetical: `search.golden` once recorded a `bestmove` with no ponder move and
passed every run for as long as it existed, while upstream printed one.

The regenerator is `cargo xtask golden-audit --write [CASE...]`, which drives the **oracle**,
so a re-derived golden is still a reference. `RFISH_GOLDEN_UPDATE_FROM_RFISH=1` is the
deliberate escape for a case that genuinely cannot be driven through upstream — say so in the
commit body. **It is not a way past a red gate:** a red `golden` is rfish disagreeing with
upstream, and writing that disagreement into the reference deletes the finding.

`signature-update` has no oracle-driven equivalent and stays dangerous in the old way.
**Regenerating a golden on a red gate pins the defect.** Before running it, establish that the
behaviour change is *intended*, and say in the commit body what moved and why.

**Check the exit code, never a piped fragment.** `cargo xtask parity | tail -1` reads `0` from
`tail` while the gate is red, and that has laundered red gates in both sibling ports more than
once. Run it unpiped, or redirect and test `$?`. A red gate is never regenerated past.

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
