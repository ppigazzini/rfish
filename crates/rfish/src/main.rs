//! The rfish binary: a UCI chess engine.
//!
//! The binary is named `stockfish`, not `rfish`. Every GUI, every test harness and
//! fishtest itself invoke a UCI engine by that name, and a port that renames it stops being
//! a drop-in replacement for the thing it reproduces.
//!
//! Command-line arguments are treated as a UCI command and executed, then the process
//! exits — so `stockfish bench` and `echo bench | stockfish` do the same thing, which is
//! what every measurement harness relies on.

use std::io::{self, BufWriter, Write};

mod bench;
mod debug_log;
mod options;
mod speedtest;
mod uci;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    if !args.is_empty() {
        // Arguments form ONE command line, so `stockfish bench 16 1 13` works. A second
        // command needs a second invocation, or standard input.
        // The network is loaded here too, not only on the interactive path. Forgetting it
        // makes `stockfish bench` measure the classical fallback while reporting a node
        // count that looks entirely plausible -- exactly the kind of wrong number an anchor
        // must never be derived from.
        let mut out = debug_log::TeeWriter::new(out);
        let mut engine = uci::Engine::new();
        // The net is loaded here but NOT announced: upstream announces before a
        // search, so a session that never searches announces nothing.
        engine.load_network(&mut out);
        let line = args.join(" ");
        debug_log::record_input(&line);
        engine.handle(&line, &mut out);
        let _ = out.flush();
        // A command that failed fatally leaves with upstream's status, not with success.
        if engine.is_fatal() {
            std::process::exit(1);
        }
        return;
    }

    let _ = writeln!(out, "{} by the Stockfish developers (see AUTHORS)", uci::engine_name());
    let _ = out.flush();

    // NOT `stdin.lock()`: the reader is moved to its own thread, and a lock guard is not
    // `Send`. Nothing else in this process reads stdin, so there is no lock to hold.
    if uci::run(io::BufReader::new(io::stdin()), out) {
        std::process::exit(1);
    }
}
