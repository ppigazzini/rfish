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
mod options;
mod uci;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());

    if !args.is_empty() {
        // Arguments form ONE command line, so `stockfish bench 16 1 13` works. A second
        // command needs a second invocation, or standard input.
        let mut engine = uci::Engine::new();
        let line = args.join(" ");
        engine.handle(&line, &mut out);
        let _ = out.flush();
        return;
    }

    let _ = writeln!(out, "{} by the Stockfish developers (see AUTHORS)", uci::engine_name());
    let _ = out.flush();

    let stdin = io::stdin();
    uci::run(stdin.lock(), out);
}
