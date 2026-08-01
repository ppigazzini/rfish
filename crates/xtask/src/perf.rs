//! Measurement tooling: the PGO build, a COMPARABLE oracle, and the differential.
//!
//! Every number in `docs/03-engine-eval.md` is a ratio against an upstream build, and a ratio
//! is only about the engines when everything else is held equal. Three things were not held
//! equal before this module existed, and each one moved the answer by more than the code
//! changes the ledger tracks:
//!
//! - **The compiler.** rfish is compiled by rustc, whose backend is LLVM. An oracle built by
//!   g++ measures GCC against LLVM as much as it measures upstream against rfish. Build the
//!   oracle with `clang++` at the same major version as rustc's LLVM — [`oracle`] does.
//! - **The optimisation level.** Upstream's own shipped recipe is `make profile-build`, which
//!   is PGO on top of LTO. rfish had no PGO path at all, so the ledger compared a
//!   profile-guided C++ binary against a rustc build that never saw a profile. [`pgo`] adds
//!   the missing half; both sides now train on the same `bench` workload.
//! - **The ISA tier.** Already covered by `--arch`, and unchanged here beyond naming the
//!   tiers so both sides move together.
//!
//! What this module does NOT do is pick the numbers. It builds comparable binaries and runs
//! two instruments over them; the instruments disagree often, and both readings belong in a
//! report. See `docs/09-tooling-ci.md` for which instrument settles which question.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::runner::{Outcome, cargo, node_total};
use crate::{capture, have, resources_dir, run, workspace_root};

/// A tier, named once so both sides of a differential move together.
///
/// rustc and upstream's Makefile spell the same machine differently, and a comparison of
/// `-C target-cpu=haswell` against `ARCH=x86-64-avx512icl` measures the ISA rather than the
/// engines. Naming the pair keeps that mistake out of the command line.
struct Tier {
    /// What this repository calls the tier.
    name: &'static str,
    /// The `-C target-cpu` rustc takes.
    rustc: &'static str,
    /// The `ARCH=` upstream's Makefile takes.
    upstream: &'static str,
    /// Whether valgrind can execute the code this tier emits.
    ///
    /// callgrind implements no AVX-512, and SIGILLs on the first instruction it does not
    /// know. An instruction differential therefore has a tier ceiling that the wall-clock
    /// A/B does not.
    callgrind_safe: bool,
}

const TIERS: [Tier; 3] = [
    Tier { name: "sse41", rustc: "nehalem", upstream: "x86-64-sse41-popcnt", callgrind_safe: true },
    Tier { name: "avx2", rustc: "haswell", upstream: "x86-64-avx2", callgrind_safe: true },
    // `native` is whatever the host is. It is the tier a player would build, and the tier a
    // callgrind run cannot be trusted at on any box with AVX-512.
    Tier { name: "native", rustc: "native", upstream: "native", callgrind_safe: false },
];

/// The bench the profile trains on and the differential measures.
///
/// Upstream's `profile-build` trains on its own default `bench`, so rfish trains on the same
/// thing: a profile taken from a different workload optimises for a program nobody runs.
const TRAIN_BENCH: &str = "bench";

/// Hash, threads and depth for the instruction differential.
///
/// Shallow enough that callgrind finishes in a minute and deep enough to leave startup a
/// minority of the profile — and startup is subtracted regardless.
const DIFF_BENCH: [&str; 3] = ["16", "1", "8"];

/// Hash, threads and depth for the TIMED differential, which needs a longer run.
///
/// Deeper than [`DIFF_BENCH`] on purpose: at depth 8 a bench is a fraction of a second here,
/// and the paired spread swamped the effect — several pairs straddled 1.000 that separate
/// cleanly at depth 13. Instructions are deterministic and need no such headroom.
const TIME_BENCH: [&str; 3] = ["16", "1", "13"];

/// The `-C target-cpu` a tier NAME means, for a caller outside this module.
///
/// `build --arch` and `perf --tier` name the same three tiers, and every measurement in the
/// docs is quoted by tier name. They must therefore resolve through one table: `--arch avx2`
/// passing `avx2` straight to rustc is not a tier at all -- rustc has no such CPU, and the
/// build fails with a rustc error that names neither the flag nor the tier.
pub(crate) fn target_cpu_for(tier: &str) -> Option<&'static str> {
    TIERS.iter().find(|t| t.name == tier).map(|t| t.rustc)
}

/// Every tier name, for an error message that can list the alternatives.
pub(crate) fn tier_names() -> Vec<&'static str> {
    TIERS.iter().map(|t| t.name).collect()
}

/// Resolve `--tier`, defaulting to the matched-ISA tier both ports have always quoted.
fn tier_of(args: &[&str]) -> Result<&'static Tier, String> {
    let want = arg_value(args, "--tier").unwrap_or("avx2");
    TIERS.iter().find(|t| t.name == want).ok_or_else(|| {
        let names: Vec<&str> = TIERS.iter().map(|t| t.name).collect();
        format!("unknown tier '{want}'; want one of {}", names.join(", "))
    })
}

