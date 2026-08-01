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
use rfish_engine::search::score;
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
    fn no_moves(&mut self, depth: i32, score: &score::Score) {
        let _ = writeln!(self.out, "info depth {depth} score {}", score.to_uci());
    }

    fn depth_finished(&mut self, r: &DepthReport<'_>) {
        let mut line = format!(
            "info depth {} seldepth {} multipv {} score {}",
            r.depth,
            // NOT `max(depth)`: upstream prints the selective depth as recorded, and a
            // search that never went past ply 1 reports 2 however deep the iteration was.
            // Clamping it up made every iteration report its own depth instead.
            r.sel_depth,
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
        // Unconditional, even when the PV is empty: upstream always writes the token and
        // the separator, so a search that produced no line still ends `... pv ` with the
        // trailing space. Omitting the field entirely is a different line.
        line.push_str(" pv ");
        line.push_str(&r.pv.join(" "));
        let _ = writeln!(self.out, "{line}");
        let _ = self.out.flush();
    }
}

/// The engine session: everything that survives between commands.
pub(crate) struct Engine {
    /// The network lines to print before each search, built when the net is loaded.
    net_report: String,
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
            net_report: String::new(),
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
            "compiler" => Self::cmd_compiler(out),
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
        // `setoption name <words...> value <words...>`. Both halves can contain spaces.
        //
        // Upstream CONSUMES THE FIRST TOKEN UNCONDITIONALLY -- `is >> token; // Consume the
        // "name" token` -- and never checks that it was the word `name`. Everything up to a
        // `value` token is then the name. Searching for the literal `name` instead made
        // `setoption value 64` report an empty option name, where upstream swallows `value`
        // as the keyword and reports `No such option: 64`.
        let rest = args.split_first().map_or(&[][..], |(_, rest)| rest);
        let value_pos = rest.iter().position(|&t| t == "value");
        let (name_end, value) = match value_pos {
            Some(i) => (i, rest[i + 1..].join(" ")),
            None => (rest.len(), String::new()),
        };
        let name = rest[..name_end].join(" ");

