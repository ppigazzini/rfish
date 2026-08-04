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
    (
        "perf-budget",
        "LOCAL -- the golden is per-machine (gitignored), because a retired-instruction \
         count is toolchain- and CPU-specific",
    ),
    ("perf-budget-update", "writes that per-machine golden"),
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
fn dispatch_steps(root: &Path) -> Result<Vec<String>, String> {
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
