//! The `Debug Log File` option: a transcript of everything the engine read and wrote.
//!
//! When a GUI and an engine disagree about what was said, the only settlement is a record
//! of what actually crossed the pipe. That is what this writes, in upstream's format: every
//! line the engine READ is prefixed `>> `, every line it WROTE is prefixed `<< `, both in
//! the order they happened.
//!
//! # Why a global
//!
//! The option can be turned on and off mid-session, from a `setoption` handled deep inside
//! the command dispatch, while the thing being logged is the output stream the dispatch is
//! writing to. Threading a handle from the option down to the writer and back would put a
//! borrow across the whole shell for a feature that is off by default. A single mutex-
//! guarded file, opened and closed by the option, is the smaller construct.
//!
//! Golden: `Stockfish/src/misc.cpp`, `Logger` and `Tie`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::Mutex;

/// The open transcript, or `None` when logging is off.
static LOG: Mutex<Option<BufWriter<File>>> = Mutex::new(None);

/// Point the transcript at `path`, or turn it off when `path` is empty.
///
/// Returns the error when the file cannot be opened, so the shell can say so rather than
/// silently logging nowhere — a debug feature that fails quietly is worse than one that is
/// off, because the operator believes they have a record.
pub(crate) fn set_path(path: &str) -> std::io::Result<()> {
    let mut guard = LOG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // Flush and drop the previous file first, so switching paths mid-session leaves a
    // complete transcript behind rather than a truncated one.
    if let Some(mut old) = guard.take() {
        let _ = old.flush();
    }
    if path.is_empty() {
        return Ok(());
    }
    *guard = Some(BufWriter::new(File::create(path)?));
    Ok(())
}

/// True when a transcript is open. Lets the caller skip the formatting work when it is not.
pub(crate) fn is_active() -> bool {
    LOG.lock().unwrap_or_else(std::sync::PoisonError::into_inner).is_some()
}

/// Record one line, with its direction prefix.
fn record(prefix: &str, line: &str) {
    let mut guard = LOG.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(f) = guard.as_mut() {
        let _ = writeln!(f, "{prefix}{line}");
        // Flushed per line on purpose. A transcript exists to survive the crash that is
        // being investigated, and a buffered tail is exactly the part that would be lost.
        let _ = f.flush();
    }
}

/// Record a line the engine READ.
pub(crate) fn record_input(line: &str) {
    record(">> ", line);
}

/// A writer that mirrors everything written through it into the transcript.
///
/// Splits on newlines so each logged line gets its own prefix, and holds a partial line
/// until its terminator arrives — the engine writes some lines in several calls.
pub(crate) struct TeeWriter<W: Write> {
    inner: W,
    pending: String,
}

impl<W: Write> TeeWriter<W> {
    pub(crate) fn new(inner: W) -> TeeWriter<W> {
        TeeWriter { inner, pending: String::new() }
    }
}

impl<W: Write> Write for TeeWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        if is_active() {
            // Lossy on purpose: the transcript is a diagnostic, and a byte sequence that is
            // not UTF-8 must not cost the engine its output.
            self.pending.push_str(&String::from_utf8_lossy(&buf[..n]));
            while let Some(i) = self.pending.find('\n') {
                let line: String = self.pending.drain(..=i).collect();
                record("<< ", line.trim_end_matches(['\n', '\r']));
            }
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise the tests in this module against each other.
    ///
    /// `LOG` is one global for the whole process and libtest runs these on parallel threads,
    /// so without this a `set_path` in one test closes the transcript another is midway
    /// through writing — which reads as a truncated file rather than as a race, and fails on
    /// whichever runner lost the timing that day. Ignore poison: a panic in one test must
    /// report ITS assertion, not resurface as a lock error in the next.
    static SERIAL: Mutex<()> = Mutex::new(());

    /// The transcript records both directions, in order, with upstream's prefixes.
    #[test]
    fn a_transcript_records_both_directions_in_order() {
        let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = std::env::temp_dir().join(format!("rfish-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        let path = dir.join("transcript.txt");
        let path_str = path.to_str().expect("a utf-8 path");

        set_path(path_str).expect("the log opens");
        record_input("uci");
        {
            let mut w = TeeWriter::new(Vec::new());
            writeln!(w, "uciok").expect("write");
            // A line delivered in pieces still logs as one line.
            write!(w, "read").expect("write");
            writeln!(w, "yok").expect("write");
        }
        record_input("quit");
        set_path("").expect("the log closes");

        let text = std::fs::read_to_string(&path).expect("the transcript exists");
        assert_eq!(text, ">> uci\n<< uciok\n<< readyok\n>> quit\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Turning it off must stop recording, and must not error.
    #[test]
    fn an_empty_path_turns_the_transcript_off() {
        let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        set_path("").expect("closing an unopened log is fine");
        assert!(!is_active());
        record_input("this goes nowhere");
    }

    /// A path that cannot be opened is reported, not swallowed.
    #[test]
    fn an_unopenable_path_is_an_error() {
        let _serial = SERIAL.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(set_path("/this/directory/does/not/exist/log.txt").is_err());
        // And the failure leaves logging OFF rather than half-configured.
        assert!(!is_active());
    }
}
