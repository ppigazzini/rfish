//! The Syzygy file parse, fuzzed the way a user reaches it.
//!
//! A tablebase file is UNTRUSTED input in the strongest sense: it is a binary blob from a
//! mirror, and the decoder walks it with indices derived from its own header. Both sibling
//! ports fuzz this surface — `../mcfish` with a dedicated lane, `../zfish` with its own
//! targets — and each found real bugs there. rfish had UCI and search fuzzing and nothing
//! aimed at the parser.
//!
//! **What can go wrong here in safe Rust is a PANIC**, not memory corruption:
//! `forbid(unsafe_code)` means a bad index is a bounds check that aborts the process rather
//! than a read of someone else's memory. That is still a denial of service reached from a
//! file, and it is exactly what the sibling ports' fixes were about — "bound the decoder's
//! block index, descent and bit window" is `../zfish` `99602ca8`.
//!
//! The iteration is what a user does, rather than a reimplementation of the carve: mutate a
//! REAL table's bytes, write them to a `.rtbw`/`.rtbz`, point discovery at that directory and
//! probe a position of the matching material. Everything between the file and the decoder —
//! discovery, the size and magic checks, the group set-up, the index arithmetic and the DTZ
//! remap — is engine code on the path, not a stand-in for it. Seeding from a table that
//! PARSES is the point: a random blob dies at the magic number and never reaches the decoder.

use super::TableRegistry;
use crate::board::position::Position;

/// The workspace root, from this crate's manifest rather than the working directory.
///
/// A test runs with the CRATE as its working directory, so `resources/syzygy` resolved from
/// there points inside `crates/rfish-engine` and finds nothing.
fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default()
}

/// xorshift64, so a failing run replays exactly from its printed seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// The 3-man tables the fetcher installs, and a position of each material.
///
/// The position must MATCH the table's material or the probe never reaches the decoder: it
/// looks the material key up, misses, and returns before reading a byte.
const SEEDS: [(&str, &str); 3] = [
    ("KQvK", "8/8/8/4k3/8/8/8/K6Q w - - 0 1"),
    ("KRvK", "8/8/8/4k3/8/8/8/K6R w - - 0 1"),
    ("KPvK", "8/8/8/4k3/8/8/4P3/K7 w - - 0 1"),
];

/// Mutate one table and probe it, returning whether the parse was reached at all.
///
/// A rejected file is a PASS: refusing a corrupt table is the correct answer, and the
/// property under test is only that the refusal is a refusal rather than a panic.
fn one_round(rng: &mut Rng, dir: &std::path::Path, source: &std::path::Path) -> bool {
    let Ok(mut bytes) = std::fs::read(source) else {
        return false;
    };
    if bytes.len() < 16 {
        return false;
    }
    let stem = source.file_stem().and_then(|s| s.to_str()).unwrap_or("KQvK").to_string();
    let ext = source.extension().and_then(|s| s.to_str()).unwrap_or("rtbw").to_string();

    // A handful of byte flips per round, never in the first four: the magic is checked before
    // anything else, so corrupting it spends the whole round proving the magic check works.
    let flips = 1 + (rng.next() % 8) as usize;
    for _ in 0..flips {
        let at = 4 + (rng.next() as usize % (bytes.len() - 4));
        bytes[at] ^= (rng.next() % 255 + 1) as u8;
    }
    // Occasionally truncate as well: a short file is the other shape a mirror produces, and
    // it exercises every length check the decoder makes rather than the value checks.
    if rng.next().is_multiple_of(4) {
        let keep = 16 + (rng.next() as usize % bytes.len().saturating_sub(16).max(1));
        bytes.truncate(keep.min(bytes.len()));
    }

    let target = dir.join(format!("{stem}.{ext}"));
    if std::fs::write(&target, &bytes).is_err() {
        return false;
    }

    // Through the engine's own entry points, from discovery onwards.
    let registry = TableRegistry::discover(&dir.to_string_lossy());
    let fen = SEEDS.iter().find(|(s, _)| *s == stem).map_or(SEEDS[0].1, |(_, f)| f);
    let Ok(pos) = Position::from_fen(fen, false) else {
        return false;
    };
    // Both probes: WDL and DTZ read different tables and different decoder paths.
    let _ = registry.probe_wdl(&pos);
    let _ = registry.probe_dtz(&pos);
    let _ = registry.root_probe_wdl(&pos, true);
    true
}

/// Mutate and probe for `seconds`, returning how many rounds reached a probe.
#[must_use]
pub fn run_for(seed: u64, seconds: u64, source_dir: &std::path::Path) -> u64 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let scratch = workspace_root().join("target/fuzz-tb");
    let _ = std::fs::create_dir_all(&scratch);

    let mut sources = Vec::new();
    for (stem, _) in SEEDS {
        for ext in ["rtbw", "rtbz"] {
            let p = source_dir.join(format!("{stem}.{ext}"));
            if p.is_file() {
                sources.push(p);
            }
        }
    }
    if sources.is_empty() {
        return 0;
    }

    let mut rng = Rng(seed | 1);
    let mut rounds = 0u64;
    while std::time::Instant::now() < deadline {
        let source = &sources[(rng.next() as usize) % sources.len()];
        // Each round gets its own directory: discovery caches what it found, and a stale
        // registry would probe the previous round's bytes.
        let dir = scratch.join(format!("r{rounds}"));
        let _ = std::fs::create_dir_all(&dir);
        if one_round(&mut rng, &dir, source) {
            rounds += 1;
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    rounds
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scheduled soak. `#[ignore]` for the same reason the search soak is: it spends a
    /// wall-clock budget rather than asserting a fact.
    ///
    /// SKIPS rather than fails without tables. The set is fetched by `cargo xtask tb-fetch`,
    /// and a soak that cannot find a seed has proven nothing — saying so is the honest
    /// outcome, and it is printed rather than passed silently.
    #[test]
    #[ignore = "spends a wall-clock budget; run via `cargo xtask fuzz`"]
    fn tb_parse_soak() {
        let seconds =
            std::env::var("RFISH_FUZZ_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(30);
        let seed = std::env::var("RFISH_FUZZ_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(1, |d| d.as_nanos() as u64)
            });
        let dir = workspace_root().join("resources/syzygy");
        if !dir.is_dir() {
            println!(
                "fuzz tb: SKIPPED, no tables at {} -- run `cargo xtask tb-fetch`",
                dir.display()
            );
            return;
        }
        println!("fuzz tb: seed {seed}, {seconds}s -- replay with RFISH_FUZZ_SEED={seed}");
        let rounds = run_for(seed, seconds, &dir);
        println!("fuzz tb: {rounds} mutated tables probed, no panics");
        assert!(rounds > 0, "the budget expired before a single table was probed");
    }
}
