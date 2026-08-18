//! The rfish build driver and gate battery.
//!
//! `cargo xtask <step>` is the single entry point for every build and every gate. There is
//! no Makefile and no shell driver: the gates are Rust, in the workspace, type-checked and
//! clippy-linted by the same CI lane that checks the engine — so a gate cannot rot in a way
//! the compiler would have caught.
//!
//! **Check a gate's EXIT CODE, never a piped fragment.** `cargo xtask parity | tail -1`
//! reads `0` from `tail` while the gate is red. That has laundered red gates in both
//! sibling ports. Run it unpiped, or redirect and test `$?`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

mod codegen;
mod devsweep;
mod fuzz;
mod gates;
mod meta;
mod net;
mod perf;
mod runner;

use runner::Outcome;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let step = args.first().map_or("help", String::as_str);
    let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();

    match dispatch(step, &rest) {
        Ok(Outcome::Pass) => {
            println!("\x1b[32mPASS\x1b[0m {step}");
            ExitCode::SUCCESS
        }
        Ok(Outcome::Skipped(why)) => {
            // A SKIPPED gate proves nothing, and must never read as a pass to a script.
            // Both sibling ports learned this the hard way: a missing tool exited 0 and a
            // whole campaign reported green.
            eprintln!("\x1b[33mSKIPPED\x1b[0m {step}: {why}");
            // Not always a missing tool: a step that REFUSES to run reports the same way,
            // because the property it would have asserted is equally unasserted either way.
            eprintln!("A skipped gate proves nothing — it did not run.");
            ExitCode::from(2)
        }
        Ok(Outcome::Fail(why)) => {
            eprintln!("\x1b[31mFAIL\x1b[0m {step}: {why}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("\x1b[31mERROR\x1b[0m {step}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(step: &str, args: &[&str]) -> Result<Outcome, String> {
    match step {
        "help" | "--help" | "-h" => {
            print_help();
            Ok(Outcome::Pass)
        }
        "build" => gates::build(args),
        "test" => gates::test(),
        "fmt" => gates::fmt(false),
        "fmt-fix" => gates::fmt(true),
        "clippy" => gates::clippy(),
        "bench" => gates::bench(args),
        "signature" => gates::signature(false),
        "signature-update" => gates::signature(true),
        "perft" => gates::perft(),
        "tsan" => gates::tsan(),
        "arch-determinism" => gates::arch_determinism(args),
        "golden" => gates::golden(false),
        "golden-audit" => gates::golden_audit(args),
        "golden-update" => gates::golden_update(),
        "docs-lint" => gates::docs_lint(),
        "unsafe-lint" => gates::unsafe_lint(),
        "nnue-check" => gates::nnue_check(),
        "net-roundtrip" => gates::net_roundtrip(),
        "tb" => gates::tb(),
        "net" => net::fetch(args),
        "tb-fetch" => net::fetch_tb(),
        "pgo" => perf::pgo(args),
        "oracle" => perf::oracle(args),
        "perf" => perf::perf(args),
        "counters" => perf::counters(args),
        "perf-budget" => perf::perf_budget(args, false),
        "perf-budget-update" => perf::perf_budget(args, true),
        "upstream-nodes" => perf::upstream_nodes(args),
        "fingerprint" => perf::fingerprint(args),
        "codegen-equiv" => codegen::codegen_equiv(args),
        "budget-ab" => perf::budget_ab(args),
        "fuzz" => fuzz::fuzz(args),
        "lane-coverage" => meta::lane_coverage(),
        "fixture-coverage" => meta::fixture_coverage(),
        "negative-control" => meta::negative_control(args),
        "sync-status" => meta::sync_status(),
        "async-check" => meta::async_check(),
        "repro-search" => meta::repro_search(),
        "parity" => gates::parity(),
        other => Err(format!("unknown step '{other}'; run `cargo xtask help`")),
    }
}

fn print_help() {
    print!(
        "\
cargo xtask <step> — the rfish build driver

  Build and run
    build [--profile P] [--arch TIER]  build the engine binary
    net [NAME]                         fetch the NNUE network into resources/
    tb-fetch                           fetch the 3-man Syzygy set into resources/syzygy/
    bench [ARGS...]                    run the benchmark

  Gates — a behaviour-changing edit is not done until one of these says so
    parity                the aggregate; run it before calling anything done
    signature             bench reproduces tools/signature.golden
    perft                 the reference counts in tools/perft.table match
    golden                the UCI case outputs match tools/*.golden
    golden-audit [--write [CASE...]]
                          every golden is what UPSTREAM produces, not just what we do.
                          --write RE-DERIVES from the oracle: this is the regenerator,
                          because a golden written from rfish is a photograph of rfish
    upstream-nodes        node-for-node vs the oracle on RANDOM positions
    budget-ab [--tier T] [--base REF] [--rounds N] [--syzygy]
                          the instruction budget with NO stored golden: build the WORKING
                          TREE and REF, count both, compare. Refuses unequal node counts
    codegen-equiv [--tier T] [--base REF]
                          the gate for a no-functional-change claim: disassemble the WORKING TREE
                          and REF and compare symbol by symbol. Refuses a clean checkout,
                          where it would be comparing a tree with itself
    fingerprint [--tier T]  assert rfish CALLS what upstream calls, as often, under
                          callgrind; catches a state divergence no value gate can see
    test                  the unit and property suite
    fmt / fmt-fix         cargo fmt --check / cargo fmt
    clippy                cargo clippy -D warnings
    docs-lint             no dead doc links, no named paths that do not exist, and no
                          shipped file naming the internal working area
    lane-coverage         every step runs in a workflow, runs in parity, or is excused
                          with a reason -- a lane that is in no gate is not a lane
    fixture-coverage      tools/fixture_properties.tsv still holds: every property has a
                          fixture that presents it, and every fixture is classified
    unsafe-lint           no `unsafe` and no allow(unsafe_code) anywhere
    tsan                  a 4-thread search under ThreadSanitizer, Linux only
    arch-determinism [--host-tiers]
                          every enumerated tier benches the anchor -- the gate that makes a
                          tier safe to add, since `signature` only ever builds the portable
                          arm and cannot see an ISA-gated divergence. Every tier BUILDS on
                          any host; a tier only BENCHES on a host whose ISA can execute it,
                          so a host below the top tier skips unless --host-tiers accepts a
                          run that builds those tiers and benches the rest
    nnue-check            the network's output equals upstream's, position by position
    net-roundtrip         the net survives `export_net` byte for byte, so the format
                          reader and writer agree -- an ORDER stated twice, welded
    tb                    Syzygy discovery, and that an empty path changes nothing
    repro-search          node counts repeat across `ucinewgame`, at twenty budgets --
                          the question about what a COMPLETED search leaves for the next
    async-check           stop, ponderhit and quit on a RUNNING search. INVARIANTS, not
                          values: no golden can reach this path, because an interrupted
                          search ends wherever the clock got to
    sync-status           the golden checkout at ../Stockfish is AT tools/upstream/
                          UPSTREAM_BASE -- BEHIND the pin is red, because every oracle
                          built from it would then answer from source already ported past

  Measurement — a ratio is about the engines only when everything else is held equal
    pgo [--tier T] [--spine]           PGO build: instrument, train on bench, rebuild
    oracle [--tier T] [--spine]        upstream at UPSTREAM_BASE, built clang + PGO + LTO
    perf [--tier T] [--spine] [--rounds N]
                                       the differential: callgrind instructions, then a
                                       paired interleaved A/B of search time
                                       tiers: sse41, avx2 (default), avx512, vnni512,
                                       avx512icl; `native` SELECTS one of them rather than
                                       compiling host-specific code
    counters [--tier T] [--spine]      the cache and branch table against upstream: reads,
                                       writes, D1 and icache misses, conditional and
                                       indirect branches and their mispredicts. Simulated,
                                       so DETERMINISTIC -- unlike the paired clock this one
                                       is worth reading on a loaded box
    perf-budget [--tier T] [--rounds N] [--syzygy]
                                       hold this tier's absolute instruction count to
                                       tools/instr_budget.golden -- the cost regression
                                       `signature` cannot see. Local, NOT in parity: the
                                       count moves with the toolchain as well as the code.
                                       --syzygy benches a corpus the TABLES cover, which is
                                       the only workload here that enters the prober at all
    perf-budget-update [--tier T]       re-record this tier's row. DANGEROUS on a red gate,
                                       exactly as signature-update is

  The gates on the gates — LOCAL: they mutate the tree, so they cannot share a checkout
    negative-control [GATE...]
                          break the engine on purpose and require each named gate to go
                          RED. A gate is done when it has been SEEN TO FAIL, not when it
                          passes

  Scheduled — a wall-clock budget, so NOT part of parity
    fuzz [SECONDS]        random UCI text at the shell, random positions at the search
                          and mutated bytes at the tablebase decoder; prints its seed,
                          replay with RFISH_FUZZ_SEED=N

  Regenerate a golden — DANGEROUS, read CONTRIBUTING.md first
    signature-update      re-derive tools/signature.golden
    golden-update         REFUSES: it drives rfish, so it writes a photograph of rfish.
                          `golden-audit --write` is the regenerator

Exit codes: 0 pass, 1 fail, 2 SKIPPED (tool missing — proves nothing).
"
    );
}

/// The workspace root, found from this crate's manifest rather than the working directory,
/// so a gate behaves the same wherever it is invoked from.
#[must_use]
pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("xtask lives two levels below the workspace root")
        .to_path_buf()
}

/// Where the gates run the engine from.
///
/// `resources/` holds every runtime input — the net, the tablebases, an opening book — and
/// the engine looks for its net relative to the working directory, so running it from
/// anywhere else finds no net and produces an unrelated number.
#[must_use]
pub(crate) fn resources_dir() -> PathBuf {
    workspace_root().join("resources")
}

/// Run a command and return its captured stdout, or an error naming the failure.
pub(crate) fn capture(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.stderr(Stdio::inherit()).output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd:?} exited with {}", out.status));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("{cmd:?}: output is not UTF-8: {e}"))
}

/// Run a command for its exit status alone.
pub(crate) fn run(cmd: &mut Command) -> Result<(), String> {
    let status = cmd.status().map_err(|e| format!("{cmd:?}: {e}"))?;
    if status.success() { Ok(()) } else { Err(format!("{cmd:?} exited with {status}")) }
}

/// True when `tool` is on the path.
#[must_use]
pub(crate) fn have(tool: &str) -> bool {
    Command::new(tool)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}
