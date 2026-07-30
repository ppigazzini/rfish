# Writing rules

These govern the shipped set — `docs/`, `README.md`, `CONTRIBUTING.md` — and the comments
in the source. They live here, in the set that ships, because a rule nobody can read is a
rule nobody can follow.

## Comments state the invariant the code cannot show

A comment that restates the code is noise the next edit will falsify. A comment earns its
place by saying something the reader cannot see:

- **Why this and not the obvious alternative.** "`between_bb` includes the destination
  because the set a checked king may block on must contain the checker's square."
- **What breaks if this changes.** "The seed and the draw order are load-bearing: every
  golden is a function of these tables."
- **What the caller must guarantee.** "`gives_check` is TRUSTED, never re-derived."

**Imperative mood.** "Return the set of…", not "This function returns…". Upstream's
comments are in that mood and a ported one should stay in it.

## Never pin a number a gate computes

The bench anchor, a table size, a module count, a cluster count, the current bench depth —
quote the gate or the file that owns it, never a figure in prose.

Every such number written into a doc in the sibling ports went stale within a day, and a
stale number is worse than an absent one: it tells a reader to hold the wrong invariant.

## No history in shipped prose

"It used to be X", "this was fixed in Y", "we tried Z and it did not work" is out of date
the day after it is written. The before-and-after belongs in the commit message, which is
the durable per-task record. A doc states what is true **now**.

The one exception is [08-idiomatic-rust.md](08-idiomatic-rust.md) §10, which is a
deliberate list of dead ideas — its purpose is to stop them being re-derived, and it says so.

## A doc claim is a claim about the tree

Verify it against live files, and drive the binary when the claim is behavioural.

Docs are accurate when written and rot where the code moves under them.
`cargo xtask docs-lint` settles the mechanical half — links, paths. It cannot tell you a
sentence has become false, and that is where every false claim in the sibling ports came
from: a commit that changed the code and not the page.

**Change a zone, fix its page in the same commit.**

## Say what is not done

A page that describes an unfinished zone as if it were finished costs a reader more than
one that admits the gap. Every zone page here names its milestone where something is
missing, and [AGENTS.md](../AGENTS.md) collects them.

Do not write around a gap, and do not optimise or gate around the current shape as if it
were the intended end state.

## Two surfaces, no overlap

- **`docs/` + `README.md` + `CONTRIBUTING.md` ship.** They describe the codebase for a
  contributor reading it cold.
- **`__DEV/` is internal and gitignored.** The engineering contract, the operator prompt,
  the port map, user-requested analyses.

Do not converge them. A shipped doc must not carry campaign history; an internal note must
not be the only place a shipped fact lives.

## Length

If a paragraph does not help someone implement or verify a change, cut it. A shorter page
that is read beats a complete one that is skimmed.