/// The value following `flag`, when present.
fn arg_value<'a>(args: &'a [&'a str], flag: &str) -> Option<&'a str> {
    args.iter().position(|a| *a == flag).and_then(|i| args.get(i + 1)).copied()
}

/// True when `flag` is present.
fn has_flag(args: &[&str], flag: &str) -> bool {
    args.contains(&flag)
}

/// The `llvm-profdata` that matches the LLVM rustc is built against.
///
/// The raw profile format is versioned with LLVM, so merging last year's `llvm-profdata`
/// over this year's `.profraw` fails — and fails late, after the instrumented run. Resolve
/// the exact major first and say so when it is missing.
fn profdata_tool() -> Result<String, String> {
    let major = rustc_llvm_major()?;
    let exact = format!("llvm-profdata-{major}");
    if have(&exact) {
        return Ok(exact);
    }
    if have("llvm-profdata") {
        return Ok("llvm-profdata".to_string());
    }
    Err(format!("neither {exact} nor llvm-profdata is on the path"))
}

/// The major version of the LLVM rustc carries, read from `rustc -vV`.
fn rustc_llvm_major() -> Result<String, String> {
    let out = capture(Command::new("rustc").arg("-vV"))?;
    out.lines()
        .find_map(|l| l.strip_prefix("LLVM version: "))
        .and_then(|v| v.split('.').next())
        .map(str::to_string)
        .ok_or_else(|| "rustc -vV names no LLVM version".to_string())
}

/// Build rfish with profile-guided optimisation: instrument, train on `bench`, rebuild.
///
/// Upstream ships `make profile-build` and every published upstream binary is built that
/// way, so a ledger entry taken against a PGO'd oracle from a non-PGO rfish is not a
/// measurement of the port. `--spine` swaps the evaluation for the material stand-in, which
/// is the build the spine differential needs on this side.
pub(crate) fn pgo(args: &[&str]) -> Result<Outcome, String> {
    let tier = tier_of(args)?;
    let spine = has_flag(args, "--spine");
    let Ok(profdata) = profdata_tool() else {
        return Ok(Outcome::Skipped(
            "no llvm-profdata matching rustc's LLVM; PGO needs it to merge the raw profile"
                .to_string(),
        ));
    };

    let root = workspace_root();
    let suffix = if spine { format!("{}-spine", tier.name) } else { tier.name.to_string() };
    let profile_dir = root.join("target/pgo").join(&suffix);
    let gen_dir = root.join("target/pgo-gen").join(&suffix);
    let use_dir = root.join("target/pgo-use").join(&suffix);
    let _ = std::fs::remove_dir_all(&profile_dir);
    std::fs::create_dir_all(&profile_dir).map_err(|e| format!("{}: {e}", profile_dir.display()))?;

    // Phase 1: the instrumented binary, at the same tier the final one gets, so the profile
    // describes the code that ships rather than a differently vectorised twin.
    println!("pgo {suffix}: phase 1/3, instrumented build");
    cargo_build(
        &format!("-C target-cpu={} -C profile-generate={}", tier.rustc, profile_dir.display()),
        &gen_dir,
        spine,
    )?;

    // Phase 2: train. From `resources/`, or the run finds no net, profiles the classical
    // fallback and optimises a path the shipped binary never takes.
    println!("pgo {suffix}: phase 2/3, training on `{TRAIN_BENCH}`");
    run(Command::new(gen_dir.join("release/stockfish"))
        .current_dir(resources_dir())
        .arg(TRAIN_BENCH)
        .stdout(std::process::Stdio::null()))?;
    let raw: Vec<PathBuf> = std::fs::read_dir(&profile_dir)
        .map_err(|e| format!("{}: {e}", profile_dir.display()))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "profraw"))
        .collect();
    if raw.is_empty() {
        return Err("the instrumented run wrote no .profraw to merge".to_string());
    }
    let merged = profile_dir.join("merged.profdata");
    let mut merge = Command::new(&profdata);
    merge.arg("merge").arg(format!("-output={}", merged.display())).args(&raw);
    run(&mut merge)?;

    // Phase 3: rebuild under the profile. It steers layout and inlining only, so it cannot
    // move the node count — which `signature` against this binary re-asserts.
    println!("pgo {suffix}: phase 3/3, profile-guided rebuild");
    cargo_build(
        &format!("-C target-cpu={} -C profile-use={}", tier.rustc, merged.display()),
        &use_dir,
        spine,
    )?;

    println!("pgo {suffix}: {}", use_dir.join("release/stockfish").display());
    Ok(Outcome::Pass)
}

/// One `cargo build --release` with `flags` in RUSTFLAGS and its own target directory.
///
/// The flags go through the CHILD's environment rather than this process's, so a later step
/// in the same run cannot inherit a target-cpu nobody asked it for. The target directory is
/// per configuration because cargo keys its fingerprint on the profile, not on RUSTFLAGS,
/// and sharing one directory rebuilds the world on every alternation.
fn cargo_build(flags: &str, target_dir: &Path, spine: bool) -> Result<(), String> {
    let mut cmd = Command::new(cargo());
    cmd.current_dir(workspace_root())
        .env("RUSTFLAGS", flags)
        .env("CARGO_TARGET_DIR", target_dir)
        .args(["build", "--release", "--package", "rfish", "--bin", "stockfish"]);
    if spine {
        cmd.args(["--features", "rfish-engine/eval-material"]);
    }
    run(&mut cmd)
}

