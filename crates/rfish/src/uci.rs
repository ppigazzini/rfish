//! The UCI transport and the engine session.
//!
//! This is the only place in rfish that reads standard input or writes standard output.
//! The engine crate reports through an [`InfoSink`]; the implementation here turns that
//! into `info` lines. Keeping the split at the crate boundary is what lets a test drive a
//! whole search without capturing a stream.
//!
//! Golden: `Stockfish/src/uci.cpp`, `Stockfish/src/engine.cpp`.

use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::time::Instant;

use rfish_engine::board::movegen::{move_to_uci, parse_uci_move, perft_divide};
use rfish_engine::board::position::{Position, START_FEN};
use rfish_engine::board::types::Color;
use rfish_engine::eval::nnue;
use rfish_engine::platform::syzygy::TableRegistry;
use rfish_engine::platform::threads::ThreadPool;
use rfish_engine::search::tt::TranspositionTable;
use rfish_engine::search::worker::{DepthReport, InfoSink};
use rfish_engine::state::{Limits, SearchOptions};

use crate::bench::{BenchEntry, BenchSpec, parse_entry};
use crate::options::Options;

/// The `id name` line's engine name.
pub(crate) fn engine_name() -> String {
    format!("rfish {}", rfish_engine::VERSION)
}

/// An [`InfoSink`] that writes UCI `info` lines to a stream.
struct UciSink<W: Write> {
    out: W,
    show_wdl: bool,
}

impl<W: Write> InfoSink for UciSink<W> {
    fn depth_finished(&mut self, r: &DepthReport<'_>) {
        let mut line = format!(
            "info depth {} seldepth {} multipv {} score {}",
            r.depth,
            r.sel_depth.max(r.depth),
            r.multi_pv,
            r.score.to_uci()
        );
        match r.bound {
            Some(true) => line.push_str(" lowerbound"),
            Some(false) => line.push_str(" upperbound"),
            None => {}
        }
        if self.show_wdl {
            let [w, d, l] = r.wdl;
            let _ = write!(line, " wdl {w} {d} {l}");
        }
        let _ = write!(
            line,
            " nodes {} nps {} hashfull {} tbhits {} time {}",
            r.nodes, r.nps, r.hashfull, r.tb_hits, r.time_ms
        );
        if !r.pv.is_empty() {
            line.push_str(" pv ");
            line.push_str(&r.pv.join(" "));
        }
        let _ = writeln!(self.out, "{line}");
        let _ = self.out.flush();
    }
}

/// The engine session: everything that survives between commands.
pub(crate) struct Engine {
    pos: Position,
    options: Options,
    tt: TranspositionTable,
    pool: ThreadPool,
    tablebases: Option<Arc<TableRegistry>>,
    network: Option<Arc<nnue::Network>>,
}

impl core::fmt::Debug for Engine {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Engine").field("threads", &self.pool.len()).finish_non_exhaustive()
    }
}

impl Default for Engine {
    fn default() -> Engine {
        Engine::new()
    }
}

impl Engine {
    /// A session in its startup state: the start position, default options, an empty table.
    #[must_use]
    pub(crate) fn new() -> Engine {
        let options = Options::default();
        let tt = TranspositionTable::new(options.spin("Hash") as usize);
        let pool = ThreadPool::new(options.spin("Threads") as usize);
        Engine { pos: Position::startpos(), options, tt, pool, tablebases: None, network: None }
    }

