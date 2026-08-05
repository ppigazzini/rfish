//! The gates.
//!
//! Every gate answers one question and reports one of three outcomes. A gate that cannot
//! run reports [`Outcome::Skipped`] and exits 2 — never 0, because a skipped gate has
//! proven nothing.

use std::process::Command;

use crate::runner::{
    GATE_PROFILE, Outcome, build_engine, cargo, compared_something, drive, drive_at, engine_path,
    node_total,
};
use crate::{capture, have, resources_dir, run, workspace_root};

/// The bench depth the signature gate uses: **upstream's own 13**.
///
/// The number the gate compares against is now upstream's number too, not a private
/// anchor. `tools/signature.golden` and a pristine upstream build's `Bench:` line at
/// `tools/upstream/UPSTREAM_BASE` are the same integer, and a diff between them is a
/// porting regression rather than a tuning difference.
const SIGNATURE_DEPTH: &str = "13";
const SIGNATURE_HASH: &str = "16";
const SIGNATURE_THREADS: &str = "1";

/// Build the engine.
pub(crate) fn build(args: &[&str]) -> Result<Outcome, String> {
    let profile = arg_value(args, "--profile").unwrap_or(GATE_PROFILE);
    // `--arch` sets `-C target-cpu`, which changes which vector width the NNUE loops
    // autovectorise to. The DEFAULT build sets nothing, because the anchor has to be
    // reproducible on a machine nobody here owns.
    if let Some(tier) = arg_value(args, "--arch") {
        // Through the SAME tier table `perf --tier` uses. This took its argument as a raw
        // `-C target-cpu` before, so the tier vocabulary every doc and every measurement is
        // quoted in -- sse41, avx2, native -- was the one vocabulary it did not accept:
        // `--arch avx2` died in rustc with "unknown target-cpu", naming neither the flag nor
        // the tier. A perf number is only meaningful with its tier attached, so the two
        // commands have to mean the same thing by the same word.
        let cpu = crate::perf::target_cpu_for(tier).ok_or_else(|| {
            format!(
                "unknown arch tier '{tier}'; want one of {}",
                crate::perf::tier_names().join(", ")
            )
        })?;
        // SAFETY-FREE alternative to a global: pass it through the child's environment
        // rather than mutating this process's, which a later gate would inherit.
        let mut cmd = Command::new(cargo());
        cmd.current_dir(workspace_root()).env("RUSTFLAGS", format!("-C target-cpu={cpu}")).args([
            "build",
            "--package",
            "rfish",
            "--bin",
            "stockfish",
            "--profile",
            profile,
        ]);
        run(&mut cmd)?;
        return Ok(Outcome::Pass);
    }
    build_engine(profile)?;
    Ok(Outcome::Pass)
}

/// The unit and property suite.
///
/// Built under the `gate` profile: release code generation plus the debug assertions the
/// search states its invariants with, so a violated invariant fails here instead of
/// producing a plausible wrong number in a release run.
pub(crate) fn test() -> Result<Outcome, String> {
    run(Command::new(cargo()).current_dir(workspace_root()).args([
        "test",
        "--workspace",
        "--profile",
        "gate",
    ]))?;
    Ok(Outcome::Pass)
}

/// `cargo fmt --check`, or `cargo fmt` when `fix`.
pub(crate) fn fmt(fix: bool) -> Result<Outcome, String> {
    let mut cmd = Command::new(cargo());
    cmd.current_dir(workspace_root()).args(["fmt", "--all"]);
    if !fix {
        cmd.arg("--check");
    }
    run(&mut cmd)?;
    Ok(Outcome::Pass)
}

/// `cargo clippy` with every warning fatal.
pub(crate) fn clippy() -> Result<Outcome, String> {
    run(Command::new(cargo()).current_dir(workspace_root()).args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--all-features",
        "--",
        "-D",
        "warnings",
    ]))?;
    Ok(Outcome::Pass)
}

/// Run the benchmark, passing the arguments straight through.
pub(crate) fn bench(args: &[&str]) -> Result<Outcome, String> {
    let engine = build_engine(GATE_PROFILE)?;
    let cmd = format!("bench {}", args.join(" "));
    let out = drive(&engine, &[&cmd])?;
    print!("{out}");
    Ok(Outcome::Pass)
}

/// The anchor: `bench` must reproduce `tools/signature.golden`.
///
/// When `update`, re-derive the golden instead of checking it. **Re-deriving on a red gate
/// launders a bug into the anchor** — establish that the behaviour change is intended
/// first, and say in the commit body what moved it.
pub(crate) fn signature(update: bool) -> Result<Outcome, String> {
    let engine = build_engine(GATE_PROFILE)?;
    let cmd = format!("bench {SIGNATURE_HASH} {SIGNATURE_THREADS} {SIGNATURE_DEPTH}");
    let out = drive(&engine, &[&cmd])?;
    let nodes = node_total(&out)?;

    let path = workspace_root().join("tools/signature.golden");
    if update {
        let text = format!(
            "# rfish bench node signature: the full default entry list at depth \
             {SIGNATURE_DEPTH},\n\
             # Threads {SIGNATURE_THREADS}, Hash {SIGNATURE_HASH}, and a SINGLE table clear \
             before the run -- the table and\n\
             # the history block carry across positions. Every one of those four facts \
             changes\n\
             # the number; see crates/rfish/src/bench.rs.\n\
             #\n\
             # This EQUALS a pristine upstream build's `Bench:` at\n\
             # tools/upstream/UPSTREAM_BASE. It is not an rfish-only anchor: a diff against\n\
             # upstream at that SHA is a porting regression, not a tuning difference.\n\
             #\n\
             # Regenerate ONLY for an intended behaviour change, and say what moved it in\n\
             # the commit body. Updating this on a red gate launders a bug into the anchor.\n\
             {nodes}\n"
        );
        std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        println!("signature.golden re-derived: {nodes}");
        return Ok(Outcome::Pass);
    }

    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Outcome::Skipped(format!(
            "{} does not exist; run `cargo xtask signature-update`",
            path.display()
        )));
    };
    let expected: u64 = text
        .lines()
        .find(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .ok_or("signature.golden holds no number")?
        .trim()
        .parse()
        .map_err(|e| format!("signature.golden does not parse: {e}"))?;

    println!("signature: {nodes} (golden {expected})");
    Ok(Outcome::check(nodes == expected, format!("bench {nodes} != golden {expected}")))
}

