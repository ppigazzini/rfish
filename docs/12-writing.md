# Writing rules

These govern the shipped set — `docs/`, `README.md`, `CONTRIBUTING.md`, `AGENTS.md` — and
the comments in the source. They live here, in the set that ships, because a rule nobody can
read is a rule nobody can follow.

There is a second, **internal** surface this repository does not carry: the engineering
contract, the operator prompt, the port map, user-requested analyses. **Do not converge the
two.** A shipped page must not carry campaign history; an internal note must not be the only
place a shipped fact lives. And a shipped file must not name that surface's LOCATION —
`.gitignore` excludes it, so the reference dangles for every reader but its author.
`cargo xtask docs-lint` sweeps the whole index for it, because the path check next to it
cannot: an ignored path is exempt there by design.

## The rules

Each one is here because breaking it shipped a defect in this project or in a sibling.

**Describe a gap as a gap, never as a design.** *"rfish does not aim to match Stockfish"* and
*"the classical evaluation is the evaluation"* read as architecture. They are not: NNUE,
Syzygy, Lazy-SMP and NUMA are **required**, and the classical term is scaffolding with a
deletion date. Framing a hole as a decision is what keeps it alive — nobody fixes a design.
Say unimplemented, name the upstream golden that owns it, and say what its absence costs
today. [AGENTS.md](../AGENTS.md) collects those, and NUMA pinning is the one that is
genuinely impossible here rather than merely undone — say which kind it is.

**Never rationalise a defect into a convention.** The same rule one level down. When you find
yourself writing the sentence that makes the odd thing sound intended, stop and check whether
it is. In the sibling C port one such sentence — *"the engine routes UCI output to stderr
(same convention as the bench signature)"* — kept a P0 alive for months.

**Name the owner and the invariant, not just the mechanism.** Say which file and symbol owns
the behaviour and what must stay true about it. "`StateInfo` holds the halfmove clock" is
accurate and useless; what a reader needs is that `Position::key()` mixes that clock in past
move 14, so a key computed without it makes positions share table entries upstream keeps
apart. Write the sentence a reader needs before they delete your line.

**Verify the claim against the tree; drive the binary when it is behavioural.** Not "read it
carefully" — run it. `grep -n` for the symbol, `printf 'uci\n' | ./stockfish` for a
handshake, `cargo xtask bench 16 1 8` for a node count. Claims that took seconds to disprove
have shipped in this set.

**Separate upstream fact from rfish state.** "Upstream does X" is checkable against the SHA in
`tools/upstream/UPSTREAM_BASE`. "rfish does Y" is a claim about a tree mid-port, and the
reader needs to know whether Y is the target or the scaffolding. Blur them and nobody can
tell what they are allowed to change.

**Never pin a number a gate computes.** The bench anchor, the node count, a case count, a
table size, the current bench depth: quote the gate or the file that owns it. Every such
figure written into prose in the sibling ports went stale within a day, and a stale number is
worse than an absent one — it tells a reader to hold the wrong invariant.

**Never pin a list a gate owns, either.** The gates `parity` runs, the lanes in CI, the tiers
in the tier table. A list that drifts by one entry reads exactly like one that has not, and
`docs-lint` cannot count. Name the function or the file that owns the list beside it.

**State the limit.** A page that omits its own boundary invites over-trust. Say what the
thing does *not* cover: `signature` builds at the default arch and cannot see an ISA-gated
path; a skipped gate exits 2 and proves nothing; `filter_volatile` drops lines from every
golden, so nothing then guards them.

**Show the command.** "It is faster" is not a claim; `cargo xtask perf --tier avx2` output is,
and so is an instruction count with its tier attached. A performance or behaviour claim ships
with what produced it, so the next reader can re-run it instead of trusting you.

**No history in shipped prose.** "It used to be X", "this was fixed in Y", "we tried Z" is out
of date the day after it is written. The before-and-after belongs in the commit message,
which is the durable per-task record. A doc states what is true **now**.

The one exception is [08-idiomatic-rust.md](08-idiomatic-rust.md), which is a deliberate
measurement ledger: §11 lists the dead ideas so they are not re-derived, and §§13–17 record
what measured and what each shape was worth. A measurement is a fact about the tree, not a
story about the week — write it as the rule a reader applies now, and let the number be the
evidence.

**One example beats three paragraphs**, and **pair every prohibition with an alternative**.
"Do not reach for an intrinsic" leaves a reader stuck; "do not reach for an intrinsic — every
`std::arch` one is an `unsafe fn`; use `std::simd`, which needs no `unsafe` block" does not.

**Cut anything that does not help implement or verify.** Background a reader could get from
Stockfish's own wiki belongs in [11-references.md](11-references.md) as a link. Length is not
thoroughness; it is where rot hides.

## Hot and cold

These pages do not age alike, and treating them the same is why they rot. A page is **hot**
when it describes code that moves, **cold** when what it describes barely does.

**Change hot code, fix its page in the same commit.** A doc is wrong from the moment the code
lands, and nobody knows which claim broke better than the person who broke it.