/// Build an upstream oracle that is comparable to rfish: clang, PGO, LTO, matched tier.
///
/// The tree is extracted from `../Stockfish` at `tools/upstream/UPSTREAM_BASE` with
/// `git archive`, which reads the object store and touches neither that repository's working
/// tree nor its index — a checkout or a worktree there would be a side effect on the golden.
///
/// `--spine` replaces the body of `Eval::evaluate` with the same material sum the engine's
/// `eval-material` feature installs. Both sides must carry the SAME formula or the trees
/// diverge and the differential measures nothing.
pub(crate) fn oracle(args: &[&str]) -> Result<Outcome, String> {
    let tier = tier_of(args)?;
    let spine = has_flag(args, "--spine");
    if !have("clang++") {
        return Ok(Outcome::Skipped("clang++ is not on the path".to_string()));
    }
    if profdata_tool().is_err() {
        return Ok(Outcome::Skipped("no llvm-profdata for the oracle's PGO phase".to_string()));
    }
    let src_repo = workspace_root().parent().map(|p| p.join("Stockfish")).unwrap_or_default();
    if !src_repo.join(".git").exists() {
        return Ok(Outcome::Skipped(format!("no upstream clone at {}", src_repo.display())));
    }

    let base = upstream_base()?;
    let dest = oracle_dir(tier, spine);
    let _ = std::fs::remove_dir_all(&dest);
    std::fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;

    println!("oracle {}: extracting {base}", dest.display());
    let tar = dest.join("upstream.tar");
    run(Command::new("git").current_dir(&src_repo).args([
        "archive",
        "--format=tar",
        "-o",
        &tar.to_string_lossy(),
        &base,
    ]))?;
    run(Command::new("tar").arg("-xf").arg(&tar).arg("-C").arg(&dest))?;
    let _ = std::fs::remove_file(&tar);

    copy_oracle_net(&dest, &src_repo)?;
    if spine {
        patch_material_eval(&dest.join("src/evaluate.cpp"))?;
    }

    println!("oracle {}: make profile-build COMP=clang ARCH={}", dest.display(), tier.upstream);
    run(Command::new("make")
        .current_dir(dest.join("src"))
        .args(["-j8", "profile-build", "COMP=clang"])
        .arg(format!("ARCH={}", tier.upstream)))?;

    stamp_oracle(&dest, &base)?;
    println!("oracle: {}", dest.join("src/stockfish").display());
    Ok(Outcome::Pass)
}

/// The file naming the upstream commit an oracle directory was extracted at.
fn oracle_stamp_path(dir: &Path) -> PathBuf {
    dir.join(".rfish-oracle-base")
}

/// Record which commit this oracle IS, beside the tree it was built from.
fn stamp_oracle(dir: &Path, base: &str) -> Result<(), String> {
    let path = oracle_stamp_path(dir);
    std::fs::write(&path, format!("{base}\n")).map_err(|e| format!("{}: {e}", path.display()))
}

/// Refuse an oracle that is not at the pin this port claims to be a translation of.
///
/// **This caught a real one.** The oracle directory is built once and reused, and it lives
/// OUTSIDE the repository, so advancing `UPSTREAM_BASE` leaves it untouched and nothing about
/// its filename changes. After the `c5aef2bf1` sync the avx2 oracle here was still the
/// `23cf5d82` tree — it benched 3184328 where the pin benches 2508687 — and every measurement
/// taken against it was a comparison with an upstream this port is not a translation of.
///
/// The instruction differential would eventually have caught it, because it compares node
/// counts before quoting a ratio. Eventually is the problem: that check only runs when
/// callgrind does, so at `--tier native`, or on a box without valgrind, the wall-clock A/B ran
/// against the wrong binary and reported a ratio with no warning at all. `../zfish` stamps its
/// oracle by SHA for the same reason (`b96f9f24`).
fn verify_oracle(dir: &Path, tier: &Tier) -> Result<(), String> {
    let want = upstream_base()?;
    let path = oracle_stamp_path(dir);
    let rebuild = format!("rebuild it with `cargo xtask oracle --tier {}`", tier.name);
    let Ok(got) = std::fs::read_to_string(&path) else {
        return Err(format!(
            "the oracle at {} carries no {} stamp, so which upstream it is cannot be \
             established; {rebuild}",
            dir.display(),
            path.file_name().unwrap_or_default().to_string_lossy()
        ));
    };
    let got = got.trim();
    if got != want {
        return Err(format!(
            "the oracle at {} was built at {got}, and the pin is {want}. Comparing against it \
             measures this port against an upstream it is not a translation of; {rebuild}",
            dir.display()
        ));
    }
    Ok(())
}

/// The net an engine actually loaded, from the line it prints before it searches.
///
/// A node comparison across two different nets is meaningless — the trees differ because the
/// evaluations differ, not because either engine is wrong — and it fails in the direction that
/// looks like a porting bug. Both sibling ports added this check after being misled by one
/// (`../zfish` `a45bb2a0`, `../mcfish` `2abfce75`).
fn engine_net(bin: &Path, cwd: &Path) -> Result<String, String> {
    let out = capture_both(Command::new(bin).current_dir(cwd).args(["bench", "1", "1", "1"]))?;
    out.lines()
        .find_map(|l| l.split("NNUE evaluation using ").nth(1))
        .map(|n| n.split_whitespace().next().unwrap_or(n).to_string())
        .ok_or_else(|| {
            format!(
                "{} printed no `NNUE evaluation using` line, so it ran with NO net and is a \
                 different engine from the one being measured",
                bin.display()
            )
        })
}

/// Refuse a comparison between two engines that did not evaluate with the same net.
fn same_net(ours: &Path, our_dir: &Path, theirs: &Path, their_dir: &Path) -> Result<(), String> {
    let mine = engine_net(ours, our_dir)?;
    let yours = engine_net(theirs, their_dir)?;
    if mine != yours {
        return Err(format!(
            "the two engines loaded different nets (rfish {mine}, upstream {yours}); a node \
             count is a property of the net as much as of the search, so nothing measured \
             across the pair would mean anything"
        ));
    }
    println!("net {mine} on both sides");
    Ok(())
}

/// Where an oracle of this tier and kind lives: beside the repository, never inside it.
fn oracle_dir(tier: &Tier, spine: bool) -> PathBuf {
    let kind = if spine { "spine" } else { "nnue" };
    let name = format!(".rfish-oracle-{}-{kind}", tier.name);
    workspace_root().parent().map_or_else(|| PathBuf::from(&name), |p| p.join(&name))
}

