//! How a gate reports, and how the engine is driven.

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// What a gate concluded.
///
/// `Skipped` is a distinct outcome from `Pass` on purpose, and the process exit code
/// distinguishes them too. A gate whose tool is missing has proven NOTHING, and every
/// report that treats the two as the same has eventually laundered a red gate.
#[derive(Debug)]
pub(crate) enum Outcome {
    Pass,
    Fail(String),
    Skipped(String),
}

impl Outcome {
    /// `Pass` when `ok`, otherwise `Fail(why)`.
    pub(crate) fn check(ok: bool, why: impl Into<String>) -> Outcome {
        if ok { Outcome::Pass } else { Outcome::Fail(why.into()) }
    }

    /// True only for a real pass — never for a skip.
    #[must_use]
    pub(crate) fn is_pass(&self) -> bool {
        matches!(self, Outcome::Pass)
    }
}

/// Refuse a gate that had nothing to compare.
///
/// `Ok(false) == no mismatches` is true of an empty subject list, so a gate whose corpus is
/// missing, filtered away or renamed reports "0 of 0 match" and exits 0 — it does not merely
/// pass having compared nothing, it passes having reported a comparison it never made.
/// ../mcfish 01e0b71c found this in its transcript gate and ../zfish 108e7af6 in a step
/// checker reading 6% of its subject; the shape is the same wherever a denominator is
/// computed rather than asserted.
///
/// A missing corpus is [`Outcome::Skipped`] elsewhere in this file and stays that way — the
/// tool is absent, and exit 2 says so. This is the other case: the corpus is THERE and empty,
/// which is a rig fault the gate must go red for.
pub(crate) fn compared_something(count: usize, subject: &str, source: &str) -> Option<Outcome> {
    (count == 0).then(|| {
        Outcome::Fail(format!(
            "compared no {subject}: {source} yielded none, so this gate proves nothing. \
             Restore the corpus rather than reading the pass"
        ))
    })
}

/// Where the engine binary lands for `profile`.
#[must_use]
pub(crate) fn engine_path(profile: &str) -> PathBuf {
    let dir = match profile {
        "dev" | "debug" => "debug",
        other => other,
    };
    crate::workspace_root().join("target").join(dir).join(engine_file_name())
}

fn engine_file_name() -> &'static str {
    if cfg!(windows) { "stockfish.exe" } else { "stockfish" }
}

/// The profile the gates build and measure.
///
/// `release`, not `gate`: a measurement of the gate profile is a measurement of a binary
/// nobody ships. The unit tests run under `gate`, which is where the debug assertions are
/// wanted.
pub(crate) const GATE_PROFILE: &str = "release";

/// Build the engine binary and return its path.
pub(crate) fn build_engine(profile: &str) -> Result<PathBuf, String> {
    crate::run(Command::new(cargo()).current_dir(crate::workspace_root()).args([
        "build",
        "--package",
        "rfish",
        "--bin",
        "stockfish",
        "--profile",
        profile,
    ]))?;
    let path = engine_path(profile);
    if !path.is_file() {
        return Err(format!("built, but {} does not exist", path.display()));
    }
    Ok(path)
}

/// The cargo that invoked this xtask, so a gate uses the same toolchain the caller does.
#[must_use]
pub(crate) fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// Feed `script` to the engine on standard input and return everything it wrote.
///
/// The engine runs from `resources/`, which is where the net and every other runtime input
/// live. Running it from the repository root would find no net and produce an unrelated
/// number — and the number would look plausible.
pub(crate) fn drive(engine: &std::path::Path, script: &[&str]) -> Result<String, String> {
    drive_at(engine, &crate::resources_dir(), script)
}

/// Feed `script` to a binary running from `cwd`.
///
/// The oracle keeps its net beside itself rather than in this repository's `resources/`, so
/// a differential gate has to be able to say where each side runs.
pub(crate) fn drive_at(
    engine: &std::path::Path,
    cwd: &std::path::Path,
    script: &[&str],
) -> Result<String, String> {
    let cwd = cwd.to_path_buf();
    std::fs::create_dir_all(&cwd).map_err(|e| format!("{}: {e}", cwd.display()))?;

    let mut child = Command::new(engine)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|e| format!("{}: {e}", engine.display()))?;

    {
        let stdin = child.stdin.as_mut().ok_or("the engine has no standard input")?;
        for line in script {
            writeln!(stdin, "{line}").map_err(|e| format!("writing to the engine: {e}"))?;
        }
        writeln!(stdin, "quit").map_err(|e| format!("writing to the engine: {e}"))?;
    }

    let out = child.wait_with_output().map_err(|e| format!("waiting for the engine: {e}"))?;
    // Exit 1 is upstream's CRITICAL ERROR status, and rfish now uses it for the same
    // commands. The engine reporting a reason and leaving is valid output to capture, not a
    // failure to run -- but anything else (a signal, a panic under `panic = "abort"`) still
    // is, so the code is checked rather than ignored.
    if !out.status.success() && out.status.code() != Some(1) {
        return Err(format!("the engine exited with {}", out.status));
    }
    // Strip CR so a Windows checkout compares byte-for-byte against an LF golden.
    Ok(String::from_utf8_lossy(&out.stdout).replace('\r', ""))
}

/// The single number a `bench` run printed, or an error naming what was missing.
pub(crate) fn node_total(output: &str) -> Result<u64, String> {
    output
        .lines()
        .find_map(|l| l.strip_prefix("Nodes searched  : "))
        .ok_or_else(|| "the bench output has no 'Nodes searched' line".to_string())?
        .trim()
        .parse()
        .map_err(|e| format!("the node total does not parse: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skip_is_not_a_pass() {
        assert!(Outcome::Pass.is_pass());
        assert!(!Outcome::Skipped("no tool".into()).is_pass());
        assert!(!Outcome::Fail("red".into()).is_pass());
    }

    #[test]
    fn an_empty_corpus_reddens_a_gate_rather_than_passing_it() {
        // The whole point: `failures.is_empty()` is true of a corpus that was never read, so
        // the refusal has to key on the DENOMINATOR and nothing else.
        let refusal = compared_something(0, "positions", "tools/cases/eval.fens");
        assert!(matches!(refusal, Some(Outcome::Fail(_))));
        assert!(compared_something(1, "positions", "tools/cases/eval.fens").is_none());
    }

    #[test]
    fn the_node_total_is_read_from_the_bench_footer() {
        let out = "Total time (ms) : 100\nNodes searched  : 123456\nNodes/second    : 9\n";
        assert_eq!(node_total(out), Ok(123_456));
        assert!(node_total("nothing here").is_err());
    }

    #[test]
    fn the_engine_path_follows_the_profile_directory_naming() {
        // Cargo puts `dev` output in `debug/`, and every other profile in a directory of
        // its own name. Getting this wrong makes a gate measure a stale binary.
        assert!(engine_path("dev").ends_with(format!("debug/{}", engine_file_name())));
        assert!(engine_path("release").ends_with(format!("release/{}", engine_file_name())));
        assert!(engine_path("gate").ends_with(format!("gate/{}", engine_file_name())));
    }
}