/// Every enumerated tier must reproduce the anchor.
///
/// **The gate that makes a tier safe to add.** rfish's NNUE kernels are `std::simd`, so the
/// `-C target-cpu` decides how each lane operation lowers — a 512-bit build of the same
/// source is a different instruction sequence over the same integers, and a saturation or a
/// narrowing that behaves differently at one width produces a different tree while every
/// other gate stays green. `signature` cannot see it: it builds at the DEFAULT arch, which is
/// the portable arm, so an ISA-gated divergence is invisible to it by construction.
///
/// Local rather than part of `parity`: it builds the engine once per tier. ../mcfish carries
/// the same check as `arch-determinism` and ran it to land its own tier expansion; this is
/// rfish's, and `docs/08-idiomatic-rust.md` §16.4 records what it replaces — a by-hand check
/// at two tiers.
///
/// **A tier is BUILT on any host and BENCHED only on a host that can execute it.** Building
/// at `-C target-cpu=skylake-avx512` emits AVX-512 wherever the build runs, so benching that
/// binary on a box without AVX-512 raises SIGILL before the first node — which is a fact
/// about the runner, not about the anchor. This gate's first CI run drew an AMD runner from
/// a fleet that also holds Intel ones and died there, having been green on every AVX-512 box
/// it had ever run on. So the executable set is DERIVED from the host, and the tiers left
/// unbenched are named rather than counted as checked.
///
/// `--host-tiers` accepts that reduced coverage and still passes; without it a host that
/// cannot reach every tier reports [`Outcome::Skipped`], because the property the gate
/// exists to assert is unasserted for the tiers it could not drive. The flag is the OWNER of
/// the allowance, it sits in the workflow where a reader meets it, and it expires by itself:
/// a runner that gains AVX-512 benches all five and the flag stops excusing anything.
pub(crate) fn arch_determinism(args: &[&str]) -> Result<Outcome, String> {
    let path = workspace_root().join("tools/signature.golden");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Outcome::Skipped(format!("{} does not exist", path.display())));
    };
    let expected: u64 = text
        .lines()
        .find(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
        .ok_or("signature.golden holds no number")?
        .trim()
        .parse()
        .map_err(|e| format!("signature.golden does not parse: {e}"))?;

    let accept_hole = args.contains(&"--host-tiers");
    let tiers = crate::perf::enumerated_tiers()?;
    println!("arch-determinism: {} tiers must all bench {expected}", tiers.len());
    let cmd = format!("bench {SIGNATURE_HASH} {SIGNATURE_THREADS} {SIGNATURE_DEPTH}");
    let mut checked = 0usize;
    let mut failures = Vec::new();
    let mut unexecuted = Vec::new();
    for tier in &tiers {
        let (name, cpu) = (tier.name, tier.rustc);
        // One target directory per tier, so a tier's binary cannot be measured as another's
        // and `target/release` is left alone for the gates that own it.
        let dir = workspace_root().join("target/arch").join(name);
        // Build EVERY tier, including one this host cannot run: a tier that stops compiling
        // is a break to catch here, and the compiler needs no ISA the host has.
        run(Command::new(cargo())
            .current_dir(workspace_root())
            .env("RUSTFLAGS", format!("-C target-cpu={cpu}"))
            .env("CARGO_TARGET_DIR", &dir)
            .args(["build", "--release", "--package", "rfish", "--bin", "stockfish"]))?;
        if !tier.missing.is_empty() {
            let lacks = tier.missing.join(", ");
            println!("  {name} ({cpu}): BUILT, NOT benched — this host lacks {lacks}");
            unexecuted.push(format!("{name} (lacks {lacks})"));
            continue;
        }
        let engine = dir.join("release").join(crate::runner::engine_file_name());
        // Name the tier on the way out: an engine that dies here dies as a signal, and the
        // raw error says which signal without saying which of five binaries raised it.
        let out = drive_at(&engine, &resources_dir(), &[&cmd])
            .map_err(|e| format!("tier {name} ({cpu}): {e}"))?;
        let nodes = node_total(&out)?;
        checked += 1;
        if nodes == expected {
            println!("  {name} ({cpu}): {nodes}");
        } else {
            println!("  {name} ({cpu}): {nodes} != {expected}");
            failures.push(format!("{name} benched {nodes}"));
        }
    }

    if let Some(refusal) = compared_something(checked, "tiers", "the tier table") {
        return Ok(refusal);
    }
    if !failures.is_empty() {
        return Ok(Outcome::Fail(format!(
            "tiers that do not reproduce the anchor: {}",
            failures.join(", ")
        )));
    }
    println!("arch-determinism: {checked} of {} tiers benched the anchor", tiers.len());
    if unexecuted.is_empty() {
        return Ok(Outcome::Pass);
    }
    // State the hole either way. A bounded run that reports only its passes reads as full
    // coverage to the next person, which is the whole failure mode this gate is built around.
    let hole = unexecuted.join("; ");
    if accept_hole {
        println!("  --host-tiers: {} tier(s) BUILT but not benched: {hole}", unexecuted.len());
        return Ok(Outcome::Pass);
    }
    Ok(Outcome::Skipped(format!(
        "this host cannot execute {} of {} tiers ({hole}), so the anchor is unasserted for \
         them. Re-run on a host of the top tier, or pass --host-tiers to accept a run that \
         BUILDS those tiers and benches the rest",
        unexecuted.len(),
        tiers.len()
    )))
}

/// A multi-threaded search under `ThreadSanitizer`.
///
/// The one gate that can see a DATA RACE. `forbid(unsafe_code)` rules out the pointer
/// mistakes a C++ port has to fear, and it rules out nothing about atomics: the shared
/// table, the stop flag and the node counters are `Relaxed` loads and stores by design, and
/// an ordering that is too weak is a logic bug the type system is happy with. Both sibling
/// ports gate on the same thing.
///
/// `-Zbuild-std=std,panic_abort` is not optional. This toolchain refuses to link an
/// instrumented crate against an uninstrumented `std`, and `panic_abort` has to be named
/// because the release profile sets `panic = "abort"` -- without it the build fails on a
/// duplicate lang item rather than on anything to do with sanitizers.
pub(crate) fn tsan() -> Result<Outcome, String> {
    if !have("rustup") {
        return Ok(Outcome::Skipped("rustup is needed to add rust-src".to_string()));
    }
    let _ = Command::new("rustup").args(["component", "add", "rust-src"]).status();

    let target_dir = workspace_root().join("target/tsan");
    run(Command::new(cargo())
        .current_dir(workspace_root())
        .env("RUSTFLAGS", "-Zsanitizer=thread")
        .env("CARGO_TARGET_DIR", &target_dir)
        .args([
            "build",
            "--release",
            "-Zbuild-std=std,panic_abort",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--package",
            "rfish",
            "--bin",
            "stockfish",
        ]))?;

    let engine = target_dir.join("x86_64-unknown-linux-gnu/release/stockfish");
    // Several threads over one table and one stop flag is the shape a race would live in;
    // one thread would instrument the same code and observe nothing.
    let out = Command::new(&engine)
        .current_dir(resources_dir())
        .args(["bench", "16", "4", "7"])
        .env("TSAN_OPTIONS", "halt_on_error=0")
        .output()
        .map_err(|e| format!("{}: {e}", engine.display()))?;
    let text =
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let races = text.matches("WARNING: ThreadSanitizer").count();
    if races > 0 {
        eprint!("{text}");
    }
    println!("tsan: {races} race reports over a 4-thread search");
    Ok(Outcome::check(races == 0, format!("ThreadSanitizer reported {races} races")))
}

/// The reference perft counts.
///
/// `tools/perft.table` is deliberately NOT a `.golden` and no step regenerates it. Those
/// node counts are facts about chess, identical for every correct engine, so a mismatch is
/// always a bug in rfish.
pub(crate) fn perft() -> Result<Outcome, String> {
    let engine = build_engine(GATE_PROFILE)?;
    let path = workspace_root().join("tools/perft.table");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Outcome::Skipped(format!("{} does not exist", path.display())));
    };

    let mut failures = Vec::new();
    let mut checked = 0;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // `<chess960>;<depth>;<expected>;<fen>`
        let parts: Vec<&str> = line.splitn(4, ';').collect();
        if parts.len() != 4 {
            return Err(format!("malformed perft.table row: {line}"));
        }
        let (c960, depth, expected, fen) = (parts[0], parts[1], parts[2], parts[3]);
        let script = [
            format!("setoption name UCI_Chess960 value {c960}"),
            format!("position fen {fen}"),
            format!("go perft {depth}"),
        ];
        let refs: Vec<&str> = script.iter().map(String::as_str).collect();
        let out = drive(&engine, &refs)?;
        let got = out
            .lines()
            .find_map(|l| l.strip_prefix("Nodes searched: "))
            .ok_or_else(|| format!("no perft total for {fen}"))?
            .trim();
        checked += 1;
        if got != expected {
            failures.push(format!("{fen} depth {depth}: got {got}, want {expected}"));
        }
    }

    for f in &failures {
        eprintln!("  {f}");
    }
    if let Some(refusal) = compared_something(checked, "positions", "tools/perft.table") {
        return Ok(refusal);
    }
    println!("perft: {} of {checked} positions match", checked - failures.len());
    Ok(Outcome::check(failures.is_empty(), format!("{} perft mismatches", failures.len())))
}

/// Refuse to re-derive a golden from the engine the golden is supposed to be checking.
///
/// **A golden regenerated from rfish is a photograph of rfish.** Whatever the binary does
/// today becomes the reference, including a bug, and the gate is green from then on. That is
/// not a hypothetical: `search.golden` once recorded a `bestmove` with no ponder move and
/// passed every run for as long as it existed, while upstream printed one.
/// `extract_ponder_from_tt` had never been ported, and no gate in this tree could have said
/// so — it surfaced only because someone drove the oracle by hand.
///
/// So the regenerator is `golden-audit --write`, which drives the ORACLE. This step stays
/// only for the case that genuinely cannot be driven through upstream, and that case has to
/// be claimed on purpose. Both sibling ports converged on the same refusal.
///
/// **The override is not a way past a red gate.** A `golden` failure is rfish disagreeing
/// with upstream; writing the disagreement into the reference deletes the finding.
pub(crate) fn golden_update() -> Result<Outcome, String> {
    const OVERRIDE: &str = "RFISH_GOLDEN_UPDATE_FROM_RFISH";
    if std::env::var(OVERRIDE).as_deref() != Ok("1") {
        eprintln!(
            "golden-update drives RFISH, so it writes a photograph of rfish, not a reference."
        );
        eprintln!();
        eprintln!("  Use the oracle-driven regenerator instead:");
        eprintln!("      cargo xtask golden-audit --write            # every case");
        eprintln!("      cargo xtask golden-audit --write <case>     # just one");
        eprintln!();
        eprintln!("  If a case genuinely cannot be driven through upstream, say so on purpose:");
        eprintln!("      {OVERRIDE}=1 cargo xtask golden-update");
        eprintln!("  and record WHY in the commit body.");
        return Ok(Outcome::Skipped(
            "refusing to write a golden from the engine under test".to_string(),
        ));
    }
    eprintln!("golden-update: writing goldens FROM RFISH by explicit override.");
    eprintln!("  Every golden written here is a photograph of this binary, not upstream's bytes.");
    golden(true)
}