| page | owns | temperature |
| --- | --- | --- |
| [00-architecture.md](00-architecture.md) | the three crates, five zones, the dependency direction | warm — the crate boundary is checked by the compiler, so it moves rarely |
| [01-engine-board.md](01-engine-board.md) | `crates/rfish-engine/src/board/` | hot |
| [02-engine-search.md](02-engine-search.md) | `crates/rfish-engine/src/search/` | hot — the addressing is rfish's own, and per-node work is where the campaigns land |
| [03-engine-eval.md](03-engine-eval.md) | `crates/rfish-engine/src/eval/` and the measurement ledger | hot — the classical term is scaffolding awaiting deletion, and the NNUE numbers move with every perf commit |
| [04-multithreading.md](04-multithreading.md) | Lazy-SMP, the worker set, the best-move vote | warm |
| [05-tablebases.md](05-tablebases.md) | `crates/rfish-engine/src/platform/syzygy/` | warm — the prober is ported and gated; the open items are table DATA, not code |
| [06-platform.md](06-platform.md) | `crates/rfish-engine/src/platform/` | warm — NUMA pinning is impossible here, not pending |
| [07-shell.md](07-shell.md) | `crates/rfish/src/` | warm |
| [08-idiomatic-rust.md](08-idiomatic-rust.md) | the Rust patterns, the falsified list, the measured shapes | cold as prose, append-only as a ledger |
| [09-type-design.md](09-type-design.md) | the value domain, its algebra, and the boundary of what it promises | warm — a type added without a row here makes the page wrong |
| [10-tooling-ci.md](10-tooling-ci.md) | `cargo xtask` steps, the tiers, `.github/workflows/` | hot — a step added here is a page edit there |
| [11-references.md](11-references.md) | external links | cold |
| this page | the rules | cold |
| [13-glossary.md](13-glossary.md) | the vocabulary, in four tiers | warm — every entry names an owner, and a rename dates it |

Cold does not mean unowned. It means the claim outlives a release, so when it *is* wrong it
has usually been wrong for a long time.

## Code comments

Same rules, plus these. Rust states more in the types than C can — a slice carries its
length, a `Result` carries its error set — so a comment that restates the signature earns
nothing. What it must carry is what the type cannot.

**Imperative mood, leading with a verb.** "Return the set of…", not "This function returns…"
or "Returns…". Upstream's comments are in that mood and a ported one should stay in it.

**Write only the constraint the code cannot show.** Never restate the next line. Never say
where the change came from or why it is right — that is the commit message's job, and it is
noise the moment the commit merges.

**Name the invariant, and what breaks without it.** "Carry the state forward in place" says
nothing; "every field must be written before return, because the recomputed group is no
longer zeroed" survives a refactor.

**Cite upstream as `file.cpp:line` or by symbol.** Checkable against the SHA in
`tools/upstream/UPSTREAM_BASE`; "upstream does this too" is not. A reader must be able to
tell a translated line from an invented one.

**Keep the integer-semantics comments.** Where a line relies on wrapping, on a truncation, or
on C++'s signed-to-unsigned conversion, that note is the whole reason it looks the way it
does — and release builds here set `overflow-checks = false`, so every intended wrap says
`wrapping_*` and the comment says why. See [08-idiomatic-rust.md](08-idiomatic-rust.md) §7.

**No history, no meta.** Not "was a `Vec`", not "changed in the delta campaign", not "the
following block does". A comment describes the code as it is, to someone who has never seen
it.

## The gate, and what it cannot see

```sh
cargo xtask docs-lint      # also runs inside cargo xtask parity
```

It reads every `*.md` outside `target/`, `.git/` and `resources/` — tracked or not — and
fails on three things:

- **A dead internal link.** Any `[text](target)` that is not an external URL, a `mailto:` or
  a bare `#anchor` must resolve relative to the linking file or to the workspace root. A
  trailing `#anchor` is stripped first, so the anchor itself is **not** verified: a link to a
  heading that no longer exists passes.
- **A named path that does not exist.** Any `crates/…` or `tools/…` written in prose is a
  claim about this tree. A word holding `*`, `<`, `>` or `…` is a placeholder and is skipped,
  which is what lets `tools/<name>.golden` be written at all.
- **A quoted bench signature, and an xtask step no page names.** The current value of
  `tools/signature.golden` appearing in prose is a failure — it is the number the "never pin
  a number" rule is most often broken with — and a step in the dispatch table that no shipped
  page mentions is a step nobody can discover.

**Three classes stay out of its reach, and they are the common ones:**

- a real symbol attributed to the **wrong file**;
- a list with the wrong **count or order** — the gates `parity` runs, the lanes in CI, the
  tiers. Each lints perfectly clean, and each has been wrong in this set;
- a behaviour or flag described as absent from a build that has it.

### It cannot tell you a sentence is false

That is the whole point of this section. A page can link cleanly, name only real paths, quote
no signature — and still describe code replaced three commits ago, or frame an unported
subsystem as a design decision. Both have happened here, and neither is mechanically
detectable.

The gate buys the mechanical half so review can spend its attention on the half that needs a
reader. Docs are accurate when written and rot where the code moves under them, and in a
repository mid-port the code moves a lot. Prefer the claim that stays true: name the owner
and the invariant, name the upstream golden for what is missing, and point at the gate for
the number.
