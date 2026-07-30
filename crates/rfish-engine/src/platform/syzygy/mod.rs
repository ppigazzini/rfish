//! The Syzygy endgame tablebase prober.
//!
//! A tablebase answers a position exactly: won, drawn or lost, and how many plies to the
//! next irreversible move. Below its piece limit that answer replaces the search entirely.
//!
//! # No memory mapping
//!
//! Upstream maps each table file into the address space. A mapping is `unsafe` in Rust for
//! a real reason — the file can be truncated under the map, and the program then reads
//! unmapped memory — so rfish reads each file into a `Vec<u8>` on first probe and stores
//! OFFSETS where upstream stores pointers. Same information, no aliasing question.
//!
//! The cost is that a table is resident in full rather than paged on demand. For the 3-to-5
//! man sets that is megabytes; for a 7-man set it would be gigabytes, and a block cache
//! would be needed. See `docs/05-tablebases.md`.
//!
//! Golden: `Stockfish/src/syzygy/tbprobe.cpp`.

pub mod discovery;
pub mod pairs;
pub mod probe;
pub mod table;
pub mod tables;

use std::collections::HashMap;

pub use discovery::{Tablebases, cardinality_of, stems_for};
pub use probe::{ProbeState, Wdl};

use crate::board::movegen::generate_legal;
use crate::board::position::Position;
use crate::board::types::{Move, MoveType, PieceType};
use table::{TbTable, TbType};

/// Every table found on the configured path.
#[derive(Debug, Default)]
pub struct TableRegistry {
    /// Keyed by material key; both of a table's two keys map to the same entry.
    by_key: HashMap<u64, usize>,
    tables: Vec<(Option<TbTable>, Option<TbTable>)>,
    max_cardinality: u32,
}