    /// Handle one command line. Returns `false` on `quit`.
    ///
    /// Unknown commands are reported and ignored, never fatal: a GUI sends what it likes,
    /// and an engine that exits on an unrecognised word is unusable.
    pub(crate) fn handle(&mut self, line: &str, out: &mut impl Write) -> bool {
        let mut tokens = line.split_ascii_whitespace();
        let Some(cmd) = tokens.next() else { return true };
        let rest: Vec<&str> = tokens.collect();

        match cmd {
            "uci" => self.cmd_uci(out),
            "isready" => {
                let _ = writeln!(out, "readyok");
            }
            "setoption" => self.cmd_setoption(&rest, out),
            "ucinewgame" => self.cmd_newgame(),
            "position" => self.cmd_position(&rest, out),
            "go" => self.cmd_go(&rest, out),
            "stop" => self.pool.shared().request_stop(),
            "ponderhit" => self.pool.shared().ponder_hit(),
            "d" => {
                let _ = writeln!(out, "{}", self.pos);
                // The tablebase view of the position, in upstream's own format so a
                // differential gate can diff the two engines line for line. Printed here
                // because it is the only place a probe's answer is readable with no search
                // around it.
                if let Some(tb) = self.tablebases.as_deref() {
                    if let Ok(wdl) = tb.probe_wdl(&self.pos) {
                        let _ = writeln!(out, "Tablebases WDL: {}", wdl as i32);
                    }
                    if let Ok(dtz) = tb.probe_dtz(&self.pos) {
                        let _ = writeln!(out, "Tablebases DTZ: {dtz}");
                    }
                }
            }
            "eval" => self.cmd_eval(out),
            "bench" => self.cmd_bench(&rest, out),
            "compiler" => {
                let _ = writeln!(
                    out,
                    "Compiled by rustc {} for {}",
                    env!("RFISH_RUSTC"),
                    std::env::consts::ARCH
                );
            }
            "quit" => {
                self.pool.shared().request_stop();
                return false;
            }
            _ => {
                let _ = writeln!(out, "Unknown command: '{line}'. Type help for more information.");
            }
        }
        let _ = out.flush();
        true
    }

    fn cmd_uci(&self, out: &mut impl Write) {
        let _ = writeln!(out, "id name {}", engine_name());
        let _ = writeln!(out, "id author the Stockfish developers (see AUTHORS)");
        let _ = writeln!(out);
        for line in self.options.handshake_lines() {
            let _ = writeln!(out, "{line}");
        }
        let _ = writeln!(out, "uciok");
    }

    fn cmd_setoption(&mut self, args: &[&str], out: &mut impl Write) {
        // `setoption name <words...> value <words...>`. Both halves can contain spaces, so
        // split on the keywords rather than on position.
        let name_start = args.iter().position(|&t| t == "name").map_or(0, |i| i + 1);
        let value_pos = args.iter().position(|&t| t == "value");
        let (name_end, value) = match value_pos {
            Some(i) => (i, args[i + 1..].join(" ")),
            None => (args.len(), String::new()),
        };
        let name = args[name_start.min(name_end)..name_end].join(" ");

        if !self.options.set(&name, &value) {
            let _ = writeln!(out, "No such option: {name}");
            return;
        }
        self.apply_option(&name, out);
    }

    /// React to an option whose new value changes engine state.
    ///
    /// Setting an option is not just recording it: `Hash` reallocates, `Threads` resizes
    /// the pool, `EvalFile` reloads the network. An option the engine records but never
    /// acts on is worse than one it does not declare.
    fn apply_option(&mut self, name: &str, out: &mut impl Write) {
        match name.to_ascii_lowercase().as_str() {
            "hash" => self.tt.resize(self.options.spin("Hash") as usize),
            "clear hash" => {
                self.tt.clear();
                self.pool.clear();
            }
            "threads" => self.pool.resize(self.options.spin("Threads") as usize),
            "syzygypath" | "syzygyprobedepth" | "syzygyprobelimit" | "syzygy50moverule" => {
                if name.eq_ignore_ascii_case("SyzygyPath") {
                    let reg = TableRegistry::discover(self.options.text("SyzygyPath"));
                    if reg.is_empty() {
                        self.tablebases = None;
                    } else {
                        let _ = writeln!(
                            out,
                            "info string Found {} tablebases up to {} pieces",
                            reg.len(),
                            reg.max_cardinality()
                        );
                        self.tablebases = Some(Arc::new(reg));
                    }
                }
                self.apply_tb_options();
            }
            "evalfile" => self.load_network(out),
            _ => {}
        }
    }

