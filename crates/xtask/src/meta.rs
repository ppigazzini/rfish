//! The gates on the gates.
//!
//! Every other step here asks whether the ENGINE is right. These ask whether the battery is:
//! does anything run this check, and can this check fail? Both questions are invisible to a
//! green `parity`, and both sibling ports found real holes the moment they were made
//! mechanical rather than remembered — including, in ../mcfish, that the finish line of the
//! whole port was in no lane at all.

use std::path::Path;

use crate::runner::Outcome;
use crate::workspace_root;

/// Steps that legitimately run in no lane, each with the reason.
///
/// This list is the hole, so it is short and every line is argued. It expires in one
/// direction by construction: a step named here that DOES run somewhere is a stale excuse and
/// is reported, which is what stops the list growing into a place to put anything awkward.
const EXCUSED: &[(&str, &str)] = &[
    ("help", "prints the step list; asserts nothing"),
    ("bench", "a command, not a gate -- `signature` is what asserts its output"),
    ("fmt-fix", "the writing half of `fmt`, which parity runs"),
    ("signature-update", "re-derives the anchor; `signature` is what asserts it"),
    (
        "golden-update",
        "REFUSES by default -- `golden-audit --write` is the regenerator, and that runs",
    ),
    ("pgo", "a build mode for measurement; the shipped build is what `signature` asserts"),
    ("perf", "a MEASUREMENT, not a gate: it reports a ratio and has no verdict to assert"),
    ("counters", "the same MEASUREMENT on the cache and branch axes; also no verdict"),
    (
        "perf-budget",
        "LOCAL -- the golden is per-machine (gitignored), because a retired-instruction \
         count is toolchain- and CPU-specific",
    ),
    ("perf-budget-update", "writes that per-machine golden"),
    (
        "negative-control",
        "LOCAL -- it MUTATES tracked sources and rebuilds per row, so it cannot share a \
         checkout with anything; run it when a gate is edited",
    ),
    (
        "budget-ab",
        "LOCAL -- like `codegen-equiv` it measures a WORKING TREE against a ref, and a clean \
         checkout gives it nothing to compare",
    ),
    (
        "warm-ab",
        "LOCAL, and the heaviest step here -- `budget-ab`'s refusal plus a warm 60-ply \
         replay under callgrind on BOTH sides, which a hosted runner would spend its \
         whole budget on and then measure the hypervisor",
    ),
    (
        "codegen-equiv",
        "LOCAL -- it proves a WORKING-TREE refactor against a git ref, and inside parity \
         (where a clean checkout makes the two the same tree) it has nothing to compare",
    ),
    (
        "parity",
        "the aggregate. CI runs its members as separate jobs on purpose, so a red lane \
         names the gate that failed rather than the batch",
    ),
];

/// The smallest number of dispatch entries this gate will report a verdict over.
const STEP_FLOOR: usize = 25;

/// Every step runs in a workflow, runs inside `parity`, or is excused with a reason.
///
/// "A lane that is in no gate is not a lane" was a rule held by somebody remembering it. In
/// ../mcfish four differentials had quietly stopped being lanes before anything checked, and
/// `upstream-parity` — the finish line of that port — was one of them.
pub(crate) fn lane_coverage() -> Result<Outcome, String> {
    let root = workspace_root();
    let steps = dispatch_steps(&root)?;
    // The same floor `docs-lint`'s path check and its index sweep carry, for the same reason:
    // this reads a source file as TEXT, and an extraction that silently shrinks reports OK
    // over nothing.
    if steps.len() < STEP_FLOOR {
        return Ok(Outcome::Fail(format!(
            "parsed only {} dispatch entries (floor {STEP_FLOOR}) — the extraction went \
             stale, and a coverage verdict over a subject it did not read is worthless",
            steps.len()
        )));
    }

    let workflows = workflow_text(&root)?;
    let parity = parity_text(&root)?;

    let mut unlaned = Vec::new();
    let mut stale = Vec::new();
    for step in &steps {
        let excuse = EXCUSED.iter().find(|(name, _)| name == step).map(|(_, why)| *why);
        let laned = invokes(&workflows, step) || parity.contains(&format!("\"{step}\""));
        match (laned, excuse) {
            (true, Some(_)) => stale.push(step.clone()),
            (false, None) => unlaned.push(step.clone()),
            _ => {}
        }
    }

    for step in &stale {
        eprintln!("  STALE EXCUSE  {step} is excused but DOES run in a lane — delete the excuse");
    }
    for step in &unlaned {
        eprintln!("  NO LANE  {step} runs nowhere — put it in a workflow, in parity, or excuse it");
    }
    println!("lane-coverage: {} steps, {} excused with a reason", steps.len(), EXCUSED.len());
    Ok(Outcome::check(
        unlaned.is_empty() && stale.is_empty(),
        format!(
            "{} step(s) in no lane ({}), {} stale excuse(s) ({})",
            unlaned.len(),
            unlaned.join(", "),
            stale.len(),
            stale.join(", ")
        ),
    ))
}

/// The zones of `rfish-engine`, in the order they are declared to depend.
///
/// `board` reads nothing, `state` reads `board`, `eval` and `search` read both, `platform`
/// reads all of them. A module may name a zone BELOW it and not one at or above it.
const ZONES: [&str; 5] = ["board", "state", "eval", "search", "platform"];

/// One edge that crosses the declared direction, and why it is allowed to.
struct CrossingEdge {
    from: &'static str,
    to: &'static str,
    /// The file that carries it, relative to `crates/rfish-engine/src/`.
    file: &'static str,
    reason: &'static str,
}

/// The crossings this tree has, each with the reason it is not a defect.
///
/// **A baseline that expires in BOTH directions.** A crossing that is not here reddens the
/// gate, and an entry here that no longer exists reddens it too — otherwise the list becomes
/// a permanent excuse for work someone already did. `../Stockfish refish`'s `depcheck.sh`
/// keeps its baselines the same way, and it is the half that makes them worth having.
const CROSSINGS: &[CrossingEdge] = &[
    CrossingEdge {
        from: "board",
        to: "eval",
        file: "board/threats.rs",
        reason: "TESTS ONLY: the threat recorder is checked against the encoder that consumes it, and a differential is the only thing that can say the two agree",
    },
    CrossingEdge {
        from: "state",
        to: "search",
        file: "state/mod.rs",
        reason: "a real cycle, and DEBT rather than design: a stack frame stores `ContKey` and `CorrKey`, whose types are the search's while the frame is shared",
    },
    CrossingEdge {
        from: "search",
        to: "platform",
        file: "search/fuzz.rs",
        reason: "the harness drives a whole search, which needs a `ThreadPool`; `pub mod fuzz`, so unlike the board edge this one is compiled into every build",
    },
    CrossingEdge {
        from: "search",
        to: "platform",
        file: "search/worker.rs",
        reason: "the worker holds a `TableRegistry` and names its types directly. Upstream inverts this edge with a seam and this port does not, so it is structural rather than incidental — and it is the one the hand-written inventory in docs/00-architecture.md did not carry until this gate found it",
    },
];