impl TableRegistry {
    /// Scan `path` and register every table found.
    ///
    /// A DTZ file without its WDL counterpart is ignored: the prober reaches DTZ only
    /// through a WDL verdict, so a lone DTZ table can never be used.
    #[must_use]
    pub fn discover(path: &str) -> TableRegistry {
        let mut reg = TableRegistry::default();
        if path.is_empty() || path == "<empty>" {
            return reg;
        }
        let sep = if cfg!(windows) { ';' } else { ':' };
        for dir in path.split(sep).filter(|s| !s.is_empty()) {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            let mut stems: Vec<String> = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if let Some((stem, ext)) = name.rsplit_once('.')
                    && ext == TbType::Wdl.ext()
                    && cardinality_of(stem).is_some()
                {
                    stems.push(stem.to_string());
                }
            }
            stems.sort();
            for stem in stems {
                reg.add(std::path::Path::new(dir), &stem);
            }
        }
        reg
    }

    fn add(&mut self, dir: &std::path::Path, stem: &str) {
        let wdl_path = dir.join(format!("{stem}.{}", TbType::Wdl.ext()));
        let Some(wdl) = TbTable::new(TbType::Wdl, stem, wdl_path) else { return };
        if self.by_key.contains_key(&wdl.key) {
            return; // Already registered from an earlier directory.
        }

        let dtz_path = dir.join(format!("{stem}.{}", TbType::Dtz.ext()));
        let dtz = dtz_path.is_file().then(|| TbTable::new(TbType::Dtz, stem, dtz_path)).flatten();

        self.max_cardinality = self.max_cardinality.max(wdl.piece_count as u32);
        let slot = self.tables.len();
        self.by_key.insert(wdl.key, slot);
        self.by_key.insert(wdl.key2, slot);
        self.tables.push((Some(wdl), dtz));
    }

    /// The largest piece count any registered table covers.
    #[must_use]
    pub fn max_cardinality(&self) -> u32 {
        self.max_cardinality
    }

    /// How many tables are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tables.len()
    }

    /// True when nothing was found.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }

    fn get(&self, key: u64, kind: TbType) -> Option<&TbTable> {
        let slot = *self.by_key.get(&key)?;
        let (wdl, dtz) = &self.tables[slot];
        match kind {
            TbType::Wdl => wdl.as_ref(),
            TbType::Dtz => dtz.as_ref(),
        }
    }

    /// Read one value straight out of a table, with no search around it.
    fn probe_table(&self, pos: &Position, kind: TbType, wdl: Wdl) -> Result<i32, ProbeState> {
        if pos.piece_total() == 2 {
            return Ok(0); // Bare kings: drawn, and no file exists for it.
        }
        let entry = self.get(pos.st().material_key, kind).ok_or(ProbeState::Fail)?;
        let loaded = entry.loaded().ok_or(ProbeState::Fail)?;
        probe::do_probe_table(pos, entry, loaded, wdl)
    }

    /// The win/draw/loss verdict for `pos`, from the side to move's point of view.
    ///
    /// Not a bare table read: captures and pawn moves are searched first, because a table
    /// stores no information about en-passant rights and a position with them would
    /// otherwise be answered wrongly. The recursion is bounded — every step removes a piece
    /// or advances a pawn.
    pub fn probe_wdl(&self, pos: &Position) -> Result<Wdl, ProbeState> {
        self.wdl_search(&mut pos.clone(), false).map(|(v, _)| v)
    }

    /// The shared body of the WDL probe.
    ///
    /// `check_zeroing` extends the search from captures alone to every clock-zeroing move,
    /// which is what the DTZ path needs: a DTZ table stores nothing for a position whose
    /// best move resets the halfmove clock.
    fn wdl_search(
        &self,
        pos: &mut Position,
        check_zeroing: bool,
    ) -> Result<(Wdl, ProbeState), ProbeState> {
        let mut best = Wdl::Loss;
        let moves = generate_legal(pos);
        let total = moves.len();
        let mut searched = 0usize;

        for &mv in moves.as_slice() {
            let zeroing = pos.is_capture(mv)
                || (check_zeroing && pos.piece_on(mv.from()).piece_type() == PieceType::Pawn);
            if !zeroing {
                continue;
            }
            searched += 1;
            pos.do_move(mv);
            let inner = self.wdl_search(pos, false);
            pos.undo_move(mv);
            let (v, _) = inner?;
            let v = v.negate();
            if v > best {
                best = v;
                if v >= Wdl::Win {
                    // A zeroing move that wins outright: DTZ is one ply and the table need
                    // not be consulted at all.
                    return Ok((v, ProbeState::ZeroingBestMove));
                }
            }
        }

        // Having searched every legal move, the table must NOT be consulted: it stores
        // nothing about en-passant rights, and a position where every move is a capture
        // would be answered from a state the table does not model.
        let no_more_moves = searched != 0 && searched == total;
        let value = if no_more_moves {
            best
        } else {
            Wdl::from_i32(self.probe_table(pos, TbType::Wdl, Wdl::Draw)?)
        };

        if best >= value {
            let state = if best > Wdl::Draw || no_more_moves {
                ProbeState::ZeroingBestMove
            } else {
                ProbeState::Ok
            };
            return Ok((best, state));
        }
        Ok((value, ProbeState::Ok))
    }

    /// Distance to the next clock-zeroing move, from the side to move's point of view.
    ///
    /// Positive when winning, negative when losing, zero when drawn. Beyond ±100 the result
    /// is a win or loss that the fifty-move rule turns into a draw.
    ///
    /// The value can be off by one: a returned `n` may mean `n + 1` plies. That is a
    /// property of the format, not of this port.
    pub fn probe_dtz(&self, pos: &Position) -> Result<i32, ProbeState> {
        let mut work = pos.clone();
        let (wdl, state) = self.wdl_search(&mut work, true)?;
        if wdl == Wdl::Draw {
            return Ok(0); // DTZ tables store no draws.
        }
        if state == ProbeState::ZeroingBestMove {
            return Ok(dtz_before_zeroing(wdl));
        }

        match self.probe_table(pos, TbType::Dtz, wdl) {
            Ok(dtz) => {
                let cursed = i32::from(wdl == Wdl::BlessedLoss || wdl == Wdl::CursedWin);
                Ok((dtz + 100 * cursed) * sign_of(wdl as i32))
            }
            // The table stores the other side to move. Play one ply and take the winning
            // move that minimises the distance.
            Err(ProbeState::ChangeStm) => self.dtz_by_one_ply_search(pos, wdl),
            Err(e) => Err(e),
        }
    }

    fn dtz_by_one_ply_search(&self, pos: &Position, wdl: Wdl) -> Result<i32, ProbeState> {
        let mut work = pos.clone();
        let mut min_dtz = 0xFFFF;
        for &mv in generate_legal(pos).as_slice() {
            let zeroing = work.is_capture(mv)
                || work.piece_on(mv.from()).piece_type() == PieceType::Pawn
                || mv.move_type() == MoveType::Promotion;
            work.do_move(mv);

            // For a zeroing move the distance wanted is the one BEFORE it, or the answer
            // would be the length of the next sequence rather than of this one.
            let mut dtz = if zeroing {
                let (v, _) = self.wdl_search(&mut work.clone(), false)?;
                -dtz_before_zeroing(v)
            } else {
                -self.probe_dtz(&work)?
            };

            // A move that mates ends it in one ply, whatever the table says.
            if dtz == 1 && work.in_check() && generate_legal(&work).is_empty() {
                min_dtz = 1;
            }
            if !zeroing {
                dtz += sign_of(dtz);
            }
            if dtz < min_dtz && sign_of(dtz) == sign_of(wdl as i32) {
                min_dtz = dtz;
            }
            work.undo_move(mv);
        }
        // No legal move at all means mate.
        Ok(if min_dtz == 0xFFFF { -1 } else { min_dtz })
    }

    /// Rank the root moves by their tablebase verdict.
    ///
    /// Returns `None` when any move could not be probed — a partial ranking is worse than
    /// none, because the search would then trust an ordering built from a mixture of
    /// tablebase truth and missing data.
    #[must_use]
    pub fn rank_root_moves(&self, pos: &Position) -> Option<Vec<(Move, i32)>> {
        if pos.piece_total() > self.max_cardinality || self.max_cardinality == 0 {
            return None;
        }
        let mut work = pos.clone();
        let mut out = Vec::new();
        for &mv in generate_legal(pos).as_slice() {
            work.do_move(mv);
            let dtz = self.probe_dtz(&work);
            work.undo_move(mv);
            let dtz = dtz.ok()?;
            // The child's distance is from the OPPONENT's point of view, so negate; and a
            // non-zeroing move costs a ply.
            let rank = -(dtz + sign_of(dtz));
            out.push((mv, rank));
        }
        out.sort_by_key(|(_, r)| -r);
        Some(out)
    }
}

