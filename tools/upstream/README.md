# Tracking upstream

`UPSTREAM_BASE` pins the Stockfish commit rfish is being ported to. The fork's history is
non-ancestral to upstream's, so `git merge-base` does not define "where we are" — this file
does.

Upstream's own `Bench:` for that commit is what `tools/signature.golden` holds — the two are
**equal**, so a diff against the golden is a porting regression rather than a tuning
difference. Neither number is written here, because it is a number a gate should compute:

```sh
cd ../Stockfish && git checkout "$(cat ../rfish/tools/upstream/UPSTREAM_BASE)"
make -j build ARCH=x86-64-avx2 && ./stockfish bench
```

`cargo xtask sync-status` asserts that this checkout is actually AT the pin before anything
is compared against it. See [CONTRIBUTING.md](../../CONTRIBUTING.md), "One number, and what a
diff against it means".

## Syncing

An upstream sync ports a real upstream change — a bench-mover or an NNUE-architecture
change — and lands bit-exact at that commit's `Bench:`. It advances `UPSTREAM_BASE` and
re-derives `tools/signature.golden` in the **same** commit, and the commit body says what
moved the number.

A sync that cannot land bit-exact is not a sync; it is a bug report against the port.