/// No module names a zone at or above its own, except where a baseline says why.
///
/// **`cargo` cannot answer this.** The crate boundary is checked by the compiler, but this
/// graph is inside ONE crate and a cycle between modules of one crate builds fine. So the
/// direction was a property a reviewer maintained, and `docs/00-architecture.md` said so:
/// it carried a hand-written inventory of what crosses, with the note that a fourth edge
/// would be noticed by nobody. There was already a fourth.
///
/// **What it cannot see.** A `use` inside a block comment, and whether an edge is behind
/// `#[cfg(test)]` — the baseline records which are, because deciding it needs a parser and
/// the question the gate exists for is whether a FIFTH appears, not how the four are gated.
pub(crate) fn zone_check() -> Result<Outcome, String> {
    let root = workspace_root().join("crates/rfish-engine/src");
    let rank = |z: &str| ZONES.iter().position(|c| *c == z);

    let mut found: Vec<(String, String, String)> = Vec::new();
    for zone in ZONES {
        let mut files = Vec::new();
        crate::gates::collect_rust(&root.join(zone), &mut files);
        for file in files {
            let text =
                std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let rel = file
                .strip_prefix(&root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            for line in text.lines() {
                // A doc link is not a dependency. `state/mod.rs` names
                // `crate::search::worker::SearchWorker` in its header comment, and counting
                // that as an edge would put a phantom entry in the baseline for ever.
                if line.trim_start().starts_with("//") {
                    continue;
                }
                for other in ZONES {
                    if other != zone && line.contains(&format!("crate::{other}")) {
                        let edge = (zone.to_string(), other.to_string(), rel.clone());
                        if !found.contains(&edge) {
                            found.push(edge);
                        }
                    }
                }
            }
        }
    }

    // A crossing is an edge to a zone at or above the reader's own rank.
    let mut crossings: Vec<&(String, String, String)> =
        found.iter().filter(|(from, to, _)| rank(to) >= rank(from)).collect();
    crossings.sort();

    let mut problems = Vec::new();
    for (from, to, file) in &crossings {
        if !CROSSINGS.iter().any(|c| c.from == from && c.to == to && c.file == file) {
            problems.push(format!(
                "UNDECLARED {from} -> {to} in {file}: it names a zone at or above its own, and no baseline entry says why"
            ));
        }
    }
    for c in CROSSINGS {
        if !crossings.iter().any(|(from, to, file)| c.from == from && c.to == to && c.file == file)
        {
            problems.push(format!(
                "STALE the baseline still allows {} -> {} in {}, and that edge is gone.                  Delete the entry: a baseline that outlives its edge is an excuse",
                c.from, c.to, c.file
            ));
        }
    }

    // The reasons are PRINTED, not merely stored. A baseline nobody reads is a list of
    // exemptions that stops being questioned; printing it is what keeps each entry something
    // a reader can disagree with.
    for c in CROSSINGS {
        println!(" {} -> {} in {}\n      {}", c.from, c.to, c.file, c.reason);
    }
    for p in &problems {
        eprintln!(" \x1b[31m{p}\x1b[0m");
    }
    println!(
        "zone-check: {} edges across {} zones, {} crossing, {} declared",
        found.len(),
        ZONES.len(),
        crossings.len(),
        CROSSINGS.len()
    );
    Ok(Outcome::check(problems.is_empty(), format!("{} zone problem(s)", problems.len())))
}

/// Node counts repeat across `ucinewgame`, at twenty node budgets.
///
/// **The question no other gate here asks: does a search LEAVE anything behind?** `signature`
/// runs one bench, `perft` counts a tree, `golden` pins a transcript — every one of them
/// reads the first answer the process gives. This runs the same two positions twice in one
/// process with a `ucinewgame` between the rounds, and requires the second round to reproduce
/// the first node for node. Anything a search writes and `ucinewgame` fails to reset shows up
/// as a divergence: a history table, a stack entry, a correction bank, a root-move field, a
/// time-manager carry-over.
///
/// This is upstream's own `tests/reprosearch.sh`, which this port had never taken. Its budget
/// progression is upstream's too — `100 * 3^i / 2^i` for i in 1..=20 — and the reason it is
/// not a round number at any step is to land the stop inside the search at as many different
/// points as possible.
///
/// **What it cannot see.** Whether those node counts are the RIGHT ones, which is
/// `signature`'s question, and what a second thread would do to them: it runs at the default
/// thread count, so it establishes reproducibility for one worker only. A Lazy-SMP search is
/// not node-reproducible and no gate can make it so.
///
/// Upstream's version drives the engine through `expect` and exits 2 when it is missing —
/// and before that was fixed, a missing interpreter left `grep` matching nothing, `awk`
/// rejecting nothing, and the script printing `reprosearch testing OK` having checked
/// NOTHING. This one drives the binary the way every other gate here does, so there is no
/// interpreter to be absent and no pipeline whose exit status belongs to the last stage.
pub(crate) fn repro_search() -> Result<Outcome, String> {
    // The two positions upstream uses: the start position and a short opening line, so the
    // second search runs with a table the first one filled.
    const POSITIONS: [&str; 2] = ["position startpos", "position startpos moves e2e4 e7e6"];

    let engine = crate::runner::build_engine(crate::runner::GATE_PROFILE)?;

    let mut checked = 0usize;
    let mut problems = Vec::new();
    for i in 1..=20u32 {
        // Upstream's progression, at upstream's width. `3^20` needs more than 32 bits before
        // the division brings it back down, so the arithmetic is done at i64 throughout.
        let nodes = 100i64 * 3i64.pow(i) / 2i64.pow(i);

        let mut script = Vec::new();
        for round in 0..2 {
            if round == 1 {
                script.push("ucinewgame".to_string());
            }
            for pos in POSITIONS {
                script.push(pos.to_string());
                script.push(format!("go nodes {nodes}"));
            }
        }
        let lines: Vec<&str> = script.iter().map(String::as_str).collect();
        let out = crate::runner::drive(&engine, &lines)?;

        // One search per `bestmove`, and its node total is the last `nodes N` it reported.
        let mut totals = Vec::new();
        let mut last: Option<u64> = None;
        for line in out.lines() {
            if let Some(n) = line
                .split_whitespace()
                .skip_while(|t| *t != "nodes")
                .nth(1)
                .and_then(|t| t.parse::<u64>().ok())
            {
                last = Some(n);
            }
            if line.starts_with("bestmove") {
                totals.push(last.take());
            }
        }

        // An empty read satisfies every comparison below, so it is the failure it looks
        // like rather than a vacuous pass — the defect upstream's own version shipped with.
        if totals.len() != 4 {
            problems
                .push(format!("{nodes} nodes: the engine answered {} of 4 searches", totals.len()));
            continue;
        }
        for (slot, pos) in POSITIONS.iter().enumerate() {
            match (totals[slot], totals[slot + 2]) {
                (Some(first), Some(second)) if first == second => checked += 1,
                (Some(first), Some(second)) => problems.push(format!(
                    "{nodes} nodes, `{pos}`: {first} nodes before ucinewgame, {second} after"
                )),
                _ => problems.push(format!("{nodes} nodes, `{pos}`: a search reported no nodes")),
            }
        }
    }

    for p in &problems {
        eprintln!("  \x1b[31mdiffers\x1b[0m {p}");
    }
    println!("repro-search: {checked} of 40 searches reproduced across ucinewgame");
    Ok(Outcome::check(
        problems.is_empty(),
        format!("{} search(es) did not reproduce", problems.len()),
    ))
}

/// The invariants that hold on an interrupted search, whatever the clock did.
///
/// **No byte-golden can reach this path.** `tools/cases/` is driven by writing every line and
/// closing the pipe, so a `stop` there is read after the search already ended. A stop that
/// lands inside a RUNNING search ends it wherever the clock got to, and the final `info`
/// line's node count moves run to run — there is nothing to pin.
///
/// So this asserts INVARIANTS instead of values, which needs no reference at all: a search
/// that is stopped still answers with exactly one legal `bestmove` and leaves an engine that
/// is still alive. Those hold whatever the clock did. They are not rfish-authored expectations
/// of upstream's OUTPUT — they are properties of the UCI contract, and this is the only
/// instrument in the tree that reaches the interrupted-search path at all.
pub(crate) fn async_check() -> Result<Outcome, String> {
    let engine = crate::runner::build_engine(crate::runner::GATE_PROFILE)?;
    let cwd = crate::resources_dir();

    // The legal move list, from the engine itself: `go perft 1` enumerates the root. Read it
    // rather than hard-coding a list, or this gate carries an expectation of its own.
    let perft = crate::runner::drive(&engine, &["position startpos", "go perft 1"])?;
    let legal: Vec<String> = perft
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(m, _)| m.trim().to_string())
        .filter(|m| m.len() == 4 || m.len() == 5)
        .collect();
    if legal.len() != 20 {
        return Ok(Outcome::Skipped(format!(
            "RIG FAULT: read {} root moves from `go perft 1`, not the 20 the start position \
             has — the legality check below would compare against nothing",
            legal.len()
        )));
    }

    let mut problems = Vec::new();

    // 1. A stop INSIDE a running search: exactly one bestmove, and it is legal.
    let out = drive_async(
        &engine,
        &cwd,
        &["position startpos", "go infinite"],
        2,
        &["stop", "isready"],
        30,
    )?;
    let moves: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("bestmove ")).collect();
    match moves.as_slice() {
        [one] => {
            let played = one.split_whitespace().next().unwrap_or_default();
            if !legal.iter().any(|m| m == played) {
                problems.push(format!("stop: bestmove {played:?} is not legal in the position"));
            } else if !out.lines().any(|l| l == "readyok") {
                problems.push("stop: the engine did not answer isready afterwards".to_string());
            } else {
                println!(
                    "  \x1b[32mok\x1b[0m a stop ended a running search with one legal bestmove ({played})"
                );
            }
        }
        other => problems.push(format!("stop: expected exactly one bestmove, got {}", other.len())),
    }

    // 2. A bare stop with NO search running answers nothing and stays up. Upstream ignores it;
    //    an engine that answered here would be inventing a move.
    let out = crate::runner::drive(&engine, &["position startpos", "stop", "isready"])?;
    if out.lines().any(|l| l.starts_with("bestmove ")) {
        problems.push("idle stop: emitted a bestmove with no search running".to_string());
    } else if !out.lines().any(|l| l == "readyok") {
        problems.push("idle stop: the engine did not answer isready afterwards".to_string());
    } else {
        println!("  \x1b[32mok\x1b[0m a stop with no search running answers nothing and stays up");
    }

    // 3. `ponderhit` converts a pondering search into a real one and it still ends with one
    //    bestmove. `go ponder` searches without a clock until told the move was played.
    let out = drive_async(
        &engine,
        &cwd,
        &["position startpos", "go ponder wtime 1000 btime 1000"],
        2,
        &["ponderhit"],
        30,
    )?;
    let hits = out.lines().filter(|l| l.starts_with("bestmove ")).count();
    if hits == 1 {
        println!("  \x1b[32mok\x1b[0m ponderhit ended the pondering search with one bestmove");
    } else {
        problems.push(format!("ponderhit: expected exactly one bestmove, got {hits}"));
    }

    // 4. `quit` during a running search terminates. THE TIMEOUT IS THE ASSERTION: before `go`
    //    ran off the UCI thread this would have hung, and a hang in CI reads as an
    //    infrastructure flake rather than as the engine ignoring quit.
    match drive_async_status(&engine, &cwd, &["position startpos", "go infinite"], 2, &[], 30)? {
        Run::TimedOut => {
            problems.push(
                "quit: the engine did not exit within 30s of quit during a search".to_string(),
            );
        }
        Run::Exited(_) => {
            println!("  \x1b[32mok\x1b[0m quit during a running search exits");
        }
    }

    // 5. The same `quit`, arriving BEFORE the search starts. This is not a weaker version of
    //    invariant 4 — it is the opposite race, and the engine answers it in a different
    //    place. With no wait, the whole script sits in one buffer: the reader thread has read
    //    `quit` before the main loop has dispatched the `go` in front of it, so anything that
    //    decides by asking "is a search running" answers no and lets an infinite search run
    //    forever. It is also the ONLY shape that matters in practice, because writing a
    //    script and closing the pipe is how every gate, every harness and a piping GUI drive
    //    this binary — invariant 4's two-second wait is the artificial one.
    match drive_async_status(&engine, &cwd, &["position startpos", "go infinite"], 0, &[], 30)? {
        Run::TimedOut => {
            problems.push(
                "quit before the search started: the engine did not exit within 30s".to_string(),
            );
        }
        Run::Exited(_) => {
            println!("  \x1b[32mok\x1b[0m quit read before the search started still exits");
        }
    }

    for p in &problems {
        eprintln!("  \x1b[31m{p}\x1b[0m");
    }
    println!(
        "async-check: {} of 5 invariants hold on the interrupted-search path",
        5 - problems.len()
    );
    Ok(Outcome::check(problems.is_empty(), format!("{} invariant(s) broken", problems.len())))
}