/// The distance a zeroing move implies, given the verdict after it.
///
/// A DTZ table stores nothing for a position whose best move resets the clock, but the
/// value is recoverable from the verdict alone: the move itself is the zeroing one.
#[must_use]
pub const fn dtz_before_zeroing(wdl: Wdl) -> i32 {
    match wdl {
        Wdl::Win => 1,
        Wdl::CursedWin => 101,
        Wdl::BlessedLoss => -101,
        Wdl::Loss => -1,
        Wdl::Draw => 0,
    }
}

/// -1, 0 or 1.
#[inline]
#[must_use]
const fn sign_of(v: i32) -> i32 {
    if v > 0 {
        1
    } else if v < 0 {
        -1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where the 3-man set lives when it has been fetched. Absent on a fresh clone, so
    /// every test below skips rather than fails without it — the tables are 100 KiB but
    /// they are still a download.
    fn table_dir() -> Option<String> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)?
            .join("resources/syzygy");
        dir.join("KQvK.rtbw").is_file().then(|| dir.to_string_lossy().into_owned())
    }

    #[test]
    fn an_empty_path_registers_nothing() {
        for p in ["", "<empty>"] {
            let r = TableRegistry::discover(p);
            assert!(r.is_empty());
            assert_eq!(r.max_cardinality(), 0);
        }
    }

    #[test]
    fn the_three_man_set_registers_and_reports_its_cardinality() {
        let Some(dir) = table_dir() else { return };
        let r = TableRegistry::discover(&dir);
        assert_eq!(r.max_cardinality(), 3);
        assert_eq!(r.len(), 5, "KQvK, KRvK, KBvK, KNvK, KPvK");
    }

    /// The decisive test: known three-man verdicts, from both sides.
    #[test]
    fn known_three_man_verdicts_are_correct() {
        let Some(dir) = table_dir() else { return };
        let r = TableRegistry::discover(&dir);

        let cases: &[(&str, Wdl)] = &[
            // King and queen against a bare king is won for the stronger side.
            ("4k3/8/8/8/8/8/8/3QK3 w - - 0 1", Wdl::Win),
            // Same position with the weaker side to move: lost.
            ("4k3/8/8/8/8/4K3/8/3Q4 b - - 0 1", Wdl::Loss),
            // King and rook likewise.
            ("4k3/8/8/8/8/8/8/R3K3 w - - 0 1", Wdl::Win),
            // A lone bishop cannot mate.
            ("4k3/8/8/8/8/8/8/2B1K3 w - - 0 1", Wdl::Draw),
            // Nor a lone knight.
            ("4k3/8/8/8/8/8/8/2N1K3 w - - 0 1", Wdl::Draw),
            // A stalemate trap: the weaker king in the corner with the queen too close.
            ("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1", Wdl::Draw),
        ];
        for (fen, want) in cases {
            let pos = Position::from_fen(fen, false).expect("valid");
            let got = r.probe_wdl(&pos).unwrap_or_else(|e| panic!("{fen}: {e:?}"));
            assert_eq!(got, *want, "{fen}");
        }
    }

    /// A won position must have a positive distance, a drawn one zero.
    #[test]
    fn dtz_signs_agree_with_the_verdict() {
        let Some(dir) = table_dir() else { return };
        let r = TableRegistry::discover(&dir);
        for (fen, positive) in
            [("4k3/8/8/8/8/8/8/3QK3 w - - 0 1", true), ("4k3/8/8/8/8/8/8/2B1K3 w - - 0 1", false)]
        {
            let pos = Position::from_fen(fen, false).expect("valid");
            let dtz = r.probe_dtz(&pos).unwrap_or_else(|e| panic!("{fen}: {e:?}"));
            if positive {
                assert!(dtz > 0, "{fen}: dtz {dtz} should be a win");
            } else {
                assert_eq!(dtz, 0, "{fen}");
            }
        }
    }

    /// A position with more pieces than any table covers must report failure rather than a
    /// wrong answer.
    #[test]
    fn a_position_beyond_the_tables_fails_rather_than_guessing() {
        let Some(dir) = table_dir() else { return };
        let r = TableRegistry::discover(&dir);
        let pos = Position::startpos();
        assert!(r.probe_wdl(&pos).is_err());
        assert!(r.rank_root_moves(&pos).is_none());
    }

    #[test]
    fn bare_kings_are_drawn_without_a_file() {
        let Some(dir) = table_dir() else { return };
        let r = TableRegistry::discover(&dir);
        let pos = Position::from_fen("4k3/8/8/8/8/8/8/4K3 w - - 0 1", false).expect("valid");
        assert_eq!(r.probe_wdl(&pos), Ok(Wdl::Draw));
    }

    #[test]
    fn the_zeroing_distance_follows_the_verdict() {
        assert_eq!(dtz_before_zeroing(Wdl::Win), 1);
        assert_eq!(dtz_before_zeroing(Wdl::Loss), -1);
        assert_eq!(dtz_before_zeroing(Wdl::CursedWin), 101);
        assert_eq!(dtz_before_zeroing(Wdl::BlessedLoss), -101);
        assert_eq!(dtz_before_zeroing(Wdl::Draw), 0);
    }
}
