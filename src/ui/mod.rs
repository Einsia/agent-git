//! Terminal rendering.
//!
//! # tty awareness is this layer's core responsibility
//!
//! The output of one command has two consumers: the person sitting at the terminal, and the
//! script at the other end of the pipe. The first wants color and alignment, the second wants
//! stable, greppable plain text. Every rendering function splits on `is_tty()`, so callers do
//! not have to care.
//!
//! This degradation is not polish — it decides whether agit can be consumed by a script.

pub mod prompt;
pub mod session;
pub mod table;
pub mod theme;
pub mod transcript;

use owo_colors::OwoColorize;
use std::io::IsTerminal;

/// Whether stdout is attached to a terminal. It tests stdout and not stderr — hints go to
/// stderr, which stays colored while stdout is redirected.
#[must_use = "this is a query function; dropping the answer is the same as not asking"]
pub fn is_tty() -> bool {
    std::io::stdout().is_terminal()
}

/// Whether color is enabled. Beyond the tty test it honors the `NO_COLOR` convention.
#[must_use = "this is a query function; dropping the answer is the same as not asking"]
pub fn color_enabled() -> bool {
    is_tty() && std::env::var_os("NO_COLOR").is_none()
}

macro_rules! colorize {
    ($name:ident, $method:ident) => {
        /// Colors and **returns** — it does not print. To get output, hand the result to
        /// `println!`/`eprintln!`, or use `success` / `error` / `warning` / `hint` instead
        /// (those are the ones that write to a stream).
        ///
        /// `#[must_use]` has the compiler catch "the return value was dropped" in every
        /// syntactic form — `if c { dim("x"); }`, inside a match arm, after macro expansion,
        /// all go red. A source-scanning test only recognizes the spellings whoever wrote it
        /// thought of.
        ///
        /// One known boundary: pass the function **as a value** (`let d = dim; d("x");`) and
        /// the attribute is gone, the compiler stops caring. Confirmed in practice. A coloring
        /// function has no reason to be passed that way, so nothing else guards it — just do
        /// not take this gate for airtight.
        #[must_use = "this is a coloring function; it returns a string and prints nothing, so dropping it does nothing"]
        pub fn $name(s: &str) -> String {
            if color_enabled() {
                s.$method().to_string()
            } else {
                s.to_string()
            }
        }
    };
}

colorize!(accent, cyan);
colorize!(dim, bright_black);
colorize!(bold, bold);
colorize!(ok, green);
colorize!(warn_text, yellow);
colorize!(err_text, red);

colorize!(transient, bright_magenta);

/// Errors go to stderr — there they stay visible while `agit log > out.txt` redirects stdout.
pub fn error(msg: &str) {
    eprintln!("{} {msg}", err_text("error"));
}

pub fn warning(msg: &str) {
    eprintln!("{} {msg}", warn_text("note"));
}

/// The next-step hint. Cargo's transient/help style is applied to the complete line so the
/// message remains legible even when a command is not wrapped in backticks.
pub fn hint(msg: &str) {
    eprintln!("{}", transient(&format!("  → {msg}")));
}

pub fn success(msg: &str) {
    println!("{} {msg}", ok(theme::symbols().check));
}

pub fn section(title: &str) {
    if is_tty() {
        println!("\n{}", bold(title));
    } else {
        println!("\n=== {title} ===");
    }
}

/// "How long ago". A relative time scans faster than an absolute timestamp — what the user
/// cares about is "just now or last week".
#[must_use = "this is a formatting function; it returns a string and prints nothing"]
pub fn ago(t: std::time::SystemTime) -> String {
    let Ok(d) = t.elapsed() else {
        // A time in the future (a clock stepped backwards, or an mtime from another machine) —
        // do not pretend to know.
        return "just now".into();
    };
    match d.as_secs() {
        s @ 0..=59 => format!("{s}s ago"),
        s @ 60..=3599 => format!("{}m ago", s / 60),
        s @ 3600..=86399 => format!("{}h ago", s / 3600),
        s @ 86400..=2591999 => format!("{}d ago", s / 86400),
        s => format!("{}mo ago", s / 2592000),
    }
}

/// Replaces $HOME with `~` to keep the output short.
#[must_use = "this is a path-shortening function; it returns a string and prints nothing"]
pub fn tilde(p: &std::path::Path) -> String {
    let s = p.to_string_lossy().to_string();
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
        && s.starts_with(&home)
    {
        return format!("~{}", &s[home.len()..]);
    }
    s
}

/// Truncates to the given number of characters.
///
/// It counts `chars()` and not bytes — transcripts hold plenty of Chinese, and cutting on bytes
/// garbles the text.
#[must_use = "this is a truncation function; it returns a string and prints nothing"]
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// A progress indicator that goes silent by itself off a tty.
///
/// An expensive operation (scanning the runtime directories) with no progress indicator looks to
/// the user like the program has hung.
#[must_use = "a progress bar spins only while it is held — dropping it ends it immediately"]
pub fn spinner(msg: &str) -> indicatif::ProgressBar {
    if !is_tty() {
        return indicatif::ProgressBar::hidden();
    }
    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(
        indicatif::ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap_or_else(|_| indicatif::ProgressStyle::default_spinner()),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(100));
    pb
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn truncate_counts_chars_not_bytes() {
        // CJK byte/char fixture: the literal stays Chinese so the two counts differ.
        let s = "重构错误处理层"; // 7 CJK characters, 21 bytes
        let t = truncate(s, 4);
        assert_eq!(t.chars().count(), 4, "3 chars + the ellipsis: {t}");
        assert!(t.ends_with('…'));
        assert_eq!(
            truncate("abc", 3),
            "abc",
            "a string exactly at the cap is not truncated"
        );
    }

    #[test]
    fn ago_buckets() {
        use std::time::{Duration, SystemTime};
        let now = SystemTime::now();
        assert!(ago(now).contains("s ago"));
        assert!(ago(now - Duration::from_secs(120)).contains("m ago"));
        assert!(ago(now - Duration::from_secs(7200)).contains("h ago"));
        assert!(ago(now - Duration::from_secs(172800)).contains("d ago"));
    }

    #[test]
    fn tilde_shortens_home_only() {
        assert_eq!(tilde(std::path::Path::new("/etc/hosts")), "/etc/hosts");
        if let Ok(h) = std::env::var("HOME")
            && !h.is_empty()
        {
            let p = std::path::PathBuf::from(&h).join("proj");
            assert_eq!(tilde(&p), "~/proj");
        }
    }
}
