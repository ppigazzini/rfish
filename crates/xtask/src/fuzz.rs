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
//!   pruning, qsearch and the accumulator rather than on tokenising;
//! - the TABLEBASE PARSE, in `rfish_engine::platform::syzygy::fuzz`, which is the only input
//!   here that is a binary FILE rather than text. Both sibling ports fuzz it -- ../mcfish with
//!   a dedicated lane, ../zfish with its own targets -- and it is the surface where a bad byte
//!   becomes an index rather than a rejected token. It found six panics on the day it was
//!   written; see `docs/05-tablebases.md`.
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

/// How long each `stop` is given to land in the search it is meant for.
const STOP_PAUSE: Duration = Duration::from_millis(120);

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
        // A write that fails is the engine having EXITED, not a harness fault. Upstream
        // terminates on a command it cannot use -- a malformed FEN is a critical error and
        // exit 1 -- and rfish now does the same, so random input reaches that path often.
        // What this harness is looking for is a HANG or a crash, and an engine that reports
        // the reason and leaves is neither: stop feeding it and read what it said.
        macro_rules! feed {
            ($($arg:tt)*) => {
                if writeln!(stdin, $($arg)*).is_err() {
                    None::<()>
                } else {
                    Some(())
                }
            };
        }
        let mut alive = true;
        for l in script {
            if feed!("{l}").is_none() {
                alive = false;
                break;
            }
        }
        let _ = stdin.flush();

        // One `stop` per line of the burst, each after a pause, because a burst can start
        // SEVERAL unbounded searches -- `go infinite`, `go mate 1`, or a bare `go` with no
        // limit at all -- and the commands queued behind the first are not dispatched until
        // it returns.
        //
        // The pause is what makes each stop land in the search it is meant for. Writing them
        // all at once puts every one of them in the buffer BEFORE the first search starts, so
        // they collapse into the single flag that search consumes, and the second unbounded
        // `go` then runs forever. A pristine upstream build does exactly the same thing on
        // exactly the same input -- verified, both hang -- so this is the shape of the
        // protocol, not a defect in either engine, and the harness has to drive it the way a
        // GUI does. That is also why it cannot be tuned away: unpaced runs stay green for
        // fifty-odd scripts and then wedge, which is how this was missed the first time.
        for _ in 0..=script.len() {
            if !alive {
                break;
            }
            std::thread::sleep(STOP_PAUSE);
            if feed!("stop").is_none() {
                alive = false;
            }
            let _ = stdin.flush();
        }

        if alive {
            let _ = feed!("isready");
            let _ = feed!("quit");
        }
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

/// `cargo xtask fuzz [seconds] [harness]` — the scheduled step.
///
/// `harness` is `uci`, `search`, `tb` or `all` (the default). Naming ONE gives it the whole
/// budget, which is how the nightly workflow runs them: three jobs in parallel, each with the
/// full time, rather than one job dividing it — the shape ../mcfish's `mcfish_fuzz.yml` uses,
/// and for its reason. The three harnesses run at throughputs orders of magnitude apart, so a
/// single shared budget is really a budget for the fastest one, and a failure in the first
/// costs the other two their run.
///
/// `RFISH_FUZZ_SEED` pins the run for a replay; without it the seed comes from the clock, so a
/// nightly job broadens coverage instead of re-walking the positions it walked yesterday.
pub(crate) fn fuzz(args: &[&str]) -> Result<Outcome, String> {
    let seconds: u64 = args.first().and_then(|s| s.parse().ok()).unwrap_or(30);
    let harness = args.get(1).copied().unwrap_or("all");
    if !matches!(harness, "all" | "uci" | "search" | "tb") {
        return Err(format!("unknown fuzz harness `{harness}`: expected uci, search, tb or all"));
    }
    let seed = std::env::var("RFISH_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(1, |d| d.as_nanos() as u64)
        });
    // Divided only when asked for everything at once, which is what a local run wants.
    let share = if harness == "all" { (seconds / 3).max(1) } else { seconds.max(1) };
    println!("fuzz: seed {seed} -- replay this run with RFISH_FUZZ_SEED={seed}");

    let resources = crate::resources_dir();
    let mut rng = Rng(seed.max(1));
    let deadline = Instant::now() + Duration::from_secs(share);
    let mut scripts = 0usize;

    if matches!(harness, "all" | "uci") {
        let engine = build_engine(GATE_PROFILE)?;

        while Instant::now() < deadline {
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
                // A reported CRITICAL ERROR is the engine terminating ON PURPOSE, which is what
                // upstream does for a command it cannot use. It answers no `isready` afterwards
                // because it is gone, and that is the correct outcome rather than a wedge -- the
                // burst is random text, so it reaches that path often.
                Some(text) if text.contains("CRITICAL ERROR") => {}
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
    }

    // The in-process halves, under the profile whose debug assertions and overflow checks are
    // what turn a silent wrong number into a failure. Both are `#[ignore]`d tests rather than
    // xtask code: they need the engine's own internals, and reaching those from here would
    // mean making them public for the benefit of a harness.
    for (name, filter, what) in [
        ("search", "search::fuzz::tests::soak", "the search soak"),
        ("tablebase parse", "platform::syzygy::fuzz::tests::tb_parse_soak", "the tablebase soak"),
    ]
    .into_iter()
    .filter(|(name, _, _)| {
        harness == "all" || harness == if *name == "search" { "search" } else { "tb" }
    }) {
        println!("fuzz: {name}, {share}s");
        let status = Command::new(cargo())
            .current_dir(crate::resources_dir())
            .args([
                "test",
                "--package",
                "rfish-engine",
                // The literal, NOT `GATE_PROFILE`: that one is `release`, and release turns
                // OFF the debug assertions and overflow checks that make a soak able to see a
                // wrong number at all.
                "--profile",
                "gate",
                filter,
                "--",
                "--ignored",
                "--nocapture",
            ])
            .env("RFISH_FUZZ_SECONDS", share.to_string())
            .env("RFISH_FUZZ_SEED", seed.to_string())
            .status()
            .map_err(|e| format!("running {what}: {e}"))?;

        if !status.success() {
            return Ok(Outcome::Fail(format!("{what} failed; replay with RFISH_FUZZ_SEED={seed}")));
        }
    }

    Ok(Outcome::Pass)
}
