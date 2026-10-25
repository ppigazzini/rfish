//! Fetch the NNUE network.
//!
//! The net is a RUNTIME INPUT, not a build product. A clean build never touches the
//! network, and nothing downloads it implicitly: this step exists so that fetching is
//! something the operator asked for, at a moment they chose.
//!
//! It is not committed either. The file is around 100 MiB and changes with upstream, which
//! is reason enough — but the deciding reason is that committing it would make the bench
//! anchor look like a property of this repository rather than of a file fetched separately.

use std::path::PathBuf;
use std::process::Command;

use crate::runner::Outcome;
use crate::{have, resources_dir, run};

/// Where Stockfish publishes its networks.
const NET_BASE_URL: &str = "https://tests.stockfishchess.org/api/nn";

/// Download the net named by the argument, or the engine's default.
pub(crate) fn fetch(args: &[&str]) -> Result<Outcome, String> {
    let name = args.first().copied().unwrap_or(rfish_default_net());
    let dir = resources_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let dest = dir.join(name);

    if dest.is_file() {
        println!("net: {} is already present", dest.display());
        return Ok(Outcome::Pass);
    }

    let url = format!("{NET_BASE_URL}/{name}");
    if have("curl") {
        run(Command::new("curl").args(["-fL", "--retry", "3", "-o"]).arg(&dest).arg(&url))?;
    } else if have("wget") {
        run(Command::new("wget").arg("-O").arg(&dest).arg(&url))?;
    } else {
        // No downloader is a SKIP, not a failure: the operator can fetch the file by hand,
        // and the message has to say exactly where to put it.
        return Ok(Outcome::Skipped(format!(
            "neither curl nor wget is available. Download {url} and save it as {}",
            dest.display()
        )));
    }

    let size = std::fs::metadata(&dest).map_or(0, |m| m.len());
    // A truncated download is worse than a missing one: the engine would load a header,
    // fail somewhere in the weights, and the failure would look like an engine bug.
    if size < 1_000_000 {
        std::fs::remove_file(&dest).ok();
        return Err(format!("the download is only {size} bytes; removed it as truncated"));
    }
    println!("net: fetched {} ({size} bytes)", dest.display());
    Ok(Outcome::Pass)
}

/// The default net name the engine looks for.
///
/// Read from the engine's own constant rather than repeated here, so a net-swapping
/// upstream sync changes one line and this step follows it.
fn rfish_default_net() -> &'static str {
    // xtask deliberately does not depend on the engine crate -- a gate that needs the thing
    // it is checking in order to build is a gate that cannot report a build failure. The
    // name is read from the source instead, at the one place it is defined.
    const FALLBACK: &str = "nn-c9e480e92425.nnue";
    let path = crate::workspace_root().join("crates/rfish-engine/src/eval/nnue/mod.rs");
    let Ok(text) = std::fs::read_to_string(path) else { return FALLBACK };
    text.lines()
        .find_map(|l| {
            let l = l.trim();
            let rest = l.strip_prefix("pub const DEFAULT_NET: &str = \"")?;
            let name = rest.strip_suffix("\";")?;
            // Leak: this runs once, in a short-lived process, and the alternative is
            // threading a String through a signature that has no other reason to own one.
            Some(&*Box::leak(name.to_string().into_boxed_str()))
        })
        .unwrap_or(FALLBACK)
}

/// Where the Syzygy tables are mirrored.
const TB_BASE_URL: &str = "https://tablebase.lichess.ovh/tables/standard";

/// One set of tables `tb-fetch` installs, and the corpus `tb` differences it over.
pub(crate) struct TbSet {
    /// The directory under `resources/`.
    pub(crate) dir: &'static str,
    /// The material configurations, each a `.rtbw` and a `.rtbz`.
    pub(crate) stems: &'static [&'static str],
    /// The positions that probe the set, relative to the workspace root.
    pub(crate) corpus: &'static str,
}

/// Every set `tb-fetch` installs, each in its own directory.
///
/// The 3-man set is the one both sibling ports fetch, and the one `tbpv` and the probing
/// perf workload read. `KPvKP` sits apart, as `../mcfish` keeps its 5-man set in `syzygy5/`,
/// because it presents two things no 3-man file does: it is the smallest SYMMETRIC pawnful
/// table, and its DTZ file REMAPS its values, a branch of `map_score` no 3-man file enters.
/// Kept apart, it adds to what `tb` proves without moving anything the 3-man gates see.
pub(crate) const TB_SETS: [TbSet; 2] = [
    TbSet {
        dir: "syzygy",
        stems: &["KPvK", "KNvK", "KBvK", "KRvK", "KQvK"],
        corpus: "tools/cases/tb.fens",
    },
    TbSet { dir: "syzygy4", stems: &["KPvKP"], corpus: "tools/cases/tb4.fens" },
];

impl TbSet {
    /// The set's directory.
    pub(crate) fn path(&self) -> PathBuf {
        resources_dir().join(self.dir)
    }

    /// The set's files that are absent or empty.
    pub(crate) fn missing(&self) -> Vec<String> {
        let dir = self.path();
        self.stems
            .iter()
            .flat_map(|stem| ["rtbw", "rtbz"].map(|ext| format!("{stem}.{ext}")))
            .filter(|f| !dir.join(f).metadata().is_ok_and(|m| m.len() > 0))
            .collect()
    }