/// Drive the engine, optionally WAITING between the opening script and the interrupting one.
///
/// The wait selects which race is under test, and both are. A non-zero wait puts the
/// interruption inside a search that is already running; a zero wait leaves the whole script
/// in one buffer, so the reader thread sees the interruption before the main loop has
/// dispatched the `go` in front of it.
///
/// **`tools/cases/` covers neither.** A golden is a byte comparison, and an interrupted search
/// ends wherever the clock got to, so there is nothing there to pin — and a case that opened an
/// unbounded search would hang the runner rather than fail it. Believing otherwise is what left
/// invariant 5 unwritten while the engine failed it.
fn drive_async(
    engine: &Path,
    cwd: &Path,
    open: &[&str],
    wait_secs: u64,
    then: &[&str],
    bound_secs: u64,
) -> Result<String, String> {
    let (out, _) = drive_async_inner(engine, cwd, open, wait_secs, then, bound_secs)?;
    Ok(out)
}

/// The same, for the case where the STATUS is the assertion rather than the output.
fn drive_async_status(
    engine: &Path,
    cwd: &Path,
    open: &[&str],
    wait_secs: u64,
    then: &[&str],
    bound_secs: u64,
) -> Result<Run, String> {
    let (_, run) = drive_async_inner(engine, cwd, open, wait_secs, then, bound_secs)?;
    Ok(run)
}

