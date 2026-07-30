//! The gates.
//!
//! Every gate answers one question and reports one of three outcomes. A gate that cannot
//! run reports [`Outcome::Skipped`] and exits 2 — never 0, because a skipped gate has
//! proven nothing.

use std::process::Command;

use crate::runner::{GATE_PROFILE, Outcome, build_engine, cargo, drive, engine_path, node_total};
use crate::{capture, have, run, workspace_root};

/// The bench depth the signature gate uses.
///
/// **Not upstream's 13.** rfish evaluates with the classical scaffolding until milestone M3
/// lands NNUE, and a scaffolding evaluation prunes far worse — depth 13 over the full list
/// takes hours rather than seconds. Seven is the depth at which the gate still runs in
/// under a minute, which is the property that decides whether anyone runs it.
///
/// Raise it to 13 in the SAME commit that lands the NNUE forward pass, and re-derive the
/// golden then. Until that happens `tools/signature.golden` is rfish's own number, not
/// upstream's; see `__DEV/PORTING.md`, "Two different numbers".
const SIGNATURE_DEPTH: &str = "7";
const SIGNATURE_HASH: &str = "16";
const SIGNATURE_THREADS: &str = "1";

/// Build the engine.
pub(crate) fn build(args: &[&str]) -> Result<Outcome, String> {
    let profile = arg_value(args, "--profile").unwrap_or(GATE_PROFILE);
    // `--arch` sets `-C target-cpu`, which changes which vector width the NNUE loops
    // autovectorise to. The DEFAULT build sets nothing, because the anchor has to be
    // reproducible on a machine nobody here owns.
    if let Some(tier) = arg_value(args, "--arch") {
        // SAFETY-FREE alternative to a global: pass it through the child's environment
        // rather than mutating this process's, which a later gate would inherit.
        let mut cmd = Command::new(cargo());
        cmd.current_dir(workspace_root()).env("RUSTFLAGS", format!("-C target-cpu={tier}")).args([
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
             # This is rfish's number TODAY, not the finish line. Upstream's `Bench:` at\n\
             # tools/upstream/UPSTREAM_BASE is the target, and rfish will not reach it until\n\
             # the NNUE forward pass lands. See __DEV/PORTING.md.\n\
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
    println!("perft: {} of {checked} positions match", checked - failures.len());
    Ok(Outcome::check(failures.is_empty(), format!("{} perft mismatches", failures.len())))
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
    for case in &cases {
        let stem = case.file_stem().and_then(|s| s.to_str()).ok_or("a case has no name")?;
        let script =
            std::fs::read_to_string(case).map_err(|e| format!("{}: {e}", case.display()))?;
        let lines: Vec<&str> = script.lines().filter(|l| !l.trim().is_empty()).collect();
        let out = drive(&engine, &lines)?;
        // Drop the lines that legitimately differ run to run, or every golden would be a
        // record of one machine's timing rather than of the engine's behaviour.
        let out = filter_volatile(&out);

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
    println!("golden: {} of {} cases match", cases.len() - failures.len(), cases.len());
    Ok(Outcome::check(failures.is_empty(), format!("golden mismatches: {}", failures.join(", "))))
}

/// Drop the output lines whose content depends on the clock or the machine.
fn filter_volatile(out: &str) -> String {
    let mut kept = String::with_capacity(out.len());
    for line in out.lines() {
        if line.starts_with("info depth")
            || line.starts_with("Total time")
            || line.starts_with("Nodes/second")
            || line.starts_with("Time:")
            || line.starts_with("info string NNUE")
            || line.starts_with("Compiled by")
        {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept
}

/// Documentation rot: every link resolves, and every named repository path exists.
///
/// This settles the mechanical half. It CANNOT tell you a sentence has become false —
/// that part is yours, and every false claim ever found in the sibling ports' docs got
/// there by a commit that changed the code and not the page.
pub(crate) fn docs_lint() -> Result<Outcome, String> {
    let root = workspace_root();
    let mut problems = Vec::new();
    let mut checked = 0;

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
                    if !root.join(word).exists() {
                        problems.push(format!(
                            "{}:{}: names a path that does not exist: {word}",
                            rel.display(),
                            n + 1
                        ));
                    }
                }
            }
        }
    }

    for p in &problems {
        eprintln!("  {p}");
    }
    println!("docs-lint: {checked} references checked across {} files", files.len());
    Ok(Outcome::check(problems.is_empty(), format!("{} documentation problems", problems.len())))
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
        // `__DEV/` is internal and gitignored; `target/` and `.git/` are not documentation.
        if name.starts_with('.') || name == "target" || name == "__DEV" || name == "resources" {
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
        if bytes[i] == b']' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            if let Some(end) = line[i + 2..].find(')') {
                let target = &line[i + 2..i + 2 + end];
                if !target.is_empty() {
                    out.push(target);
                }
                i += 2 + end;
                continue;
            }
        }
        i += 1;
    }
    out
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
        ("test", test),
        ("perft", perft),
        ("golden", || golden(false)),
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
    fn markdown_links_are_extracted_without_their_anchors() {
        let line = "See [the docs](docs/README.md) and [a section](docs/x.md#here).";
        assert_eq!(markdown_link_targets(line), vec!["docs/README.md", "docs/x.md#here"]);
        assert!(markdown_link_targets("no links here").is_empty());
        // An unterminated link must not panic or run off the end.
        assert!(markdown_link_targets("[broken](unclosed").is_empty());
    }

    #[test]
    fn volatile_lines_are_dropped_from_a_golden() {
        let out = "info depth 3 nodes 5\nreadyok\nTotal time (ms) : 7\nuciok\n";
        assert_eq!(filter_volatile(out), "readyok\nuciok\n");
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