/// The UCI case outputs must match `tools/*.golden`.
pub(crate) fn golden(update: bool) -> Result<Outcome, String> {
    let engine = build_engine(GATE_PROFILE)?;
    let cases_dir = workspace_root().join("tools/cases");
    let Ok(entries) = std::fs::read_dir(&cases_dir) else {
        return Ok(Outcome::Skipped(format!("{} does not exist", cases_dir.display())));
    };

    let mut cases: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "uci"))
        .collect();
    cases.sort();

    let mut failures = Vec::new();
    let mut skipped = 0usize;
    for case in &cases {
        let stem = case.file_stem().and_then(|s| s.to_str()).ok_or("a case has no name")?;
        let script =
            std::fs::read_to_string(case).map_err(|e| format!("{}: {e}", case.display()))?;
        if let Some(why) = missing_case_resource(&script) {
            println!("  {stem}: SKIPPED, {why}");
            skipped += 1;
            continue;
        }
        let lines: Vec<&str> = script.lines().filter(|l| !l.trim().is_empty()).collect();
        let out = drive(&engine, &lines)?;
        // Drop the lines that legitimately differ run to run, or every golden would be a
        // record of one machine's timing rather than of the engine's behaviour.
        let out = filter_volatile(&out);

        // A case that produced NOTHING is a dead engine, not a behaviour: every case here
        // ends by printing something, so a blank side means the run failed. Refused in both
        // modes, because the two failures compose — an update writes the blank golden, and
        // the check then passes it against the next dead run (blank == blank). ../zfish
        // a4f0b6e9 is the same shape one gate over.
        if out.trim().is_empty() {
            return Ok(Outcome::Fail(format!(
                "{stem}: the engine printed nothing, so there is no behaviour to \
                 {}. A blank golden would then match every future blank run",
                if update { "record" } else { "compare" }
            )));
        }

        let golden_path = workspace_root().join(format!("tools/{stem}.golden"));
        if update {
            std::fs::write(&golden_path, &out)
                .map_err(|e| format!("{}: {e}", golden_path.display()))?;
            println!("  {stem}.golden re-derived");
            continue;
        }
        match std::fs::read_to_string(&golden_path) {
            Ok(want) if want.replace('\r', "") == out => {}
            Ok(want) => {
                failures.push(stem.to_string());
                eprintln!("--- {stem}: first differing line ---");
                for (i, (a, b)) in want.lines().zip(out.lines()).enumerate() {
                    if a != b {
                        eprintln!("  line {}: golden {a:?}", i + 1);
                        eprintln!("  line {}: engine {b:?}", i + 1);
                        break;
                    }
                }
            }
            Err(_) => failures.push(format!("{stem} (no golden; run golden-update)")),
        }
    }

    if update {
        return Ok(Outcome::Pass);
    }
    // Every case skipped is the same hole as no case at all: the denominator below goes to
    // zero and the gate reports a comparison it never made.
    if let Some(refusal) = compared_something(cases.len() - skipped, "cases", "tools/cases/*.uci") {
        return Ok(refusal);
    }
    println!(
        "golden: {} of {} cases match{}",
        cases.len() - failures.len() - skipped,
        cases.len() - skipped,
        if skipped == 0 { String::new() } else { format!(" ({skipped} skipped)") }
    );
    Ok(Outcome::check(failures.is_empty(), format!("golden mismatches: {}", failures.join(", "))))
}

/// Drop the output lines whose content depends on the clock or the machine.
fn filter_volatile(out: &str) -> String {
    let mut kept = String::with_capacity(out.len());
    for line in out.lines() {
        // An `info` line is mostly DETERMINISTIC -- the depth, the score, the node count and
        // the PV are all properties of the search, and two engines running the same tree
        // must agree on them. Only the clock is not. Dropping the whole line, as this gate
        // did, meant no golden ever compared a SCORE or a PV against anything.
        if line.starts_with("info depth") {
            kept.push_str(&strip_clock(line));
            kept.push('\n');
            continue;
        }
        if line.starts_with("Total time")
            || line.starts_with("Nodes/second")
            || line.starts_with("Time:")
            || line.starts_with("info string NNUE")
            || line.starts_with("Compiled by")
            // The processor topology is a fact about the HOST, not about the engine.
            // Recording "0-15" would make the golden fail on every machine with a
            // different core count -- a golden that pins the runner rather than the code.
            // The engine's handling of it is covered by the numa unit tests instead.
            || line.starts_with("info string Available processors")
            || line.starts_with("info string Using ")
            || line.starts_with("info string NUMA threads are distributed")
        {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept
}

/// The resource a case needs and this checkout does not have, if any.
///
/// A case that sets `SyzygyPath` is meaningless without the tables: the engine reports
/// finding none and every line after it describes a different run. CI deliberately does not
/// fetch them -- `nnue-check` and `tb` are left out of the workflow for the same reason -- so
/// the case is SKIPPED and named, never silently passed and never failed for a file the
/// checkout was never going to have.
fn missing_case_resource(script: &str) -> Option<String> {
    const KEY: &str = "setoption name SyzygyPath value ";
    for line in script.lines() {
        let Some(value) = line.trim().strip_prefix(KEY) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let path = std::path::Path::new(value);
        let full = if path.is_absolute() { path.to_path_buf() } else { resources_dir().join(path) };
        if !full.is_dir() {
            return Some(format!("no tablebases at {}", full.display()));
        }
    }
    None
}

/// Every golden is what UPSTREAM produces for the same input, not merely what rfish does.
///
/// **A golden is a photograph of this engine, so it cannot see a divergence from upstream.**
/// `golden-update` records whatever the binary currently does, including a bug, and the gate
/// is green from then on. That is not hypothetical here: `search.golden` recorded a
/// `bestmove` with no ponder move and passed every run for as long as it existed, while
/// upstream printed one — `extract_ponder_from_tt` had never been ported, and the gate could
/// not have told anyone. It surfaced only because a resync moved the tree and someone drove
/// the oracle by hand.
///
/// Every case in `tools/cases/` is UCI-observable behaviour, so upstream's own binary
/// answers all of them. This gate asks it, and adjudicates the golden against the answer.
/// ../zfish reached the same conclusion and audits its goldens the same way.
///
/// The identity lines are excluded by design and by name: the two engines report different
/// `id name` and a different banner, which is intended and is the only difference a byte
/// comparison must forgive.
///
/// `--write` makes this gate the REGENERATOR too, which is the half `golden-update` cannot
/// be: it writes upstream's bytes, so a re-derived golden is still a reference rather than a
/// photograph of whatever rfish does today. `--write <case>…` writes only the named cases.
pub(crate) fn golden_audit(args: &[&str]) -> Result<Outcome, String> {
    let write = args.contains(&"--write");
    let only: Vec<&str> = args.iter().copied().filter(|a| !a.starts_with("--")).collect();
    let Some(oracle) = find_oracle() else {
        return Ok(Outcome::Skipped("no upstream build to adjudicate against".to_string()));
    };
    // Probe the shell by RUNNING it: `sh --version` is a bashism and dash exits non-zero,
    // so the usual `have` check would report no shell on a Debian-family box.
    let shell = Command::new("sh").args(["-c", "exit 0"]).status().is_ok_and(|s| s.success());
    if !shell {
        return Ok(Outcome::Skipped("the oracle driver needs a shell".to_string()));
    }
    let oracle_dir = oracle.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
    let cases_dir = workspace_root().join("tools/cases");
    let Ok(entries) = std::fs::read_dir(&cases_dir) else {
        return Ok(Outcome::Skipped(format!("{} does not exist", cases_dir.display())));
    };
    let mut cases: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "uci"))
        .collect();
    cases.sort();

    // Only `--write` needs rfish itself, and it needs it for ONE thing: the identity lines.
    // A golden holds this engine's banner and `id name`, the audit strips both before
    // comparing, and a golden written straight from upstream's stdout would swap them for
    // upstream's — leaving `golden` red against a file the audit calls correct.
    let engine = if write { Some(build_engine(GATE_PROFILE)?) } else { None };

    let (mut agree, mut differ) = (0usize, Vec::new());
    let (mut wrote, mut missing) = (Vec::new(), Vec::new());
    let mut skipped: Vec<String> = Vec::new();
    for case in &cases {
        let stem = case.file_stem().and_then(|s| s.to_str()).ok_or("a case has no name")?;
        if !only.is_empty() && !only.contains(&stem) {
            continue;
        }
        let golden_path = workspace_root().join(format!("tools/{stem}.golden"));
        // A case with no golden is MISSING when auditing and a NEW GOLDEN when writing.
        // Silently continuing was the hole: adding a case left the audit with nothing to
        // adjudicate and said so nowhere, so the only way to seed the golden was the step
        // that drives rfish — exactly the self-photograph this gate exists to prevent.
        let want = match std::fs::read_to_string(&golden_path) {
            Ok(text) => text,
            Err(_) if write => String::new(),
            Err(_) => {
                missing.push(stem.to_string());
                continue;
            }
        };
        let script =
            std::fs::read_to_string(case).map_err(|e| format!("{}: {e}", case.display()))?;
        if let Some(why) = missing_case_resource(&script) {
            skipped.push(format!("{stem} ({why})"));
            continue;
        }
        let lines: Vec<&str> = script.lines().filter(|l| !l.trim().is_empty()).collect();

        let upstream_raw = filter_volatile(&drive_oracle(&oracle, &oracle_dir, &lines)?);
        if let Some(engine) = &engine {
            let ours_raw = filter_volatile(&drive(engine, &lines)?);
            let (spliced, diverged) = splice_identity(&upstream_raw, &ours_raw)
                .map_err(|e| format!("{stem}: refusing to write — {e}"))?;
            std::fs::write(&golden_path, &spliced)
                .map_err(|e| format!("{}: {e}", golden_path.display()))?;
            if diverged {
                println!(
                    "  {stem}.golden written from upstream — CONTENT DIVERGED, so `golden` \
                     will be red until rfish reproduces it"
                );
            } else {
                println!("  {stem}.golden written from upstream");
            }
            wrote.push(stem.to_string());
            continue;
        }
        let theirs = strip_identity(&upstream_raw);
        let ours = strip_identity(&want.replace('\r', ""));
        // **TWO BLANK SIDES COMPARE EQUAL**, and every way a side can fail blanks it: an
        // oracle that dies before its banner, a driver whose filter eats the output, a
        // golden re-derived against a dead engine. That scores an `agree` — the gate does
        // not merely pass having compared nothing, it reports a comparison it never made.
        // ../zfish a4f0b6e9 found the same equality in its transcript gate. A rig fault, not
        // a diff, and checked BEFORE the tally either side lands in.
        if nothing_was_compared(&ours, &theirs) {
            return Ok(Outcome::Fail(format!(
                "{stem}: both the golden and upstream are blank, so nothing was compared. \
                 Check the oracle runs and that the golden was not re-derived against a dead \
                 engine"
            )));
        }
        if ours == theirs {
            agree += 1;
            continue;
        }
        differ.push(stem.to_string());
        eprintln!("--- {stem}: the golden and upstream disagree ---");
        for (i, (a, b)) in ours.lines().zip(theirs.lines()).enumerate() {
            if a != b {
                eprintln!("  line {}: golden   {a:?}", i + 1);
                eprintln!("  line {}: upstream {b:?}", i + 1);
                break;
            }
        }
        if ours.lines().count() != theirs.lines().count() {
            eprintln!(
                "  golden has {} lines, upstream {}",
                ours.lines().count(),
                theirs.lines().count()
            );
        }
    }

    if write {
        // Writing NOTHING is the same refusal as comparing nothing: a `--write case-typo`
        // that matched no case would otherwise report success having touched no file.
        if wrote.is_empty() {
            return Ok(Outcome::Fail(format!(
                "wrote no golden at all: {} matched no case in tools/cases/*.uci, so this \
                 re-derived nothing. Name a case that exists rather than reading the pass",
                if only.is_empty() { "the corpus".to_string() } else { only.join(", ") }
            )));
        }
        println!("golden-audit --write: {} golden(s) re-derived FROM UPSTREAM", wrote.len());
        println!("  Run `cargo xtask golden` next: it is what asserts rfish reproduces them.");
        return Ok(Outcome::Pass);
    }
    if let Some(refusal) = compared_something(agree + differ.len(), "goldens", "tools/cases/*.uci")
    {
        return Ok(refusal);
    }
    for stem in &missing {
        eprintln!("  {stem}: no tools/{stem}.golden — seed it with `golden-audit --write {stem}`");
    }
    println!(
        "golden-audit: {agree} agree, {} differ, {} not answerable ({})",
        differ.len(),
        skipped.len(),
        skipped.join(", ")
    );
    Ok(Outcome::check(
        differ.is_empty() && missing.is_empty(),
        format!(
            "goldens upstream does not agree with: {}; cases with no golden: {}",
            differ.join(", "),
            missing.join(", ")
        ),
    ))
}

