//! Fetch the NNUE network.
//!
//! The net is a RUNTIME INPUT, not a build product. A clean build never touches the
//! network, and nothing downloads it implicitly: this step exists so that fetching is
//! something the operator asked for, at a moment they chose.
//!
//! It is not committed either. The file is around 100 MiB and changes with upstream, which
//! is reason enough — but the deciding reason is that committing it would make the bench
//! anchor look like a property of this repository rather than of a file fetched separately.

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
    const FALLBACK: &str = "nn-0ee0657fb25e.nnue";
    let path = crate::workspace_root().join("crates/rfish-engine/src/eval/nnue.rs");
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
            crate::workspace_root().join("crates/rfish-engine/src/eval/nnue.rs"),
        )
        .expect("the nnue module exists");
        let line = text
            .lines()
            .find(|l| l.trim_start().starts_with("pub const DEFAULT_NET"))
            .expect("DEFAULT_NET is declared");
        line.split('"').nth(1).expect("a quoted name").to_string()
    }
}