    /// The option values the search reads, gathered in one place.
    ///
    /// `UCI_Elo` is passed only when `UCI_LimitStrength` is on: a GUI that leaves the Elo
    /// spin at some value it never meant to apply must not find itself handicapped.
    fn search_options(&self) -> SearchOptions {
        SearchOptions {
            multi_pv: self.options.spin("MultiPV") as usize,
            move_overhead: self.options.spin("Move Overhead") as u64,
            nodestime: self.options.spin("nodestime") as u64,
            ponder: self.options.check("Ponder"),
            skill_level: self.options.spin("Skill Level") as i32,
            uci_elo: self
                .options
                .check("UCI_LimitStrength")
                .then(|| self.options.spin("UCI_Elo") as i32),
        }
    }

    /// Push the tablebase registry and its bounding options into the pool.
    fn apply_tb_options(&mut self) {
        self.pool.set_tablebases(
            &self.tablebases,
            self.options.spin("SyzygyProbeDepth") as i32,
            self.options.spin("SyzygyProbeLimit") as u32,
            self.options.check("Syzygy50MoveRule"),
        );
    }

    /// Load the network named by `EvalFile`, reporting either way.
    ///
    /// A missing net is NOT an error: rfish runs on the classical scaffolding and says so,
    /// which is what makes every gate above the evaluation runnable before M3 lands.
    pub(crate) fn load_network(&mut self, out: &mut impl Write) {
        let name = self.options.text("EvalFile").to_string();
        self.network = match nnue::find_and_load(&name) {
            Some(Ok(net)) => {
                let _ = writeln!(out, "info string NNUE evaluation using {}", net.name());
                Some(Arc::new(net))
            }
            // A file that exists but does not load is reported as the failure it is. Falling
            // back silently would leave the engine playing on the scaffolding while the user
            // believes it has its evaluation.
            Some(Err(e)) => {
                let _ = writeln!(out, "info string Failed to load {name}: {e}");
                None
            }
            None => {
                let _ = writeln!(
                    out,
                    "info string NNUE file {name} not found; using the classical evaluation"
                );
                None
            }
        };
        self.pool.set_network(&self.network);
    }

    fn cmd_newgame(&mut self) {
        self.tt.clear();
        self.pool.clear();
    }

    fn cmd_position(&mut self, args: &[&str], out: &mut impl Write) {
        let chess960 = self.options.check("UCI_Chess960");
        let moves_at = args.iter().position(|&t| t == "moves");
        let head = &args[..moves_at.unwrap_or(args.len())];

        let fen = match head.first().copied() {
            Some("startpos") | None => START_FEN.to_string(),
            Some("fen") => head[1..].join(" "),
            Some(other) => other.to_string(),
        };

        match Position::from_fen(&fen, chess960) {
            Ok(p) => self.pos = p,
            Err(e) => {
                let _ = writeln!(out, "info string Invalid FEN: {e}");
                return;
            }
        }

        if let Some(i) = moves_at {
            for token in &args[i + 1..] {
                if let Some(m) = parse_uci_move(&self.pos, token) {
                    self.pos.do_move(m);
                } else {
                    let _ = writeln!(out, "info string Illegal move: {token}");
                    return;
                }
            }
        }
    }

    fn cmd_go(&mut self, args: &[&str], out: &mut impl Write) {
        // `go perft N` is a movegen command, not a search: answer it and return.
        if let Some(i) = args.iter().position(|&t| t == "perft") {
            let depth: u32 = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(1);
            let start = Instant::now();
            let (moves, total) = perft_divide(&mut self.pos, depth);
            for (m, n) in moves {
                let _ = writeln!(out, "{}: {n}", move_to_uci(&self.pos, m));
            }
            let ms = start.elapsed().as_millis().max(1);
            let _ = writeln!(out, "\nNodes searched: {total}");
            let _ = writeln!(out, "Time: {ms} ms  ({} nps)", total as u128 * 1000 / ms);
            return;
        }

        let limits = self.parse_limits(args);
        let result = {
            let opts = self.search_options();
            let mut sink = UciSink { out: &mut *out, show_wdl: self.options.check("UCI_ShowWDL") };
            self.pool.search(&self.pos, &limits, &self.tt, &opts, &mut sink)
        };

        let best = if result.best_move.is_none() {
            "(none)".to_string()
        } else {
            move_to_uci(&self.pos, result.best_move)
        };
        match result.ponder_move {
            Some(p) if self.options.check("Ponder") => {
                // The ponder move is named in the position AFTER the best move, which is
                // the only place its castling notation is well defined.
                let mut after = self.pos.clone();
                after.do_move(result.best_move);
                let _ = writeln!(out, "bestmove {best} ponder {}", move_to_uci(&after, p));
            }
            _ => {
                let _ = writeln!(out, "bestmove {best}");
            }
        }
        let _ = out.flush();
    }