/// Upstream's content, carrying rfish's own identity lines in rfish's own places.
///
/// The two engines are required to agree on everything a golden holds EXCEPT which engine
/// answered — the banner, `id name`, `id author`, and the network-replica line rfish cannot
/// have. `strip_identity` drops exactly those before the audit compares, so a golden written
/// from upstream's stdout verbatim would be adjudicated correct and would still fail
/// `golden`, which drives rfish and compares bytes.
///
/// **The identity lines are not one-for-one, and measuring that was the surprise.** Upstream
/// prints `info string Network replica 1: Shared memory.` where rfish prints nothing at all
/// for `eval`, and in `benchmodes` it prints its replica line AFTER the position header where
/// rfish prints it before. `strip_identity` drops both, so neither the count nor the position
/// is compared by anything — which is what makes substituting them positionally wrong.
///
/// So take the NON-IDENTITY lines from upstream, in order, and let rfish decide where its own
/// identity lines sit among them. Every line the audit compares is then upstream's by
/// construction, and every line it ignores is rfish's. When the two engines agree the result
/// is byte-identical to rfish's own output, which is the round trip this is verified by.
///
/// When they DISAGREE the content counts differ and the identity placement becomes
/// approximate — say so, and expect `golden` to be red afterwards. That is the tool working:
/// the reference now holds upstream's behaviour and rfish does not reproduce it.
fn splice_identity(theirs: &str, ours: &str) -> Result<(String, bool), String> {
    // Nothing to splice is a rig fault, not a divergence: a relative oracle path, a missing
    // net or a worktree mid-checkout all surface as an empty or truncated capture, and
    // writing THAT would replace a reference with noise while reporting success. Both sides
    // are checked — a dead rfish would place every identity line wrongly just as silently.
    if theirs.trim().is_empty() {
        return Err("the oracle produced no output at all".to_string());
    }
    if ours.trim().is_empty() {
        return Err("rfish produced no output at all".to_string());
    }

    let mut content = theirs.lines().filter(|l| !is_identity(l));
    let mut out = String::with_capacity(theirs.len());
    for line in ours.lines() {
        if is_identity(line) {
            out.push_str(line);
            out.push('\n');
        } else if let Some(upstream) = content.next() {
            out.push_str(upstream);
            out.push('\n');
        }
    }
    // Upstream said more than rfish did. Those lines are content the reference must carry, so
    // they go at the end rather than being dropped -- a truncated reference would make the
    // audit green over a divergence it had just written away.
    let mut trailing = false;
    for upstream in content {
        trailing = true;
        out.push_str(upstream);
        out.push('\n');
    }
    let ours_content = ours.lines().filter(|l| !is_identity(l)).count();
    let theirs_content = theirs.lines().filter(|l| !is_identity(l)).count();
    Ok((out, trailing || ours_content != theirs_content))
}

/// True when a comparison had nothing on EITHER side, which equality reports as agreement.
///
/// Named and separate so it can be tested: the end-to-end path needs an engine that dies
/// before printing, and nothing in this corpus can produce one — every case here ends by
/// printing something, and even an empty script gets the net banner.
fn nothing_was_compared(ours: &str, theirs: &str) -> bool {
    ours.trim().is_empty() && theirs.trim().is_empty()
}

