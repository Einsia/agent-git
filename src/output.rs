//! A small, swappable output sink for the library.
//!
//! Library modules used to call `println!`/`eprintln!` directly, which writes straight to the process's
//! stdout/stderr and so cannot be observed by a test. Routing every library print through a process-global
//! sink keeps the emitted bytes identical in production — the default sink IS real stdout/stderr, written
//! through the process's shared handles so ordering with any direct `println!` in the binary layer and with
//! inherited-stdio child processes is exactly what `print!`/`println!` would give — while letting a test
//! swap in a buffer and assert on what a command printed.
//!
//! Use the `outln!`/`errln!` (and `out!`/`err!`) macros exactly like `println!`/`eprintln!`/`print!`/`eprint!`.

use std::io::{self, Write};
use std::sync::{Mutex, OnceLock};

/// A process-global writable sink. Boxed so a test can replace the writer with a buffer.
type Sink = Mutex<Box<dyn Write + Send>>;

fn out_sink() -> &'static Sink {
    static OUT: OnceLock<Sink> = OnceLock::new();
    OUT.get_or_init(|| Mutex::new(Box::new(StdoutSink)))
}

fn err_sink() -> &'static Sink {
    static ERR: OnceLock<Sink> = OnceLock::new();
    ERR.get_or_init(|| Mutex::new(Box::new(StderrSink)))
}

/// Default stdout target: write through the process's shared stdout handle (never a private buffer), so
/// ordering with any direct `println!` in the binary layer and with inherited-stdio children is exactly
/// what `print!`/`println!` would give.
struct StdoutSink;
impl Write for StdoutSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::stdout().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

/// Default stderr target — the mirror of [`StdoutSink`] for the error stream (stderr is unbuffered, so a
/// flush is a no-op, matching `eprintln!`).
struct StderrSink;
impl Write for StderrSink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::stderr().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        io::stderr().flush()
    }
}

/// Backing the `out!` macro: write formatted bytes to the stdout sink (no trailing newline), then flush so
/// a no-newline prompt is not left buffered — matching a `print!` followed by an explicit flush.
#[doc(hidden)]
pub fn _out(args: std::fmt::Arguments) {
    let mut sink = out_sink().lock().unwrap_or_else(|e| e.into_inner());
    let _ = sink.write_fmt(args);
    let _ = sink.flush();
}

/// Backing the `outln!` macro: write formatted bytes to the stdout sink, then a newline, then flush —
/// matching `println!` (whose trailing '\n' flushes line-buffered stdout).
#[doc(hidden)]
pub fn _outln(args: std::fmt::Arguments) {
    let mut sink = out_sink().lock().unwrap_or_else(|e| e.into_inner());
    let _ = sink.write_fmt(args);
    let _ = sink.write_all(b"\n");
    let _ = sink.flush();
}

/// Backing the `err!` macro: the stderr counterpart of [`_out`].
#[doc(hidden)]
pub fn _err(args: std::fmt::Arguments) {
    let mut sink = err_sink().lock().unwrap_or_else(|e| e.into_inner());
    let _ = sink.write_fmt(args);
    let _ = sink.flush();
}

/// Backing the `errln!` macro: the stderr counterpart of [`_outln`].
#[doc(hidden)]
pub fn _errln(args: std::fmt::Arguments) {
    let mut sink = err_sink().lock().unwrap_or_else(|e| e.into_inner());
    let _ = sink.write_fmt(args);
    let _ = sink.write_all(b"\n");
    let _ = sink.flush();
}

/// `println!` for library code, routed through the swappable stdout sink.
#[macro_export]
macro_rules! outln {
    () => { $crate::output::_outln(::std::format_args!("")) };
    ($($arg:tt)*) => { $crate::output::_outln(::std::format_args!($($arg)*)) };
}

/// `print!` for library code (no trailing newline), routed through the swappable stdout sink.
#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => { $crate::output::_out(::std::format_args!($($arg)*)) };
}

/// `eprintln!` for library code, routed through the swappable stderr sink.
#[macro_export]
macro_rules! errln {
    () => { $crate::output::_errln(::std::format_args!("")) };
    ($($arg:tt)*) => { $crate::output::_errln(::std::format_args!($($arg)*)) };
}

/// `eprint!` for library code (no trailing newline), routed through the swappable stderr sink.
#[macro_export]
macro_rules! err {
    ($($arg:tt)*) => { $crate::output::_err(::std::format_args!($($arg)*)) };
}

/// Test-only helpers: redirect the library's stdout/stderr sinks into in-memory buffers so a test can
/// assert on what a command printed. Reusable by any unit test in the crate.
#[cfg(test)]
pub(crate) mod testing {
    use super::{err_sink, out_sink, StderrSink, StdoutSink};
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    // The sink is process-global, so two captures must not overlap; serialize them.
    static CAPTURE_LOCK: Mutex<()> = Mutex::new(());

    /// A writer that appends into a shared buffer.
    struct BufSink(Arc<Mutex<Vec<u8>>>);
    impl Write for BufSink {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Redirect the library's stdout+stderr sinks into in-memory buffers for the lifetime of this guard;
    /// the real stdout/stderr sinks are restored on drop. Holds a process-wide lock so captures serialize.
    pub struct Captured {
        _lock: std::sync::MutexGuard<'static, ()>,
        out: Arc<Mutex<Vec<u8>>>,
        err: Arc<Mutex<Vec<u8>>>,
    }

    impl Captured {
        /// Everything written to the stdout sink since capture began.
        pub fn out(&self) -> String {
            String::from_utf8_lossy(&self.out.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
        }
        /// Everything written to the stderr sink since capture began.
        pub fn err(&self) -> String {
            String::from_utf8_lossy(&self.err.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
        }
    }

    impl Drop for Captured {
        fn drop(&mut self) {
            *out_sink().lock().unwrap_or_else(|e| e.into_inner()) = Box::new(StdoutSink);
            *err_sink().lock().unwrap_or_else(|e| e.into_inner()) = Box::new(StderrSink);
        }
    }

    /// Begin capturing library output. Drop the returned guard to restore real stdout/stderr.
    pub fn capture() -> Captured {
        let lock = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        *out_sink().lock().unwrap_or_else(|e| e.into_inner()) = Box::new(BufSink(out.clone()));
        *err_sink().lock().unwrap_or_else(|e| e.into_inner()) = Box::new(BufSink(err.clone()));
        Captured { _lock: lock, out, err }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::capture;

    #[test]
    fn macros_route_to_the_swappable_sink() {
        let cap = capture();
        crate::outln!("hello {}", 42);
        crate::errln!("oops {}", 7);
        crate::out!("tail-no-newline");
        // outln! appends exactly one newline; errln! lands on the stderr buffer, never stdout.
        assert!(cap.out().contains("hello 42\n"));
        assert!(cap.out().contains("tail-no-newline"));
        assert!(cap.err().contains("oops 7\n"));
        assert!(!cap.out().contains("oops 7"));
    }

    #[test]
    fn real_lib_command_output_is_capturable() {
        let cap = capture();
        let code = crate::commands::adapter_list().unwrap();
        assert_eq!(code, 0);
        // Proof a library command's own output now flows through the sink (it was a bare `println!`).
        assert!(cap.out().contains("Registered runtime adapters:\n"), "got: {:?}", cap.out());
    }
}
