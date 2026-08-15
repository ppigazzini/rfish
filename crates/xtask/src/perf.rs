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
//! report. See `docs/10-tooling-ci.md` for which instrument settles which question.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::runner::{Outcome, cargo, node_total};
use crate::{capture, have, resources_dir, run, workspace_root};

/// A tier, named once so both sides of a differential move together.
///
/// rustc and upstream's Makefile spell the same machine differently, and a comparison of
/// `-C target-cpu=haswell` against `ARCH=x86-64-avx512icl` measures the ISA rather than the
/// engines. Naming the pair keeps that mistake out of the command line.
pub(crate) struct Tier {
    /// What this repository calls the tier.
    pub(crate) name: &'static str,
    /// The `-C target-cpu` rustc takes.
    pub(crate) rustc: &'static str,
    /// The `ARCH=` upstream's Makefile takes.
    upstream: &'static str,
    /// The `target_feature`s a host must report before `native` may select this tier.
    ///
    /// Named individually rather than implied by the `-C target-cpu`: the selector must
    /// answer "can this box RUN the tier", which is a question about the host, while the
    /// target-cpu answers "what does the tier emit", which is a question about the build.
    needs: &'static [&'static str],
    /// Whether valgrind can execute the code this tier emits.
    ///
    /// callgrind implements no AVX-512, and SIGILLs on the first instruction it does not
    /// know. An instruction differential therefore has a tier ceiling that the wall-clock
    /// A/B does not.
    callgrind_safe: bool,
}

/// The enumerated tiers, lowest first. **Every one is a FIXED `-C target-cpu`.**
///
/// `native` is not among them, and that is the point: `-C target-cpu=native` emits code that
/// is a property of the machine that ran the build — znver4 tuning on this box — so two hosts
/// reporting the same tier label ship different binaries, and every per-tier number in this
/// repository is a comparison across builds. `native` is a SELECTOR over this table instead;
/// see [`resolve_tier`]. ../mcfish 3b9fc8ae removed the same cause after patching the symptom
/// first, and ../zfish has always resolved it this way.
const TIERS: [Tier; 5] = [
    Tier {
        name: "sse41",
        rustc: "nehalem",
        upstream: "x86-64-sse41-popcnt",
        needs: &["sse4.1", "popcnt"],
        callgrind_safe: true,
    },
    Tier {
        name: "avx2",
        rustc: "haswell",
        upstream: "x86-64-avx2",
        needs: &["avx2", "bmi2"],
        callgrind_safe: true,
    },
    // The rung between avx2 and VNNI, so an AVX-512 host without VNNI is not dropped two
    // tiers by the selector's floor — ../mcfish added the same one for the same reason.
    Tier {
        name: "avx512",
        rustc: "skylake-avx512",
        upstream: "x86-64-avx512",
        needs: &["avx512f", "avx512bw", "avx512dq", "avx512vl"],
        callgrind_safe: false,
    },
    Tier {
        name: "vnni512",
        rustc: "cascadelake",
        upstream: "x86-64-vnni512",
        needs: &["avx512f", "avx512bw", "avx512dq", "avx512vl", "avx512vnni"],
        callgrind_safe: false,
    },
    // Upstream's top x86-64 tier. Its vbmi2/bitalg/vpopcntdq paths were reachable here only
    // through `target-cpu=native` on a capable box — which is to say by accident of the
    // build machine rather than by asking for a tier.
    Tier {
        name: "avx512icl",
        rustc: "icelake-server",
        upstream: "x86-64-avx512icl",
        needs: &[
            "avx512f",
            "avx512bw",
            "avx512dq",
            "avx512vl",
            "avx512vnni",
            "avx512vbmi2",
            "avx512bitalg",
            "avx512vpopcntdq",
        ],
        callgrind_safe: false,
    },
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
    resolve_tier(tier).ok().map(|t| t.rustc)
}

/// Every tier name, for an error message that can list the alternatives.
pub(crate) fn tier_names() -> Vec<&'static str> {
    TIERS.iter().map(|t| t.name).chain(std::iter::once("native")).collect()
}

/// An enumerated tier, and what the CURRENT host lacks before it could execute one.
pub(crate) struct TierRun {
    /// What this repository calls the tier.
    pub(crate) name: &'static str,
    /// The `-C target-cpu` rustc takes.
    pub(crate) rustc: &'static str,
    /// The `target_feature`s the tier needs and this host does not report.
    ///
    /// Empty when the host can execute a binary built at the tier; otherwise every name in
    /// it is a reason the binary would SIGILL on the first kernel that reaches for one.
    pub(crate) missing: Vec<&'static str>,
}