/// Drive the oracle, WAITING for each search to finish before sending the next command.
///
/// Upstream runs `go` on its own thread and treats end of input as `quit`, so writing every
/// line at once and closing the pipe stops the search early and collects a `bestmove` from a
/// search that never finished. That reads as a divergence and is not one — it cost real time
/// in the `c5aef2bf1` resync before it was spotted. Everything else upstream does is
/// synchronous, so only `go` needs the wait.
///
/// Standard error is merged in through a shell, because upstream writes part of its output
/// there and a stdout-only capture would silently drop it.
pub(crate) fn drive_oracle(
    oracle: &std::path::Path,
    cwd: &std::path::Path,
    lines: &[&str],
) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::sync::mpsc;
    use std::time::Duration;

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(format!("exec {} 2>&1", shell_quote(&oracle.to_string_lossy())))
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", oracle.display()))?;

    let mut stdin = child.stdin.take().ok_or("the oracle has no standard input")?;
    let stdout = child.stdout.take().ok_or("the oracle has no output")?;

    // Read on a thread and hand lines over a channel, so every wait below can carry a
    // DEADLINE. A command that never reports -- `go perft` prints no bestmove, and a
    // malformed `go` in the error cases prints nothing at all -- would otherwise block the
    // gate forever, which is how the first version of this hung.
    let (tx, rx) = mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        loop {
            let mut got = String::new();
            match r.read_line(&mut got) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(got).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let mut out = String::new();
    for line in lines {
        // Point the oracle at the SAME tables. Every case runs from the engine's own
        // directory, so `SyzygyPath value syzygy` is relative -- and relative to the ORACLE
        // it resolves to upstream's own `src/syzygy/`, which holds source files and no
        // tables. The oracle then reports finding none and the case reads as a divergence
        // that is really a rig with no tablebases in it.
        let line = &rewrite_syzygy_path(line);
        // A case may end the oracle -- the error cases feed it `quit` among other things --
        // and writing past that is a broken pipe, not a failure of the audit. Stop feeding
        // it and keep whatever it said.
        if writeln!(stdin, "{line}").and_then(|()| stdin.flush()).is_err() {
            break;
        }
        if !line.trim_start().starts_with("go") {
            continue;
        }
        // Wait for this search to report before sending the next command: upstream runs
        // `go` on its own thread and treats end of input as `quit`, so writing everything at
        // once truncates the search and collects a bestmove from one that never finished.
        while let Ok(got) = rx.recv_timeout(Duration::from_secs(20)) {
            let done = got.starts_with("bestmove") || got.starts_with("Nodes searched");
            out.push_str(&got);
            if done {
                break;
            }
        }
    }
    let _ = writeln!(stdin, "quit");
    drop(stdin);
    while let Ok(got) = rx.recv_timeout(Duration::from_secs(10)) {
        out.push_str(&got);
    }
    let _ = child.wait();
    let _ = reader.join();
    Ok(out.replace('\r', ""))
}

/// Make a relative `SyzygyPath` absolute, against this repository's `resources/`.
///
/// Left alone when the case names an absolute path or sets something else.
fn rewrite_syzygy_path(line: &str) -> String {
    const KEY: &str = "setoption name SyzygyPath value ";
    let Some(value) = line.strip_prefix(KEY) else {
        return line.to_string();
    };
    let value = value.trim();
    if value.is_empty() || std::path::Path::new(value).is_absolute() {
        return line.to_string();
    }
    format!("{KEY}{}", resources_dir().join(value).display())
}

/// Quote a path for the shell that merges the oracle's two output streams.
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// Remove the fields of an `info` line that a clock decides.
///
/// `time` and `nps` differ run to run on the same binary, and `hashfull` is a percentage of a
/// table whose fill depends on them. Everything else -- depth, seldepth, multipv, score,
/// nodes, tbhits, the PV -- is a fact about the search and is kept, so a golden pins it.
fn strip_clock(line: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut it = line.split_whitespace();
    while let Some(tok) = it.next() {
        if matches!(tok, "time" | "nps" | "hashfull") {
            it.next();
            continue;
        }
        kept.push(tok);
    }
    kept.join(" ")
}

