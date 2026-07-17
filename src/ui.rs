//! Presentation primitives: ANSI, box drawing, numbered pickers — and plain text everywhere else.
//!
//! Not a TUI, deliberately (§11c). agit is git-shaped, so it must stay pipeable and scriptable: nothing
//! here takes over the screen, nothing repaints, and everything degrades to plain text when stdout is
//! not a terminal or `NO_COLOR` is set.
//!
//! Two rules the rest of the codebase depends on:
//!   * **Colour is emphasis only.** Every line must read the same with the escapes stripped — a pipe, a
//!     NO_COLOR terminal and a screen reader all get the words, and lose only the emphasis. A line that
//!     is red because something is wrong must also SAY what is wrong.
//!   * **Anything interactive has a non-interactive counterpart** that exits non-zero and prints what
//!     needed deciding. A prompt that blocks when stdin is not a TTY hangs CI forever.

use anyhow::{bail, Result};
use std::io::{stdin, stdout, BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const DIM: &str = "\x1b[2m";
const BOLD: &str = "\x1b[1m";
const ACCENT: &str = "\x1b[36m";
const WARN: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Whether stdout gets ANSI, as a pure function of what decides it. Pure because the alternative is
/// `std::env::set_var` in tests, which is process-global and races every other test in this binary
/// (the reason `scope::agit_home_from` is shaped this way too).
///
/// `no_color` follows the NO_COLOR standard: present **and non-empty** disables colour, regardless of
/// its value. An empty `NO_COLOR=` deliberately does NOT — that carve-out is what lets someone who
/// exports `NO_COLOR` in their shell profile re-enable colour for one command. Matches `anstyle-query`
/// (the implementation clap pulls in), which tests exactly this case.
pub fn styled_from(is_tty: bool, no_color: Option<&str>) -> bool {
    is_tty && !no_color.map(|v| !v.is_empty()).unwrap_or(false)
}

fn no_color() -> Option<String> {
    // var_os, not var: a non-UTF8 NO_COLOR is still present and non-empty, and must still disable.
    std::env::var_os("NO_COLOR").map(|v| v.to_string_lossy().into_owned())
}

/// Whether stdout gets ANSI.
pub fn styled() -> bool {
    styled_from(stdout().is_terminal(), no_color().as_deref())
}

/// Whether stderr gets ANSI. Progress (spinners) goes to stderr so stdout stays pipeable, and the two
/// streams are redirected independently — `agit … > out.txt` leaves stderr a terminal.
pub fn styled_err() -> bool {
    styled_from(std::io::stderr().is_terminal(), no_color().as_deref())
}

/// The pure half of every helper below: the `on` decision comes in, so a test never depends on whether
/// the harness happened to capture stdout. (`cargo test -- --nocapture` from a terminal makes the test's
/// stdout a real TTY — tests that called `styled()` transitively passed only by accident of capture.)
fn paint_with(on: bool, code: &str, s: &str) -> String {
    if on {
        format!("{code}{s}{RESET}")
    } else {
        s.to_string()
    }
}

pub fn dim(s: &str) -> String {
    paint_with(styled(), DIM, s)
}

pub fn bold(s: &str) -> String {
    paint_with(styled(), BOLD, s)
}

pub fn accent(s: &str) -> String {
    paint_with(styled(), ACCENT, s)
}

/// Emphasis for a line that reports trouble. The words must carry the meaning on their own: this only
/// makes them easier to find.
pub fn warn(s: &str) -> String {
    paint_with(styled(), WARN, s)
}

/// Printable width: what a terminal shows, so the ANSI a caller already applied doesn't skew a column.
fn display_width(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for e in chars.by_ref() {
                if e == 'm' {
                    break;
                }
            }
            continue;
        }
        n += 1;
    }
    n
}

/// A plain aligned table: column widths follow the CONTENT, headers included, and the last column is
/// never padded (trailing spaces are noise in a pipe and in a test).
///
/// Pass `&[]` for headers to align rows without a header line — the numbered pickers do that.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    table_with(styled(), headers, rows)
}

pub fn table_with(styled: bool, headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len().max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if cols == 0 {
        return String::new();
    }
    let mut w = vec![0usize; cols];
    for (i, h) in headers.iter().enumerate() {
        w[i] = display_width(h);
    }
    for r in rows {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(display_width(c));
        }
    }
    let mut lines: Vec<String> = Vec::new();
    if !headers.is_empty() {
        let head: Vec<String> = headers.iter().map(|h| h.to_string()).collect();
        lines.push(row(&head, &w, styled));
    }
    for r in rows {
        lines.push(row(r, &w, false));
    }
    lines.join("\n")
}