/// The upstream SHA this port is a translation of.
fn upstream_base() -> Result<String, String> {
    let path = workspace_root().join("tools/upstream/UPSTREAM_BASE");
    std::fs::read_to_string(&path)
        .map(|s| s.trim().to_string())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Put the net the extracted tree names beside its sources, so the build can embed it.
///
/// The nets are not in upstream's object store — `make net` downloads them — so they are
/// copied from whichever tree on this box already has one: the upstream clone first, then
/// this repository's `resources/`.
fn copy_oracle_net(dest: &Path, src_repo: &Path) -> Result<(), String> {
    let header = dest.join("src/evaluate.h");
    let text =
        std::fs::read_to_string(&header).map_err(|e| format!("{}: {e}", header.display()))?;
    let name = text
        .lines()
        .find_map(|l| l.strip_prefix("#define EvalFileDefaultName "))
        .and_then(|v| v.split('"').nth(1))
        .ok_or("the oracle's evaluate.h names no default net")?
        .to_string();

    for dir in [src_repo.join("src"), resources_dir()] {
        let from = dir.join(&name);
        if from.is_file() {
            let to = dest.join("src").join(&name);
            std::fs::copy(&from, &to).map_err(|e| format!("{}: {e}", to.display()))?;
            return Ok(());
        }
    }
    Err(format!("{name} is on neither the upstream clone nor resources/; fetch it first"))
}

/// Replace the body of `Eval::evaluate` with the ports' material sum.
///
/// Written as surgery on the function body rather than as a stored patch file, because a
/// patch goes stale the moment upstream touches a line near it and then fails in a way that
/// reads as a build error rather than as "the harness needs updating".
fn patch_material_eval(path: &Path) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let signature = "Value Eval::evaluate(";
    let start = src.find(signature).ok_or("the oracle has no Eval::evaluate to patch")?;
    let marker = "    assert(!pos.checkers());";
    let body =
        src[start..].find(marker).map(|i| start + i).ok_or("Eval::evaluate has no assert")?;
    let end = src[body..].find("\n}\n").map(|i| body + i).ok_or("Eval::evaluate does not end")?;

    // The weights, the perspective and the ABSENCES are all load-bearing: the engine's
    // `material_only` has no optimism term, no rule50 damping and no clamp, and a side that
    // keeps one of them searches a different tree.
    let replacement = "    assert(!pos.checkers());\n\n\
        \x20   // MEASUREMENT HARNESS installed by `cargo xtask oracle --spine`: material only,\n\
        \x20   // matching the engine's `material_only` term for term so the differential measures\n\
        \x20   // the search spine and not the network.\n\
        \x20   (void) network;\n\
        \x20   (void) accumulators;\n\
        \x20   (void) caches;\n\
        \x20   (void) optimism;\n\n\
        \x20   Color us = pos.side_to_move(), them = ~us;\n\
        \x20   int   v  = 100 * (pos.count<PAWN>(us) - pos.count<PAWN>(them))\n\
        \x20          + 320 * (pos.count<KNIGHT>(us) - pos.count<KNIGHT>(them))\n\
        \x20          + 330 * (pos.count<BISHOP>(us) - pos.count<BISHOP>(them))\n\
        \x20          + 500 * (pos.count<ROOK>(us) - pos.count<ROOK>(them))\n\
        \x20          + 900 * (pos.count<QUEEN>(us) - pos.count<QUEEN>(them));\n\n\
        \x20   return Value(v);";

    let patched = format!("{}{replacement}{}", &src[..body], &src[end..]);
    std::fs::write(path, patched).map_err(|e| format!("{}: {e}", path.display()))
}

/// Run the differential: instructions by callgrind, then search time by a paired A/B.
///
/// Both instruments are reported because they disagree, and a report that quotes only the
/// one that flatters the port is the failure mode this command exists to prevent.
pub(crate) fn perf(args: &[&str]) -> Result<Outcome, String> {
    let tier = tier_of(args)?;
    let spine = has_flag(args, "--spine");
    let rounds: usize = arg_value(args, "--rounds").unwrap_or("9").parse().unwrap_or(9);

    let suffix = if spine { format!("{}-spine", tier.name) } else { tier.name.to_string() };
    let ours = workspace_root().join("target/pgo-use").join(&suffix).join("release/stockfish");
    let theirs = oracle_dir(tier, spine).join("src/stockfish");
    if !ours.is_file() {
        return Ok(Outcome::Skipped(format!(
            "no PGO build at {}; run `cargo xtask pgo --tier {}{}`",
            ours.display(),
            tier.name,
            if spine { " --spine" } else { "" }
        )));
    }
    if !theirs.is_file() {
        return Ok(Outcome::Skipped(format!(
            "no oracle at {}; run `cargo xtask oracle --tier {}{}`",
            theirs.display(),
            tier.name,
            if spine { " --spine" } else { "" }
        )));
    }
    let their_dir = theirs.parent().map(Path::to_path_buf).unwrap_or_default();
    // BEFORE either instrument, and unconditionally: the node-count check inside the
    // instruction differential is the only other thing that would notice, and it does not run
    // at every tier.
    verify_oracle(&oracle_dir(tier, spine), tier)?;
    // The spine pair evaluates with a stubbed material eval on both sides, so neither loads a
    // net and there is nothing to match.
    if !spine {
        same_net(&ours, &resources_dir(), &theirs, &their_dir)?;
    }

    println!("tier {} ({} vs ARCH={})", tier.name, tier.rustc, tier.upstream);

    if tier.callgrind_safe && have("valgrind") {
        instruction_differential(&ours, &resources_dir(), &theirs, &their_dir)?;
    } else if !tier.callgrind_safe {
        println!(
            "\ninstructions: SKIPPED at tier {} — callgrind implements no AVX-512 and SIGILLs\n\
             on the first instruction it does not know. Measure instructions at avx2.",
            tier.name
        );
    } else {
        println!("\ninstructions: SKIPPED — valgrind is not installed");
    }

    nps_ab(&ours, &resources_dir(), &theirs, &their_dir, rounds)
}

