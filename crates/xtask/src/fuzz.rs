//! The scheduled fuzz step: random UCI text at the shell, random positions at the search.
//!
//! # Two harnesses, because they fail differently
//!
//! ../mcfish learned this the hard way and says so in `2b8eaad7`: fuzzing the shipped
//! binary's stdin spends most of a mutation's budget on the PARSER, and never reaches the
//! search behind it. So it grew a second, in-process harness. rfish has the same split:
//!
//! - the UCI surface, driven here as a subprocess, which is where a malformed command, a
//!   nonsense option value or a truncated FEN has to be survived;
//! - the search itself, in `rfish_engine::search::fuzz`, reached in-process because that is
//!   the only way to spend a budget on movegen, the move picker, the transposition table,
//!   pruning, qsearch and the accumulator rather than on tokenising.
//!
//! # What a clean run means
//!
//! "Nothing failed inside that budget." NOT "there is nothing to find." The step prints the
//! seed it used for exactly that reason: the value of a fuzz run is a reproducible failure,
//! and a seed nobody wrote down is a failure nobody can act on.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::runner::{GATE_PROFILE, Outcome, build_engine, cargo};

/// How long one script may take before it is treated as wedged.
///
/// Generous: a mutation can legitimately ask for a real search, and the slowest legitimate
/// script measured here is a couple of seconds. What this catches is the case worth
/// catching — an input that makes the engine stop responding — and it catches it as a
/// REPORTED failure with the input attached, rather than as a CI job that hangs until the
/// runner's own timeout kills it with no evidence.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Tokens a mutation is built from.
///
/// Half of these are legal UCI and half are not, on purpose. A fuzzer that only emits
/// well-formed input tests the happy path at great expense; one that only emits garbage
/// never gets past the first token. The interesting failures are in between — a valid
/// command with one wrong argument, which is what mixing these produces.
///
/// `bench` is deliberately absent: a 51-position benchmark that takes minutes, so a mutation
/// containing it spends the entire budget proving what the `signature` gate proves exactly.
/// Everything that makes a search UNBOUNDED -- `infinite`, `ponder`, `mate`, and a bare `go`
/// with no limit at all -- is deliberately present, because being able to stop one is part of
/// what this is testing. See `drive_bounded` for how the stop is delivered.
const TOKENS: &[&str] = &[
    "uci",
    "isready",
    "ucinewgame",
    "position",
    "startpos",
    "fen",
    "moves",
    "go",
    "depth",
    "nodes",
    "movetime",
    "wtime",
    "btime",
    "winc",
    "binc",
    "movestogo",
    "infinite",
    "ponder",
    "mate",
    "searchmoves",
    "perft",
    "stop",
    "setoption",
    "name",
    "value",
    "Hash",
    "Threads",
    "MultiPV",
    "Skill Level",
    "UCI_Elo",
    "SyzygyPath",
    "d",
    "eval",
    "flip",
    "e2e4",
    "e7e8q",
    "0000",
    "zzzz",
    "-1",
    "0",
    "1",
    "3",
    "99999999999999999999",
    "-9223372036854775808",
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
    "8/8/8/8/8/8/8/8 w - - 0 1",
    "not a fen at all",
    "",
    "\u{fffd}",
];

/// Feed `script` to the engine and read its reply, killing it if it stops answering.
///
/// `runner::drive` is the gate's version and waits forever, which is right for a gate whose
/// input is fixed and wrong for one whose input is random.
fn drive_bounded(engine: &Path, cwd: &Path, script: &[String]) -> Result<Option<String>, String> {
    let mut child = Command::new(engine)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", engine.display()))?;

    {
        let mut stdin = child.stdin.take().ok_or("the engine has no standard input")?;
        for l in script {
            writeln!(stdin, "{l}").map_err(|e| format!("writing to the engine: {e}"))?;
        }
        stdin.flush().map_err(|e| format!("writing to the engine: {e}"))?;

        // One `stop` per line of the burst, and no pauses. A burst can legitimately start an
        // UNBOUNDED search -- `go infinite`, `go mate 1`, or a bare `go` with no limit -- and
        // it can contain SEVERAL, each needing its own stop, because the commands queued
        // behind the first are not dispatched until it returns.
        //
        // Buffering them all is safe now and was not always: a `stop` behind a `go` in the
        // same write used to be dropped, because the input reader saw it before the main loop
        // had dispatched the `go`, whose reset then cleared it. This step found that, and it
        // is fixed -- see `SharedState::clear_stop`. Sending them unpaced is the stronger
        // test of the two, so it is what runs.
        for _ in 0..=script.len() {
            writeln!(stdin, "stop").map_err(|e| format!("writing to the engine: {e}"))?;
        }

        writeln!(stdin, "isready").map_err(|e| format!("writing to the engine: {e}"))?;
        writeln!(stdin, "quit").map_err(|e| format!("writing to the engine: {e}"))?;
    }

    // Drained on a thread: an engine that fills the pipe while nothing reads it would block
    // forever, and the harness would report a hang that is its own fault.
    let mut out = child.stdout.take().ok_or("the engine has no standard output")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = out.read_to_string(&mut text);
        let _ = tx.send(text);
    });

    if let Ok(text) = rx.recv_timeout(SCRIPT_TIMEOUT) {
        let _ = child.wait();
        return Ok(Some(text));
    }
    // Nothing came back in time. Kill it, so a wedged engine cannot outlive the step and
    // hold the runner until its own timeout fires with no evidence attached.
    let _ = child.kill();
    let _ = child.wait();
    Ok(None)
}