fn drive_async_inner(
    engine: &Path,
    cwd: &Path,
    open: &[&str],
    wait_secs: u64,
    then: &[&str],
    bound_secs: u64,
) -> Result<(String, Run), String> {
    use std::io::Write;

    let mut child = std::process::Command::new(engine)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", engine.display()))?;

    {
        let stdin = child.stdin.as_mut().ok_or("the engine has no standard input")?;
        for line in open {
            writeln!(stdin, "{line}").map_err(|e| format!("writing to the engine: {e}"))?;
        }
        stdin.flush().map_err(|e| format!("flushing to the engine: {e}"))?;
        std::thread::sleep(std::time::Duration::from_secs(wait_secs));
        for line in then {
            writeln!(stdin, "{line}").map_err(|e| format!("writing to the engine: {e}"))?;
        }
        writeln!(stdin, "quit").map_err(|e| format!("writing to the engine: {e}"))?;
    }
    // Drop stdin so the engine sees end of input even if it is waiting on more.
    drop(child.stdin.take());

    // Read stdout on this thread while bounding the wait: `wait_with_output` cannot be
    // interrupted, so a hung engine would hang the gate — and a gate that never answers is
    // exactly what this invariant is testing for.
    let mut stdout = child.stdout.take().ok_or("the engine has no standard output")?;
    let reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut buf);
        buf
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(bound_secs);
    let run = loop {
        match child.try_wait().map_err(|e| format!("waiting on the engine: {e}"))? {
            Some(status) => break Run::Exited(status.code().unwrap_or(-1)),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Run::TimedOut;
            }
            None => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    };
    let out = reader.join().map_err(|_| "the engine's output reader panicked")?;
    Ok((out.replace('\r', ""), run))
}

/// The pinned upstream commit and the golden checkout must agree, in BOTH directions.
///
/// `tools/upstream/UPSTREAM_BASE` names the commit rfish claims to match. Everything
/// differential in this tree — `golden-audit`, `upstream-nodes`, `nnue-check`, `tb`, `perf`'s
/// oracle — is built from `../Stockfish`, so the pin is only meaningful while that checkout
/// is actually AT it.
///
/// **The two directions are not the same finding.**
///
/// - The checkout **ahead** of the pin is normal: upstream moved and this port has not
///   followed yet. Informational, with the commit list, because that list is the re-port
///   worklist.
/// - The checkout **behind** the pin is a defect in the workspace, and RED. `../Stockfish` is
///   the golden; a checkout behind the pin means every grep of it, and every oracle built from
///   it, answers from source this tree has already ported past. Counting only the first
///   direction prints that state as "in sync", which is worse than silence — it asserts the
///   thing a reader would otherwise go and verify.
///
/// **rfish has no `UPSTREAM_TARGET`, and that is deliberate.** ../mcfish carries one because
/// it is mid-catch-up and needs to name the commit it is aiming AT while the base says what it
/// matches today. A sync here is atomic — `tools/upstream/README.md` requires the base and
/// `tools/signature.golden` to advance in the same commit, and a sync that cannot land
/// bit-exact is a bug report rather than a sync — so there is no catch-up state for a second
/// pin to describe. A file with no role is the scaffolding this tree deletes, not adds.
pub(crate) fn sync_status() -> Result<Outcome, String> {
    let root = workspace_root();
    let pin_file = root.join("tools/upstream/UPSTREAM_BASE");
    let pin = std::fs::read_to_string(&pin_file)
        .map_err(|e| format!("{}: {e}", pin_file.display()))?
        .trim()
        .to_string();
    if pin.is_empty() {
        return Ok(Outcome::Fail(format!(
            "{} is empty — the pin every differential gate resolves against names nothing",
            pin_file.display()
        )));
    }

    let golden = root.parent().map(|p| p.join("Stockfish")).unwrap_or_default();
    if !golden.join(".git").exists() {
        return Ok(Outcome::Skipped(format!(
            "no golden checkout at {} — the pin cannot be verified against anything",
            golden.display()
        )));
    }
    let git = |args: &[&str]| -> Result<String, String> {
        crate::capture(std::process::Command::new("git").current_dir(&golden).args(args))
            .map(|s| s.trim().to_string())
    };

    // A pin the golden does not contain is a broken pin, not a drift of zero. Reporting "up
    // to date" for a SHA nobody can resolve is the same class as comparing nothing.
    if git(&["cat-file", "-e", &format!("{pin}^{{commit}}")]).is_err() {
        return Ok(Outcome::Fail(format!(
            "UPSTREAM_BASE {pin} is not a commit in {} — fetch the golden, or the pin names \
             a commit that was rewritten away",
            golden.display()
        )));
    }
    let head = git(&["rev-parse", "HEAD"])?;
    let ahead: usize = git(&["rev-list", "--count", &format!("{pin}..HEAD")])?
        .parse()
        .map_err(|e| format!("counting commits ahead of the pin: {e}"))?;
    let behind: usize = git(&["rev-list", "--count", &format!("HEAD..{pin}")])?
        .parse()
        .map_err(|e| format!("counting commits behind the pin: {e}"))?;

    if behind > 0 {
        eprintln!("  the golden checkout is BEHIND the pin by {behind} commit(s)");
        eprintln!("      pinned {}, {} HEAD {}", &pin[..9], golden.display(), &head[..9]);
        if ahead > 0 {
            eprintln!("      and {ahead} ahead as well — the two have diverged");
        }
        eprintln!("      {} is the golden. Check it out AT the pin before", golden.display());
        eprintln!("      building an oracle or comparing anything against it.");
        return Ok(Outcome::Fail(format!(
            "the golden checkout is {behind} commit(s) behind UPSTREAM_BASE"
        )));
    }
    if ahead == 0 {
        println!("sync-status: the golden is checked out AT the pin, {}", &pin[..9]);
    } else {
        println!(
            "sync-status: upstream has moved {ahead} commit(s) past the pin ({} -> {})",
            &pin[..9],
            &head[..9]
        );
        println!("  Porting is human-gated: land ONE upstream commit per commit here.");
        for line in git(&["log", "--oneline", "--reverse", &format!("{pin}..HEAD")])?.lines() {
            println!("      {line}");
        }
    }
    Ok(Outcome::Pass)
}