/// Instructions retired over one bench, startup subtracted from both sides by measurement.
///
/// A whole-process counter carries the magic-table build and the net parse, and both are
/// large next to a shallow bench — and they are not the same size on the two sides, so the
/// unsubtracted ratio can carry the wrong SIGN. Subtract on the instruction axis only.
fn instruction_differential(
    ours: &Path,
    our_dir: &Path,
    theirs: &Path,
    their_dir: &Path,
) -> Result<(), String> {
    println!("\ninstructions (callgrind, bench {}, startup subtracted)", DIFF_BENCH.join(" "));
    let (our_search, our_nodes) = callgrind_search_ir(ours, our_dir)?;
    let (their_search, their_nodes) = callgrind_search_ir(theirs, their_dir)?;

    if our_nodes != their_nodes {
        return Err(format!(
            "node counts differ (rfish {our_nodes}, upstream {their_nodes}); different trees are \
             different workloads and every ratio below would be meaningless"
        ));
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = our_search as f64 / their_search as f64;
    println!("  rfish     {our_search:>15}");
    println!("  upstream  {their_search:>15}");
    println!("  ratio     {ratio:>15.3}   over {our_nodes} nodes on both sides");
    Ok(())
}

/// Search-only instructions for one binary: the bench profile less a `quit`-only profile.
///
/// The profile file is named for the binary AND this process, because two of these run
/// concurrently in a fleet and a shared scratch name has silently clobbered one side's
/// output in the sibling ports.
fn callgrind_search_ir(bin: &Path, cwd: &Path) -> Result<(u64, u64), String> {
    let out = std::env::temp_dir().join(format!("rfish-cg-{}.out", scratch_key(bin)));
    let mut cmd = Command::new("valgrind");
    cmd.current_dir(cwd)
        .args(["--tool=callgrind", "--cache-sim=no", "--branch-sim=no"])
        .arg(format!("--callgrind-out-file={}", out.display()))
        .arg(bin)
        .arg("bench")
        .args(DIFF_BENCH);
    let bench_out = capture_both(&mut cmd)?;
    let nodes = node_total(&bench_out)?;
    let total = callgrind_total(&out)?;
    Ok((total.saturating_sub(startup_ir(bin, cwd)?), nodes))
}

/// Run a command and return stdout AND stderr together.
///
/// Upstream's `bench` writes its summary — the node total and the time this module reads —
/// to STANDARD ERROR, and so does valgrind. Capturing stdout alone gets an empty string from
/// the oracle and a confident "the bench output has no 'Nodes searched' line" from the parse.
fn capture_both(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!("{cmd:?} exited with {}", out.status));
    }
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(text)
}

/// A scratch filename unique to this binary and this process.
fn scratch_key(bin: &Path) -> String {
    let name = bin.parent().and_then(Path::file_name).unwrap_or_default();
    format!("{}-{}", name.to_string_lossy(), std::process::id())
}

/// The instructions a `quit`-only run costs: the net parse, the magic tables, the zero-fill.
fn startup_ir(bin: &Path, cwd: &Path) -> Result<u64, String> {
    let out = std::env::temp_dir().join(format!("rfish-cg-quit-{}.out", scratch_key(bin)));
    let mut child = Command::new("valgrind")
        .current_dir(cwd)
        .args(["--tool=callgrind", "--cache-sim=no", "--branch-sim=no"])
        .arg(format!("--callgrind-out-file={}", out.display()))
        .arg(bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("valgrind: {e}"))?;
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().ok_or("valgrind child has no stdin")?;
        writeln!(stdin, "quit").map_err(|e| format!("writing quit: {e}"))?;
    }
    let status = child.wait().map_err(|e| format!("waiting for valgrind: {e}"))?;
    if !status.success() {
        return Err(format!("the startup profile exited with {status}"));
    }
    callgrind_total(&out)
}

/// The `summary:` line of a callgrind profile: its first counter is instructions retired.
fn callgrind_total(path: &Path) -> Result<u64, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.lines()
        .find_map(|l| l.strip_prefix("summary:"))
        .and_then(|l| l.split_whitespace().next())
        .ok_or_else(|| format!("{}: no summary line", path.display()))?
        .parse()
        .map_err(|e| format!("{}: the summary does not parse: {e}", path.display()))
}