/// Every ENUMERATED tier, lowest first — `native` is not one — against this host.
///
/// For a caller that has to build each of them, which is the arch-determinism gate. That
/// gate also has to RUN each of them, and building a tier says nothing about whether this
/// box can execute it: `-C target-cpu=skylake-avx512` emits AVX-512 on any host, so driving
/// one on a host without AVX-512 is a SIGILL rather than a verdict. The answer comes from
/// the same feature list [`resolve_tier`] reads for `native`, so the two cannot drift apart.
pub(crate) fn enumerated_tiers() -> Result<Vec<TierRun>, String> {
    let features = host_features()?;
    Ok(TIERS
        .iter()
        .map(|t| TierRun {
            name: t.name,
            rustc: t.rustc,
            missing: t.needs.iter().copied().filter(|f| !features.iter().any(|h| h == f)).collect(),
        })
        .collect())
}

/// Resolve `--tier`, defaulting to the matched-ISA tier both ports have always quoted.
fn tier_of(args: &[&str]) -> Result<&'static Tier, String> {
    resolve_tier(arg_value(args, "--tier").unwrap_or("avx2"))
}

/// [`tier_of`], for a step in another module.
///
/// The tier belongs here because the tier TABLE does: a second enumeration elsewhere is a
/// second thing to keep in step with the first, and `--tier` must mean one thing everywhere.
pub(crate) fn tier_for(args: &[&str]) -> Result<&'static Tier, String> {
    tier_of(args)
}

/// A tier NAME to a tier — resolving `native` to the highest one this host can run.
///
/// **`native` selects; it never compiles `-C target-cpu=native`.** A build under that flag
/// carries whatever tuning and ISA extensions the build machine has, none of which any tier
/// label records, so a number filed under it cannot be reproduced anywhere — including on
/// this box after a CPU change. Resolving to an enumerated tier costs some host-specific
/// tuning and buys a build that is a property of its NAME.
fn resolve_tier(want: &str) -> Result<&'static Tier, String> {
    if want == "native" {
        let features = host_features()?;
        // Highest first: the selector's floor must not drop a capable host two rungs.
        let tier = TIERS
            .iter()
            .rev()
            .find(|t| t.needs.iter().all(|f| features.iter().any(|h| h == f)))
            .ok_or_else(|| {
                format!(
                    "this host reports none of the tier feature sets; it cannot even run \
                     '{}'. Name a tier explicitly",
                    TIERS[0].name
                )
            })?;
        println!("tier native resolves to {} ({})", tier.name, tier.rustc);
        return Ok(tier);
    }
    TIERS
        .iter()
        .find(|t| t.name == want)
        .ok_or_else(|| format!("unknown tier '{want}'; want one of {}", tier_names().join(", ")))
}

