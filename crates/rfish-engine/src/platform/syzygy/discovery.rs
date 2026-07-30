//! Resolving a `SyzygyPath` into a set of tables.
//!
//! Kept apart from the prober because it is what the UCI layer needs before any position
//! exists: the option's effect, the reported cardinality and the "which files am I looking
//! for" rules are all answerable without reading a byte of table data.
//!
//! Golden: `Stockfish/src/syzygy/tbprobe.cpp: Tablebases::init`.

use std::path::PathBuf;

/// The file extensions a Syzygy table set uses.
const EXTENSIONS: [&str; 2] = ["rtbw", "rtbz"];

/// What a `SyzygyPath` setting resolved to.
#[derive(Clone, Debug, Default)]
pub struct Tablebases {
    /// The directories that were searched.
    dirs: Vec<PathBuf>,
    /// The largest number of pieces any discovered table covers, 0 when none were found.
    max_cardinality: u32,
}

impl Tablebases {
    /// Scan `path` — a list of directories separated by the platform's path separator — and
    /// record what is there.
    ///
    /// The empty string and the literal `<empty>` both mean "no tablebases", which is
    /// upstream's convention and what every GUI sends when the user has not set one.
    #[must_use]
    pub fn discover(path: &str) -> Tablebases {
        if path.is_empty() || path == "<empty>" {
            return Tablebases::default();
        }
        let sep = if cfg!(windows) { ';' } else { ':' };
        let dirs: Vec<PathBuf> =
            path.split(sep).filter(|s| !s.is_empty()).map(PathBuf::from).collect();

        let mut max_cardinality = 0;
        for dir in &dirs {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Some((stem, ext)) = name.rsplit_once('.') else { continue };
                if !EXTENSIONS.contains(&ext) {
                    continue;
                }
                if let Some(n) = cardinality_of(stem) {
                    max_cardinality = max_cardinality.max(n);
                }
            }
        }
        Tablebases { dirs, max_cardinality }
    }

    /// The largest piece count any discovered table covers.
    ///
    /// Zero means no tables were found, and the search's tablebase step is skipped
    /// entirely — which is why wiring the option surface can never change a search that has
    /// no path set.
    #[must_use]
    pub fn max_cardinality(&self) -> u32 {
        self.max_cardinality
    }

    /// The directories that were searched.
    #[must_use]
    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    /// True when at least one table was found.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.max_cardinality > 0
    }

    /// Where a table named `stem` would live, if it exists.
    #[must_use]
    pub fn locate(&self, stem: &str, ext: &str) -> Option<PathBuf> {
        self.dirs.iter().map(|d| d.join(format!("{stem}.{ext}"))).find(|p| p.is_file())
    }
}

/// How many pieces a table name describes, or `None` when it is not a table name.
///
/// A Syzygy stem is the two material sides joined by `v`, each a run of piece letters
/// starting with `K`: `KQvK` is four pieces, `KRPvKR` five. Anything else in the directory
/// is not ours.
#[must_use]
pub fn cardinality_of(stem: &str) -> Option<u32> {
    let (white, black) = stem.split_once('v')?;
    if white.is_empty() || black.is_empty() {
        return None;
    }
    let valid = |s: &str| {
        s.starts_with('K')
            && s.bytes().all(|b| matches!(b, b'K' | b'Q' | b'R' | b'B' | b'N' | b'P'))
    };
    if !valid(white) || !valid(black) {
        return None;
    }
    Some((white.len() + black.len()) as u32)
}

/// The set of table stems needed for `n`-man endgames, in the order a fetch should get
/// them.
///
/// Used by `cargo xtask tb-fetch` to know what to download, and by the discovery gate to
/// report which of them are missing.
#[must_use]
pub fn stems_for(n: u32) -> Vec<String> {
    let mut out = Vec::new();
    // Enumerate every material split with `n` pieces, kings included, in the canonical
    // ordering Syzygy names use: the stronger side first, pieces in descending value.
    let pieces = ['Q', 'R', 'B', 'N', 'P'];
    let extra = n.saturating_sub(2) as usize;
    let mut stack = vec![(String::new(), 0usize)];
    while let Some((acc, start)) = stack.pop() {
        if acc.len() == extra {
            for split in 0..=acc.len() {
                let (w, b) = acc.split_at(split);
                // Syzygy names the stronger side first, so only one of `KQvK` and `KvKQ`
                // is a real table. "Stronger" is more pieces, then earlier in the
                // descending-value order the generator already emits them in.
                if w.len() < b.len() || (w.len() == b.len() && w > b) {
                    continue;
                }
                let stem = format!("K{w}vK{b}");
                if !out.contains(&stem) {
                    out.push(stem);
                }
            }
            continue;
        }
        for (i, &p) in pieces.iter().enumerate().skip(start) {
            stack.push((format!("{acc}{p}"), i));
        }
    }
    out.sort_by_key(|s| (s.len(), s.clone()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_path_means_no_tablebases() {
        for p in ["", "<empty>"] {
            let tb = Tablebases::discover(p);
            assert_eq!(tb.max_cardinality(), 0);
            assert!(!tb.is_available());
            assert!(tb.dirs().is_empty());
        }
    }

    #[test]
    fn a_nonexistent_directory_is_recorded_but_finds_nothing() {
        let tb = Tablebases::discover("/nonexistent-syzygy-directory");
        assert_eq!(tb.dirs().len(), 1);
        assert_eq!(tb.max_cardinality(), 0);
    }

    #[test]
    fn table_names_are_recognised_by_their_material_split() {
        assert_eq!(cardinality_of("KQvK"), Some(3));
        assert_eq!(cardinality_of("KRPvKR"), Some(5));
        assert_eq!(cardinality_of("KQQvKQQ"), Some(6));
        // Not tables.
        assert_eq!(cardinality_of("README"), None);
        assert_eq!(cardinality_of("QvK"), None, "a side must start with its king");
        assert_eq!(cardinality_of("KXvK"), None, "X is not a piece");
        assert_eq!(cardinality_of("KvK"), Some(2));
        assert_eq!(cardinality_of("Kv"), None);
    }

    #[test]
    fn discovery_finds_tables_in_a_real_directory() {
        // Build a directory that looks like a 3-man table set, without shipping one.
        let dir = std::env::temp_dir().join(format!("rfish-tb-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        for name in ["KQvK.rtbw", "KQvK.rtbz", "KRvK.rtbw", "notes.txt"] {
            std::fs::write(dir.join(name), b"").expect("write");
        }
        let tb = Tablebases::discover(&dir.to_string_lossy());
        assert_eq!(tb.max_cardinality(), 3);
        assert!(tb.is_available());
        assert!(tb.locate("KQvK", "rtbw").is_some());
        assert!(tb.locate("KQvK", "rtbm").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_three_man_stem_set_is_the_five_pawnless_and_pawn_endings() {
        let stems = stems_for(3);
        for expected in ["KQvK", "KRvK", "KBvK", "KNvK", "KPvK"] {
            assert!(stems.contains(&expected.to_string()), "{expected} missing from {stems:?}");
        }
        assert_eq!(stems.len(), 5);
    }

    #[test]
    fn the_stem_set_grows_with_the_piece_count() {
        assert!(stems_for(4).len() > stems_for(3).len());
        assert!(stems_for(5).len() > stems_for(4).len());
        assert!(stems_for(4).iter().all(|s| cardinality_of(s) == Some(4)));
    }
}