/// Paired A/B search time, interleaved, with the order alternating every round.
///
/// The protocol is the sibling port's `tools/nps_ab.sh`, and every rule in it was paid for
/// by a wrong published number:
///
/// - **Interleave, never batch.** Absolute speed is thermally void — the same binary reads
///   differently between batches, so only same-round pairs count.
/// - **Alternate the order.** The second slot of a round runs on a hotter core. Fixing the
///   order biases every round the same way and reverses the sign of a small effect.
/// - **Report the MEDIAN of the paired ratios, with the spread.** A spread that straddles
///   1.000 has not established a direction, and saying so is the point.
/// - **Read each engine's own `Total time`.** `bench` starts its clock after the table
///   clear, so no process startup is inside it and nothing has to be subtracted.
fn nps_ab(
    ours: &Path,
    our_dir: &Path,
    theirs: &Path,
    their_dir: &Path,
    rounds: usize,
) -> Result<Outcome, String> {
    println!("\nsearch time (paired A/B, {rounds} rounds, order alternates each round)");
    let mut ratios: Vec<f64> = Vec::with_capacity(rounds);
    for round in 1..=rounds {
        let ((our_ms, our_nodes), (their_ms, their_nodes)) = if round % 2 == 1 {
            (timed_bench(ours, our_dir)?, timed_bench(theirs, their_dir)?)
        } else {
            let t = timed_bench(theirs, their_dir)?;
            (timed_bench(ours, our_dir)?, t)
        };
        if our_nodes != their_nodes {
            return Err(format!(
                "node counts differ (rfish {our_nodes}, upstream {their_nodes}) on round {round}"
            ));
        }
        #[allow(clippy::cast_precision_loss)]
        let ratio = our_ms as f64 / their_ms.max(1) as f64;
        ratios.push(ratio);
        println!(
            "  round {round:<3} rfish {our_ms:>6} ms   upstream {their_ms:>6} ms   {ratio:.4}"
        );
    }
    ratios.sort_by(f64::total_cmp);
    let median = ratios[ratios.len() / 2];
    let (low, high) = (ratios[0], ratios[ratios.len() - 1]);
    println!("\n  MEDIAN paired search time {median:.4}   (rfish {:+.1}%)", (median - 1.0) * 100.0);
    println!("  spread {low:.4}..{high:.4}");
    if low <= 1.0 && high >= 1.0 {
        println!("  the spread STRADDLES 1.000 — this run establishes no direction");
    } else {
        println!("  the spread excludes 1.000 — the direction holds at this sample size");
    }
    Ok(Outcome::Pass)
}

/// One bench run, returning the engine's own search milliseconds and its node total.
fn timed_bench(bin: &Path, cwd: &Path) -> Result<(u64, u64), String> {
    let out = capture_both(Command::new(bin).current_dir(cwd).arg("bench").args(TIME_BENCH))?;
    let ms = out
        .lines()
        .find_map(|l| l.strip_prefix("Total time (ms) : "))
        .ok_or("the bench output has no 'Total time' line")?
        .trim()
        .parse()
        .map_err(|e| format!("the time does not parse: {e}"))?;
    Ok((ms, node_total(&out)?))
}

/// A node-for-node differential against the oracle, on RANDOM positions.
///
/// The strongest fidelity probe available, and the one thing the other gates cannot be:
/// `signature` pins one fixed list, the goldens pin fixed scripts and `nnue-check` pins a
/// fixed FEN file. A position reached by random legal play appears in none of them, so a
/// divergence that misses every fixed list still has to survive this. ../mcfish carries the
/// same probe and records that until its lane existed it only ran when someone remembered.
///
/// Positions are reached by asking the ENGINE for its legal moves and picking one with a
/// seeded generator, so the walk needs no chess knowledge here and replays from the seed.
pub(crate) fn upstream_nodes(args: &[&str]) -> Result<Outcome, String> {
    let Some(oracle) = crate::gates::find_oracle() else {
        return Ok(Outcome::Skipped("no upstream build at the pin to compare against".to_string()));
    };
    let ours = crate::runner::build_engine(crate::runner::GATE_PROFILE)?;
    let their_dir = oracle.parent().map(Path::to_path_buf).unwrap_or_default();
    // `find_oracle` has already established that this binary is the PIN. It says nothing
    // about which net either side loads, and this whole command is a node comparison.
    same_net(&ours, &resources_dir(), &oracle, &their_dir)?;
    let positions: usize = arg_value(args, "--positions").unwrap_or("20").parse().unwrap_or(20);
    let depth = arg_value(args, "--depth").unwrap_or("8").to_string();
    let mut rng: u64 =
        arg_value(args, "--seed").and_then(|s| s.parse().ok()).unwrap_or(0x5DEE_CE66_D1CE_B00D);
    let seed = rng;

    println!("upstream-nodes: {positions} positions at depth {depth}, seed {seed}");
    let mut mismatches = 0usize;
    for i in 0..positions {
        let plies = 6 + (next_rand(&mut rng) % 16) as usize;
        let mut moves: Vec<String> = Vec::new();
        for _ in 0..plies {
            let pos_cmd = position_command(&moves);
            let out = crate::runner::drive_at(&ours, &resources_dir(), &[&pos_cmd, "go perft 1"])?;
            let legal: Vec<String> = out
                .lines()
                .filter_map(|l| l.split_once(':'))
                .map(|(m, _)| m.trim().to_string())
                // A UCI move and nothing else: the perft block also ends with a
                // `Nodes searched:` line, which splits on the same colon and would be
                // fed back as a move.
                .filter(|m| is_uci_move(m))
                .collect();
            if legal.is_empty() {
                break;
            }
            let pick = (next_rand(&mut rng) % legal.len() as u64) as usize;
            moves.push(legal[pick].clone());
        }

        let pos_cmd = position_command(&moves);
        let go = format!("go depth {depth}");
        let mine =
            searched_nodes(&crate::runner::drive_at(&ours, &resources_dir(), &[&pos_cmd, &go])?)?;
        let theirs =
            searched_nodes(&crate::gates::drive_oracle(&oracle, &their_dir, &[&pos_cmd, &go])?)?;
        if mine == theirs {
            continue;
        }
        mismatches += 1;
        eprintln!("  MISMATCH {i}: {pos_cmd}");
        eprintln!("    rfish {mine} nodes, upstream {theirs} nodes");
    }

    println!("upstream-nodes: {} of {positions} match node for node", positions - mismatches);
    Ok(Outcome::check(mismatches == 0, format!("{mismatches} positions differ from upstream")))
}

