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
use rfish_engine::platform::numa::{self, NumaConfig};
use rfish_engine::platform::syzygy::TableRegistry;
use rfish_engine::platform::threads::ThreadPool;
use rfish_engine::search::tt::TranspositionTable;
use rfish_engine::search::worker::{DepthReport, InfoSink, SearchResult};
use rfish_engine::state::{Limits, SearchOptions};

use crate::bench::{BenchEntry, BenchSpec, parse_entry};
use crate::options::Options;

/// What `help` and `license` print.
///
/// The unknown-command reply has always pointed at this text; it just did not exist. The
/// wording is upstream's, aimed at the person who ran a UCI engine expecting a chess
/// program with a board, and pointed at this repository's own files rather than upstream's
/// because those are the ones shipped alongside this binary.
const HELP: &str = "\nrfish is a safe-Rust port of Stockfish, a powerful chess engine for \
playing and analyzing.\nIt is released as free software licensed under the GNU GPLv3 \
License.\nrfish is normally used with a graphical user interface (GUI) and implements\nthe \
Universal Chess Interface (UCI) protocol to communicate with a GUI, an API, etc.\nFor any \
further information, read the README.md and Copying.txt files distributed along with this \
program.";

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
    /// The command line being handled, as upstream's `currentCmd`.
    ///
    /// Only the critical-error report reads it, and that report quotes the WHOLE line.
    current_cmd: String,
    /// Set when a command failed in a way upstream treats as FATAL.
    ///
    /// Upstream calls `terminate_on_critical_error`, which prints and then `std::exit(1)`.
    /// rfish records it and lets the driver leave, so the shell stays drivable from a test
    /// and from the golden harness -- both of which call `handle` in this process and would
    /// take the exit with it. `main` turns the flag into the exit code.
    fatal: bool,
    pos: Position,
    options: Options,
    tt: TranspositionTable,
    pool: ThreadPool,
    tablebases: Option<Arc<TableRegistry>>,
    network: Option<Arc<nnue::Network>>,
    /// The processor topology the `NumaPolicy` option selected.
    numa: NumaConfig,
    /// Whether an input reader is clearing the stop flag on this engine's behalf.
    ///
    /// There are two ways in: the shipped binary, which reads ahead on its own thread, and a
    /// direct `handle` call from a test or a golden harness, which has no reader at all. The
    /// stop flag must be cleared exactly once per search command, at the earliest point that
    /// OBSERVES it -- the reader where there is one, `handle` where there is not. Doing it in
    /// both would race the reader and undo a real `stop`; doing it in neither leaves a stale
    /// one to truncate the next search. Both mistakes were made before this flag existed.
    reader_owns_stop: bool,
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
        let numa = NumaConfig::from_system(numa::DEFAULT_AUTO_POLICY, true);
        Engine {
            fatal: false,
            current_cmd: String::new(),
            pos: Position::startpos(),
            options,
            tt,
            pool,
            tablebases: None,
            network: None,
            numa,
            reader_owns_stop: false,
        }
    }

    /// Report a command that failed fatally, exactly as upstream reports it.
    ///
    /// Upstream's text, its backticks and its TRAILING BLANK LINE -- `sync_endl` after an
    /// explicit newline -- because this string reaches a GUI and a paraphrase is a
    /// divergence like any other.
    fn critical(&mut self, reason: &impl std::fmt::Display, out: &mut impl Write) {
        let line = self.current_cmd.clone();
        let _ = writeln!(
            out,
            "info string CRITICAL ERROR: Command `{line}` failed. Reason: {reason}\n"
        );
        let _ = out.flush();
        self.fatal = true;
    }

    /// Whether a command failed in a way that must end the process.
    pub(crate) fn is_fatal(&self) -> bool {
        self.fatal
    }

    /// Handle one command line. Returns `false` on `quit`.
    ///
    /// Unknown commands are reported and ignored, never fatal: a GUI sends what it likes,
    /// and an engine that exits on an unrecognised word is unusable.
    pub(crate) fn handle(&mut self, line: &str, out: &mut impl Write) -> bool {
        self.current_cmd = line.to_string();
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
            // Mirror the position. The evaluation of a position and of its mirror should
            // agree up to sign, so this is how an asymmetric evaluation is caught.
            "flip" => {
                if let Err(e) = self.pos.flip() {
                    let _ = writeln!(out, "info string Cannot flip the position: {e}");
                }
            }
            // Write the loaded net back out. The round trip is the point: a net that
            // survives it proves the reader and the writer agree.
            "export_net" => {
                let name = rest.first().map_or_else(
                    || self.options.text("EvalFile").to_string(),
                    |s| (*s).to_string(),
                );
                match self.network.as_deref() {
                    None => {
                        let _ = writeln!(out, "info string No network is loaded");
                    }
                    Some(net) => match net.save(std::path::Path::new(&name)) {
                        Ok(()) => {
                            let _ = writeln!(out, "info string Network saved to {name}");
                        }
                        Err(e) => {
                            let _ = writeln!(out, "info string Failed to save {name}: {e}");
                        }
                    },
                }
            }
            "help" | "--help" | "license" | "--license" => {
                let _ = writeln!(out, "{HELP}");
            }
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
            // A leading '#' is a comment. Command lists are kept in files and commented,
            // and an engine that answered "Unknown command" to every comment would bury the
            // real output.
            _ if cmd.starts_with('#') => {}
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
            "threads" => {
                self.pool.resize(self.options.spin("Threads") as usize);
                self.report_thread_allocation(out);
            }
            "numapolicy" => {
                let value = self.options.text("NumaPolicy").to_string();
                if self.set_numa_config_from_option(&value) {
                    self.report_numa_config(out);
                    self.report_thread_allocation(out);
                } else {
                    // Upstream's wording, and upstream's behaviour: a policy the engine
                    // cannot honour leaves the previous one in place rather than falling
                    // back to something the operator did not ask for.
                    let _ = writeln!(
                        out,
                        "info string NumaPolicy: invalid value '{value}', keeping previous config."
                    );
                }
            }
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
            "debug log file" => {
                let path = self.options.text("Debug Log File").to_string();
                if let Err(e) = crate::debug_log::set_path(&path) {
                    // Reported, not swallowed: an operator who asked for a transcript and
                    // silently got none would trust a record that was never written.
                    let _ = writeln!(out, "info string Cannot open debug log {path}: {e}");
                }
            }
            _ => {}
        }
    }

    /// Apply a `NumaPolicy` value. False when it names no config the engine can build.
    ///
    /// The four keywords are upstream's. `hardware` deliberately ignores the affinity the
    /// process was started under, which is how an operator asks about the machine rather
    /// than about this process's slice of it.
    fn set_numa_config_from_option(&mut self, value: &str) -> bool {
        self.numa = match value {
            "auto" | "system" => NumaConfig::from_system(numa::DEFAULT_AUTO_POLICY, true),
            "hardware" => NumaConfig::from_system(numa::DEFAULT_AUTO_POLICY, false),
            "none" => NumaConfig::default(),
            explicit => match NumaConfig::from_string(explicit) {
                Some(cfg) => cfg,
                None => return false,
            },
        };
        true
    }

    /// Whether this policy asks for the thread set to be spread across nodes.
    fn numa_distributes_threads(&self, threads: usize) -> bool {
        match self.options.text("NumaPolicy") {
            "none" => false,
            "auto" => self.numa.suggests_binding_threads(threads),
            // `system`, `hardware`, or an explicit spec: the operator asked for it.
            _ => true,
        }
    }

    /// `info string Available processors: …`, in upstream's wording.
    fn report_numa_config(&self, out: &mut impl Write) {
        let _ = writeln!(out, "info string Available processors: {}", self.numa.to_string_spec());
    }

    /// `info string Using N thread(s)`, plus the per-node split when one is in effect.
    ///
    /// Upstream says "thread binding" here because it has bound them. rfish says
    /// "distribution", because it has NOT: `sched_setaffinity` is FFI, and this engine has
    /// no `unsafe` and no dependency to reach it through. The numbers are the same numbers
    /// -- how many workers were ASSIGNED to each node, against that node's processor count
    /// -- and the assignment is real, because it is what would select a memory replica.
    /// What rfish cannot promise is that the scheduler leaves them there, so it does not
    /// use a word that would promise it.
    fn report_thread_allocation(&self, out: &mut impl Write) {
        let threads = self.pool.len();
        let plural = if threads > 1 { "threads" } else { "thread" };

        if !self.numa_distributes_threads(threads) {
            let _ = writeln!(out, "info string Using {threads} {plural}");
            return;
        }

        let assignment = self.numa.distribute_threads_among_numa_nodes(threads);
        let mut counts = vec![0usize; self.numa.num_numa_nodes()];
        for &n in &assignment {
            if let Some(slot) = counts.get_mut(n) {
                *slot += 1;
            }
        }
        let split: Vec<String> = counts
            .iter()
            .enumerate()
            .map(|(n, c)| format!("{c}/{}", self.numa.num_cpus_in_numa_node(n)))
            .collect();
        let _ = writeln!(
            out,
            "info string Using {threads} {plural} with NUMA node thread distribution: {}",
            split.join(":")
        );
        let _ = writeln!(
            out,
            "info string NUMA threads are distributed, not pinned: this engine is 100% safe \
             Rust and sched_setaffinity has no safe interface."
        );
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

    /// Report the processor topology and the thread allocation, once at startup.
    ///
    /// Upstream emits these lazily, when the first search forces the thread pool into
    /// existence. rfish builds its pool eagerly, so it reports eagerly; the content is the
    /// same and an operator reading a log sees the same facts.
    pub(crate) fn report_configuration(&self, out: &mut impl Write) {
        self.report_numa_config(out);
        self.report_thread_allocation(out);
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
                // One replica, in this process's own memory. Upstream can report "Shared
                // memory." here because it maps the weights through POSIX shm so several
                // engine processes share one copy; that is `shm_open` plus `mmap`, which
                // are FFI, so rfish always holds its own. The wording is upstream's for the
                // case rfish is always in.
                let _ = writeln!(out, "info string Network replica 1: Local memory.");
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
                // Upstream terminates here: a position it cannot set is a CRITICAL ERROR and
                // exit 1, not a message the GUI can ignore.
                self.critical(&e, out);
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
        // With no reader thread ahead of this call, here is the earliest point that observes
        // the command, so here is where the previous search's stop is dropped.
        if !self.reader_owns_stop {
            self.pool.shared().clear_stop();
        }
        // `go perft N` is a movegen command, not a search: answer it and return.
        if let Some(i) = args.iter().position(|&t| t == "perft") {
            let depth: u32 = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(1);
            let (moves, total) = perft_divide(&mut self.pos, depth);
            for (m, n) in moves {
                let _ = writeln!(out, "{}: {n}", move_to_uci(&self.pos, m));
            }
            // Upstream's exact block: a blank line, the total, and a TRAILING blank line
            // from its `sync_endl`. It prints no timing here, and neither does this -- an
            // extra line is a divergence even when it is only informational, and the
            // goldens could not see this one because the volatile filter drops any `Time:`.
            let _ = writeln!(out, "\nNodes searched: {total}\n");
            return;
        }

        let limits = match self.parse_limits(args) {
            Ok(l) => l,
            Err(reason) => {
                // Upstream does not merely decline to search: `terminate_on_critical_error`
                // prints and exits 1. Route it through the same reporter as every other
                // fatal command, which quotes the whole line the way upstream's `currentCmd`
                // does.
                self.critical(&reason, out);
                return;
            }
        };
        let result = {
            let opts = self.search_options();
            let mut sink = UciSink { out: &mut *out, show_wdl: self.options.check("UCI_ShowWDL") };
            self.pool.search(&self.pos, &limits, &self.tt, &opts, &mut sink)
        };

        emit_bestmove(out, &self.pos, &result);
    }

    /// Parse a `go` argument list, or name the key whose value was unusable.
    ///
    /// Every key here TAKES a value, and upstream rejects the whole command when one is
    /// missing or does not parse, rather than ignoring it. That difference is not cosmetic:
    /// a swallowed `wtime` leaves no clock, a swallowed `depth` leaves no depth, and a `go`
    /// with no limit at all searches until it is stopped — so an engine that ignores the bad
    /// value answers nothing at all, where upstream answers with an error. A fuzz run found
    /// exactly that: `go value Hash binc isready SyzygyPath` left rfish searching forever.
    ///
    /// Parsed as `i64` because that is the width and the signedness upstream parses at, and
    /// both edges are observable: `movestogo -5` is ACCEPTED there, and
    /// `nodes 99999999999999999999` is REJECTED for overflowing it.
    fn parse_limits(&self, args: &[&str]) -> Result<Limits, String> {
        let mut l =
            Limits { start: Some(Instant::now()), ply: self.pos.game_ply(), ..Limits::default() };
        let mut i = 0;
        while i < args.len() {
            // A key with no value, or one that does not parse, fails the command and names
            // itself. Unknown tokens are still ignored — upstream accepts `go value Hash`.
            let need = |i: usize, key: &str| -> Result<i64, String> {
                args.get(i + 1)
                    .and_then(|s| s.parse::<i64>().ok())
                    .ok_or_else(|| format!("Invalid argument for '{key}'"))
            };
            match args[i] {
                "wtime" => l.time[Color::White.index()] = Some(need(i, "wtime")? as u64),
                "btime" => l.time[Color::Black.index()] = Some(need(i, "btime")? as u64),
                "winc" => l.inc[Color::White.index()] = need(i, "winc")? as u64,
                "binc" => l.inc[Color::Black.index()] = need(i, "binc")? as u64,
                "movestogo" => l.moves_to_go = Some(need(i, "movestogo")? as u32),
                "depth" => l.depth = Some(need(i, "depth")? as i32),
                "nodes" => l.nodes = Some(need(i, "nodes")? as u64),
                "movetime" => l.move_time = Some(need(i, "movetime")? as u64),
                "mate" => l.mate = Some(need(i, "mate")? as i32),
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
        Ok(l)
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
        if !self.reader_owns_stop {
            self.pool.shared().clear_stop();
        }
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
                    // Upstream's bench runs each entry through the same path a `go` takes,
                    // so every position ends with its own `bestmove`. A harness that reads
                    // the played move rather than the node total needs it.
                    emit_bestmove(out, &pos, &result);
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

/// Write the `bestmove` line that ends a search.
///
/// The ponder move is emitted whenever the principal variation has one, NOT only when the
/// `Ponder` option is on. The option says whether the engine may think on the opponent's
/// clock; it does not say whether the GUI is allowed to be told what reply was expected,
/// and upstream reports it either way.
fn emit_bestmove(out: &mut impl Write, pos: &Position, result: &SearchResult) {
    let best = if result.best_move.is_none() {
        "(none)".to_string()
    } else {
        move_to_uci(pos, result.best_move)
    };
    match result.ponder_move {
        Some(p) => {
            // The ponder move is named in the position AFTER the best move, which is the
            // only place its castling notation is well defined.
            let mut after = pos.clone();
            after.do_move(result.best_move);
            let _ = writeln!(out, "bestmove {best} ponder {}", move_to_uci(&after, p));
        }
        None => {
            let _ = writeln!(out, "bestmove {best}");
        }
    }
    let _ = out.flush();
}

/// Read commands from `input` until `quit` or end of stream.
///
/// **Reading and searching cannot share a thread.** The search runs where `go` was
/// dispatched, so a loop that reads one line, dispatches it, and only then reads the next
/// cannot see a `stop` until the search it would stop has already ended. `go infinite`
/// followed by `stop` hung forever, which is the shape every analysis GUI uses.
///
/// So stdin is drained by its own thread. The two commands that must act DURING a search
/// act on that thread, against the shared atomics they were built for; everything else
/// queues and is dispatched here, in order, exactly as before.
///
/// **A `quit` interrupts the search only when the search cannot end by itself.** Upstream
/// aborts unconditionally, and rfish deliberately does not: `go depth 13` followed by
/// `quit` is how every gate and every measurement harness drives this binary, and aborting
/// there would turn a node count into a number that depends on scheduling. A `go infinite`
/// has no such answer to wait for, so that one is stopped.
pub(crate) fn run(input: impl BufRead + Send + 'static, output: impl Write) -> bool {
    // Every byte the engine writes passes through the transcript wrapper. It costs a
    // newline scan per write when logging is off, and nothing else.
    let mut output = crate::debug_log::TeeWriter::new(output);
    let mut engine = Engine::new();
    // Report the topology and the network situation once at startup, the way upstream does,
    // so a user who forgot the net -- or who is on a machine whose nodes the engine could
    // not read -- finds out immediately rather than after a weak game.
    engine.report_configuration(&mut output);
    engine.load_network(&mut output);

    // The reader below clears the stop flag, so `handle` must not: see `reader_owns_stop`.
    engine.reader_owns_stop = true;
    let shared = Arc::clone(engine.pool.shared());
    let (tx, rx) = std::sync::mpsc::channel::<String>();

    // Detached on purpose: it owns only stdin and a sender, and blocking on a read that
    // will never complete is precisely what it is for.
    std::thread::spawn(move || {
        for line in input.lines() {
            let Ok(line) = line else { break };
            let trimmed = line.trim();
            let starts_search = trimmed == "go"
                || trimmed == "bench"
                || trimmed.starts_with("go ")
                || trimmed.starts_with("bench ");
            match trimmed {
                "stop" => shared.request_stop(),
                "ponderhit" => shared.ponder_hit(),
                "quit" if shared.searching_unbounded() => shared.request_stop(),
                // Cleared HERE, where the command is read, and not where the search starts:
                // a `stop` behind it in the same buffer reaches this thread first, and
                // clearing any later would drop it.
                _ if starts_search => shared.clear_stop(),
                _ => {}
            }
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // Re-dispatching `stop` and `ponderhit` here is harmless: both are idempotent, and the
    // next `go` resets the signals regardless.
    for line in rx {
        crate::debug_log::record_input(&line);
        if !engine.handle(&line, &mut output) {
            break;
        }
        // Upstream exits the PROCESS from inside the failing command. rfish leaves the loop
        // and lets `main` set the code, so the same shell stays drivable from a test.
        if engine.is_fatal() {
            break;
        }
    }
    engine.is_fatal()
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
        let line = out.rsplit("bestmove ").next().expect("a bestmove line").trim();
        // The line is `<move>` or `<move> ponder <move>`; both moves are plain UCI.
        let mut fields = line.split_whitespace();
        let best = fields.next().expect("a best move");
        assert_eq!(best.len(), 4, "bestmove '{best}' is not a UCI move");
        if let Some(kw) = fields.next() {
            assert_eq!(kw, "ponder");
            let ponder = fields.next().expect("a ponder move after the keyword");
            assert_eq!(ponder.len(), 4, "ponder '{ponder}' is not a UCI move");
        }
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
