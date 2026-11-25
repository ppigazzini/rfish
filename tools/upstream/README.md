# Tracking upstream

`UPSTREAM_BASE` pins the Stockfish commit rfish is being ported to. The fork's history is
non-ancestral to upstream's, so `git merge-base` does not define "where we are" — this file
does.

Upstream's own `Bench:` for that commit is what `tools/signature.golden` holds — the two are
**equal**, so a diff against the golden is a porting regression rather than a tuning
difference. Neither number is written here, because it is a number a gate should compute:

```sh
$(tools/upstream/upstream_oracle.sh --verify) bench
```

`upstream_oracle.sh` is the same file in every port and on the golden's `refish` branch, and
all of them share its builds in `../.upstream-oracle`; `oracle_profile.sh` and
`oracle_counters.py` measure those builds. Change the three in every tree that carries them or
in none. The script archives the pin out of `../Stockfish` and does not touch that checkout.

`cargo xtask sync-status` asserts that the golden is not BEHIND the pin. See [CONTRIBUTING.md](../../CONTRIBUTING.md), "One number, and what a
diff against it means".

## Syncing

An upstream sync ports a real upstream change — a bench-mover or an NNUE-architecture
change — and lands bit-exact at that commit's `Bench:`. It advances `UPSTREAM_BASE` and
re-derives `tools/signature.golden` in the **same** commit, and the commit body says what
moved the number.

A sync that cannot land bit-exact is not a sync; it is a bug report against the port.
