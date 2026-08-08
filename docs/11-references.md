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

### How to sweep them

**A sibling's refactor is a hypothesis about what this tree never wired up** — ask what it
implies about rfish's *use* of the same thing, not only whether the same duplication is
here. The 2026-08-06 sweep took `mcfish ac7d02c7` and `zfish b09519f6`, which each gave the
`currmove` node threshold one owner after finding it declared twice. rfish already had one
owner and all three of upstream's call sites, so on the sibling's own terms there was
nothing to take. The site that announces the move called nothing at all, and the whole
`info depth N currmove X currmovenumber M` line was missing from the port.

**A measurement does not transfer, in either direction.** What a sibling's language pays for
a construct says nothing about what Rust pays. Take the idea, then price it here — the same
sweep priced three ways of routing that reporter and two of them were gates-red.

**Probe against this tree, not against the subject line.** The commits worth recording are
as often the ones NOT taken, with the reason, so the next sweep does not re-open them.

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

## Type theory and type design

What [09-type-design.md](09-type-design.md) rests on. Each entry says what it is *for* here;
none of them is implemented as an algorithm, and where a paper describes machinery this port
does not run, that is said.

### The bound in the type

- **Xi & Pfenning, "Eliminating Array Bound Checking Through Dependent Types", PLDI 1998** —
  <https://www.cs.cmu.edu/~fp/papers/pldi98dml.pdf>. Index and array types carrying a static
  size let a checker discharge the bound and emit no check. This is the mechanism behind
  `Box<[T; N]>` and the const-generic layer widths, with LLVM's value-range propagation
  standing in for the dependent typechecker. The effect is therefore *not* guaranteed:
  [08-idiomatic-rust.md](08-idiomatic-rust.md) records both where it paid and where the same
  move cost instructions.
- **Xi & Pfenning, "Dependent Types in Practical Programming", POPL 1999** —
  <https://doi.org/10.1145/292540.292560>. The language (Dependent ML) the above became.
- **Rust RFC 2000, const generics** — <https://rust-lang.github.io/rfcs/2000-const-generics.html>,
  and the 2026 project goal for the full feature —
  <https://rust-lang.github.io/rust-project-goals/2026/const-generics.html>.

### Affine quantities: why a score is a torsor

- **Baez, "Torsors Made Easy"** — <https://math.ucr.edu/home/baez/torsors.html>. The standard
  informal account of quantities that have differences but no origin. `Value`, `Ply` and
  `GamePly` are three instances; `Value - Value` yields a margin for exactly this reason.
- **Baker, "Torsors as proportion spaces", 2023** —
  <https://mattbaker.blog/2023/09/18/torsors-as-proportion-spaces/>. A readable modern
  restatement.
- **`std::time::Instant`** — <https://doc.rust-lang.org/std/time/struct.Instant.html>. The
  reference instance in the standard library: `Instant - Instant` is a `Duration`,
  `Instant + Duration` is an `Instant`, and `Instant + Instant` does not exist.

### Units of measure

- **Kennedy, "Types for Units-of-Measure: Theory and Practice", CEFP 2009** —
  <https://link.springer.com/chapter/10.1007/978-3-642-17685-2_8>. Why dimensioned quantities
  want distinct types, and why the discipline is cheap. Shipped in F#.
- **Kennedy, "Relational Parametricity and Units of Measure", POPL 1997** —
  <https://people.mpi-sws.org/~dreyer/tor/papers/kennedy.pdf>. The theorem that a function
  polymorphic in its unit cannot inspect it.
- **`uom`** — <https://github.com/iliekturtles/uom> — and **`dimensioned`** —
  <https://github.com/paholg/dimensioned>. The Rust realisations, named as the reference
  implementations of the discipline and **not** as candidates: the engine crate has zero
  dependencies and that is a reviewed property. Unit *polymorphism* is what they have and
  rfish cannot express, which is why there is no `Depth`.

### Index newtypes

- **`rustc_index::newtype_index!`** —
  <https://doc.rust-lang.org/nightly/nightly-rustc/rustc_index/macro.newtype_index.html>. The
  same pattern at compiler scale: conversions marked `inline(always)` so they vanish even in
  debug, and an explicit maximum so the type keeps a niche.
- **matklad, "Newtype Index Pattern", 2018** —
  <https://matklad.github.io/2018/06/04/newtype-index-pattern.html>. The short practical
  statement of it.
- **Rust Design Patterns, "Newtype"** —
  <https://rust-unofficial.github.io/patterns/patterns/behavioural/newtype.html>.

### Making illegal states unrepresentable

- **Minsky, "Make illegal states unrepresentable"** — the maxim, from a 2010 lecture on
  OCaml. `Score`'s three variants and the `Square`/`SquareOrNone` split are the two places it
  is applied here.
- **Wlaschin, "Designing with types: making illegal states unrepresentable"** —
  <https://fsharpforfunandprofit.com/posts/designing-with-types-making-illegal-states-unrepresentable/>.
  The canonical worked write-up.
- **King, "Parse, Don't Validate", 2019** —
  <https://lexi-lambda.github.io/blog/2019/11/05/parse-don-t-validate/>. Push the check to the
  boundary and let the result type carry that it happened. rfish parses at its edges — FEN,
  UCI, the net file, a Syzygy table — and validates nowhere.

### Proofs carried by values