/// Drop the lines that name WHICH engine answered.
///
/// The banner and `id name` are the port's own, deliberately: everything else in a golden is
/// behaviour the two engines are required to share.
fn strip_identity(out: &str) -> String {
    let mut kept = String::with_capacity(out.len());
    for line in out.lines() {
        if is_identity(line) {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept
}

/// True for a line that names WHICH engine answered.
///
/// One predicate, two readers: the audit strips these before comparing, and `--write`
/// substitutes rfish's for upstream's. Two lists would drift, and the drift would be silent
/// in the direction that matters — a line stripped by one and spliced by the other produces a
/// golden the audit accepts and `golden` rejects.
fn is_identity(line: &str) -> bool {
    line.starts_with("id name ")
        || line.starts_with("id author ")
        || line.starts_with("Stockfish ")
        || line.starts_with("rfish ")
        // rfish cannot replicate the network: replication follows thread PINNING, which has
        // no filesystem equivalent, so there is one shared copy. AGENTS.md records that as
        // deliberate, and upstream's line now names the shared-memory implementation rfish
        // equally cannot have. Filtered BY NAME, so the exemption stays visible instead of
        // being absorbed into a wildcard.
        || line.starts_with("info string Network replica")
}

/// Documentation rot: every link resolves, and every named repository path exists.
///
/// This settles the mechanical half. It CANNOT tell you a sentence has become false —
/// that part is yours, and every false claim ever found in the sibling ports' docs got
/// there by a commit that changed the code and not the page.
///
/// A path claim is resolved against the TREE rather than the working directory, because the
/// working directory is not what a reader gets. `.exists()` reads whatever the developer
/// happens to have lying around, so a doc naming a file that only ever exists locally passes
/// here and fails in CI — which is how the reference to the per-machine
/// `tools/instr_budget.golden` reached `main` green.
pub(crate) fn docs_lint() -> Result<Outcome, String> {
    let root = workspace_root();
    let mut problems = Vec::new();
    let mut checked = 0;
    let tracked = tracked_paths(&root)?;

    let mut files = Vec::new();
    collect_markdown(&root, &mut files)?;

    for file in &files {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let rel = file.strip_prefix(&root).unwrap_or(file);
        for (n, line) in text.lines().enumerate() {
            for target in markdown_link_targets(line) {
                // External links and anchors are not ours to resolve.
                if target.starts_with("http")
                    || target.starts_with('#')
                    || target.starts_with("mailto:")
                {
                    continue;
                }
                checked += 1;
                let target_path = target.split('#').next().unwrap_or(target);
                let base = file.parent().unwrap_or(&root);
                if !base.join(target_path).exists() && !root.join(target_path).exists() {
                    problems.push(format!("{}:{}: dead link {target}", rel.display(), n + 1));
                }
            }
            // A doc that names a `crates/` or `tools/` path is making a claim about the
            // tree; check it the same way.
            for word in line.split(|c: char| c.is_whitespace() || "`(),;\"".contains(c)) {
                let word = word.trim_end_matches(['.', ':']);
                // Skip anything holding a placeholder: prose legitimately writes
                // `tools/<name>.golden` and `crates/…`, and neither names a real file.
                let placeholder = word.contains(['*', '<', '>', '\u{2026}']);
                if (word.starts_with("crates/") || word.starts_with("tools/"))
                    && !placeholder
                    && !word.ends_with('/')
                {
                    checked += 1;
                    if !in_tree(&tracked, word) && !deliberately_untracked(&root, word) {
                        problems.push(format!(
                            "{}:{}: names a path the tree does not carry: {word}",
                            rel.display(),
                            n + 1
                        ));
                    }
                }
            }
        }
    }

    // The two rules `docs/12-writing.md` names as the most-broken, held mechanically.
    checked += quoted_signature(&root, &files, &mut problems)?;
    checked += undocumented_steps(&root, &files, &mut problems)?;

    // And the class the path check above cannot reach: a reference into the internal area,
    // which is gitignored and therefore lands in that check's deliberately-untracked
    // exemption. The subject is the whole INDEX, not the markdown set — a source comment
    // dangles for a reader exactly as a doc line does, and both sibling ports had this rule
    // broken by a file class their checker never read.
    let leaks = crate::devsweep::sweep(&root, &tracked)?;
    checked += tracked.len();
    problems.extend(leaks);

    for p in &problems {
        eprintln!("  {p}");
    }
    println!("docs-lint: {checked} references checked across {} files", files.len());
    Ok(Outcome::check(problems.is_empty(), format!("{} documentation problems", problems.len())))
}

/// Every path a fresh checkout carries, which is the only tree a reader or CI ever has.
fn tracked_paths(root: &std::path::Path) -> Result<std::collections::BTreeSet<String>, String> {
    let out = capture(Command::new("git").current_dir(root).arg("ls-files"))?;
    Ok(out.lines().map(str::to_string).collect())
}

/// True when the tree carries `word`, either as a file or as a directory holding one.
fn in_tree(tracked: &std::collections::BTreeSet<String>, word: &str) -> bool {
    if tracked.contains(word) {
        return true;
    }
    // A directory is not itself an entry, so match it by the files underneath. The set is
    // ordered, so the first entry at or after `word/` settles it without a scan.
    let dir = format!("{word}/");
    tracked.range(dir.clone()..).next().is_some_and(|p| p.starts_with(&dir))
}

/// True when `.gitignore` names `word`.
///
/// An ignored path is one the repository has DECIDED not to carry — `tools/instr_budget.golden`
/// is per-machine, because a retired-instruction count is toolchain- and CPU-specific — so a
/// doc naming it is documenting the tool that writes it, not making a false claim about the
/// tree. Absent from a checkout and absent from `.gitignore` is the rot this gate is for.
fn deliberately_untracked(root: &std::path::Path, word: &str) -> bool {
    run(Command::new("git").current_dir(root).args(["check-ignore", "-q", word])).is_ok()
}

/// Refuse the current bench anchor written into prose.
///
/// The number `signature` computes is the one the "never pin a number a gate computes" rule
/// is most often broken with, and a stale anchor in a page tells a reader to hold the wrong
/// invariant. Only THIS number is held — a node count, an instruction total or a case count
/// in prose passes cleanly and is just as stale.
fn quoted_signature(
    root: &std::path::Path,
    files: &[std::path::PathBuf],
    problems: &mut Vec<String>,
) -> Result<usize, String> {
    let path = root.join("tools/signature.golden");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(0);
    };
    let Some(anchor) =
        text.lines().find(|l| !l.trim_start().starts_with('#') && !l.trim().is_empty())
    else {
        return Ok(0);
    };
    let anchor = anchor.trim();
    let mut checked = 0;
    for file in files {
        let body = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let rel = file.strip_prefix(root).unwrap_or(file);
        for (n, line) in body.lines().enumerate() {
            checked += 1;
            if line.contains(anchor) {
                problems.push(format!(
                    "{}:{}: quotes the bench anchor; cite `cargo xtask signature` instead",
                    rel.display(),
                    n + 1
                ));
            }
        }
    }
    Ok(checked)
}

/// Refuse an `xtask` step no shipped page names.
///
/// A step nobody can discover is a step nobody runs, and both sibling ports gate the same
/// property. The dispatch table is the owner: parse it rather than keeping a second list
/// here, or this check rots the way the prose it guards does.
fn undocumented_steps(
    root: &std::path::Path,
    files: &[std::path::PathBuf],
    problems: &mut Vec<String>,
) -> Result<usize, String> {
    let main = root.join("crates/xtask/src/main.rs");
    let text = std::fs::read_to_string(&main).map_err(|e| format!("{}: {e}", main.display()))?;
    let mut prose = String::new();
    for file in files {
        prose.push_str(
            &std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?,
        );
    }

    let mut checked = 0;
    for line in text.lines() {
        let line = line.trim();
        // `"step" => …` in the dispatch table, and nothing else.
        let Some(rest) = line.strip_prefix('"') else {
            continue;
        };
        let Some((step, tail)) = rest.split_once('"') else {
            continue;
        };
        if !tail.trim_start().starts_with("=>") || step.starts_with('-') || step == "help" {
            continue;
        }
        checked += 1;
        if !prose.contains(step) {
            problems.push(format!(
                "crates/xtask/src/main.rs: `{step}` is a step no shipped page names"
            ));
        }
    }
    Ok(checked)
}

fn collect_markdown(
    dir: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // The shipped set is what a fresh clone carries, so skip anything the repository has
        // decided not to carry — the build tree, the runtime inputs, the internal working
        // area. Ask `.gitignore` rather than naming those directories here: a list of names
        // rots, and one of the names is the very location no shipped file may spell.
        // `target/` stays named as well as ignored: outside a checkout `check-ignore` cannot
        // answer, and walking a build tree full of vendored markdown is the one skip that
        // must hold even then.
        if name.starts_with('.')
            || name == "target"
            || (path.is_dir() && deliberately_untracked(dir, &name))
        {
            continue;
        }
        if path.is_dir() {
            collect_markdown(&path, out)?;
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    Ok(())
}

/// Every `[text](target)` on a line.
fn markdown_link_targets(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b']'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'('
            && let Some(end) = line[i + 2..].find(')')
        {
            let target = &line[i + 2..i + 2 + end];
            if !target.is_empty() {
                out.push(target);
            }
            i += 2 + end;
            continue;
        }
        i += 1;
    }
    out
}

/// The differential Syzygy gate: rfish's WDL verdict and DTZ distance must equal a pristine
/// upstream build's, position by position.
///
/// A tablebase answer is exact, so "close" is meaningless: an index computed one off reads a
/// different position's entry and returns a confident wrong verdict. Only a differential
/// comparison catches that, which is why this gate drives both engines rather than pinning
/// rfish's own output in a golden.
///
/// A position where upstream declines to answer — an illegal one, where the side not to move
/// is in check — is skipped by both sides rather than counted as a mismatch.
///
/// Needs the 3-man table set and an upstream binary; either missing is a SKIP.
pub(crate) fn tb() -> Result<Outcome, String> {
    let engine = build_engine(GATE_PROFILE)?;
    let dir = resources_dir().join("syzygy");
    if !dir.join("KQvK.rtbw").is_file() {
        return Ok(Outcome::Skipped(format!(
            "{} holds no tables; fetch the 3-man set into it",
            dir.display()
        )));
    }

    // Discovery first, and the property that makes a missing path safe: no tables, no
    // effect on the search, so the bench signature cannot move.
    let set = format!("setoption name SyzygyPath value {}", dir.display());
    let found = drive(&engine, &[&set, "isready"])?;
    if !found.contains("Found 5 WDL and 5 DTZ tablebase files (up to 3-man).") {
        return Ok(Outcome::Fail(format!("discovery failed in {}", dir.display())));
    }
    let empty = drive(&engine, &["setoption name SyzygyPath value <empty>", "isready"])?;
    if empty.contains("Found ") {
        return Ok(Outcome::Fail("an empty SyzygyPath still reported tablebases".to_string()));
    }

    let Some(oracle) = find_oracle() else {
        println!("tb: discovery green; the differential half needs an upstream build");
        return Ok(Outcome::Skipped(
            "no upstream binary; build ../Stockfish/src with `make -j build ARCH=x86-64-avx2`"
                .to_string(),
        ));
    };
    let oracle_dir = oracle.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
    let oracle_set = format!("setoption name SyzygyPath value {}", dir.display());

    let path = workspace_root().join("tools/cases/tb.fens");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Outcome::Skipped(format!("{} does not exist", path.display())));
    };

    // One invocation each, for the same reason as `nnue-check`: a per-position spawn reloads
    // the network 264 times and turns a seconds-long gate into a minutes-long one.
    let fens: Vec<&str> =
        text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    let mut script = vec![set.clone()];
    for fen in &fens {
        script.push(format!("position fen {fen}"));
        script.push("d".to_string());
    }
    let refs: Vec<&str> = script.iter().map(String::as_str).collect();
    let mut oracle_script = vec![oracle_set];
    oracle_script.extend(script[1..].iter().cloned());
    let oracle_refs: Vec<&str> = oracle_script.iter().map(String::as_str).collect();

    let ours = tb_verdict(&drive(&engine, &refs)?);
    let theirs = tb_verdict(&drive_at(&oracle, &oracle_dir, &oracle_refs)?);

    if ours.len() != theirs.len() {
        return Ok(Outcome::Fail(format!(
            "the two engines answered a different number of probes: {} vs {}",
            ours.len(),
            theirs.len()
        )));
    }
    let mut checked = 0;
    let mut failures = Vec::new();
    for (i, (a, b)) in ours.iter().zip(theirs.iter()).enumerate() {
        checked += 1;
        if a != b {
            failures.push(format!("probe {i}: rfish {a} != upstream {b}"));
        }
    }

    for f in failures.iter().take(8) {
        eprintln!("  {f}");
    }
    if let Some(refusal) = compared_something(checked, "probes", "tools/cases/tb.fens") {
        return Ok(refusal);
    }
    println!(
        "tb: {} of {checked} probes match upstream ({} positions x WDL and DTZ)",
        checked - failures.len(),
        checked / 2
    );
    Ok(Outcome::check(failures.is_empty(), format!("{} tablebase mismatches", failures.len())))
}

/// The WDL verdict and DTZ distance an engine's `d` output reported, in order.
fn tb_verdict(out: &str) -> Vec<i64> {
    out.lines()
        .filter(|l| {
            l.trim_start().starts_with("Tablebases WDL:")
                || l.trim_start().starts_with("Tablebases DTZ:")
        })
        .filter_map(|l| {
            // Upstream appends a parenthesised rank; take the first number only.
            l.split(':').nth(1)?.split_whitespace().next()?.parse().ok()
        })
        .collect()
}