    /// The most men any table in the set covers.
    pub(crate) fn men(&self) -> usize {
        self.stems
            .iter()
            .map(|s| s.chars().filter(char::is_ascii_uppercase).count())
            .max()
            .unwrap_or(0)
    }
}

/// Fetch every set in [`TB_SETS`] into its directory under `resources/`.
///
/// The tablebase gates and the `tbpv` golden case need real tables; without them both skip,
/// and a gate that can only skip teaches people to ignore a skip. The 3-man set is ~26 KiB and
/// `KPvKP` ~300 KiB, so both are cheap enough for CI to carry.
///
/// **The magic number is checked, not just the HTTP status.** A mirror that answers a missing
/// file with a 200 and an HTML error page would otherwise be stored as a table and fail much
/// later inside the decoder, reported as a corrupt file rather than as a bad download.
///
/// **A file that fails is removed and the fetch carries on**, as `../mcfish`'s does, then
/// fails at the end naming every file it could not install. CI runs it best effort, and each
/// table gate narrows to the sets that ARE complete — so a mirror that drops `KPvKP` still
/// leaves the 3-man half of every gate proving what it proves.
pub(crate) fn fetch_tb() -> Result<Outcome, String> {
    if !have("curl") && !have("wget") {
        return Ok(Outcome::Skipped(
            "neither curl nor wget is available to fetch the tablebases".to_string(),
        ));
    }

    let mut fetched = 0usize;
    let mut failed = Vec::new();
    for set in &TB_SETS {
        let dir = set.path();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for stem in set.stems {
            for (ext, sub, magic) in [
                ("rtbw", "3-4-5-wdl", [0x71u8, 0xe8, 0x23, 0x5d]),
                ("rtbz", "3-4-5-dtz", [0xd7u8, 0x66, 0x0c, 0xa5]),
            ] {
                let dest = dir.join(format!("{stem}.{ext}"));
                if dest.metadata().is_ok_and(|m| m.len() > 0) {
                    continue;
                }
                let url = format!("{TB_BASE_URL}/{sub}/{stem}.{ext}");
                // Bounded in time as well as in tries. A mirror that accepts the connection and
                // never answers hung this step with no limit at all, and in CI a hang is not a
                // failure `continue-on-error` can absorb: it runs into the JOB's timeout, which
                // fails the lane the best-effort fetch exists to keep green.
                let got = if have("curl") {
                    run(Command::new("curl")
                        .args(["-fL", "--retry", "3", "--connect-timeout", "30"])
                        .args(["--max-time", "600", "-o"])
                        .arg(&dest)
                        .arg(&url))
                } else {
                    run(Command::new("wget")
                        .args(["--timeout=30", "--tries=3", "-O"])
                        .arg(&dest)
                        .arg(&url))
                };
                let head = std::fs::read(&dest).unwrap_or_default();
                let why = match got {
                    Err(e) => Some(e),
                    Ok(()) if head.len() < 4 || head[..4] != magic => {
                        Some(format!("not a Syzygy table: it does not start with the {ext} magic"))
                    }
                    Ok(()) => None,
                };
                if let Some(why) = why {
                    // Never leave a partial or foreign file behind: the gates read a file's
                    // PRESENCE as a table, and the next fetch skips any file that is there.
                    let _ = std::fs::remove_file(&dest);
                    eprintln!("\x1b[31m  REJECT {stem}.{ext}: {why}\x1b[0m");
                    failed.push(format!("{stem}.{ext}"));
                    continue;
                }
                fetched += 1;
            }
        }
    }
    if !failed.is_empty() {
        return Ok(Outcome::Fail(format!(
            "{fetched} downloaded, {} not installed: {} -- the table gates narrow without them",
            failed.len(),
            failed.join(", ")
        )));
    }
    let dirs: Vec<String> = TB_SETS.iter().map(|s| s.path().display().to_string()).collect();
    println!("tb-fetch: {fetched} downloaded, every set present in {}", dirs.join(" and "));
    Ok(Outcome::Pass)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name must come from the engine's constant, not from a copy that can drift. If
    /// this fails, the two have diverged and `cargo xtask net` is fetching the wrong file.
    #[test]
    fn the_default_net_name_is_read_from_the_engine_source() {
        let name = rfish_default_net();
        assert!(name.starts_with("nn-"), "{name} is not a network file name");
        assert!(
            std::path::Path::new(name).extension().is_some_and(|e| e == "nnue"),
            "{name} is not a network file name"
        );
        assert_eq!(name, rfish_engine_default_net_literal());
    }

    /// Read the same constant a second way, so the test cannot pass by both sides being
    /// wrong in the same manner.
    fn rfish_engine_default_net_literal() -> String {
        let text = std::fs::read_to_string(
            crate::workspace_root().join("crates/rfish-engine/src/eval/nnue/mod.rs"),
        )
        .expect("the nnue module exists");
        let line = text
            .lines()
            .find(|l| l.trim_start().starts_with("pub const DEFAULT_NET"))
            .expect("DEFAULT_NET is declared");
        line.split('"').nth(1).expect("a quoted name").to_string()
    }
}