/// One mutation, and the gate that must go red for it.
struct Mutant {
    /// What the mutation does, in the report.
    label: &'static str,
    /// The file it edits, relative to the workspace root.
    file: &'static str,
    /// Text that must appear exactly once' worth of meaning in that file.
    find: &'static str,
    /// What it becomes.
    replace: &'static str,
    /// The step that must exit non-zero while the mutation is in place.
    gate: &'static str,
}

/// One representative mutant per gate.
///
/// **Perturb the VALUE, do not remove the BOUND.** Every mutation here leaves a sane engine
/// searching a different tree. ../mcfish learnt this the expensive way: inverting an
/// activation clamp handed the search an evaluation with no ceiling, and the gate ran past
/// 900s twice — once for over 25 minutes — without returning a verdict. A gate that never
/// answers is not a gate that failed, so a timeout here is a RIG FAULT and never a detection.
///
/// One gate per row. The mutations are deliberately narrow, so a row proves one gate's teeth
/// and nothing else — the `perft` mutant moves `signature` and `golden` too, and that is not
/// what the row claims.
const MUTANTS: &[Mutant] = &[
    Mutant {
        label: "futility margin base 45 -> 46",
        file: "crates/rfish-engine/src/search/worker.rs",
        find: "(45 + depth * 4).min(85)",
        replace: "(46 + depth * 4).min(85)",
        gate: "signature",
    },
    Mutant {
        label: "the board display omits `Checkers:`",
        file: "crates/rfish-engine/src/board/position.rs",
        find: "write!(f, \"Checkers: \")?",
        replace: "write!(f, \"CheckersZ: \")?",
        gate: "golden",
    },
    Mutant {
        label: "no knight under-promotion",
        file: "crates/rfish-engine/src/board/movegen.rs",
        find: "[PieceType::Rook, PieceType::Bishop, PieceType::Knight]",
        replace: "[PieceType::Rook, PieceType::Bishop]",
        gate: "perft",
    },
    Mutant {
        // SCALES the network's output rather than unbounding it: the activations are still
        // clamped, so the engine stays a sane chess engine and the gate answers in seconds.
        label: "network output scale 16 -> 17",
        file: "crates/rfish-engine/src/eval/nnue/common.rs",
        find: "pub const OUTPUT_SCALE: i64 = 16;",
        replace: "pub const OUTPUT_SCALE: i64 = 17;",
        gate: "nnue-check",
    },
    Mutant {
        // The list a gate computes and a page writes down. The page said, directly under its
        // own copy, that a list which drifts reads exactly like one that has not — and it had
        // drifted by three entries with nothing to catch it.
        label: "the documented parity order loses a gate",
        file: "docs/10-tooling-ci.md",
        find: "`lane-coverage` → `zone-check` →",
        replace: "`lane-coverage` →",
        gate: "docs-lint",
    },
    Mutant {
        // The routing every page's gates section states. A page rewritten without its section
        // is the shape the check exists for: nothing on the page tells a reader what holds it,
        // while every page that routes a gate there still points at it. Both halves fire —
        // the missing section, and the dangling pointer from the far page.
        label: "a page loses the gates section other pages route to",
        file: "docs/05-tablebases.md",
        find: "## The gates",
        replace: "## The checks",
        gate: "docs-lint",
    },
    Mutant {
        // The direction `cargo` cannot check. A `use` is enough: `board` is the zone nothing
        // below it may influence, which is what makes perft a complete test of it.
        label: "the board zone reads the search zone",
        file: "crates/rfish-engine/src/board/bitboard.rs",
        find: "use crate::board::types::",
        replace: "use crate::search::tt::Bound as _Zone;\nuse crate::board::types::",
        gate: "zone-check",
    },
    Mutant {
        // Aimed at the one question this gate exists for: what a COMPLETED search leaves
        // behind. `ucinewgame` stops resetting the histories, so the second round searches
        // a tree the first round taught — a divergence no value gate can see, because every
        // one of them reads the FIRST answer the process gives.
        label: "ucinewgame no longer clears the worker histories",
        file: "crates/rfish/src/uci.rs",
        find: "        self.tt.clear();\n        self.pool.clear();",
        replace: "        self.tt.clear();",
        gate: "repro-search",
    },
    Mutant {
        // Aimed at the ONE invariant no golden can reach. The mutant is still bounded, but
        // by the gate rather than by the engine: `async-check` bounds its own wait at 30s
        // and reports a broken invariant, so a search that ignores `quit` reddens in seconds
        // instead of hanging the run.
        //
        // The LATCH is what it removes, because that is where the decision now lives: the
        // reader cannot answer whether a search is running, so it records the `quit` and
        // `set_searching_unbounded` answers it when the search declares itself. Dropping
        // the latch leaves nothing for the search to find.
        label: "quit no longer stops an unbounded search",
        file: "crates/rfish/src/uci.rs",
        find: "\"quit\" => shared.latch_quit(),",
        replace: "\"quit\" => {}",
        gate: "async-check",
    },
    Mutant {
        // Aimed at the one OUTPUT PATH nothing else drives. This swaps two of the writer's
        // eight operations and touches the reader not at all, so the engine evaluates and
        // searches exactly as before: measured, `signature` and `nnue-check` BOTH stay green
        // over this mutant while `export_net` emits a net this build cannot read back. A
        // gate that only reads what the engine CONSUMES cannot see a format writer drift
        // away from its reader.
        label: "export_net writes the pp weights before the threat psqt block",
        file: "crates/rfish-engine/src/eval/nnue/transformer.rs",
        find: "        w.leb128(threat_psqt)?;\n        w.i8s(&pp_w[..pp_dims * L1])?;",
        replace: "        w.i8s(&pp_w[..pp_dims * L1])?;\n        w.leb128(threat_psqt)?;",
        gate: "net-roundtrip",
    },
];

/// Seconds a mutated gate gets before the run is called a rig fault.
const MUTANT_TIMEOUT_SECS: u64 = 900;

/// Restore every file this run edited, whatever happens to the run.
///
/// The mutations are real edits to tracked sources, so the restore cannot be a step at the
/// end of a happy path: an error return, a panic or a `?` anywhere between would leave the
/// tree mutated and the next command would measure a deliberately broken engine.
struct Restore(Vec<(std::path::PathBuf, String)>);