fn row(cells: &[String], w: &[usize], dim_it: bool) -> String {
    let mut out = String::new();
    for (i, c) in cells.iter().enumerate() {
        out.push_str(&paint_with(dim_it, DIM, c));
        if i + 1 != cells.len() {
            let pad = w[i].saturating_sub(display_width(c)) + 2;
            out.push_str(&" ".repeat(pad));
        }
    }
    out.trim_end().to_string()
}

/// Roughly how long ago. Precision past "2h ago" is noise in a header.
pub fn ago(t: SystemTime) -> String {
    match SystemTime::now().duration_since(t) {
        Ok(d) => ago_secs(d.as_secs()),
        Err(_) => "just now".into(),
    }
}

/// The pure core of `ago` — "now" is the untestable half, so it stays out.
pub fn ago_secs(s: u64) -> String {
    match s {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

/// `$HOME/code/web` → `~/code/web`. A header is for reading, and the prefix is the same on every line.
pub fn tilde(p: &Path) -> String {
    tilde_from(p, std::env::var("HOME").ok().as_deref())
}

pub fn tilde_from(p: &Path, home: Option<&str>) -> String {
    let s = p.to_string_lossy().into_owned();
    let Some(home) = home.map(str::trim).filter(|h| !h.is_empty() && *h != "/") else { return s };
    match s.strip_prefix(home) {
        // Only at a path BOUNDARY: `/home/joe-backup` does not live under `/home/joe`.
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => s,
    }
}

/// One line, whitespace collapsed, cut to `max` — for an excerpt shown inside a header.
pub fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

// ── pickers ──
//
// Both take their options pre-rendered: what a choice needs in order to be informed differs per call
// site (an agent row wants its aid and session count; a conflict wants the two field names), and none
// of that belongs in here.

/// Are we able to ask? stdin, not stdout: a prompt is answered by a keyboard, and `agit … | less` must
/// still be able to ask. Colour and interactivity are separate questions.
pub fn interactive() -> bool {
    stdin().is_terminal()
}

fn read_choice() -> Option<String> {
    let mut line = String::new();
    (stdin().lock().read_line(&mut line).ok()? > 0).then(|| line.trim().to_string())
}

fn choice_hint(n: usize) -> String {
    (1..=n).map(|i| i.to_string()).collect::<Vec<_>>().join("/")
}

/// The non-interactive counterpart, shared by both pickers: never block, exit non-zero, and print what
/// needed deciding along with every option it could have been.
fn cannot_ask(what: &str, options: &[String], fix: &str) -> anyhow::Error {
    let list =
        options.iter().enumerate().map(|(i, o)| format!("  {}) {o}", i + 1)).collect::<Vec<_>>().join("\n");
    anyhow::anyhow!("{what}\n{list}\n{fix}")
}

/// A numbered picker. Returns the chosen index (0-based).
pub fn pick(prompt: &str, options: &[String], fix: &str) -> Result<usize> {
    if options.is_empty() {
        bail!("nothing to pick from");
    }
    if !interactive() {
        return Err(cannot_ask(prompt, options, fix));
    }
    print!("{}", pick_render(styled(), prompt, options));
    let _ = stdout().flush();
    match read_choice().and_then(|c| c.parse::<usize>().ok()) {
        Some(n) if n >= 1 && n <= options.len() => Ok(n - 1),
        _ => bail!("nothing picked. {fix}"),
    }
}

/// The picker's rendering, split from the half that reads stdin so the shape is testable in both modes.
fn pick_render(styled: bool, prompt: &str, options: &[String]) -> String {
    let mut s = format!("{}\n", paint_with(styled, BOLD, prompt));
    for (i, o) in options.iter().enumerate() {
        s.push_str(&format!("  {}) {o}\n", paint_with(styled, ACCENT, &(i + 1).to_string())));
    }
    s.push_str(&format!("Pick [{}]: ", choice_hint(options.len())));
    s
}

/// What a boxed picker came back with: a listed option, something the user typed instead, or nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum Choice {
    Option(usize),
    Typed(String),
    None,
}

/// A conflict-shaped picker: a titled rule, the context that makes the choice informed, numbered
/// options, and the question on the closing edge (§11c).
///
/// ```text
/// ┌ conflict 1/2 ─────────────────────────────────
/// │ field name: user_id (frontend) vs uid (api)
/// │   1) keep uid, frontend updates its caller
/// │   2) keep user_id, api reverts the rename
/// │   3) leave open, decide later
/// └ your call [1/2/3]:
/// ```
/// Anything typed that is not one of the numbers is taken as the decision itself — the options are a
/// shortcut, never the whole vocabulary.
///
/// Refuses outright when there is nobody to ask, so this can never become the thing that hangs a
/// pipeline. Callers own the non-interactive counterpart (they know how to print what needed deciding);
/// this only guarantees the primitive itself never blocks and never silently decides "nothing".
pub fn pick_boxed(title: &str, context: &str, options: &[String], question: &str) -> Result<Choice> {
    if !interactive() {
        return Err(cannot_ask(&format!("{title}: {context}"), options, "Rerun at a terminal to answer."));
    }
    print!("{}", box_render(styled(), title, context, options, question));
    let _ = stdout().flush();
    let Some(answer) = read_choice() else { return Ok(Choice::None) };
    if answer.is_empty() {
        return Ok(Choice::None);
    }
    match answer.parse::<usize>() {
        Ok(n) if n >= 1 && n <= options.len() => Ok(Choice::Option(n - 1)),
        _ => Ok(Choice::Typed(answer)),
    }
}

/// The box's rendering, split from the half that reads stdin. The rule is padded to a fixed 48 columns
/// rather than to the terminal's width: a box that reflowed with the window would be a layout engine,
/// and §11c's whole constraint is that this is not one.
fn box_render(styled: bool, title: &str, context: &str, options: &[String], question: &str) -> String {
    let rule = "─".repeat(48usize.saturating_sub(display_width(title) + 3));
    let mut s = format!("\n┌ {} {}\n", paint_with(styled, BOLD, title), paint_with(styled, DIM, &rule));
    for l in context.lines() {
        s.push_str(&format!("│ {l}\n"));
    }
    for (i, o) in options.iter().enumerate() {
        s.push_str(&format!("│   {}) {o}\n", paint_with(styled, ACCENT, &(i + 1).to_string())));
    }
    let hint = if options.is_empty() { String::new() } else { format!(" [{}]", choice_hint(options.len())) };
    s.push_str(&format!("└ {question}{hint}: "));
    s
}

// ── progress ──

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK: Duration = Duration::from_millis(100);

/// A `\r`-safe elapsed-time spinner on stderr, for a step that takes minutes and would otherwise read
/// as a hang.
///
/// Cleared on `stop`, and on drop — a step that fails must not leave half a line behind for the error
/// to land on. Off a terminal it degrades to one plain line per step (a CI log wants a heartbeat, not
/// ten frames a second).
///
/// Animation is gated on stderr being a TTY, and only the COLOUR on `NO_COLOR`: the standard is about
/// colour, and someone who turned colour off still wants to know a six-minute step is alive.
pub struct Spinner {
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    label: String,
    started: Instant,
    live: bool,
}

impl Spinner {
    pub fn start(label: &str) -> Spinner {
        let live = std::io::stderr().is_terminal();
        let t0 = Instant::now();
        if !live {
            eprintln!("… {label}");
            return Spinner {
                done: Arc::new(AtomicBool::new(true)),
                handle: None,
                label: label.to_string(),
                started: t0,
                live,
            };
        }
        let done = Arc::new(AtomicBool::new(false));
        let (d, l) = (done.clone(), label.to_string());
        let (open, close) = if styled_err() { (DIM, RESET) } else { ("", "") };
        let handle = std::thread::spawn(move || {
            let (mut f, mut width) = (0usize, 0usize);
            while !d.load(Ordering::Relaxed) {
                let line = format!("{} {l}… {}s", FRAMES[f % FRAMES.len()], t0.elapsed().as_secs());
                width = display_width(&line);
                eprint!("\r{open}{line}{close}");
                let _ = std::io::stderr().flush();
                f += 1;
                std::thread::sleep(TICK);
            }
            // Whatever writes next must land on a clean line, on either stream.
            eprint!("\r{}\r", " ".repeat(width));
            let _ = std::io::stderr().flush();
        });
        Spinner { done, handle: Some(handle), label: label.to_string(), started: t0, live }
    }

    /// Clear the line and report how long the step took.
    pub fn stop(mut self) -> Duration {
        let d = self.started.elapsed();
        self.finish();
        d
    }

    fn finish(&mut self) {
        if self.done.swap(true, Ordering::Relaxed) {
            return;
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        if !self.live {
            eprintln!("… {} took {}s", self.label, self.started.elapsed().as_secs());
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gate is one decision in one place, and it is testable without touching the environment.
    #[test]
    fn styling_needs_a_terminal_and_no_no_color() {
        assert!(styled_from(true, None), "a bare terminal is styled");
        assert!(!styled_from(false, None), "a pipe never is — this is what keeps agit scriptable");
        assert!(!styled_from(true, Some("1")), "NO_COLOR wins over a terminal");
        assert!(!styled_from(true, Some("0")), "…regardless of its value: 0 is not 'off'");
        assert!(!styled_from(false, Some("1")));
        // The standard's carve-out, verbatim: "present and not an empty string". An empty NO_COLOR= is
        // how you re-enable colour for one command when your profile exports it, so it must NOT disable.
        // anstyle-query (clap's) agrees and tests the same case; yansi does not — the ecosystem is split.
        assert!(styled_from(true, Some("")), "an empty NO_COLOR= does not disable colour");
    }

    /// Colour is emphasis only: stripping the escapes must leave the words byte-identical to the plain
    /// rendering. That is what makes a pipe, a NO_COLOR terminal and a screen reader equivalent, and it
    /// is an accessibility requirement rather than a style note.
    #[test]
    fn no_helper_ever_carries_meaning_in_the_colour_alone() {
        for code in [DIM, BOLD, ACCENT, WARN] {
            let painted = paint_with(true, code, "conflict: uid vs user_id");
            assert_ne!(painted, "conflict: uid vs user_id", "styled output must actually be styled");
            assert_eq!(strip_ansi(&painted), "conflict: uid vs user_id", "the colour added a word: {painted:?}");
            assert_eq!(paint_with(false, code, "conflict: uid vs user_id"), "conflict: uid vs user_id");
            assert_eq!(display_width(&painted), "conflict: uid vs user_id".len(), "escapes are zero-width");
        }
    }

    #[test]
    fn a_column_is_as_wide_as_its_widest_cell_header_or_not() {
        let rows = vec![
            vec!["frontend".into(), "11".into()],
            vec!["a".into(), "2".into()],
        ];
        let t = table_with(false, &["AGENT", "SESSIONS"], &rows);
        let lines: Vec<&str> = t.lines().collect();
        // the cell is wider than its header: the column follows the cell
        assert_eq!(lines[0], "AGENT     SESSIONS");
        assert_eq!(lines[1], "frontend  11");
        // …and a short cell still lands under the header, which is wider than it
        assert_eq!(lines[2], "a         2");
        assert!(lines.iter().all(|l| !l.ends_with(' ')), "a trailing space is noise in a pipe: {t:?}");
    }

    #[test]
    fn a_table_survives_no_rows_no_headers_and_ragged_rows() {
        assert_eq!(table_with(false, &[], &[]), "");
        assert_eq!(table_with(false, &["AGENT", "LAST"], &[]), "AGENT  LAST", "headers alone still align");
        // no header line at all: what the pickers use to align their options
        assert_eq!(table_with(false, &[], &[vec!["ref".into(), "x".into()]]), "ref  x");
        // a row shorter than the header must not panic on the missing cells
        assert_eq!(table_with(false, &["A", "B", "C"], &[vec!["only".into()]]), "A     B  C\nonly");
    }

    /// A cell that already carries ANSI must not skew its column: the escapes are zero-width on screen,
    /// so the styled row must land in exactly the columns the plain rows do.
    #[test]
    fn styling_inside_a_cell_does_not_skew_the_column() {
        let rows = vec![vec![format!("{ACCENT}running{RESET}"), "11".into()], vec!["idle".into(), "2".into()]];
        let t = table_with(true, &["STATUS", "N"], &rows);
        let seen: Vec<String> = t.lines().map(strip_ansi).collect();
        assert_eq!(seen, vec!["STATUS   N", "running  11", "idle     2"], "raw: {t:?}");
    }

    /// §11c's conflict picker, rendered exactly. The words must survive with the colour off, and the
    /// question must sit on the closing edge where the answer is typed.
    #[test]
    fn the_conflict_box_matches_the_spec() {
        let options = vec![
            "keep uid, frontend updates its caller".to_string(),
            "keep user_id, api reverts the rename".to_string(),
            "leave open, decide later".to_string(),
        ];
        let plain = box_render(false, "conflict 1/2", "field name: user_id (frontend) vs uid (api)", &options, "your call");
        assert_eq!(
            plain,
            "\n┌ conflict 1/2 ─────────────────────────────────\n\
             │ field name: user_id (frontend) vs uid (api)\n\
             │   1) keep uid, frontend updates its caller\n\
             │   2) keep user_id, api reverts the rename\n\
             │   3) leave open, decide later\n\
             └ your call [1/2/3]: "
        );
        // …and styled, it is the same box with emphasis added and not one word changed
        let styled = box_render(true, "conflict 1/2", "field name: user_id (frontend) vs uid (api)", &options, "your call");
        assert_ne!(styled, plain);
        assert_eq!(strip_ansi(&styled), plain, "styling rewrote the box: {styled:?}");
    }

    #[test]
    fn the_numbered_picker_renders_the_same_words_in_both_modes() {
        let opts = vec!["agent  bob".to_string(), "ref    refs/heads/bob".to_string()];
        let plain = pick_render(false, "\"bob\" is ambiguous:", &opts);
        assert_eq!(plain, "\"bob\" is ambiguous:\n  1) agent  bob\n  2) ref    refs/heads/bob\nPick [1/2]: ");
        assert_eq!(strip_ansi(&pick_render(true, "\"bob\" is ambiguous:", &opts)), plain);
    }

    /// What a terminal actually shows: the escapes, gone.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for e in chars.by_ref() {
                    if e == 'm' {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn ago_reads_like_a_human_at_every_boundary() {
        assert_eq!(ago_secs(0), "just now");
        assert_eq!(ago_secs(59), "just now");
        assert_eq!(ago_secs(60), "1m ago");
        assert_eq!(ago_secs(300), "5m ago");
        assert_eq!(ago_secs(3599), "59m ago");
        assert_eq!(ago_secs(3600), "1h ago");
        assert_eq!(ago_secs(7200), "2h ago");
        assert_eq!(ago_secs(86_399), "23h ago");
        assert_eq!(ago_secs(86_400), "1d ago");
        assert_eq!(ago_secs(432_000), "5d ago");
        // a clock that went backwards must not panic or print a negative age
        assert_eq!(ago(SystemTime::now() + Duration::from_secs(60)), "just now");
    }

    #[test]
    fn tilde_only_collapses_home_at_a_path_boundary() {
        assert_eq!(tilde_from(Path::new("/home/joe/code/web"), Some("/home/joe")), "~/code/web");
        assert_eq!(tilde_from(Path::new("/home/joe"), Some("/home/joe")), "~");
        // the prefix trap: a different user's home merely STARTS with the same bytes
        assert_eq!(tilde_from(Path::new("/home/joe-backup/x"), Some("/home/joe")), "/home/joe-backup/x");
        assert_eq!(tilde_from(Path::new("/srv/code"), Some("/home/joe")), "/srv/code");
        assert_eq!(tilde_from(Path::new("/home/joe/x"), None), "/home/joe/x");
        // HOME=/ would turn every path on the machine into a `~` path
        assert_eq!(tilde_from(Path::new("/etc"), Some("/")), "/etc");
    }

    #[test]
    fn an_excerpt_is_one_line_and_fits() {
        assert_eq!(one_line("the login form\n  posts user_id", 60), "the login form posts user_id");
        assert_eq!(one_line("abcdef", 6), "abcdef");
        assert_eq!(one_line("abcdefgh", 6), "abcde…");
        assert_eq!(one_line("ab cdefgh", 4), "ab…", "the cut never leaves a dangling space");
        assert_eq!(one_line("   ", 10), "");
    }

    /// The CI contract: a picker with nobody to ask must never block — it errors, and the error names
    /// every option it could not ask about.
    #[test]
    fn a_picker_with_nobody_to_ask_names_what_needed_deciding() {
        let e = cannot_ask(
            "\"bob\" is ambiguous:",
            &["agent  bob".to_string(), "ref    refs/heads/bob".to_string()],
            "Say which: --agent bob or --ref bob.",
        );
        let m = e.to_string();
        assert!(m.contains("1) agent  bob") && m.contains("2) ref    refs/heads/bob"), "{m}");
        assert!(m.contains("--agent bob"), "an error a script can act on names the fix: {m}");
    }
}