/// A seeded xorshift. Same reason as the engine-side harness: a failure has to replay.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// One line of between one and six tokens.
fn line(rng: &mut Rng) -> String {
    let n = 1 + rng.below(6);
    let mut parts = Vec::with_capacity(n);
    for _ in 0..n {
        parts.push(TOKENS[rng.below(TOKENS.len())]);
    }
    parts.join(" ")
}

/// `cargo xtask fuzz [seconds]` — the scheduled step.
///
/// Splits its budget between the two harnesses. `RFISH_FUZZ_SEED` pins the run for a replay;
/// without it the seed comes from the clock, so a nightly job broadens coverage instead of
/// re-walking the positions it walked yesterday.
pub(crate) fn fuzz(args: &[&str]) -> Result<Outcome, String> {
    let seconds: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(30);
    let seed = std::env::var("RFISH_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(1, |d| d.as_nanos() as u64)
        });
    let half = (seconds / 2).max(1);
    println!("fuzz: seed {seed} -- replay this run with RFISH_FUZZ_SEED={seed}");

    let engine = build_engine(GATE_PROFILE)?;
    let resources = crate::resources_dir();
    let mut rng = Rng(seed.max(1));
    let deadline = Instant::now() + Duration::from_secs(half);
    let mut scripts = 0usize;

    while Instant::now() < deadline {
        // A burst rather than one line at a time: state carries between commands, and the
        // failures worth finding are the ones that need a `position` before a `go`.
        // A burst rather than one line at a time: state carries between commands, and the
        // failures worth finding are the ones that need a `position` before a `go`.
        let burst: Vec<String> = (0..6).map(|_| line(&mut rng)).collect();
        let replay = burst.join("\n");

        let out = drive_bounded(&engine, &resources, &burst)
            .map_err(|e| format!("seed {seed}, script {scripts}: {e}\ninput was:\n{replay}"))?;
        // Every burst ends by asking the engine to prove it is still answering. A process
        // that survived by wedging, or by losing its parser state, fails here rather than
        // passing silently.
        match out {
            Some(text) if text.contains("readyok") => {}
            Some(_) => {
                return Ok(Outcome::Fail(format!(
                    "seed {seed}, script {scripts}: the engine stopped answering isready\n\
                     input was:\n{replay}"
                )));
            }
            None => {
                return Ok(Outcome::Fail(format!(
                    "seed {seed}, script {scripts}: no reply within {}s\ninput was:\n{replay}",
                    SCRIPT_TIMEOUT.as_secs()
                )));
            }
        }
        scripts += 1;
    }
    println!("fuzz: {scripts} UCI scripts survived, engine still answering");

    // The search half, in-process, under the profile whose debug assertions and overflow
    // checks are what turn a silent wrong number into a failure.
    let status = Command::new(cargo())
        .current_dir(crate::resources_dir())
        .args([
            "test",
            "--package",
            "rfish-engine",
            "--profile",
            "gate",
            "search::fuzz::tests::soak",
            "--",
            "--ignored",
            "--nocapture",
        ])
        .env("RFISH_FUZZ_SECONDS", half.to_string())
        .env("RFISH_FUZZ_SEED", seed.to_string())
        .status()
        .map_err(|e| format!("running the search soak: {e}"))?;

    Ok(Outcome::check(
        status.success(),
        format!("the search soak failed; replay with RFISH_FUZZ_SEED={seed}"),
    ))
}