/// The differential NNUE gate: rfish's raw network output must equal upstream's, exactly.
///
/// This is the gate that says the evaluation is a PORT rather than an approximation. It
/// drives both engines over the same positions and compares the number upstream's `eval`
/// prints as "internal units" — the network alone, with no optimism blend and no fifty-move
/// damping on top, so a mismatch localises to the forward pass rather than to the terms
/// around it.
///
/// Requires a built upstream binary and a net. Either missing is a SKIP, not a failure: a
/// contributor without a 90 MiB download still gets every other gate.
pub(crate) fn nnue_check() -> Result<Outcome, String> {
    let engine = build_engine(GATE_PROFILE)?;
    let net = resources_dir().join(default_net_name());
    if !net.is_file() {
        return Ok(Outcome::Skipped(format!("{} is absent; run `cargo xtask net`", net.display())));
    }
    let Some(oracle) = find_oracle() else {
        return Ok(Outcome::Skipped(
            "no upstream binary; build ../Stockfish/src with `make -j build ARCH=x86-64-avx2`"
                .to_string(),
        ));
    };
    let oracle_dir = oracle.parent().map(std::path::Path::to_path_buf).unwrap_or_default();

    let path = workspace_root().join("tools/cases/eval.fens");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(Outcome::Skipped(format!("{} does not exist", path.display())));
    };

    // ONE engine invocation for the whole battery, not one per position. Each start reads a
    // 90 MiB network, so a per-position spawn spends minutes reloading the same file and
    // turns this into a gate people skip.
    let fens: Vec<&str> =
        text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    let mut script = Vec::with_capacity(fens.len() * 2);
    for fen in &fens {
        script.push(format!("position fen {fen}"));
        script.push("eval".to_string());
    }
    let refs: Vec<&str> = script.iter().map(String::as_str).collect();
    let ours = internal_units_all(&drive(&engine, &refs)?);
    let theirs = internal_units_all(&drive_at(&oracle, &oracle_dir, &refs)?);

    let mut checked = 0;
    let mut failures = Vec::new();
    // Upstream refuses to evaluate a position in check, so it emits fewer lines than there
    // are positions. Compare by count only when the two agree on how many they answered.
    if ours.len() != theirs.len() {
        return Ok(Outcome::Fail(format!(
            "the two engines answered a different number of positions: {} vs {}",
            ours.len(),
            theirs.len()
        )));
    }
    for (i, (a, b)) in ours.iter().zip(theirs.iter()).enumerate() {
        checked += 1;
        if a != b {
            let fen = fens.get(i).copied().unwrap_or("?");
            failures.push(format!("{fen}: rfish {a} != upstream {b}"));
        }
    }

    for f in failures.iter().take(8) {
        eprintln!("  {f}");
    }
    if let Some(refusal) = compared_something(checked, "positions", "tools/cases/eval.fens") {
        return Ok(refusal);
    }
    println!(
        "nnue-check: {} of {checked} positions match upstream exactly",
        checked - failures.len()
    );
    Ok(Outcome::check(failures.is_empty(), format!("{} evaluation mismatches", failures.len())))
}

/// The net survives `export_net` byte for byte, so the reader and the writer agree.
///
/// **This is the gate for a layout stated twice.** `FeatureTransformer::read` and
/// `FeatureTransformer::write` are two independent statements of one order — eight operations
/// each, the same split points and the same choice of LEB128 against raw `i8`, mirrored by
/// hand forty lines apart — and `AffineLayer` and `Network::save` repeat the shape. Nothing in
/// the source relates either pair. Edit one side alone and the writer emits a file the reader
/// misreads, which fails in the worst available way: the net still loads, every other gate
/// still runs, and only the bench signature notices, and only to say that something moved.
///
/// rfish reads a net as a SEQUENTIAL STREAM — `NetReader` is `read_exact` over an
/// `impl Read`, with no mapped image and no computed region offsets — so it cannot have the
/// defect the sibling ports fixed, where the parse writes each region at one derivation of the
/// blob layout and the accessors read it back at another (`../mcfish` `ef3c0170`, `../zfish`
/// `0f66e726`, both closed with asserts tying the two spellings together). The hazard survives
/// the difference: an ORDER stated twice drifts exactly as offsets do. What changes is the
/// instrument. There are no two constants to assert equal, so the weld is the round trip
/// itself, which is stronger — it checks every group, every split point and every hash at once
/// rather than the derivations someone thought to pair up.
///
/// `docs/07-shell.md` has claimed the trip is byte-identical since the format was ported, and
/// no command produced that claim. A page that states a fact no gate computes is the shape
/// this tree keeps finding; this makes it a command.
///
/// A missing net is a SKIP, as it is for `nnue-check`: a contributor without the download
/// still gets every other gate.
pub(crate) fn net_roundtrip() -> Result<Outcome, String> {
    let engine = build_engine(GATE_PROFILE)?;
    let name = default_net_name();
    let net = resources_dir().join(&name);
    if !net.is_file() {
        return Ok(Outcome::Skipped(format!("{} is absent; run `cargo xtask net`", net.display())));
    }

    // Written OUTSIDE `resources/`, so a half-written file can never be mistaken for the net
    // the engine loads on the next run.
    let out = workspace_root().join("target/net-roundtrip.nnue");
    let _ = std::fs::remove_file(&out);
    let script = [format!("export_net {}", out.display())];
    let refs: Vec<&str> = script.iter().map(String::as_str).collect();
    let said = drive(&engine, &refs)?;
    if !said.contains("Network saved") {
        return Ok(Outcome::Fail(format!(
            "`export_net` wrote nothing: {}",
            said.lines().find(|l| l.contains("info string")).unwrap_or("no answer at all")
        )));
    }

    let original = std::fs::read(&net).map_err(|e| format!("{}: {e}", net.display()))?;
    let written = std::fs::read(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let _ = std::fs::remove_file(&out);

    // Report WHERE, not just that: the first differing byte names the region, and a length
    // that matches while the bytes do not is a different bug from a short write.
    if original.len() != written.len() {
        return Ok(Outcome::Fail(format!(
            "{name} came back {} bytes, not {}",
            written.len(),
            original.len()
        )));
    }
    if let Some(at) = original.iter().zip(&written).position(|(a, b)| a != b) {
        return Ok(Outcome::Fail(format!(
            "{name} differs at byte {at} of {}: {:#04x} was written as {:#04x}",
            original.len(),
            original[at],
            written[at]
        )));
    }

    // A zero-length net compares equal to itself. Refuse the empty subject rather than
    // reporting the pass it would otherwise earn.
    if let Some(refusal) = compared_something(original.len(), "bytes", &net.to_string_lossy()) {
        return Ok(refusal);
    }
    println!("net-roundtrip: {name} survived export_net byte for byte ({} bytes)", original.len());
    Ok(Outcome::Pass)
}

/// Every raw network output an `eval` printed, in upstream's internal units, in order.
fn internal_units_all(out: &str) -> Vec<i64> {
    out.lines()
        .filter(|l| l.trim_start().starts_with("NNUE evaluation"))
        .filter_map(|l| l.split_whitespace().nth(2)?.trim_start_matches('+').parse().ok())
        .collect()
}

/// A pristine upstream binary **built at `UPSTREAM_BASE`**, if one exists.
///
/// The SHA check is the whole point, not a formality. Upstream stamps its own short SHA into
/// `id name`, and a differential gate against a binary from a DIFFERENT commit compares the
/// new engine against the old upstream and reports a clean pass — silently, because nothing
/// in the output says which upstream answered.
///
/// That is not hypothetical. This directory held a `stockfish-new` from the previous pin
/// benching 3,184,328 beside a current `stockfish` benching 2,508,687, and because the stale
/// name sorted first, `nnue-check`, `tb` and the golden audit all adjudicated against the
/// wrong commit while passing. `docs/10-tooling-ci.md` had recorded the trap one commit
/// earlier and it still bit, because the stale binary was under a name nobody was looking at.
pub(crate) fn find_oracle() -> Option<std::path::PathBuf> {
    let src = workspace_root().parent()?.join("Stockfish/src");
    let base = std::fs::read_to_string(workspace_root().join("tools/upstream/UPSTREAM_BASE"))
        .ok()?
        .trim()
        .to_string();
    let short = base.get(..8)?.to_string();
    ["stockfish-new", "stockfish"]
        .iter()
        .map(|n| src.join(n))
        .filter(|p| p.is_file())
        .find(|p| oracle_stamp(p).is_some_and(|id| id.contains(&short)))
}

/// The `id name` an upstream binary announces, which carries the commit it was built from.
fn oracle_stamp(oracle: &std::path::Path) -> Option<String> {
    let dir = oracle.parent()?;
    let out = drive_at(oracle, dir, &["uci"]).ok()?;
    out.lines().find(|l| l.starts_with("id name ")).map(str::to_string)
}

/// The net name the engine looks for, read from its own constant.
fn default_net_name() -> String {
    let path = workspace_root().join("crates/rfish-engine/src/eval/nnue/mod.rs");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.trim_start().starts_with("pub const DEFAULT_NET"))
                .and_then(|l| l.split('"').nth(1).map(str::to_string))
        })
        .unwrap_or_default()
}

