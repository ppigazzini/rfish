//! The shipped tree must not name the internal dev area.
//!
//! This repository has a second documentation surface that it deliberately does NOT carry:
//! the engineering contract, the operator prompt, the port map, the milestone notes and
//! user-requested analyses. `.gitignore` declares it, so a fresh clone has no such
//! directory — and a shipped page or a source comment pointing into it is a dangling
//! reference for every reader but its author. Duplicating the CONTENT into a shipped page is
//! fine; naming the LOCATION is not.
//!
//! **`docs-lint`'s path check cannot see this class.** That check exempts a path
//! `.gitignore` names, because an ignored path is one the repository decided not to carry
//! and a doc naming it is usually documenting the tool that WRITES it. The dev area is
//! ignored, so every reference into it lands in that exemption and reports clean. This
//! sweep is the check that closes it.
//!
//! **SWEEP THE INDEX, never a list of places to look.** Both sibling ports wrote this rule
//! first and both wrote it against a hand-written directory list; ../zfish's read eight
//! paths, so its whole build package and all of `.github/` could name the area and report
//! clean, and a file landed in exactly that blind spot. ../mcfish established the rule,
//! verified it by hand, and had it broken twice within days by commits that had no way to
//! know. A list of directories rots. The index does not.
//!
//! **EXACTLY TWO FILES MAY NAME IT**, and neither is a claim about the tree: `.gitignore`
//! DECLARES the directory, and this module carries the needles that find it. The exemption
//! is asserted rather than assumed — a third entry, or an entry naming a file that has
//! moved, is a hole reported as a pass.

use std::path::Path;

/// The literal strings that name the internal area or a file inside it.
///
/// Substrings, not patterns: every one of these is a directory or a filename, so there is
/// nothing to compile and nothing that can silently match more than it says. They are
/// case-sensitive on purpose — `docs/12-references.md` is a shipped page and `1-REFERENCES`
/// is not it.
const NEEDLES: &[&str] = &[
    "__DEV",
    "00-CONTRACT",
    "0-DOCS-BEST-PRACTICES",
    "1-REFERENCES.md",
    "2-MILESTONES",
    "PROMPT.md",
    "port_map.tsv",
    "port_status.sh",
];

/// The only two files allowed to contain a needle, by path from the workspace root.
const EXEMPT: &[&str] = &[".gitignore", "crates/xtask/src/devsweep.rs"];

/// The smallest tracked-file count this sweep will report a verdict over.
///
/// Guard the EXTRACTION, not only the verdict: an index read as empty passes every file
/// below it and reports a clean sweep over nothing, which is the same shape as the blank
/// goldens and the empty transcript corpus that the sibling ports' gates now refuse.
const FILE_FLOOR: usize = 60;

/// Every `file:line` in the shipped tree that names the internal area.
///
/// `tracked` is the index, already read by the caller. Outside a checkout there is no index
/// to sweep, and the caller says so rather than reporting a clean run over a subject it
/// never had.
pub(crate) fn sweep(
    root: &Path,
    tracked: &std::collections::BTreeSet<String>,
) -> Result<Vec<String>, String> {
    for exempt in EXEMPT {
        if !root.join(exempt).is_file() {
            return Err(format!(
                "the __DEV sweep exempts {exempt}, which is not in the tree — the exemption \
                 has rotted and would silently widen"
            ));
        }
    }
    if tracked.len() < FILE_FLOOR {
        return Err(format!(
            "the __DEV sweep read {} tracked files (floor {FILE_FLOOR}) — the index or this \
             extraction changed shape, and a sweep over nothing reports clean",
            tracked.len()
        ));
    }

    let mut hits = Vec::new();
    let mut unreadable = Vec::new();
    for rel in tracked {
        if EXEMPT.contains(&rel.as_str()) {
            continue;
        }
        let path = root.join(rel);
        // A tracked path that is not a readable text file is reported, never skipped: a
        // sweep that quietly drops part of its subject has stopped checking that part.
        let Ok(text) = std::fs::read_to_string(&path) else {
            if path.is_file() {
                unreadable.push(rel.clone());
            }
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            if let Some(needle) = NEEDLES.iter().find(|needle| line.contains(**needle)) {
                hits.push(format!("{rel}:{}: names the internal area ({needle})", n + 1));
            }
        }
    }

    if !unreadable.is_empty() {
        return Err(format!(
            "the __DEV sweep could not read {} tracked file(s) ({}) — refusing to report a \
             clean sweep over a subject it did not read",
            unreadable.len(),
            unreadable.join(", ")
        ));
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shrunken_index_is_refused_rather_than_swept() {
        let root = crate::workspace_root();
        let one = std::collections::BTreeSet::from(["README.md".to_string()]);
        let err = sweep(&root, &one).unwrap_err();
        assert!(err.contains("floor"), "{err}");
    }

    #[test]
    fn the_exempt_files_are_the_two_that_must_carry_a_needle() {
        let root = crate::workspace_root();
        for exempt in EXEMPT {
            let text = std::fs::read_to_string(root.join(exempt)).expect("an exempt file");
            assert!(
                NEEDLES.iter().any(|needle| text.contains(needle)),
                "{exempt} is exempt but names nothing — the exemption buys a hole for free"
            );
        }
    }
}
