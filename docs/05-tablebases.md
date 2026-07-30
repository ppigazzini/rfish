# Tablebases

`crates/rfish-engine/src/platform/syzygy.rs` — discovery today, probing at milestone M5.

Golden: `Stockfish/src/syzygy/tbprobe.cpp`.

## Status: discovery only

What exists: a `SyzygyPath` is resolved into a directory list, the directories are scanned,
table names are recognised by their material split, and a **maximum cardinality** falls out.

What does not exist: the WDL and DTZ decoders, the table registry, the block decompression,
the root ranking, and the in-search probe.

## Why discovery landed first

It is not an arbitrary slice. `SyzygyPath`, `SyzygyProbeDepth`, `Syzygy50MoveRule` and
`SyzygyProbeLimit` are UCI options whose presence and defaults the handshake golden pins
byte for byte. So the option surface can be correct, and gated, before a single table is
read.

And it changes no search: with no path set the cardinality is 0, so the search's tablebase
step never enters. Wiring the options left the bench signature untouched, which is exactly
what a correctly scoped slice looks like.

## Table names

A Syzygy stem is the two material sides joined by `v`, each a run of piece letters starting
with `K`: `KQvK` is three pieces, `KRPvKR` five. The stronger side is named first, so only
one of `KQvK` and `KvKQ` is a real table.

`cardinality_of` recognises the form and rejects everything else in the directory —
`README`, a stray `.txt`, a stem whose side does not start with a king.

## No memory mapping

Upstream maps each table file into the address space. A mapping is `unsafe` in Rust for a
real reason: the file can be truncated under the map, and the program then reads unmapped
memory. Neither `memmap2` nor any other crate can make that sound, because the unsoundness
is in the operating system's contract, not in the wrapper.

The prober will read with ordinary positioned file reads behind a block cache — which is
what upstream's mapping effectively provides anyway, with the page cache doing the caching.

**Gate, when it lands:** discovery and the root probe's score and tbhits over a 3-man
battery, diffed against a golden derived from the oracle. The cursed-win and blessed-loss
branches need 5-man tables and get their own local leg, deliberately kept out of `parity` —
a gate that is usually skipped stops being read.
