# Tracking upstream

`UPSTREAM_BASE` pins the Stockfish commit rfish is being ported to. The fork's history is
non-ancestral to upstream's, so `git merge-base` does not define "where we are" — this file
does.

Upstream's own `Bench:` for that commit is the **finish line**: the node count rfish must
eventually reproduce. It is not written here, because it is a number a gate should compute:

```sh
cd ../Stockfish && git checkout "$(cat ../rfish/tools/upstream/UPSTREAM_BASE)"
make -j build ARCH=x86-64-avx2 && ./stockfish bench
```

Do not confuse it with `tools/signature.golden`, which is rfish's number *today*. See
[CONTRIBUTING.md](../../CONTRIBUTING.md), "Two different numbers".

## Syncing

An upstream sync ports a real upstream change — a bench-mover or an NNUE-architecture
change — and lands bit-exact at that commit's `Bench:`. It advances `UPSTREAM_BASE` and
re-derives `tools/signature.golden` in the **same** commit, and the commit body says what
moved the number.

A sync that cannot land bit-exact is not a sync; it is a bug report against the port.