/// Whether `s` is a UCI move: two squares, optionally a promotion piece.
fn is_uci_move(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 4 && b.len() != 5 {
        return false;
    }
    let sq = |f: u8, r: u8| (b'a'..=b'h').contains(&f) && (b'1'..=b'8').contains(&r);
    sq(b[0], b[1]) && sq(b[2], b[3]) && (b.len() == 4 || matches!(b[4], b'q' | b'r' | b'b' | b'n'))
}

/// The node count a `go` reported, read from its LAST info line.
///
/// Not `node_total`, which parses `bench`'s summary: a `go` never prints one, and the count
/// that matters is the final iteration's rather than any earlier one.
fn searched_nodes(out: &str) -> Result<u64, String> {
    out.lines()
        .filter(|l| l.starts_with("info depth"))
        .filter_map(|l| l.split_once(" nodes ")?.1.split_whitespace().next())
        .next_back()
        .ok_or_else(|| "the search reported no info line with a node count".to_string())?
        .parse()
        .map_err(|e| format!("the node count does not parse: {e}"))
}

/// `position startpos` plus the moves walked so far.
fn position_command(moves: &[String]) -> String {
    if moves.is_empty() {
        "position startpos".to_string()
    } else {
        format!("position startpos moves {}", moves.join(" "))
    }
}

/// xorshift64, so a run replays exactly from its seed and needs no dependency.
fn next_rand(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Assert this port CALLS the same things upstream does, as many times.
///
/// # The gap this fills
///
/// Every other differential here compares VALUES: `signature` pins node counts, the goldens
/// pin UCI text, `nnue-check` pins evaluations, `upstream-nodes` pins node counts on random
/// positions. All of them pass over a state divergence that happens not to move a number on
/// the positions they drive. This compares HOW the two engines got there, and it found two
/// real defects on its first run — both of which every value gate above was green through:
///
/// - the PV reporter rendered each move against a walked CLONE of the position, playing a
///   `do_move` per PV move, and broke out at the first move that failed a legality check.
///   Upstream renders against the root and prints the stored line whole. 1443 `do_move`s and
///   1443 clones a bench, plus a PV that could come out shorter than upstream's.
/// - `bestmove ... ponder X` named the ponder move in the position AFTER the best move,
///   cloning and playing it first. Upstream names it against the root. 49 more of each.
///
/// Neither moved a node count, a golden, a `bestmove` or any of the 394 `info` lines.
///
/// # Why call counts and not cost
///
/// A call count is INLINING-IMMUNE at the callee: it does not care how the callee was
/// reached, only that it was. That is what lets a rustc tree be compared against a clang one
/// at all, where any cost claim has to argue attribution first. And callgrind SIMULATES
/// rather than samples, so the answer is deterministic and a loaded box cannot flap it —
/// which matters here, where NPS cannot settle a few per cent.
///
/// It is inlining-immune at the callee and NOT at the caller, which is the whole reason
/// `tools/fingerprint_groups.tsv` holds only the symbols that survive on both sides. See that
/// file for which ones do not, and the measurement that says so.
///
/// ~50x slower than the run it profiles, so this is not part of `parity`; it belongs in the
/// weekly upstream lane, which already builds an oracle.
pub(crate) fn fingerprint(args: &[&str]) -> Result<Outcome, String> {
    let tier = tier_of(args)?;
    if !tier.callgrind_safe {
        return Ok(Outcome::Skipped(format!(
            "tier {} emits instructions callgrind does not implement",
            tier.name
        )));
    }
    if !have("valgrind") {
        return Ok(Outcome::Skipped("valgrind is not installed".to_string()));
    }
    let dir = oracle_dir(tier, false);
    let theirs = dir.join("src/stockfish");
    if !theirs.is_file() {
        return Ok(Outcome::Skipped(format!(
            "no oracle at {}; run `cargo xtask oracle --tier {}`",
            theirs.display(),
            tier.name
        )));
    }
    verify_oracle(&dir, tier)?;
    let their_dir = theirs.parent().map(Path::to_path_buf).unwrap_or_default();

    // The `profiling` profile, NOT `release`: release strips symbols, and a profile whose
    // functions are all `???` compares nothing. It is release code generation otherwise, so
    // the tree it searches is the same tree.
    println!("fingerprint: building rfish at tier {} with symbols", tier.name);
    let mut cmd = Command::new(cargo());
    cmd.current_dir(workspace_root())
        .env("RUSTFLAGS", format!("-C target-cpu={}", tier.rustc))
        .args(["build", "--profile", "profiling", "--package", "rfish", "--bin", "stockfish"]);
    run(&mut cmd)?;
    let ours = workspace_root().join("target/profiling/stockfish");

    same_net(&ours, &resources_dir(), &theirs, &their_dir)?;

    // The SAME TREE first. A different tree is a different workload and every row below would
    // be noise wearing a number — and a node divergence is a bigger finding than anything
    // this step could report, so it is reported as itself rather than as twelve odd rows.
    let bench_nodes = |bin: &Path, cwd: &Path| -> Result<u64, String> {
        let out = capture_both(Command::new(bin).current_dir(cwd).arg("bench").args(DIFF_BENCH))?;
        // The bench TOTAL, not `searched_nodes`: that one reads the last `info` line, which is
        // the final position's own count and leaves the other fifty unchecked.
        crate::runner::node_total(&out)
    };
    let our_nodes = bench_nodes(&ours, &resources_dir())?;
    let their_nodes = bench_nodes(&theirs, &their_dir)?;
    if our_nodes != their_nodes {
        return Err(format!(
            "node counts differ (rfish {our_nodes}, upstream {their_nodes}) on bench {}; fix that \
             first — it is a bigger finding than anything this step reports",
            DIFF_BENCH.join(" ")
        ));
    }
    println!(
        "fingerprint: both engines search {our_nodes} nodes on bench {}",
        DIFF_BENCH.join(" ")
    );

    let mine = callgrind_calls(&ours, &resources_dir(), "rfish")?;
    let yours = callgrind_calls(&theirs, &their_dir, "upstream")?;

    let path = workspace_root().join("tools/fingerprint_groups.tsv");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bad = 0usize;
    let mut rows = 0usize;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, patterns)) = line.split_once('\t') else {
            return Err(format!("{}: `{line}` is not NAME<TAB>PATTERNS", path.display()));
        };
        rows += 1;
        let (ours_n, ours_hits) = sum_matching(&mine, patterns);
        let (theirs_n, theirs_hits) = sum_matching(&yours, patterns);
        // A pattern matching nothing on a side is a MISS, never a zero. A symbol the compiler
        // inlined away would otherwise read as permanent silent agreement at zero-versus-zero.
        if ours_hits == 0 || theirs_hits == 0 {
            let side = if ours_hits == 0 { "rfish" } else { "upstream" };
            println!("  {name:<20} no symbol matched on the {side} side  MISS");
            bad += 1;
        } else if ours_n == theirs_n {
            println!("  {name:<20} {theirs_n:>9}  EXACT");
        } else {
            let delta = ours_n as i64 - theirs_n as i64;
            println!("  {name:<20} upstream {theirs_n:>9}, rfish {ours_n:>9} ({delta:+})  DIFFERS");
            bad += 1;
        }
    }
    if rows == 0 {
        return Err(format!("{} declares no groups", path.display()));
    }
    Ok(Outcome::check(
        bad == 0,
        format!(
            "{bad} of {rows} groups diverge from upstream. A call-count divergence is an \
             ALGORITHM difference and outranks any cost finding. Check the pattern FIRST — an \
             inlined-away symbol reads exactly like a real divergence — then attribute the \
             count to its callers before concluding anything."
        ),
    ))
}