impl Drop for Restore {
    fn drop(&mut self) {
        for (path, body) in &self.0 {
            if let Err(e) = std::fs::write(path, body) {
                eprintln!("negative-control: COULD NOT RESTORE {}: {e}", path.display());
                eprintln!("  The tree is still mutated. `git checkout -- {}`", path.display());
            }
        }
    }
}

/// Every named gate must be SEEN TO FAIL, by mutation rather than by argument.
///
/// A gate's power to detect a defect is an assumption until something breaks the engine on
/// purpose and watches the gate go red. This tree ran that experiment by hand, at the moment
/// each gate was written, and never again — and a gate that has quietly stopped being able to
/// fail is invisible: it reports success, which is what everyone was hoping for.
///
/// One representative mutant per gate is enough under the competent-programmer hypothesis,
/// which is what makes mutation testing cheap enough to be a step at all.
///
/// **A mutation that does not APPLY is a rig fault, not a verdict.** A `find` string that has
/// rotted matches nothing, the tree stays clean, the gate greens — and that reads as "the gate
/// failed to detect it", which is the worst possible way to be wrong here. Every row asserts
/// the file actually changed, and so does the compile: a mutation the compiler rejects is not
/// a behavioural one.
pub(crate) fn negative_control(args: &[&str]) -> Result<Outcome, String> {
    let root = workspace_root();
    let want: Vec<&str> = args.iter().copied().filter(|a| !a.starts_with("--")).collect();
    let selected: Vec<&Mutant> =
        MUTANTS.iter().filter(|m| want.is_empty() || want.contains(&m.gate)).collect();
    if selected.is_empty() {
        return Ok(Outcome::Fail(format!(
            "no row selected by {:?}, so this mutated nothing and proved nothing. \
             Known gates: {}",
            want,
            MUTANTS.iter().map(|m| m.gate).collect::<Vec<_>>().join(", ")
        )));
    }

    let mut blind = Vec::new();
    for mutant in &selected {
        println!("\n\x1b[1m== negative-control: {} — {} ==\x1b[0m", mutant.gate, mutant.label);
        let path = root.join(mutant.file);
        let original =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        // The guard is armed BEFORE the first write and dropped at the end of this iteration,
        // so every early return below restores.
        let _restore = Restore(vec![(path.clone(), original.clone())]);

        let mutated = original.replace(mutant.find, mutant.replace);
        if mutated == original {
            return Ok(Outcome::Skipped(format!(
                "RIG FAULT: {:?} matches nothing in {}. The pattern has rotted, so the tree \
                 was never mutated — a green gate here would have read as a gate that failed \
                 to detect it",
                mutant.find, mutant.file
            )));
        }
        std::fs::write(&path, &mutated).map_err(|e| format!("{}: {e}", path.display()))?;

        if !xtask_step(&root, &["build"], MUTANT_TIMEOUT_SECS)?.succeeded() {
            return Ok(Outcome::Skipped(format!(
                "RIG FAULT: the mutated tree does not COMPILE, so {} is not a behavioural \
                 mutation",
                mutant.label
            )));
        }
        match xtask_step(&root, &[mutant.gate], MUTANT_TIMEOUT_SECS)? {
            Run::TimedOut => {
                return Ok(Outcome::Skipped(format!(
                    "RIG FAULT: the mutated `{}` did not finish within {MUTANT_TIMEOUT_SECS}s. \
                     A mutant that hangs proves nothing — choose one whose cost is bounded. \
                     Perturb the value, do not remove the bound",
                    mutant.gate
                )));
            }
            Run::Exited(0) => {
                eprintln!("  \x1b[31mFAIL\x1b[0m {} PASSED a mutated engine", mutant.gate);
                blind.push(mutant.gate);
            }
            Run::Exited(code) => {
                println!("  \x1b[32mok\x1b[0m {} went red (exit {code})", mutant.gate);
            }
        }
    }

    // Prove the tree came back clean by RUNNING a gate green, rather than by asserting it.
    // The restores above put the sources back; the binary is still the last mutant's.
    println!("\n\x1b[1m== negative-control: the restored tree ==\x1b[0m");
    if !xtask_step(&root, &["build"], MUTANT_TIMEOUT_SECS)?.succeeded() {
        return Ok(Outcome::Fail("the restored tree does not build".to_string()));
    }
    if !xtask_step(&root, &["signature"], MUTANT_TIMEOUT_SECS)?.succeeded() {
        return Ok(Outcome::Fail(
            "the tree did NOT come back clean — `signature` is red after the restore".to_string(),
        ));
    }

    println!(
        "negative-control: {} of {} gate(s) detected their mutation, tree restored",
        selected.len() - blind.len(),
        selected.len()
    );
    Ok(Outcome::check(
        blind.is_empty(),
        format!("gate(s) that passed a mutated engine: {}", blind.join(", ")),
    ))
}

/// What a bounded run of `cargo xtask <step>` did.
enum Run {
    Exited(i32),
    TimedOut,
}

impl Run {
    fn succeeded(&self) -> bool {
        matches!(self, Run::Exited(0))
    }
}

/// Run `cargo xtask <args>` with a wall-clock bound, quietly.
///
/// The bound is the whole point: a deliberately broken engine does not always fail fast, and
/// an unbounded wait turns "the gate failed" into "the harness never came back".
fn xtask_step(root: &Path, args: &[&str], secs: u64) -> Result<Run, String> {
    let mut child = std::process::Command::new(std::env::var("CARGO").unwrap_or("cargo".into()))
        .arg("xtask")
        .args(args)
        .current_dir(root)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("cargo xtask {}: {e}", args.join(" ")))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        match child.try_wait().map_err(|e| format!("waiting on cargo xtask: {e}"))? {
            // A signal leaves no code; treat it as a non-zero exit, which is what it is.
            Some(status) => return Ok(Run::Exited(status.code().unwrap_or(-1))),
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(Run::TimedOut);
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
    }
}

/// The smallest number of property rows this gate will report a verdict over.
const ROW_FLOOR: usize = 40;