        // Upstream reports only an unknown NAME. A value a known option cannot take is
        // ignored in silence -- its `operator=` validates, returns unchanged, and runs no
        // on-change -- so `setoption name Ponder` with no value prints nothing rather than
        // claiming the option does not exist.
        if !self.options.contains(&name) {
            let _ = writeln!(out, "No such option: {name}");
            return;
        }
        if self.options.set(&name, &value) {
            self.apply_option(&name, out);
        }
    }

    /// React to an option whose new value changes engine state.
    ///
    /// Setting an option is not just recording it: `Hash` reallocates, `Threads` resizes
    /// the pool, `EvalFile` reloads the network. An option the engine records but never
    /// acts on is worse than one it does not declare.
    fn apply_option(&mut self, name: &str, out: &mut impl Write) {
        match name.to_ascii_lowercase().as_str() {
            "hash" => self.resize_table(self.options.spin("Hash") as usize),
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
                    let path = self.options.text("SyzygyPath");
                    let reg = TableRegistry::discover(path);
                    // Reported whenever a PATH was given, found or not. Upstream's
                    // `Tablebases::init` ends in an unconditional `TBTables.info()`, and only
                    // an EMPTY path returns before reaching it -- so a path with no tables
                    // behind it prints `Found 0 WDL and 0 DTZ tablebase files (up to 0-man).`
                    // Printing only on success left a GUI that had mistyped the path with no
                    // reply at all, where upstream answers with the count that says why.
                    if !path.is_empty() {
                        let (wdl, dtz) = reg.file_counts();
                        let _ = writeln!(
                            out,
                            "info string Found {wdl} WDL and {dtz} DTZ tablebase files (up to \
                             {}-man).",
                            reg.max_cardinality()
                        );
                    }
                    self.tablebases = (!reg.is_empty()).then(|| Arc::new(reg));
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
    /// What upstream prints before EVERY search, and only before a search.
    ///
    /// Upstream sends these "after the go command is sent for old GUIs and python-chess",
    /// so a session that never searches prints none of them and a `bench` prints them once
    /// per position. rfish used to print them once at startup instead, which differed on
    /// both counts.
    fn report_search_configuration(&self, out: &mut impl Write) {
        self.report_numa_config(out);
        self.report_thread_allocation(out);
        if !self.net_report.is_empty() {
            let _ = writeln!(out, "{}", self.net_report);
        }
    }

    /// Load the network named by `EvalFile`, reporting either way.
    ///
    /// A missing net is NOT an error: rfish runs on the classical scaffolding and says so,
    /// which is what makes every gate above the evaluation runnable before M3 lands.
    pub(crate) fn load_network(&mut self, out: &mut impl Write) {
        let name = self.options.text("EvalFile").to_string();
        self.network = match nnue::find_and_load(&name) {
            Some(Ok(net)) => {
                self.net_report = format!(
                    "info string NNUE evaluation using {} ({})\n\
                     info string Network replica 1: Local memory.",
                    net.name(),
                    net.arch_summary()
                );
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

        // Upstream's own reading, token by token: `startpos` takes the start position and
        // then swallows ONE more token -- the `moves` keyword, if there is one -- while `fen`
        // accumulates until it meets `moves`. Anything else, INCLUDING no argument at all,
        // returns without touching the position.
        //
        // That last branch is the one this got wrong. `position moves e2e4` played the move
        // against the current position and `position` alone reset to the start position,
        // where upstream does nothing in both cases; and `position startpos fen <FEN>` fed
        // the FEN's words to the move parser, which upstream reports as `Illegal move: 8/8/…`
        // rather than quietly setting the position.
        let (fen, move_tokens) = match args.first().copied() {
            Some("startpos") => (START_FEN.to_string(), args.get(2..).unwrap_or(&[])),
            Some("fen") => {
                let tail = &args[1..];
                let end = tail.iter().position(|&t| t == "moves");
                let fen = tail[..end.unwrap_or(tail.len())].join(" ");
                (fen, end.map_or(&[][..], |i| &tail[i + 1..]))
            }
            _ => return,
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

        {
            for token in move_tokens {
                if let Some(m) = parse_uci_move(&self.pos, token) {
                    self.pos.do_move(m);
                } else {
                    // Upstream terminates here as it does for a bad FEN: a position it
                    // cannot construct is a CRITICAL ERROR and exit 1, not a note the GUI
                    // can ignore while the engine keeps the half-applied position.
                    self.critical(&format!("Illegal move: {token}"), out);
                    return;
                }
            }
        }
    }

    fn cmd_go(&mut self, args: &[&str], out: &mut impl Write) {
        // Upstream prints these before dispatching `go`, including `go perft`.
        self.report_search_configuration(out);
        // With no reader thread ahead of this call, here is the earliest point that observes
        // the command, so here is where the previous search's stop is dropped.
        if !self.reader_owns_stop {
            self.pool.shared().clear_stop();
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
        // `if (limits.perft)`, exactly as upstream dispatches it: a NON-ZERO perft is a
        // movegen command answered here, and everything else -- including `go perft 0` -- is
        // an ordinary search. Reading the token's mere presence as "this is a perft" made
        // `go perft 0` print a depth-zero divide where upstream searches without a limit.
        if limits.perft != 0 {
            let (moves, total) = perft_divide(&mut self.pos, limits.perft);
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
            // AT UPSTREAM'S OWN WIDTH, key by key. `is >> x` sets failbit when the text does
            // not fit the field's type, and upstream turns failbit into a critical error, so
            // the width IS the accept/reject boundary and it is observable from a GUI:
            //
            //   go depth 3000000000    upstream: CRITICAL ERROR   (`int depth` overflows)
            //   go nodes 18446744073709551615
            //                          upstream: accepted         (`u64 nodes` holds it)
            //
            // Parsing everything at one width got both of those wrong, in opposite
            // directions: too wide for the five `int` fields, too narrow for `nodes`.
            let clock = |i: usize, key: &str| -> Result<i64, String> {
                // `TimePoint` is a 64-bit signed count of milliseconds.
                args.get(i + 1)
                    .and_then(|s| s.parse::<i64>().ok())
                    .ok_or_else(|| format!("Invalid argument for '{key}'"))
            };
            let count = |i: usize, key: &str| -> Result<i32, String> {
                // `int`, and negative values ARE accepted: `movestogo -5` searches there.
                args.get(i + 1)
                    .and_then(|s| s.parse::<i32>().ok())
                    .ok_or_else(|| format!("Invalid argument for '{key}'"))
            };
            let nodes = |i: usize, key: &str| -> Result<u64, String> {
                // `u64`, read the way a C++ stream reads one: a leading minus is accepted and
                // the magnitude WRAPS, so `go nodes -1` is a budget of u64::MAX on both sides.
                let text =
                    args.get(i + 1).ok_or_else(|| format!("Invalid argument for '{key}'"))?;
                let parsed = match text.strip_prefix('-') {
                    Some(magnitude) => magnitude.parse::<u64>().ok().map(u64::wrapping_neg),
                    None => text.parse::<u64>().ok(),
                };
                parsed.ok_or_else(|| format!("Invalid argument for '{key}'"))
            };
            match args[i] {
                "wtime" => l.time[Color::White.index()] = Some(clock(i, "wtime")? as u64),
                "btime" => l.time[Color::Black.index()] = Some(clock(i, "btime")? as u64),
                "winc" => l.inc[Color::White.index()] = clock(i, "winc")? as u64,
                "binc" => l.inc[Color::Black.index()] = clock(i, "binc")? as u64,
                "movestogo" => l.moves_to_go = Some(count(i, "movestogo")? as u32),
                "depth" => l.depth = Some(count(i, "depth")?),
                "nodes" => l.nodes = Some(nodes(i, "nodes")?),
                "movetime" => l.move_time = Some(clock(i, "movetime")? as u64),
                "mate" => l.mate = Some(count(i, "mate")?),
                // `perft` takes a value like every other key, and upstream's `is >> perft`
                // sets failbit on a missing or unusable one just as `depth` does. It is
                // parsed HERE rather than where it is acted on so that it is rejected on the
                // same terms.
                "perft" => l.perft = count(i, "perft")?,
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
        // A limit of ZERO means ABSENT, because upstream's fields are plain integers and
        // every test of them is a C++ truthiness check: `if (limits.depth)`,
        // `if (limits.nodes)`, `if (limits.movetime)`. `go nodes 0` therefore searches
        // WITHOUT a node limit there, where an `Option` faithfully carrying `Some(0)` stops
        // the search before it starts.
        if l.depth == Some(0) {
            l.depth = None;
        }
        if l.nodes == Some(0) {
            l.nodes = None;
        }
        if l.move_time == Some(0) {
            l.move_time = None;
        }
        if l.mate == Some(0) {
            l.mate = None;
        }
        Ok(l)
    }

    /// Resize the transposition table, failing the way upstream fails.
    ///
    /// A `Hash` the machine cannot provide is not a bug in the value -- it is inside the
    /// option's declared range -- so upstream reports it on standard error and exits 1
    /// rather than dying. rfish aborted with a core dump before this existed.
    fn resize_table(&mut self, mb: usize) {
        if !self.tt.resize(mb) {
            eprintln!("Failed to allocate {mb}MB for transposition table.");
            std::process::exit(1);
        }
    }

    /// Upstream's `compiler` block: four aligned fields between two blank lines.
    ///
    /// The CONTENT is necessarily this port's -- rustc built it, not g++ -- but the shape is
    /// upstream's, because a bug report pastes this verbatim and a reader should not have to
    /// learn a second layout to read it.
    fn cmd_compiler(out: &mut impl Write) {
        let os = match std::env::consts::OS {
            "linux" => "Linux",
            "macos" => "Apple",
            "windows" => "Windows",
            other => other,
        };
        // The feature list upstream prints, in upstream's order, for the ones rustc exposes.
        let mut settings = Vec::new();
        if cfg!(target_pointer_width = "64") {
            settings.push("64bit");
        }
        for (on, name) in [
            (cfg!(target_feature = "avx512f"), "AVX512F"),
            (cfg!(target_feature = "avx2"), "AVX2"),
            (cfg!(target_feature = "bmi2"), "BMI2"),
            (cfg!(target_feature = "sse4.1"), "SSE41"),
            (cfg!(target_feature = "ssse3"), "SSSE3"),
            (cfg!(target_feature = "sse2"), "SSE2"),
            (cfg!(target_feature = "popcnt"), "POPCNT"),
        ] {
            if on {
                settings.push(name);
            }
        }
        // `rustc 1.99.0-nightly (abc 2026-07-29)` -> `1.99.0-nightly`, which is what
        // upstream's `__VERSION__` field carries: the version and nothing else.
        let version = env!("RFISH_RUSTC").split_whitespace().nth(1).unwrap_or("unknown");

        let _ = writeln!(out);
        let _ = writeln!(out, "Compiled by                : rustc {version} on {os}");
        let _ = writeln!(out, "Compilation architecture   : {}", env!("RFISH_TARGET_CPU"));
        let _ = writeln!(out, "Compilation settings       : {}", settings.join(" "));
        let _ = writeln!(out, "Compiler __VERSION__ macro : {version}");
        let _ = writeln!(out);
    }

    fn cmd_eval(&self, out: &mut impl Write) {
        // Upstream's `Eval::trace`, line for line. It prints NO board -- `d` is the command
        // for that -- and a position in check is not evaluated at all, because the network
        // is trained on quiet positions and upstream asserts rather than answering.
        // The leading blank line belongs to the COMMAND, not to the trace: upstream is
        // `sync_cout << "\n" << Eval::trace(...)`, so it is printed whether or not the trace
        // itself has anything to say. The two early returns below omitted it, so `eval` on a
        // position in check came out one line shorter here than upstream prints it.
        if self.pos.in_check() {
            let _ = writeln!(out, "\nFinal evaluation: none (in check)");
            return;
        }
        let Some(net) = self.network.as_deref() else {
            let _ = writeln!(out, "\nFinal evaluation: none (no network)");
            return;
        };
        let mut scratch = nnue::Scratch::default();

        // The bucket table: what every output head would have said, and a marker on the one
        // the piece count selects.
        let t = net.trace_evaluate(&self.pos, &mut scratch);
        let side = if self.pos.side_to_move() == Color::White { "White" } else { "Black" };
        let rule = "+------------+------------+------------+------------+";
        let _ = writeln!(out, "\n\nNNUE network contributions (Normalized, {side} to move)");
        let _ = writeln!(out, "{rule}");
        let _ = writeln!(out, "|   Bucket   |  Material  | Positional |   Total    |");
        let _ = writeln!(out, "|            |   (PSQT)   |  (Layers)  |            |");
        let _ = writeln!(out, "{rule}");
        for b in 0..t.psqt.len() {
            let (psqt, positional) = (t.psqt[b], t.positional[b]);
            let mut row = format!("|  {b}         |  ");
            row.push_str(&self.aligned_dot(psqt));
            row.push_str("   |  ");
            row.push_str(&self.aligned_dot(positional));
            row.push_str("   |  ");
            row.push_str(&self.aligned_dot(psqt + positional));
            row.push_str("   |");
            if b == t.correct_bucket {
                row.push_str(" <-- this bucket is used");
            }
            let _ = writeln!(out, "{row}");
        }
        let _ = writeln!(out, "{rule}\n");

        let raw = net.evaluate(&self.pos, &mut scratch);
        let v = raw.psqt + raw.positional;
        let _ = writeln!(out, "NNUE evaluation          {v:+} (side to move, internal units)");
        let white = if self.pos.side_to_move() == Color::White { v } else { -v };
        let _ = writeln!(
            out,
            "NNUE evaluation        {:+.2} (white side)",
            0.01 * f64::from(score::to_cp(white, &self.pos))
        );

        let full = rfish_engine::eval::evaluate(&self.pos, Some(net), &mut scratch, 0);
        let white = if self.pos.side_to_move() == Color::White { full } else { -full };
        let _ = writeln!(
            out,
            "Final evaluation      {:+.2} (white side) [with scaled NNUE, ...]\n",
            0.01 * f64::from(score::to_cp(white, &self.pos))
        );
    }

    /// One cell of the bucket table: upstream's sign character, then six columns of pawns.
    ///
    /// The sign is written separately from the magnitude because upstream prints the sign of
    /// the raw value and the ABSOLUTE value beside it, so a zero shows as a space rather
    /// than as `+0.00`.
    fn aligned_dot(&self, v: rfish_engine::board::types::Value) -> String {
        let sign = match v {
            v if v < 0 => '-',
            v if v > 0 => '+',
            _ => ' ',
        };
        let pawns = (0.01 * f64::from(score::to_cp(v, &self.pos))).abs();
        format!("{sign}{pawns:6.2}")
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
        self.resize_table(spec.hash_mb);
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
                    // Upstream drives each bench position through `go`, so it announces once
                    // per position rather than once per run.
                    self.report_search_configuration(out);
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
                    // The position AS IT WILL BE SEARCHED, which is upstream's `engine.fen()`
                    // -- read after the entry's moves have been played, not the FEN the entry
                    // was written with. Twelve of the fifty-one bench entries carry moves, and
                    // for each of those this line named a position that is not the one the
                    // node count below it belongs to.
                    let _ =
                        writeln!(out, "\nPosition: {searched}/{total_positions} ({})", pos.fen());

                    // Upstream builds ONE command per position out of the limit type --
                    // `go = limitType == "eval" ? "eval" : "go " + limitType + " " + limit`
                    // -- so `eval` and `perft` are not searches at all. Mapping every unknown
                    // type onto `depth` ran a depth search for both: `bench 16 1 4 default
                    // eval` searched where upstream traces the evaluation, and
                    // `bench 16 1 3 default perft` searched where upstream counts moves.
                    if spec.limit_kind == "eval" {
                        // The position under evaluation is the shell's own, because that is
                        // what `engine.trace_eval()` reads.
                        let saved = core::mem::replace(&mut self.pos, pos);
                        self.cmd_eval(out);
                        self.pos = saved;
                        continue;
                    }
                    if spec.limit_kind == "perft" {
                        let (moves, total) = perft_divide(&mut pos, spec.limit);
                        for (m, n) in moves {
                            let _ = writeln!(out, "{}: {n}", move_to_uci(&pos, m));
                        }
                        let _ = writeln!(out, "\nNodes searched: {total}\n");
                        total_nodes += total;
                        continue;
                    }

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
            // Named against the ROOT, as upstream names it: `UCIEngine::move(pv[1],
            // rootPos.is_chess960())`. This cloned the position and played the best move
            // first, on the belief that the ponder move's castling notation is only defined
            // after it -- but `move_to_uci` reads exactly one thing off the position, whether
            // the game is Chess960, and that is a property of the GAME. The clone and the
            // `do_move` bought nothing and cost 49 of each over one `bench 16 1 8`, which is
            // what `cargo xtask fingerprint` measured against upstream's zero.
            let _ = writeln!(out, "bestmove {best} ponder {}", move_to_uci(pos, p));
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

    /// `eval` is upstream's `Eval::trace`, and it needs the network to say anything.
    ///
    /// A run without a net is not a failure, but it is a different engine and the output
    /// has to say so rather than printing a number from the fallback as though the network
    /// had produced it. Upstream never reaches this case -- its net is embedded -- so there
    /// is no upstream text to match, only a refusal to invent one.
    #[test]
    fn eval_without_a_network_says_so_rather_than_inventing_a_number() {
        let out = drive(&["position startpos", "eval"]);
        assert!(out.contains("Final evaluation: none (no network)"), "{out}");
    }

    /// In check, upstream declines to evaluate at all, and says which.
    #[test]
    fn eval_in_check_is_declined_the_way_upstream_declines_it() {
        let out = drive(&["position fen 4k3/8/8/8/8/8/8/4R1K1 b - - 0 1", "eval"]);
        assert!(out.contains("Final evaluation: none (in check)"), "{out}");
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