    fn parse_limits(&self, args: &[&str]) -> Limits {
        let mut l =
            Limits { start: Some(Instant::now()), ply: self.pos.game_ply(), ..Limits::default() };
        let mut i = 0;
        while i < args.len() {
            let next = |i: usize| args.get(i + 1).and_then(|s| s.parse::<u64>().ok());
            match args[i] {
                "wtime" => l.time[Color::White.index()] = next(i),
                "btime" => l.time[Color::Black.index()] = next(i),
                "winc" => l.inc[Color::White.index()] = next(i).unwrap_or(0),
                "binc" => l.inc[Color::Black.index()] = next(i).unwrap_or(0),
                "movestogo" => l.moves_to_go = next(i).map(|v| v as u32),
                "depth" => l.depth = next(i).map(|v| v as i32),
                "nodes" => l.nodes = next(i),
                "movetime" => l.move_time = next(i),
                "mate" => l.mate = next(i).map(|v| v as i32),
                // `ponder` searches until told to stop, exactly as `infinite` does; the
                // difference is what the shell does with the result, not what the search
                // does.
                "infinite" | "ponder" => l.infinite = true,
                "searchmoves" => {
                    // Everything to the end of the line is a move, so consume it here.
                    for token in &args[i + 1..] {
                        if let Some(m) = parse_uci_move(&self.pos, token) {
                            l.search_moves.push(m);
                        }
                    }
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        l
    }

    fn cmd_eval(&self, out: &mut impl Write) {
        let _ = writeln!(out, "\n{}", self.pos);
        let mut scratch = nnue::Scratch::default();

        // The raw network output, in the internal units upstream's `eval` prints. This is
        // the number the differential gate compares: it is the network alone, with no
        // optimism blend and no fifty-move damping on top, so a mismatch localises to the
        // forward pass rather than to the terms around it.
        //
        // A position IN CHECK is not evaluated at all, matching upstream, which asserts on
        // one. The network is trained on quiet positions and its answer there means nothing;
        // printing one anyway would also make this line unusable as a differential, since
        // the two engines would emit a different number of them.
        if let Some(net) = self.network.as_deref().filter(|_| !self.pos.in_check()) {
            let raw = net.evaluate(&self.pos, &mut scratch);
            let _ = writeln!(
                out,
                "\nNNUE evaluation  {:+} (side to move, internal units)",
                raw.psqt + raw.positional
            );
        }

        let v = rfish_engine::eval::evaluate(&self.pos, self.network.as_deref(), &mut scratch, 0);
        let source = if self.network.is_some() { "NNUE" } else { "classical" };
        let white = if self.pos.side_to_move() == Color::White { v } else { -v };
        let _ = writeln!(
            out,
            "Final evaluation  {:+.2} ({source}, white side)",
            f64::from(white) / 208.0
        );
    }

    fn cmd_bench(&mut self, args: &[&str], out: &mut impl Write) {
        let spec = BenchSpec::parse(args);
        let entries: Vec<String> =
            if spec.entries.is_empty() { vec![self.pos.fen()] } else { spec.entries.clone() };

        // A SINGLE `ucinewgame` before the whole run, not one per position: the table and
        // the histories carry across positions, and that is part of what the number means.
        self.tt.resize(spec.hash_mb);
        self.pool.resize(spec.threads);
        self.pool.clear();
        self.tt.clear();

        let start = Instant::now();
        let mut total_nodes = 0u64;
        let mut searched = 0usize;
        let total_positions = entries
            .iter()
            .filter(|e| matches!(parse_entry(e), BenchEntry::Position { .. }))
            .count();

        for entry in &entries {
            match parse_entry(entry) {
                BenchEntry::Command(cmd) => {
                    // Run it exactly as if it had arrived on standard input, so a
                    // `setoption` in the list has the same effect it would from a GUI.
                    self.handle(&cmd, out);
                }
                BenchEntry::Position { fen, moves } => {
                    let chess960 = self.options.check("UCI_Chess960");
                    let mut pos = match Position::from_fen(&fen, chess960) {
                        Ok(p) => p,
                        Err(e) => {
                            let _ = writeln!(out, "info string Skipping bench position: {e}");
                            continue;
                        }
                    };
                    for mv in &moves {
                        if let Some(m) = parse_uci_move(&pos, mv) {
                            pos.do_move(m);
                        } else {
                            let _ = writeln!(out, "info string Illegal bench move {mv} in {fen}");
                            break;
                        }
                    }

                    searched += 1;
                    let _ = writeln!(out, "\nPosition: {searched}/{total_positions} ({fen})");

                    let mut limits = Limits {
                        start: Some(Instant::now()),
                        ply: pos.game_ply(),
                        ..Limits::default()
                    };
                    match spec.limit_kind.as_str() {
                        "nodes" => limits.nodes = Some(spec.limit as u64),
                        "movetime" => limits.move_time = Some(spec.limit as u64),
                        _ => limits.depth = Some(spec.limit),
                    }

                    // The bench measures the engine at full strength and one PV, whatever
                    // the option map happens to hold: a benchmark that a `Skill Level` left
                    // over from an earlier command can move is not an anchor.
                    let opts = SearchOptions {
                        multi_pv: 1,
                        skill_level: 20,
                        uci_elo: None,
                        ..self.search_options()
                    };
                    let result = {
                        let mut sink = UciSink { out: &mut *out, show_wdl: false };
                        self.pool.search(&pos, &limits, &self.tt, &opts, &mut sink)
                    };
                    total_nodes += result.nodes;
                }
            }
        }

        let ms = start.elapsed().as_millis().max(1);
        let _ = writeln!(out, "\n===========================");
        let _ = writeln!(out, "Total time (ms) : {ms}");
        let _ = writeln!(out, "Nodes searched  : {total_nodes}");
        let _ = writeln!(out, "Nodes/second    : {}", u128::from(total_nodes) * 1000 / ms);
        let _ = out.flush();
    }
}

/// Read commands from `input` until `quit` or end of stream.
///
/// A `stop` must be able to interrupt a running search, and the search runs on this thread.
/// The input loop therefore reads a line, dispatches it, and returns — a blocking search is
/// interrupted by the shared stop flag, which the pool checks. That is the same shape
/// upstream uses, with the flag made explicit.
pub(crate) fn run(input: impl BufRead, mut output: impl Write) {
    let mut engine = Engine::new();
    // Report the network situation once at startup, the way upstream does, so a user who
    // forgot the net finds out immediately rather than after a weak game.
    engine.load_network(&mut output);

    for line in input.lines() {
        let Ok(line) = line else { break };
        if !engine.handle(&line, &mut output) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive the engine with a script and return everything it wrote.
    fn drive(script: &[&str]) -> String {
        let mut engine = Engine::new();
        let mut out = Vec::new();
        for line in script {
            engine.handle(line, &mut out);
        }
        String::from_utf8(out).expect("the engine writes UTF-8")
    }

    #[test]
    fn the_handshake_ends_with_uciok_and_names_the_engine() {
        let out = drive(&["uci"]);
        assert!(out.starts_with("id name rfish"));
        assert!(out.contains("option name Threads"));
        assert!(out.contains("option name Hash"));
        assert!(out.trim_end().ends_with("uciok"));
    }

    #[test]
    fn isready_answers_readyok() {
        assert_eq!(drive(&["isready"]), "readyok\n");
    }

    #[test]
    fn an_unknown_command_is_reported_and_survivable() {
        let out = drive(&["nonsense", "isready"]);
        assert!(out.contains("Unknown command: 'nonsense'"));
        assert!(out.contains("readyok"));
    }

    #[test]
    fn position_startpos_with_moves_reaches_the_right_board() {
        let out = drive(&["position startpos moves e2e4 e7e5 g1f3", "d"]);
        assert!(out.contains("rnbqkbnr/pppp1ppp/8/4p3/4P3/5N2/PPPP1PPP/RNBQKB1R b KQkq - 1 2"));
    }

    #[test]
    fn position_fen_is_accepted_and_reported_back() {
        let fen = "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 11";
        let out = drive(&[&format!("position fen {fen}"), "d"]);
        assert!(out.contains(fen), "{out}");
    }

    #[test]
    fn an_invalid_fen_is_reported_without_changing_the_position() {
        let out = drive(&["position fen not a fen at all x y", "d"]);
        assert!(out.contains("Invalid FEN"));
        // The board is still the start position.
        assert!(out.contains(START_FEN));
    }

    #[test]
    fn an_illegal_move_in_the_move_list_is_reported() {
        let out = drive(&["position startpos moves e2e5"]);
        assert!(out.contains("Illegal move: e2e5"));
    }

    #[test]
    fn go_depth_emits_info_lines_and_one_bestmove() {
        let out = drive(&["position startpos", "go depth 6"]);
        assert!(out.contains("info depth 1 "));
        assert!(out.contains(" pv "));
        assert_eq!(out.matches("bestmove ").count(), 1);
        let best = out.rsplit("bestmove ").next().expect("a bestmove line").trim();
        assert_eq!(best.len(), 4, "bestmove '{best}' is not a UCI move");
    }

    #[test]
    fn go_perft_prints_the_divide_and_the_total() {
        let out = drive(&["position startpos", "go perft 3"]);
        assert!(out.contains("Nodes searched: 8902"));
        assert!(out.contains("e2e4: 600"));
    }

    #[test]
    fn setoption_changes_a_value_and_an_unknown_one_is_reported() {
        let out = drive(&["setoption name Hash value 32", "setoption name Bogus value 1"]);
        assert!(out.contains("No such option: Bogus"));
        assert!(!out.contains("No such option: Hash"));
    }

    #[test]
    fn setoption_handles_names_and_values_containing_spaces() {
        let out = drive(&["setoption name Move Overhead value 42", "uci"]);
        assert!(!out.contains("No such option"));
        // Clear Hash is a two-word button name.
        let out = drive(&["setoption name Clear Hash"]);
        assert!(!out.contains("No such option"));
    }

    #[test]
    fn chess960_castling_is_named_by_the_rook_when_the_option_is_set() {
        let out = drive(&[
            "setoption name UCI_Chess960 value true",
            "position fen bqnb1rkr/pp3ppp/3ppn2/2p5/5P2/P2P4/NPP1P1PP/BQ1BNRKR w HFhf - 2 9",
            "go perft 1",
        ]);
        // 21 legal moves, and the 960 castling field parsed -- under the standard dialect
        // the position would have been rejected outright.
        assert!(out.contains("Nodes searched: 21"), "{out}");
    }

    #[test]
    fn quit_stops_the_loop() {
        let mut engine = Engine::new();
        let mut out = Vec::new();
        assert!(engine.handle("isready", &mut out));
        assert!(!engine.handle("quit", &mut out));
    }

    /// The `eval` output names WHICH evaluation produced the number. A run without a net
    /// is not a failure, but it is a different engine, and the output has to say so.
    #[test]
    fn eval_prints_a_number_and_names_its_source() {
        let out = drive(&["position startpos", "eval"]);
        assert!(out.contains("Final evaluation"), "{out}");
        assert!(out.contains("classical") || out.contains("NNUE"), "{out}");
    }

    #[test]
    fn bench_prints_a_node_total() {
        // A tiny bench: two positions at depth 4, so the test stays fast while exercising
        // the same path the signature gate drives.
        let out = drive(&["bench 1 1 4 current"]);
        assert!(out.contains("Nodes searched  : "));
        let n: u64 = out
            .rsplit("Nodes searched  : ")
            .next()
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .expect("a node total");
        assert!(n > 0);
    }
}