/// Hold `tools/fixture_properties.tsv` to the tree, in BOTH directions.
///
/// Direction 1 — every ROW is still true: its owner exists, its fixture exists, and its
/// witness still appears inside that fixture. A fixture that stops presenting its property is
/// what this catches: the option line deleted, the position rewritten, the case renamed.
///
/// Direction 2 — every FIXTURE is classified. A case arriving with nobody having answered "a
/// representative of WHAT?" is how a partition ends up exhaustive in one dimension and empty
/// in another, and the fixture universe is globbed from the tree rather than listed in the
/// table, because a second list would rot exactly like the first.
///
/// **What it cannot do:** prove that presenting a property exercises the owner's branch. That
/// needs coverage data this tree does not collect. A green run says the fixtures still
/// present what the table claims, not that the branches are tested.
pub(crate) fn fixture_coverage() -> Result<Outcome, String> {
    let root = workspace_root();
    let table = root.join("tools/fixture_properties.tsv");
    let text = std::fs::read_to_string(&table).map_err(|e| format!("{}: {e}", table.display()))?;

    let rows: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .collect();
    // A table read as empty passes every per-row check below and reports OK over nothing —
    // the shape the step floor and the index sweep's file floor already refuse.
    if rows.len() < ROW_FLOOR {
        return Ok(Outcome::Fail(format!(
            "parsed only {} property rows (floor {ROW_FLOOR}) — the table or this extraction \
             changed shape, and a coverage verdict over nothing is worthless",
            rows.len()
        )));
    }

    let mut problems = Vec::new();
    let mut classified = std::collections::BTreeSet::new();
    for row in &rows {
        let fields: Vec<&str> = row.split('\t').collect();
        let [property, owner, fixture, witness] = fields.as_slice() else {
            problems.push(format!("malformed row (needs 4 tab-separated fields): {row}"));
            continue;
        };
        if !root.join(owner).exists() {
            problems.push(format!("{property}: owner does not exist -> {owner}"));
        }
        let fixture_path = root.join(fixture);
        let Ok(body) = std::fs::read_to_string(&fixture_path) else {
            problems.push(format!("{property}: fixture does not exist -> {fixture}"));
            continue;
        };
        classified.insert((*fixture).to_string());
        // `\n` is the only escape, so a row can name a whole line instead of a fragment.
        if !body.replace("\r\n", "\n").contains(&witness.replace("\\n", "\n")) {
            problems.push(format!(
                "{property}: {fixture} no longer presents it — the witness {witness:?} \
                 appears nowhere in it"
            ));
        }
    }

    // A .uci fixture IS engine input: every driver here pipes the file at the engine raw, so
    // a `#` line is a COMMAND, the engine answers "Unknown command", and the case diverges
    // for a reason that has nothing to do with what it tests. ../mcfish lost a milestone to
    // exactly that. The `.fens` corpora are read by a gate rather than piped, and they do
    // carry `#` headers — which is why this asks only about `.uci`.
    let cases = root.join("tools/cases");
    let mut fixtures = Vec::new();
    for entry in
        std::fs::read_dir(&cases).map_err(|e| format!("{}: {e}", cases.display()))?.flatten()
    {
        let path = entry.path();
        let Some(rel) = path.strip_prefix(&root).ok().and_then(|p| p.to_str()) else {
            continue;
        };
        fixtures.push(rel.replace('\\', "/"));
        if path.extension().is_some_and(|e| e == "uci") {
            let body =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            if body.lines().any(|l| l.trim_start().starts_with('#')) {
                problems.push(format!(
                    "{rel} has a '#' line — a .uci fixture is piped RAW, so that is engine \
                     input, not a comment"
                ));
            }
        }
    }
    fixtures.sort();
    if fixtures.is_empty() {
        return Ok(Outcome::Fail(
            "tools/cases holds no fixtures, so every row above classified nothing".to_string(),
        ));
    }
    for fixture in &fixtures {
        if !classified.contains(fixture) {
            problems.push(format!(
                "{fixture} is a fixture no property row claims — a representative of WHAT?"
            ));
        }
    }

    for p in &problems {
        eprintln!("  {p}");
    }
    println!(
        "fixture-coverage: {} properties, {} of {} fixtures classified",
        rows.len(),
        classified.len(),
        fixtures.len()
    );
    Ok(Outcome::check(problems.is_empty(), format!("{} problem(s)", problems.len())))
}

/// Every step name the dispatch table answers to.
///
/// Parsed from the table rather than kept as a second list here: a list beside the thing it
/// describes rots exactly the way the prose this tree gates rots. `docs-lint` reads the same
/// table for the same reason.
pub(crate) fn dispatch_steps(root: &Path) -> Result<Vec<String>, String> {
    let main = root.join("crates/xtask/src/main.rs");
    let text = std::fs::read_to_string(&main).map_err(|e| format!("{}: {e}", main.display()))?;
    let mut steps = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('"') {
            continue;
        }
        // Read every name in the arm's PATTERN, so an alias arm — `"help" | "--help" | "-h"
        // =>` — contributes `help` rather than nothing. Reading only the first quoted name
        // and requiring `=>` right after it dropped `help` on the floor, and its excuse then
        // named a step this gate could not see. The dead-excuse unit test is what said so.
        let Some((pattern, _)) = line.split_once("=>") else {
            continue;
        };
        for (i, part) in pattern.split('"').enumerate() {
            // Odd indices are inside the quotes; a leading `-` is a flag alias, not a step.
            if i % 2 == 1 && !part.starts_with('-') {
                steps.push(part.to_string());
            }
        }
    }
    steps.sort();
    steps.dedup();
    Ok(steps)
}

/// Every workflow's YAML, with the comments removed.
///
/// **A step NAMED in a comment is not a step the workflow RUNS.** ../mcfish's first version of
/// this gate counted one, and the step it would have wrongly declared laned was
/// `golden-update` — the one step this tree most wants excused.
fn workflow_text(root: &Path) -> Result<String, String> {
    let dir = root.join(".github/workflows");
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?.flatten();
    let mut text = String::new();
    let mut files = 0;
    for entry in entries {
        let path = entry.path();
        if !path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
            continue;
        }
        files += 1;
        let body =
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        for line in body.lines() {
            text.push_str(strip_yaml_comment(line));
            text.push('\n');
        }
    }
    if files == 0 {
        return Err(format!(
            "{} holds no workflows — nothing to read a lane out of",
            dir.display()
        ));
    }
    Ok(text)
}

/// A YAML line with its trailing comment removed.
///
/// A `#` only opens a comment at the start of a token, so `${{ x || '#600' }}` keeps its hash.
/// This is deliberately simpler than YAML: the question is whether a step name is inside a
/// comment, and over-trimming can only make this gate stricter, never laxer.
fn strip_yaml_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'#' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            return &line[..i];
        }
    }
    line
}

/// The body of `gates::parity`'s step list.
fn parity_text(root: &Path) -> Result<String, String> {
    let path = root.join("crates/xtask/src/gates.rs");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let start = text
        .find("let steps: Vec<GateStep>")
        .ok_or("gates.rs no longer declares parity's step list where this gate reads it")?;
    let rest = &text[start..];
    let end = rest.find("];").ok_or("parity's step list is not terminated where expected")?;
    Ok(rest[..end].to_string())
}