/// The port's defining constraint: no `unsafe` anywhere, and no way to re-enable it.
///
/// `#![forbid(unsafe_code)]` in the workspace manifest already makes the compiler reject
/// both. This gate exists because a manifest edit is one line and a reviewer can miss it —
/// so the property is asserted from outside the thing that enforces it.
pub(crate) fn unsafe_lint() -> Result<Outcome, String> {
    let root = workspace_root();
    let mut problems = Vec::new();
    // The SHIPPED crates only. `xtask` is a build tool that never enters the binary, and
    // it necessarily names the patterns it looks for -- scanning it would make the gate
    // report itself. It is still covered by the workspace `forbid`, which the manifest
    // check below asserts is in place.
    let mut files = Vec::new();
    for crate_dir in ["rfish-engine", "rfish"] {
        collect_rust(&root.join("crates").join(crate_dir), &mut files);
    }

    for file in &files {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let rel = file.strip_prefix(&root).unwrap_or(file);
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or(line);
            // Match the KEYWORD, not the word: this file's own help text names
            // `unsafe_code`, and a doc sentence about the constraint is not a violation of
            // it. An `allow`/`expect` of the lint is, because that is how the forbid would
            // be worked around.
            let keyword = ["unsafe fn", "unsafe {", "unsafe impl", "unsafe trait", "unsafe extern"]
                .iter()
                .any(|k| code.contains(k));
            let opt_out = code.contains("allow(unsafe_code")
                || code.contains("expect(unsafe_code")
                || code.contains("allow(unsafe_op_in_unsafe_fn");
            if keyword || opt_out {
                problems.push(format!("{}:{}: {}", rel.display(), n + 1, line.trim()));
            }
        }
    }

    // The manifest must actually carry the forbid, or the gate above is checking a property
    // nothing enforces.
    let manifest =
        std::fs::read_to_string(root.join("Cargo.toml")).map_err(|e| format!("Cargo.toml: {e}"))?;
    if !manifest.contains(r#"unsafe_code = "forbid""#) {
        problems.push("Cargo.toml no longer sets unsafe_code = \"forbid\"".to_string());
    }

    for p in &problems {
        eprintln!("  {p}");
    }
    println!("unsafe-lint: {} files scanned, forbid(unsafe_code) in place", files.len());
    Ok(Outcome::check(problems.is_empty(), format!("{} unsafe findings", problems.len())))
}

fn collect_rust(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rust(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// One entry in the aggregate: a name, and the gate to run under it.
type GateStep = (&'static str, fn() -> Result<Outcome, String>);

/// The aggregate. Run it before calling anything done.
///
/// Every gate runs even after one fails, so a single invocation reports every problem
/// rather than the first. A gate that SKIPPED is named as such and never counted as a pass.
///
/// Always returns `Ok`: an aggregate that could not run at all is a `Fail`, not an `Err`,
/// because the caller has to distinguish "a gate is red" (exit 1) from "a gate could not
/// run" (exit 2), and an `Err` collapses both into the first.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn parity() -> Result<Outcome, String> {
    let steps: Vec<GateStep> = vec![
        ("fmt", || fmt(false)),
        ("clippy", clippy),
        ("unsafe-lint", unsafe_lint),
        ("docs-lint", docs_lint),
        // The gate on the gates. Structural, needs no engine and no oracle, so it sits with
        // the other cheap checks at the front where a mistake is reported in seconds.
        ("lane-coverage", crate::meta::lane_coverage),
        ("fixture-coverage", crate::meta::fixture_coverage),
        // The only instrument that reaches the interrupted-search path; cheap, so it runs
        // with the rest rather than waiting for someone to remember it.
        ("async-check", crate::meta::async_check),
        ("test", test),
        ("perft", perft),
        ("golden", || golden(false)),
        ("golden-audit", || golden_audit(&[])),
        ("nnue-check", nnue_check),
        ("net-roundtrip", net_roundtrip),
        ("tb", tb),
        ("signature", || signature(false)),
    ];

    let mut failed = Vec::new();
    let mut skipped = Vec::new();
    for (name, f) in steps {
        println!("\n\x1b[1m== {name} ==\x1b[0m");
        let outcome = f();
        // `is_pass` is the ONLY thing that counts as green: a skip must fall through to the
        // arm that records it separately.
        let green = outcome.as_ref().is_ok_and(Outcome::is_pass);
        match outcome {
            Ok(Outcome::Pass) => {
                debug_assert!(green);
                println!("\x1b[32m  pass\x1b[0m");
            }
            Ok(Outcome::Skipped(why)) => {
                println!("\x1b[33m  SKIPPED: {why}\x1b[0m");
                skipped.push(name);
            }
            Ok(Outcome::Fail(why)) => {
                println!("\x1b[31m  FAIL: {why}\x1b[0m");
                failed.push(name);
            }
            Err(e) => {
                println!("\x1b[31m  ERROR: {e}\x1b[0m");
                failed.push(name);
            }
        }
    }

    println!("\n\x1b[1m== parity ==\x1b[0m");
    if !skipped.is_empty() {
        // Named, loudly, and separately from the passes: a gate that did not run has proven
        // nothing, and reporting it as green is how a campaign ends up believing a bug is
        // fixed.
        println!("\x1b[33mSKIPPED: {}\x1b[0m — these prove nothing", skipped.join(", "));
    }
    Ok(Outcome::check(failed.is_empty(), format!("failed gates: {}", failed.join(", "))))
}

fn arg_value<'a>(args: &'a [&'a str], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| *a == flag).and_then(|i| args.get(i + 1)).copied()
}

/// Unused today; kept so a differential gate can be added without re-deriving the helpers.
#[allow(dead_code)]
fn oracle_available() -> bool {
    have("stockfish") || engine_path(GATE_PROFILE).exists()
}

#[allow(dead_code)]
fn capture_version() -> Result<String, String> {
    capture(Command::new(cargo()).arg("--version"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_blank_sides_are_a_rig_fault_rather_than_agreement() {
        // The equality the check exists to intercept: `"" == ""` is true, so a dead oracle
        // and a blank golden agree perfectly about nothing.
        assert!(nothing_was_compared("", ""));
        assert!(nothing_was_compared("  \n\n", "\n"));
        // One side blank is a real difference and must stay a DIFF, not a rig fault: it is
        // the shape of an oracle that died against a golden that did not.
        assert!(!nothing_was_compared("readyok", ""));
        assert!(!nothing_was_compared("", "readyok"));
        assert!(!nothing_was_compared("readyok", "readyok"));
    }

    #[test]
    fn markdown_links_are_extracted_without_their_anchors() {
        let line = "See [the docs](docs/README.md) and [a section](docs/x.md#here).";
        assert_eq!(markdown_link_targets(line), vec!["docs/README.md", "docs/x.md#here"]);
        assert!(markdown_link_targets("no links here").is_empty());
        // An unterminated link must not panic or run off the end.
        assert!(markdown_link_targets("[broken](unclosed").is_empty());
    }

    #[test]
    fn only_the_clock_is_dropped_from_an_info_line() {
        // The depth, the score, the node count and the PV are facts about the search and
        // must survive into the golden -- dropping the whole line, as this once did, meant
        // no golden ever compared a score or a PV against anything.
        let out = "info depth 3 seldepth 2 score cp 12 nodes 5 nps 900 hashfull 1 time 7 pv e2e4\n\
                   readyok\nTotal time (ms) : 7\nuciok\n";
        assert_eq!(
            filter_volatile(out),
            "info depth 3 seldepth 2 score cp 12 nodes 5 pv e2e4\nreadyok\nuciok\n"
        );
    }

    #[test]
    fn a_flag_value_is_read_from_the_next_argument() {
        let args = ["--profile", "release", "--arch", "x86-64-v3"];
        assert_eq!(arg_value(&args, "--profile"), Some("release"));
        assert_eq!(arg_value(&args, "--arch"), Some("x86-64-v3"));
        assert_eq!(arg_value(&args, "--missing"), None);
        // A trailing flag with no value must yield None, not panic.
        assert_eq!(arg_value(&["--profile"], "--profile"), None);
    }
}