/// The `target_feature`s rustc says this host has.
///
/// Asked of rustc rather than read out of `/proc/cpuinfo`: rustc is the thing that decides
/// what a feature name means to a build, the answer is already in its vocabulary, and no
/// second parser has to be kept in step with a kernel's spelling. It is also the portable
/// route — the siblings read cpuinfo because their toolchains offer nothing equivalent.
fn host_features() -> Result<Vec<String>, String> {
    let cfg = capture(Command::new("rustc").args(["--print", "cfg", "-C", "target-cpu=native"]))?;
    Ok(cfg
        .lines()
        .filter_map(|l| l.strip_prefix("target_feature="))
        .map(|f| f.trim_matches('"').to_string())
        .collect())
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
        patch_out_threat_scan(&dest.join("src/position.cpp"))?;
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

/// Stub out `Position::update_piece_threats`, which only the NNUE accumulator ever reads.
///
/// **Without this the spine differential measures nothing, and it reports a number that
/// FLATTERS this port.** Upstream maintains the threat feature set inside `do_move`, writing a
/// `DirtyThreats` that `nnue/nnue_accumulator.cpp` reads and that nothing else reads. Under the
/// material evaluation `patch_material_eval` installs, nobody reads it at all — so leaving it in
/// charges upstream for NNUE bookkeeping while rfish, which recomputes threats inside its
/// evaluation, is charged for none. Measured here at the `c5aef2bf1` pin: 1,564,677,886
/// instructions with the scan in against 1,301,230,180 with it out, which turns a real 1.09 into
/// a fictitious 0.91.
///
/// The node count is unchanged either way, and `instruction_differential` re-asserts that: it
/// refuses to quote a ratio across two different trees. That is what proves the scan is dead
/// under a material evaluation rather than load-bearing.
///
/// **This patch is one HALF of a pair and is void without the other.** The clause above —
/// "rfish recomputes threats inside its evaluation" — stopped being true the day the
/// accumulator moved to a per-ply delta, and for a while this side was stubbed while rfish's
/// own recording still ran on every node: the harness then charged rfish 380M the oracle
/// never paid, and reported 1.291 for a spine that measures 0.999. `SearchWorker::do_move`
/// carries the mirror under the same `eval-material` feature, and
/// [`the_spine_stub_has_its_mirror`] fails if it is ever deleted.
fn patch_out_threat_scan(path: &Path) -> Result<(), String> {
    let src = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let signature = "void Position::update_piece_threats(";
    let start = src.find(signature).ok_or("the oracle has no update_piece_threats to patch")?;
    let open = "                                    [[maybe_unused]] Bitboard noRaysContaining) \
                const {\n";
    let body = src[start..]
        .find(open)
        .map(|i| start + i + open.len())
        .ok_or("update_piece_threats does not end its parameter list where expected")?;

    // Cast the parameters to void rather than dropping their names: the oracle builds with
    // -Wall -Wextra, and an unused parameter in a header-visible template is noise a reader of
    // this build log would have to triage.
    let stub = "    // MEASUREMENT HARNESS installed by `cargo xtask oracle --spine`: the threat\n\
        \x20   // feature set is read only by the NNUE accumulator, which a material evaluation\n\
        \x20   // never runs. Maintaining it here would charge upstream for bookkeeping the port\n\
        \x20   // being measured against it does not do.\n\
        \x20   (void) pc;\n\
        \x20   (void) putPiece;\n\
        \x20   (void) s;\n\
        \x20   (void) noRaysContaining;\n\
        \x20   if (dts)\n\
        \x20       return;\n\n";

    let patched = format!("{}{stub}{}", &src[..body], &src[body..]);
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

/// The instruction budget as an A/B against a git ref, with NO stored golden.
///
/// **The half `perf-budget` cannot do.** That step holds an ABSOLUTE count against a row in
/// `tools/instr_budget.golden`, and the count is a property of the toolchain as much as of
/// the code — so the golden is per-machine, gitignored, and has to be re-derived by hand
/// after every nightly bump. The consequence is that it binds nowhere except the box that
/// derived it: a fresh clone has no row, and CI has no row it could trust.
///
/// Building both sides here removes the golden entirely. The toolchain, the tier, the net
/// and the workload are shared by construction, so they cancel, and what is left is the
/// change. Ported from `../Stockfish`'s `refish` branch `9c26b4d6`.
///
/// Interleaving is not needed and would buy nothing: callgrind counts instructions RETIRED
/// and is deterministic, so the two sides do not compete for a thermal state the way the
/// wall-clock A/B in `perf` does. What IS needed is the node-count equality below — a
/// smaller count is a smaller workload, and dividing one by the other would report a
/// different search as a cheaper one.
pub(crate) fn budget_ab(args: &[&str]) -> Result<Outcome, String> {
    let tier = tier_of(args)?;
    if !tier.callgrind_safe {
        return Err(format!(
            "tier '{}' cannot be counted: callgrind implements no AVX-512 and SIGILLs on the \
             first instruction it does not know",
            tier.name
        ));
    }
    if !have("valgrind") {
        return Ok(Outcome::Skipped("valgrind is needed to count instructions".to_string()));
    }
    let base = arg_value(args, "--base").unwrap_or("HEAD");
    let rounds: usize =
        arg_value(args, "--rounds").and_then(|v| v.parse().ok()).unwrap_or(BUDGET_ROUNDS);

    // The same refusal `codegen-equiv` makes, for the same reason: with a clean checkout the
    // two sides are one tree, the delta is zero by construction, and a zero that was never
    // in doubt reads exactly like a change that cost nothing.
    let changed = crate::runner::tracked_rust_diff(base)?;
    if changed.is_empty() {
        return Ok(Outcome::Skipped(format!(
            "nothing to measure: no tracked Rust source differs from {base}. This step counts \
             the WORKING TREE against a ref, so on a clean checkout both sides are one build"
        )));
    }

    let scratch = workspace_root().join("target/budget-ab");
    std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    let tree = scratch.join("base-tree");
    crate::runner::worktree_at(base, &tree)?;

    let flags = format!("-C target-cpu={}", tier.rustc);
    println!("budget-ab: building {base} and the working tree at tier {}", tier.name);
    let base_bin = build_release_at(&tree, &scratch.join("base-target"), &flags)?;
    let head_bin = build_release_at(&workspace_root(), &scratch.join("head-target"), &flags)?;
    crate::runner::worktree_remove(&tree);

    // Both sides run from THIS repository's resources/, never from the worktree's: the net is
    // a runtime input, and two sides reading two files would be two engines.
    let (base_ir, base_nodes) = budget_ir(&base_bin, &resources_dir(), rounds)?;
    let (head_ir, head_nodes) = budget_ir(&head_bin, &resources_dir(), rounds)?;

    if base_nodes != head_nodes {
        return Ok(Outcome::Fail(format!(
            "the two sides searched different trees ({base} {base_nodes} nodes, working tree \
             {head_nodes}), so there is no instruction comparison to make. Settle the node \
             count with `signature` first"
        )));
    }

    let delta = head_ir as i64 - base_ir as i64;
    let pct = delta as f64 / base_ir as f64;
    println!("budget-ab: {base_nodes} nodes on both sides, startup subtracted");
    println!("  {base:>12}  {base_ir:>15}");
    println!("  working tree  {head_ir:>15}  {delta:+} ({:+.4}%)", pct * 100.0);

    if pct > BUDGET_TOLERANCE {
        return Ok(Outcome::Fail(format!(
            "the working tree retires {delta:+} instructions ({:+.4}%) over {base}, past the \
             {:.4}% tolerance",
            pct * 100.0,
            BUDGET_TOLERANCE * 100.0
        )));
    }
    println!(
        "budget-ab: within the {:.4}% tolerance{}",
        BUDGET_TOLERANCE * 100.0,
        if delta < 0 { " -- and an improvement" } else { "" }
    );
    Ok(Outcome::Pass)
}

/// One `--release` build from `src` into its own target directory.
fn build_release_at(src: &Path, target_dir: &Path, flags: &str) -> Result<PathBuf, String> {
    run(Command::new(cargo())
        .current_dir(src)
        .env("RUSTFLAGS", flags)
        .env("CARGO_TARGET_DIR", target_dir)
        .args(["build", "--release", "--package", "rfish", "--bin", "stockfish"]))?;
    let bin = target_dir.join("release").join(crate::runner::engine_file_name());
    if !bin.is_file() {
        return Err(format!("{} was not produced", bin.display()));
    }
    Ok(bin)
}

/// Where the per-tier instruction budgets live.
const BUDGET_GOLDEN: &str = "tools/instr_budget.golden";

/// How far the measured count may sit from the recorded one before the gate reddens.
///
/// **Set by MUTATION, not by feel**, which is the discipline both sibling ports paid for:
/// ../zfish shipped 0.20% and watched a real regression sail through it, ../mcfish shipped
/// 0.5% and watched the same one. The mutation has to be re-run HERE, because the value is a
/// property of this instrument and this workload, and the number it produced is why this is
/// not either sibling's figure: forcing `Position::adjust_key50` out of line — their mutation
/// too — costs **+0.0541%** here, so 0.05% would have caught it by a factor of 1.08 and
/// missed anything smaller.
///
/// Against a spread of ten instructions in 1.7e9 across a from-scratch rebuild, 0.005% is
/// ~8000x the noise and ~11x under the mutation. `docs/10-tooling-ci.md` records the run.
const BUDGET_TOLERANCE: f64 = 0.000_05;

/// How many times the bench is profiled before the median is taken.
///
/// One would do on a deterministic instrument. Two is what makes the cross-round workload
/// check mean anything: an engine that dies mid-gate, or a net that goes missing after the
/// first launch, otherwise reports a smaller count that reads as an improvement.
const BUDGET_ROUNDS: usize = 2;

/// Hold an absolute instruction count to a recorded budget — the regression nothing else sees.
///
/// `signature` proves the same NODE count and says nothing about what those nodes cost, so a
/// change can shed no nodes, keep every gate in `parity` green, and still run measurably
/// slower. Ported from ../mcfish's `perf-budget` and ../zfish 51031f48, with the instrument
/// swapped: both siblings read hardware counters through `perf_event_open`, which many CI
/// containers refuse; rfish already has callgrind wired for the differential and it is
/// deterministic, so the budget reuses it.
///
/// **Local, and deliberately NOT in `parity`.** The count is a property of the toolchain as
/// much as of the code, so the pinned nightly moving legitimately moves it. Run it after a
/// perf commit and after a toolchain bump.
pub(crate) fn perf_budget(args: &[&str], update: bool) -> Result<Outcome, String> {
    // `native` has already resolved to an enumerated tier, so a row is keyed by a name that
    // decides the codegen — the reason ../zfish 7d4de85f could refuse a `native` key outright
    // and ../mcfish 3b9fc8ae dropped the target-cpu it had folded INTO the key. What remains
    // is an instrument limit and nothing to do with keys: callgrind implements no AVX-512.
    let tier = tier_of(args)?;
    if !tier.callgrind_safe {
        return Err(format!(
            "tier '{}' cannot carry a budget: callgrind implements no AVX-512 and SIGILLs on \
             the first instruction it does not know, so there is no count to record. Budget a \
             tier it can execute: {}",
            tier.name,
            TIERS
                .iter()
                .filter(|t| t.callgrind_safe)
                .map(|t| t.name)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !have("valgrind") {
        return Ok(Outcome::Skipped("valgrind is needed to count instructions".to_string()));
    }
    let rounds: usize =
        arg_value(args, "--rounds").and_then(|v| v.parse().ok()).unwrap_or(BUDGET_ROUNDS);

    // Its own target directory, so this gate cannot leave a tier-built binary in
    // `target/release` for the next one to measure or ship by accident.
    let dir = workspace_root().join("target/budget").join(tier.name);
    println!("perf-budget: building at tier {} ({})", tier.name, tier.rustc);
    cargo_build(&format!("-C target-cpu={}", tier.rustc), &dir, false)?;
    let bin = dir.join("release/stockfish");

    let (ir, nodes) = budget_ir(&bin, &resources_dir(), rounds)?;
    println!(
        "perf-budget: {ir} instructions over {nodes} nodes (bench {}, startup subtracted)",
        DIFF_BENCH.join(" ")
    );

    let path = workspace_root().join(BUDGET_GOLDEN);
    if update {
        write_budget(&path, tier, nodes, ir)?;
        println!("perf-budget: recorded {} {} {nodes} {ir}", tier.name, tier.rustc);
        return Ok(Outcome::Pass);
    }

    let Some(row) = read_budget(&path, tier)? else {
        return Ok(Outcome::Skipped(format!(
            "no budget recorded for tier '{}'; run `cargo xtask perf-budget-update --tier {}`",
            tier.name, tier.name
        )));
    };
    if row.nodes != nodes {
        return Ok(Outcome::Fail(format!(
            "the workload moved: {nodes} nodes against the budget's {}. An instruction count \
             over a different tree is not comparable — settle `signature` first, then re-record",
            row.nodes
        )));
    }
    #[allow(clippy::cast_precision_loss)]
    let delta = (ir as f64 - row.ir as f64) / row.ir as f64;
    println!(
        "perf-budget: budget {}, delta {:+.4}% against a tolerance of {:+.4}%",
        row.ir,
        delta * 100.0,
        BUDGET_TOLERANCE * 100.0
    );
    if delta.abs() <= BUDGET_TOLERANCE {
        return Ok(Outcome::Pass);
    }
    Ok(Outcome::Fail(format!(
        "{} by {:.4}%, outside the {:.4}% tolerance. {}",
        if delta > 0.0 { "REGRESSED" } else { "improved" },
        delta.abs() * 100.0,
        BUDGET_TOLERANCE * 100.0,
        if delta > 0.0 {
            "Find the cost before re-recording: a budget raised to fit the tree gates nothing"
        } else {
            "Re-record it with `perf-budget-update` once the win is understood"
        }
    )))
}

/// One recorded budget row.
struct Budget {
    nodes: u64,
    ir: u64,
}

/// The median instruction count over `rounds`, with the workload held across all of them.
///
/// **Every round is held to round one's node count.** ../zfish credits exactly this check
/// with catching an ablation that searched 162 860 nodes while claiming 163 081, and the
/// instruction delta read clean either way. A run whose workload moves is a rig fault, not a
/// measurement, so it refuses rather than publishing the smaller median as an improvement.
fn budget_ir(bin: &Path, cwd: &Path, rounds: usize) -> Result<(u64, u64), String> {
    if rounds == 0 {
        return Err("--rounds 0 measures nothing".to_string());
    }
    // Once, not per round: the same binary parses the same net every time, and this is the
    // half of the profile that is not the search.
    let startup = startup_ir(bin, cwd)?;
    let mut counts = Vec::with_capacity(rounds);
    let mut first_nodes = 0u64;
    for round in 1..=rounds {
        let (total, nodes, net) = budget_round(bin, cwd)?;
        // A measurement without a net is a measurement of a DIFFERENT ENGINE: the classical
        // fallback searches its own tree at its own cost, and reports a plausible number.
        if net.is_none() {
            return Err(format!(
                "the engine loaded no NNUE network from {} — it fell back to the classical \
                 evaluation, and that is a different engine. Run `cargo xtask net`",
                cwd.display()
            ));
        }
        if round == 1 {
            first_nodes = nodes;
        } else if nodes != first_nodes {
            return Err(format!(
                "the node count moved between rounds (round 1 = {first_nodes}, round {round} = \
                 {nodes}); the rounds measured different workloads and the median would read as \
                 a change in cost"
            ));
        }
        counts.push(total.saturating_sub(startup));
    }
    counts.sort_unstable();
    Ok((counts[counts.len() / 2], first_nodes))
}

/// One profiled bench: the whole-process count, the node total, and the net it loaded.
fn budget_round(bin: &Path, cwd: &Path) -> Result<(u64, u64, Option<String>), String> {
    let out = std::env::temp_dir().join(format!("rfish-cg-budget-{}.out", scratch_key(bin)));
    let mut cmd = Command::new("valgrind");
    cmd.current_dir(cwd)
        .args(["--tool=callgrind", "--cache-sim=no", "--branch-sim=no"])
        .arg(format!("--callgrind-out-file={}", out.display()))
        .arg(bin)
        .arg("bench")
        .args(DIFF_BENCH);
    let text = capture_both(&mut cmd)?;
    let net = text
        .lines()
        .find_map(|l| l.split_once("NNUE evaluation using "))
        .map(|(_, name)| name.trim().to_string());
    Ok((callgrind_total(&out)?, node_total(&text)?, net))
}

/// The row for `tier`, or `None` when the file records none.
fn read_budget(path: &Path, tier: &Tier) -> Result<Option<Budget>, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(None);
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() != 4 {
            return Err(format!("malformed budget row: {line}"));
        }
        if f[0] != tier.name {
            continue;
        }
        // The key names the BINARY, not the tier alone. A row whose target-cpu is not the one
        // this tier resolves to today was measured on a different build.
        if f[1] != tier.rustc {
            return Err(format!(
                "the budget for tier '{}' is keyed '{}' but the tier now resolves to '{}'; \
                 re-record it, because the row measured a different binary",
                tier.name, f[1], tier.rustc
            ));
        }
        let nodes =
            f[2].parse().map_err(|e| format!("{line}: the node count does not parse: {e}"))?;
        let ir = f[3].parse().map_err(|e| format!("{line}: the count does not parse: {e}"))?;
        return Ok(Some(Budget { nodes, ir }));
    }
    Ok(None)
}

/// Replace this tier's row, leaving every other tier's alone.
fn write_budget(path: &Path, tier: &Tier, nodes: u64, ir: u64) -> Result<(), String> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    // The header is rewritten from the constant every time rather than carried forward: a
    // file written months ago would otherwise keep explaining the gate as it was then, and
    // this is the only place a reader of the golden learns what the rows mean.
    let mut out = String::from(BUDGET_HEADER);
    for line in existing.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if line.split_whitespace().next().is_none_or(|t| t != tier.name) {
            out.push_str(line);
            out.push('\n');
        }
    }
    writeln!(out, "{} {} {nodes} {ir}", tier.name, tier.rustc)
        .map_err(|e| format!("building the budget row: {e}"))?;
    std::fs::write(path, out).map_err(|e| format!("{}: {e}", path.display()))
}

/// What a reader of the golden has to know before believing a row.
const BUDGET_HEADER: &str = "\
# Instructions retired over `bench 16 1 8`, startup subtracted, one row per TIER.
#
# `<tier> <target-cpu> <nodes> <instructions>`, written by `cargo xtask perf-budget-update`
# and checked by `cargo xtask perf-budget`.
#
# LOCAL AND PER-MACHINE -- .gitignore keeps this file out of the tree, because the count is
# a property of the toolchain and the libc as well as of the code. It is NOT an anchor:
# `tools/signature.golden` owns the one number this repository shares.
#
# The key names the BINARY rather than the host: the tier and the target-cpu it resolves to
# are both in it, and `native` is refused because it means a different binary on every box.
# A pinned-nightly bump or a new net moves the number legitimately -- re-record it.
#
# The node count is here so the gate can refuse a count taken over a different tree.
";

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
    let mut first_nodes = 0u64;
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
        // And hold every round to ROUND ONE, not just the two sides of a round to each other.
        // A pair that moves together — a net that goes missing after the first launch, an
        // engine restarted mid-run against a changed tree — passes the check above and
        // publishes a plausible median over a workload nobody asked for. ../mcfish 8d24312d
        // and ../zfish 03bbb6f7 both landed this after finding the guard applied on round
        // one alone.
        if round == 1 {
            first_nodes = our_nodes;
        } else if our_nodes != first_nodes {
            return Err(format!(
                "the workload moved between rounds (round 1 = {first_nodes} nodes, round \
                 {round} = {our_nodes}); the rounds are not comparable and the median would \
                 read as a change in speed"
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

/// The counter rows `docs/03-engine-eval.md` publishes, in its own order.
///
/// Callgrind's event names, paired with the label the page uses. The page's rule is that a
/// number it publishes has to come from a COMMAND rather than from a step someone remembers
/// taking, and this table was the one thing on it still being hand-run: `perf` disables both
/// simulators, so nothing here was reachable from `cargo xtask`.
const COUNTER_ROWS: [(&str, &str); 9] = [
    ("Ir", "instructions"),
    ("Dr", "data reads"),
    ("Dw", "data writes"),
    ("D1mr", "D1 read misses"),
    ("I1mr", "L1 icache misses"),
    ("Bc", "conditional branches"),
    ("Bcm", "conditional mispredicts"),
    ("Bi", "indirect branches"),
    ("Bim", "indirect mispredicts"),
];

/// `cargo xtask counters [--tier T] [--spine]` — the cache and branch table against upstream.
///
/// The same pairing discipline `perf` uses, for the same reason: clang + PGO on both sides,
/// the oracle verified at the pin, node counts required equal, and startup subtracted from
/// every event by measurement rather than assumed small. The simulators are DETERMINISTIC, so
/// unlike the paired clock this table is worth reading on a loaded box.
pub(crate) fn counters(args: &[&str]) -> Result<Outcome, String> {
    let tier = tier_of(args)?;
    let spine = has_flag(args, "--spine");

    if !tier.callgrind_safe {
        return Ok(Outcome::Skipped(format!(
            "tier {} is AVX-512; callgrind implements none of it and SIGILLs on the first \
             instruction it does not know. Measure counters at avx2.",
            tier.name
        )));
    }
    if !have("valgrind") {
        return Ok(Outcome::Skipped("valgrind is not installed".into()));
    }

    let suffix = if spine { format!("{}-spine", tier.name) } else { tier.name.to_string() };
    let ours = workspace_root().join("target/pgo-use").join(&suffix).join("release/stockfish");
    let theirs = oracle_dir(tier, spine).join("src/stockfish");
    for (what, p, how) in [("PGO build", &ours, "pgo"), ("oracle", &theirs, "oracle")] {
        if !p.is_file() {
            return Ok(Outcome::Skipped(format!(
                "no {what} at {}; run `cargo xtask {how} --tier {}{}`",
                p.display(),
                tier.name,
                if spine { " --spine" } else { "" }
            )));
        }
    }
    let their_dir = theirs.parent().map(Path::to_path_buf).unwrap_or_default();
    verify_oracle(&oracle_dir(tier, spine), tier)?;
    if !spine {
        same_net(&ours, &resources_dir(), &theirs, &their_dir)?;
    }

    println!("tier {} ({} vs ARCH={})", tier.name, tier.rustc, tier.upstream);
    println!(
        "\ncounters (cachegrind events, bench {}, startup subtracted, {} axis)",
        DIFF_BENCH.join(" "),
        if spine { "SPINE" } else { "NNUE" }
    );

    let (our_ev, our_nodes) = counter_events(&ours, &resources_dir())?;
    let (their_ev, their_nodes) = counter_events(&theirs, &their_dir)?;
    if our_nodes != their_nodes {
        return Err(format!(
            "node counts differ (rfish {our_nodes}, upstream {their_nodes}); different trees are \
             different workloads and every ratio below would be meaningless"
        ));
    }

    println!("  {:<24} {:>16} {:>16} {:>8}", "", "rfish", "upstream", "ratio");
    for (ev, label) in COUNTER_ROWS {
        let (Some(&a), Some(&b)) = (our_ev.get(ev), their_ev.get(ev)) else {
            println!("  {label:<24} {:>16}", "absent");
            continue;
        };
        if b == 0 {
            println!("  {label:<24} {a:>16} {b:>16} {:>8}", "n/a");
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let ratio = a as f64 / b as f64;
        println!("  {label:<24} {a:>16} {b:>16} {ratio:>8.3}");
    }
    println!("\n  over {our_nodes} nodes on both sides");
    Ok(Outcome::Pass)
}

/// Every simulated event for one binary, startup subtracted, with the bench's node count.
fn counter_events(bin: &Path, cwd: &Path) -> Result<(HashMap<String, u64>, u64), String> {
    let out = std::env::temp_dir().join(format!("rfish-cgd-{}.out", scratch_key(bin)));
    let mut cmd = Command::new("valgrind");
    cmd.current_dir(cwd)
        .args(["--tool=callgrind", "--cache-sim=yes", "--branch-sim=yes"])
        .arg(format!("--callgrind-out-file={}", out.display()))
        .arg(bin)
        .arg("bench")
        .args(DIFF_BENCH);
    let bench_out = capture_both(&mut cmd)?;
    let nodes = node_total(&bench_out)?;
    let bench = callgrind_events(&out)?;
    let start = counter_startup(bin, cwd)?;

    let mut net = HashMap::new();
    for (k, v) in bench {
        let s = start.get(&k).copied().unwrap_or(0);
        net.insert(k, v.saturating_sub(s));
    }
    Ok((net, nodes))
}

/// The same events over a `quit`-only run: the net parse, the magic tables, the zero-fill.
fn counter_startup(bin: &Path, cwd: &Path) -> Result<HashMap<String, u64>, String> {
    let out = std::env::temp_dir().join(format!("rfish-cgd-quit-{}.out", scratch_key(bin)));
    let mut child = Command::new("valgrind")
        .current_dir(cwd)
        .args(["--tool=callgrind", "--cache-sim=yes", "--branch-sim=yes"])
        .arg(format!("--callgrind-out-file={}", out.display()))
        .arg(bin)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
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
    callgrind_events(&out)
}

/// Zip a profile's `events:` names against its `summary:` counts.
///
/// Positional, so the two lines must be read together: the event ORDER is a property of the
/// run's simulator flags, and reading the summary against a remembered order is how a cache
/// row silently becomes a branch row.
fn callgrind_events(path: &Path) -> Result<HashMap<String, u64>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let names = text
        .lines()
        .find_map(|l| l.strip_prefix("events:"))
        .ok_or_else(|| format!("{}: no events line", path.display()))?;
    let counts = text
        .lines()
        .find_map(|l| l.strip_prefix("summary:"))
        .ok_or_else(|| format!("{}: no summary line", path.display()))?;
    let mut map = HashMap::new();
    for (name, count) in names.split_whitespace().zip(counts.split_whitespace()) {
        let v: u64 =
            count.parse().map_err(|e| format!("{}: {name} does not parse: {e}", path.display()))?;
        map.insert(name.to_string(), v);
    }
    if map.is_empty() {
        return Err(format!("{}: events and summary did not zip", path.display()));
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The oracle-side stub and the engine-side one are a PAIR, and half a pair is worse
    /// than neither half: it produces a plausible ratio measured over two different amounts
    /// of work. This asserts the engine still carries its half, because the failure is
    /// silent — the harness reports a number either way, and the number it reported while
    /// the pair was broken was 1.291 for a spine that measures 0.999.
    #[test]
    fn the_spine_stub_has_its_mirror() {
        let path = workspace_root().join("crates/rfish-engine/src/search/worker.rs");
        let src = std::fs::read_to_string(&path).expect("the search worker is in the tree");
        let stubbed = src
            .split("fn do_move(")
            .nth(1)
            .expect("SearchWorker::do_move is where the recording is made");
        let body = &stubbed[..stubbed.find("\n    fn ").unwrap_or(stubbed.len())];
        assert!(
            body.contains(r#"#[cfg(feature = "eval-material")]"#),
            "{}: SearchWorker::do_move no longer stubs its threat recording under \
             `eval-material`. The spine differential stubs the ORACLE's half in \
             `patch_out_threat_scan`; without this half it charges rfish for bookkeeping \
             upstream is excused from and the ratio it prints is void.",
            path.display()
        );
        assert!(
            body.contains("do_move_recording(mv, gives_check, None)"),
            "{}: the `eval-material` arm must still MAKE the move without recording, not \
             skip the recording by skipping the move.",
            path.display()
        );
    }
}