/// True when some workflow actually invokes `cargo xtask <step>`.
///
/// **The boundary must reject a hyphen.** ../mcfish used `\b` after the step name and `xtask
/// net` was satisfied by `xtask net-fetch` — a whole class of step declared laned by the lane
/// of a differently-named neighbour.
fn invokes(text: &str, step: &str) -> bool {
    let needle = format!("xtask {step}");
    let mut from = 0;
    while let Some(at) = text[from..].find(&needle) {
        let end = from + at + needle.len();
        let next = text[end..].chars().next();
        if !next.is_some_and(|c| c.is_alphanumeric() || c == '-' || c == '_') {
            return true;
        }
        from = end;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag table names steps that exist, and refuses a flag no step reads.
    ///
    /// The same shape as the dead-excuse test above and for the same reason: a table naming
    /// a step is an allowance, and an allowance with no owner outlives what it allowed. A
    /// renamed step would leave its flags declared against nothing, and `check_flags` would
    /// then fall through to "no flags" and refuse the flags the step still reads.
    #[test]
    fn every_flag_table_entry_names_a_step_that_dispatch_still_has() {
        let steps = dispatch_steps(&crate::workspace_root()).expect("dispatch steps");
        for (step, flags) in crate::STEP_FLAGS {
            assert!(
                steps.iter().any(|s| s == step),
                "STEP_FLAGS names '{step}', which is not a step"
            );
            assert!(!flags.is_empty(), "STEP_FLAGS gives '{step}' an empty list; drop the row");
        }
    }

    /// Every flag the sources actually READ is declared for some step.
    ///
    /// This is the direction the table cannot police on its own. A step that gains a
    /// `--flag` and is not added here keeps working -- `arg_value` finds it -- while
    /// `check_flags` refuses it, so the gate would go red on its own documented usage. The
    /// scan is by literal rather than by owner: it cannot say WHICH step should declare a
    /// flag, only that no flag is read that nothing declares, and that is the half that
    /// fails closed.
    #[test]
    fn every_flag_the_steps_read_is_declared_in_the_table() {
        let root = crate::workspace_root();
        let declared: Vec<&str> =
            crate::STEP_FLAGS.iter().flat_map(|(_, flags)| flags.iter().copied()).collect();
        for file in ["gates.rs", "perf.rs", "codegen.rs", "meta.rs", "net.rs", "fuzz.rs"] {
            let path = root.join("crates/xtask/src").join(file);
            let text = std::fs::read_to_string(&path).expect("xtask source");
            for line in text.lines() {
                for reader in ["arg_value(args, \"", "has_flag(args, \"", "args.contains(&\""] {
                    let Some(rest) = line.split_once(reader).map(|(_, r)| r) else { continue };
                    let Some((flag, _)) = rest.split_once('"') else { continue };
                    if !flag.starts_with("--") || flag == "--" {
                        continue;
                    }
                    assert!(
                        declared.contains(&flag),
                        "{file} reads '{flag}' and STEP_FLAGS declares it for no step"
                    );
                }
            }
        }
    }

    /// The behaviour itself, as a pure function -- no binary is driven to ask it.
    #[test]
    fn a_flag_a_step_does_not_read_is_refused() {
        // The typo that motivated this: one letter, and the run reports a number for the
        // tier it defaulted to rather than the one that was asked for.
        assert!(crate::check_flags("perf-budget", &["--teir", "sse41"]).is_err());
        assert!(crate::check_flags("perf-budget", &["--tier", "sse41"]).is_ok());
        // A flag that is real for another step is still wrong for this one.
        assert!(crate::check_flags("perf-budget", &["--base", "HEAD"]).is_err());
        // A step that reads no flag refuses every flag, and still takes its positionals.
        assert!(crate::check_flags("perft", &["--tier", "avx2"]).is_err());
        assert!(crate::check_flags("bench", &["16", "1", "8"]).is_ok());
        assert!(crate::check_flags("negative-control", &["golden", "signature"]).is_ok());
        assert!(crate::check_flags("golden-audit", &["--write", "search"]).is_ok());
        // Every declared flag is accepted by the step that declares it.
        for (step, flags) in crate::STEP_FLAGS {
            for flag in *flags {
                assert!(
                    crate::check_flags(step, &[flag]).is_ok(),
                    "'{step}' refuses its own '{flag}'"
                );
            }
        }
    }

    #[test]
    fn a_hyphenated_neighbour_does_not_lane_the_shorter_name() {
        assert!(!invokes("- run: cargo xtask net-fetch", "net"));
        assert!(invokes("- run: cargo xtask net", "net"));
        assert!(invokes("- run: cargo xtask net\n", "net"));
        assert!(invokes("- run: cargo xtask fuzz 600 all", "fuzz"));
    }

    #[test]
    fn a_step_named_in_a_comment_is_not_a_step_that_runs() {
        assert_eq!(strip_yaml_comment("  # cargo xtask golden-update is discussed here"), "  ");
        assert_eq!(
            strip_yaml_comment("  - run: cargo xtask net # then build"),
            "  - run: cargo xtask net "
        );
        // Not a comment: no whitespace before the hash.
        assert_eq!(strip_yaml_comment("      seconds || '#600'"), "      seconds || '#600'");
    }

    /// A mutation row whose pattern has rotted is a gate nobody is exercising.
    ///
    /// `negative-control` already refuses such a row -- it reports a RIG FAULT and exits 2
    /// rather than a verdict -- but that step is not in `parity`, so a rename in the engine
    /// can leave a gate unproven until somebody runs it by hand. One did: the `async-check`
    /// row named a `"quit"` match arm the reader replaced, and nothing said so for nineteen
    /// days. This costs a file read per row and says it on the next `cargo xtask test`.
    ///
    /// EXACTLY once, not at least once. The runner replaces every occurrence, so a pattern
    /// that has become ambiguous mutates more sites than the row describes and the gate that
    /// reddens is answering a different question.
    ///
    /// The text is read with its line endings NORMALISED, because two rows span a line and a
    /// Windows checkout separates those lines with CRLF -- a `find` written with `\n` then
    /// matches zero times and the row reads as rotted on that runner alone. This is a
    /// question about the SOURCE, not about the working copy, so it must answer the same on
    /// all three. `negative-control` needs no such treatment: it is Linux-local by
    /// construction, and normalising there would rewrite every line ending in the file it
    /// mutates.
    #[test]
    fn every_mutation_still_matches_the_file_it_targets_exactly_once() {
        let root = workspace_root();
        let steps = dispatch_steps(&root).expect("the dispatch table");
        for m in MUTANTS {
            let path = root.join(m.file);
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
                .replace("\r\n", "\n");
            assert_eq!(
                text.matches(m.find).count(),
                1,
                "{}: the pattern for {:?} does not match {} exactly once -- \
                 negative-control would report a rig fault rather than a verdict",
                m.gate,
                m.label,
                m.file
            );
            assert!(
                steps.contains(&m.gate.to_string()),
                "{:?} names gate `{}`, which the dispatch table no longer has",
                m.label,
                m.gate
            );
        }
    }

    #[test]
    fn every_excuse_names_a_step_the_dispatch_table_still_has() {
        let steps = dispatch_steps(&workspace_root()).expect("the dispatch table");
        for (name, _) in EXCUSED {
            assert!(
                steps.contains(&(*name).to_string()),
                "{name} is excused but is no longer a step — a dead excuse hides the next one"
            );
        }
    }
}