/// Sum the call counts of every symbol matching any `|`-separated substring.
///
/// Substrings rather than regexes, because the engine crate has no dependencies and neither
/// does this one. Every group needed here is a plain substring of a mangled name.
fn sum_matching(counts: &[(String, u64)], patterns: &str) -> (u64, usize) {
    let pats: Vec<&str> = patterns.split('|').map(str::trim).filter(|p| !p.is_empty()).collect();
    let mut total = 0;
    let mut hits = 0;
    for (name, n) in counts {
        if pats.iter().any(|p| name.contains(p)) {
            total += n;
            hits += 1;
        }
    }
    (total, hits)
}

/// Profile one engine and return how many times each function was CALLED.
///
/// Callgrind names a callee with `cfn=` and the count of calls to it on the following
/// `calls=` line, so summing those per callee is the number of times it was entered — the
/// caller-independent quantity, which is exactly what survives the two compilers laying the
/// code out differently.
fn callgrind_calls(bin: &Path, cwd: &Path, label: &str) -> Result<Vec<(String, u64)>, String> {
    println!("fingerprint: profiling {label} under callgrind (slow)");
    let out =
        std::env::temp_dir().join(format!("rfish-fingerprint-{label}-{}.out", std::process::id()));
    let text = capture_both(
        Command::new("valgrind")
            .current_dir(cwd)
            .args(["--tool=callgrind", "--cache-sim=no", "--branch-sim=no"])
            .arg(format!("--callgrind-out-file={}", out.display()))
            .arg(bin)
            .arg("bench")
            .args(DIFF_BENCH),
    )?;
    // A profile of a run that never searched looks entirely plausible and is worthless.
    if !text.contains("Nodes searched") {
        return Err(format!("the {label} profile carries no bench result: it did not search"));
    }
    let profile = std::fs::read_to_string(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let _ = std::fs::remove_file(&out);

    let mut names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut totals: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut callee: Option<String> = None;
    for line in profile.lines() {
        // `fn=` names the function being costed and `cfn=` the one it calls. Both use the
        // same `(id) name` compression pool: the name appears once and every later reference
        // is the bare id, so the map has to be built as the file is read.
        if let Some(rest) = line.strip_prefix("cfn=").or_else(|| line.strip_prefix("fn=")) {
            let is_callee = line.starts_with("cfn=");
            let (id, name) = split_compressed(rest);
            if let Some(name) = name {
                names.insert(id.to_string(), name.to_string());
            }
            let resolved = names.get(id).cloned().unwrap_or_else(|| format!("?{id}"));
            // An `fn=` line ENDS any pending callee: costs after it belong to the new frame.
            callee = is_callee.then_some(resolved);
        } else if let Some(rest) = line.strip_prefix("calls=")
            && let Some(callee) = callee.take()
        {
            let n: u64 = rest.split_whitespace().next().unwrap_or("0").parse().unwrap_or(0);
            *totals.entry(callee).or_default() += n;
        }
    }
    if totals.is_empty() {
        return Err(format!(
            "the {label} profile names no calls at all; was it built with symbols?"
        ));
    }
    Ok(totals.into_iter().collect())
}

/// Split callgrind's `(id) name` compression, where the name is present only the first time.
fn split_compressed(rest: &str) -> (&str, Option<&str>) {
    let rest = rest.trim_start();
    let Some(inner) = rest.strip_prefix('(') else {
        // Uncompressed: the whole thing is the name, and it is its own key.
        return (rest, Some(rest));
    };
    match inner.split_once(')') {
        Some((id, tail)) => {
            let tail = tail.trim();
            (id, (!tail.is_empty()).then_some(tail))
        }
        None => (rest, Some(rest)),
    }
}