- **Noonan, "Ghosts of Departed Proofs", Haskell Symposium 2018** —
  <https://kataskeue.com/gdp.pdf>. Preconditions encoded as proofs inhabiting phantom type
  parameters, at no runtime cost. The diagnosis this port uses, not the machinery: the failure
  mode it names — a proof discharged in a comment and represented in the code as a bare
  integer — is what every index newtype here is fixing.
- **Kiselyov & Shan, "Lightweight Static Capabilities", 2007**. The earlier statement: a
  capability type witnesses a checked property so downstream code need not re-check it.
- **Yanovski, Dang, Jung & Dreyer, "GhostCell", ICFP 2021** —
  <https://plv.mpi-sws.org/rustbelt/ghostcell/paper.pdf>. Branded types in Rust
  specifically, mechanised in RustBelt.
- **bluss, `indexing`** — <https://github.com/bluss/indexing>. Sound unchecked indexing via
  generativity. Listed to record that it is **out**: its payoff is `get_unchecked`, which
  `forbid(unsafe_code)` closes, and it is a dependency the engine may not take. What crosses
  is the discipline — state the guard against the slice you index — which
  [08-idiomatic-rust.md](08-idiomatic-rust.md) shows is worth tens of millions of instructions
  without any of the machinery.

### Typestate

- **Strom & Yemini, "Typestate", IEEE TSE 12(1), 1986** —
  <https://www.cs.cmu.edu/~aldrich/papers/classic/tse12-typestate.pdf>. The origin.
- **Aldrich, Sunshine, Saini & Sparks, "Typestate-Oriented Programming", Onward! 2009** —
  <https://www.cs.cmu.edu/~aldrich/papers/onward2009-state.pdf>. The observation that without
  language support, typestate is encoded by flag fields and design patterns. `Aligned<T>`'s
  missing resize API is the one instance in this tree: an invariant held by what the type does
  not let you do.

### Representation as a type property

- **The Rust Reference, "Type Layout"** —
  <https://doc.rust-lang.org/reference/type-layout.html>. `repr(transparent)`,
  `repr(align(N))`, `repr(u8)` — the attributes that make width and alignment properties of
  the type rather than hopes about the allocator.
- **"Niches for integer types in Rust"** —
  <https://deterministic.space/niche-int-types-in-rust.html> — and `core`'s own
  `niche_types.rs` —
  <https://doc.rust-lang.org/beta/src/core/num/niche_types.rs.html>. Why `Option<Square>` is
  two bytes here and what it would take to make it one.
- **The pattern-types RFC draft** —
  <https://gist.github.com/joboet/0cecbce925ee2ad1ee3e5520cec81e30>. The route that would give
  a range-restricted integer a niche without `unsafe`. Unstable; tracked, not adopted.

### Refinement types — the direction, not the practice

Everything above puts a *set* in a type. These put a *predicate* there, and would statically
discharge the in-range arguments this port currently makes in doc-comments. None is used.

- **Lehmann, Geller, Vazou & Jhala, "Flux: Liquid Types for Rust", PLDI 2023** —
  <https://dl.acm.org/doi/10.1145/3591283>.
- **Jhala & Vazou, "Refinement Types: A Tutorial", 2021** —
  <https://doi.org/10.1561/2500000032>.
- **Aebi & Furia, "Practical Range Refinement Types with Inference", 2026** —
  <https://arxiv.org/pdf/2607.00824>. The closest published match to the narrow case here:
  inferred integer *range* types, aimed at verifying index manipulation and in-bounds access.
- **Lattuada et al., "Verus", OOPSLA 2023** — <https://dl.acm.org/doi/10.1145/3586037> — and
  **Creusot** — <https://creusot.rs/>. Static verification, no runtime checks added. Named
  because the alternative escalation, "add an `unsafe` block and assert the bound", is
  permanently closed here, so the only direction that exists is more proof rather than less.

### The counterweight

- **"When Zero Cost Abstractions Aren't Zero Cost", 2021** —
  <https://blog.polybdenum.com/2021/08/09/when-zero-cost-abstractions-aren-t-zero-cost.html>.
  Read alongside every claim above. A newtype is free in *layout* and is not always free in
  *codegen*, and this port has the measurements to prove it — see the cost rule in
  [09-type-design.md](09-type-design.md).

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

## Terms

Here because a page of external sources is where a reader looks for them, and because two of
the three artifacts this repository handles are not covered by the same licence as its code.
[README.md](../README.md) carries the statement; this is the index into it.

- **GNU GPL v3** — <https://www.gnu.org/licenses/gpl-3.0.html>. rfish is a derivative of
  Stockfish and inherits it. The text is [Copying.txt](../Copying.txt) and the attribution is
  [AUTHORS](../AUTHORS), both taken from upstream unmodified — a port that reproduces
  upstream's output line for line does not get to reword its attribution.
- **ODbL** — <https://opendatacommons.org/licenses/odbl/odbl-10.txt>. The terms on the Leela
  Chess Zero data the networks are trained on. It reaches this repository through the file
  `cargo xtask net` downloads, not through any source file, which is why nothing in
  `crates/` names it.
- **The network itself is not in the tree.** It is fetched at build time and gitignored, so a
  clone carries no weights and the licence question travels with the download rather than with
  the checkout. [03-engine-eval.md](03-engine-eval.md) covers why it is a runtime input.
