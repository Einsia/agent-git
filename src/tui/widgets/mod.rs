//! The shell every screen shares: the status bar, the two-pane skeleton, the footer keys, the
//! filter input.
//!
//! A screen decides only what goes in the list and what goes in the detail; position and shell
//! always come from here — otherwise every added screen means laying the page out again, and all
//! of them must look like one program.
//!
//! # Layout is a pure function
//!
//! [`layout`] does arithmetic only and never touches the terminal. That way the test "a narrow
//! terminal degrades to one pane" is asserted directly, without rendering and counting
//! characters.

use crate::ui::theme;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use std::sync::atomic::{AtomicU8, Ordering};

/// Below this width there are no two panes.
///
/// 80 is the terminal's traditional default width. Narrower than that, the list pane ([`layout`]
/// gives it a bit under half the body) cannot hold one `owner/repo @ branch` — there, **one pane
/// plus an Enter to expand** beats two panes that both overflow.
/// Horizontal scrolling is not an option: it destroys taking it in at a glance.
pub const MIN_TWO_PANE_WIDTH: u16 = 80;

/// The panes of one screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Panes {
    /// The top row: identity and status.
    pub status: Rect,
    /// The main list. On a narrow terminal it fills the whole body.
    pub list: Rect,
    /// The detail. `None` when the terminal is too narrow to hold it.
    pub detail: Option<Rect>,
    /// The bottom row: the key hints.
    pub footer: Rect,
}

/// Split an area into status bar / body / footer; the body's width then decides one pane or two.
pub fn layout(area: Rect) -> Panes {
    let rows = Layout::vertical([
        Constraint::Length(1), // status bar
        Constraint::Min(0),    // body
        Constraint::Length(1), // keys
    ])
    .split(area);
    let (status, body, footer) = (rows[0], rows[1], rows[2]);

    if area.width < MIN_TWO_PANE_WIDTH {
        return Panes {
            status,
            list: body,
            detail: None,
            footer,
        };
    }
    let cols =
        Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)]).split(body);
    Panes {
        status,
        list: cols[0],
        detail: Some(cols[1]),
        footer,
    }
}

/// The single-pane skeleton: status bar / full-width body / keys.
///
/// Timeline uses this (`docs/07_tui.md` §3.3): a history ordered by time, whose rows want the
/// whole width; squeezed into [`layout`]'s half pane, `#n`, sha, kind, message and time fight
/// each other for room. Whether a screen carries a detail pane is **each screen's own business**,
/// but there is still only this one skeleton — otherwise every added screen means laying the page
/// out again.
pub fn layout_single(area: Rect) -> Panes {
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(area);
    Panes {
        status: rows[0],
        list: rows[1],
        detail: None,
        footer: rows[2],
    }
}

/// The "needs action" counters on the status bar.
///
/// Only things that **need a person to do something**. Pure information (how many branches, how
/// many repos) does not belong on this row — it is on every screen, and a full row says nothing
/// at all.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counters {
    /// Sessions that already exist but have no name yet (`docs/07_tui.md` §3.1).
    pub unnamed: usize,
}

// There is no "unsettled" counter here: no cheap honest signal can feed one. Deciding whether a
// session has unsettled content means reading and parsing the transcript, and the discipline of a
// list screen is to never touch a transcript. A cheap proxy **misses**, and a miss reads as
// "nothing to do" — while the whole point of this counter is to tell the user to go do something,
// so a miss is a failure. Better no cell at all than one that works only sometimes.

/// What the status bar shows.
#[derive(Debug, Default, Clone)]
pub struct Status {
    /// The screen name in the top left corner, for example `agit` / `agit new`.
    pub title: String,
    /// `account @ hub`. `None` when not signed in.
    pub identity: Option<String>,
    /// Whether `agit rc` is connected. `None` uses the shared snapshot when one was collected.
    pub rc_online: Option<bool>,
    pub counters: Counters,
}

const RC_UNKNOWN: u8 = 0;
const RC_OFFLINE: u8 = 1;
const RC_ONLINE: u8 = 2;
static RC_STATUS: AtomicU8 = AtomicU8::new(RC_UNKNOWN);

/// Refresh the shared daemon snapshot before taking over the terminal.
///
/// The probe happens on the ordinary screen so an unresponsive control socket cannot leave a
/// full-screen interface looking frozen. Every screen then reads the same atomic snapshot.
pub fn refresh_rc_status() {
    let value = if crate::commands::rc::tui_online() {
        RC_ONLINE
    } else {
        RC_OFFLINE
    };
    RC_STATUS.store(value, Ordering::Relaxed);
}

fn shared_rc_status() -> Option<bool> {
    match RC_STATUS.load(Ordering::Relaxed) {
        RC_ONLINE => Some(true),
        RC_OFFLINE => Some(false),
        _ => None,
    }
}

fn resolve_rc_status(explicit: Option<bool>, shared: Option<bool>) -> Option<bool> {
    explicit.or(shared)
}

impl Status {
    /// Render as one [`Line`].
    ///
    /// Split out so the **content** can be asserted on without counting cells in a render buffer.
    pub fn line(&self) -> Line<'static> {
        let s = theme::symbols();
        let mut spans = vec![Span::styled(format!(" {} ", self.title), theme::header())];
        if let Some(id) = &self.identity {
            spans.push(Span::styled(format!("{id}  "), theme::muted()));
        } else {
            // Being signed out has to be said out loud: recording a version needs an account
            // name, and it cannot be filled in afterwards. Show it before the user starts.
            spans.push(Span::styled(
                "not signed in  ",
                Style::default().fg(theme::WARN),
            ));
        }
        if let Some(online) = self.rc_online {
            let (txt, color) = if online {
                ("rc: online  ", theme::OK)
            } else {
                ("rc: offline  ", theme::MUTED)
            };
            spans.push(Span::styled(txt, Style::default().fg(color)));
        }
        if self.counters.unnamed > 0 {
            spans.push(Span::styled(
                format!("{} {} unnamed  ", s.warn, self.counters.unnamed),
                Style::default().fg(theme::WARN),
            ));
        }
        Line::from(spans)
    }
}

/// Draw the status bar.
pub fn render_status(f: &mut Frame, area: Rect, status: &Status) {
    let mut status = status.clone();
    status.rc_online = resolve_rc_status(status.rc_online, shared_rc_status());
    f.render_widget(Paragraph::new(status.line()), area);
}

/// Draw the footer key hints.
pub fn render_footer(f: &mut Frame, area: Rect, keys: &str) {
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(" {keys} "),
            theme::muted(),
        ))),
        area,
    );
}

/// Render operation feedback above a single-pane list and return the remaining list area.
///
/// A detail pane may disappear as the terminal narrows, but validation and persistence failures
/// must remain visible because they are the only explanation for an action that did not complete.
pub fn list_area_with_notice(f: &mut Frame, panes: Panes, notice: Option<&str>) -> Rect {
    let Some(notice) = notice else {
        return panes.list;
    };
    if panes.detail.is_some() || panes.list.height == 0 {
        return panes.list;
    }
    let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(panes.list);
    f.render_widget(
        Paragraph::new(clamp_line(
            Line::from(Span::styled(
                format!(" {notice}"),
                Style::default().fg(theme::WARN),
            )),
            rows[0].width as usize,
        )),
        rows[0],
    );
    rows[1]
}

/// How many columns this text occupies in the terminal.
///
/// **Not the character count.** One CJK character occupies two columns, and ratatui renders by
/// column (`Span::width` uses the same ruler). Budget width by character count and a row of
/// Chinese takes twice the columns, so the renderer silently cuts the end of the row — and the
/// end of a list row is where the time and the tags sit.
pub fn cols(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Truncate to at most `max` **columns**, ending with an ellipsis when anything was cut.
///
/// The split with [`crate::ui::truncate`]: that one cuts by **character** and serves text
/// constraints of the form "at most this many characters" (message summaries, the gist); this one
/// cuts by **column** and serves layout constraints of the form "this is all the width left". The
/// two coincide in pure ASCII, so mixing them does not fail right away — it fails on Chinese rows.
pub fn truncate_cols(s: &str, max: usize) -> String {
    if cols(s) <= max {
        return s.to_string();
    }
    // With no column at all, not even the ellipsis: `…` occupies a column itself, so returning
    // it goes over budget. A guardrail in each caller is not enough — this is `pub`, and the next
    // screen will not remember.
    if max == 0 {
        return String::new();
    }
    // Leave one column for the ellipsis.
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Pad with spaces on the right to `n` **columns**.
///
/// `format!("{:<24}")` pads by **character**: a 12-character Chinese name is exactly 24 columns,
/// yet gets padded out to 24 characters = 36 columns and pushes the whole row right. Alignment is
/// a layout constraint, so it can only be computed in columns.
pub fn pad_cols(s: &str, n: usize) -> String {
    let mut out = s.to_string();
    for _ in cols(s)..n {
        out.push(' ');
    }
    out
}

/// Clamp a whole line to `max` columns, cutting from the **end of the row**.
///
/// The yield order (which field goes first) is the strategy at ordinary widths, and it has an
/// end: the fields that never yield have a floor of their own once added up. At a width absurd
/// enough that even they do not fit, the row still must not be drawn past the border — hence this
/// last-resort truncation.
///
/// Without it, "a row never exceeds the width it was given" holds only at the widths the tests
/// picked, and on a genuinely narrow terminal the renderer does the cutting for us — and what it
/// cuts is the end of the row.
pub fn clamp_line(line: Line<'static>, max: usize) -> Line<'static> {
    if line.width() <= max {
        return line;
    }
    let mut used = 0usize;
    let mut kept: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        let room = max.saturating_sub(used);
        if room == 0 {
            break;
        }
        let text = truncate_cols(&span.content, room);
        if text.is_empty() {
            break;
        }
        used += cols(&text);
        kept.push(Span::styled(text, span.style));
    }
    Line::from(kept)
}

/// A titled box. The list and the detail share it, so the two panes look like a pair.
pub fn pane(title: &str) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
}

/// The incremental filter input (`/`).
#[derive(Debug, Default, Clone)]
pub struct Filter {
    query: String,
    active: bool,
}

impl Filter {
    pub fn is_active(&self) -> bool {
        self.active
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn open(&mut self) {
        self.active = true;
    }
    /// Cancel the filter: clear the query and close the input.
    ///
    /// Keeping the previous query **without showing it** is a trap: the user sees a list that is
    /// still short and thinks entries were lost. So either clear it (here), or keep it and show
    /// it the whole time ([`blur`]).
    ///
    /// [`blur`]: Filter::blur
    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
    }

    /// Close the input but **keep** the query — the user types a filter word, presses Enter, and
    /// then picks one of the results.
    ///
    /// This is safe only because [`hint`] still returns the query after focus is lost: "this is a
    /// filtered view" stays visible on screen, so a short list is not misread as things having
    /// been lost.
    ///
    /// [`hint`]: Filter::hint
    pub fn blur(&mut self) {
        self.active = false;
    }
    pub fn push(&mut self, c: char) {
        self.query.push(c);
    }
    pub fn pop(&mut self) {
        self.query.pop();
    }

    /// Whether this row passes the filter. Case-insensitive — nobody should have to remember
    /// capitalization to find a session.
    pub fn matches(&self, haystack: &str) -> bool {
        if self.query.is_empty() {
            return true;
        }
        haystack.to_lowercase().contains(&self.query.to_lowercase())
    }

    /// The text the filter bar shows.
    ///
    /// Shown whenever there **is a query**, whether or not the input is closed — see [`blur`] for
    /// the reason.
    ///
    /// [`blur`]: Filter::blur
    pub fn hint(&self) -> Option<String> {
        (self.active || !self.query.is_empty()).then(|| format!("/{}", self.query))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn area(w: u16, h: u16) -> Rect {
        Rect::new(0, 0, w, h)
    }

    /// A wide terminal gets two panes; the status bar and the keys take one row each.
    #[test]
    fn a_wide_terminal_gets_two_panes() {
        let p = layout(area(120, 30));
        assert_eq!(p.status.height, 1);
        assert_eq!(p.footer.height, 1);
        let detail = p.detail.expect("a wide terminal must have a detail pane");
        // The two panes sit side by side, do not overlap, and together fill the whole width.
        assert_eq!(p.list.x, 0);
        assert_eq!(p.list.x + p.list.width, detail.x);
        assert_eq!(detail.x + detail.width, 120);
        // The body takes all the height the status bar and the keys leave.
        assert_eq!(p.list.height, 28);
    }

    /// A narrow terminal degrades to one pane — no horizontal scrolling.
    #[test]
    fn a_narrow_terminal_degrades_to_one_pane() {
        let p = layout(area(MIN_TWO_PANE_WIDTH - 1, 24));
        assert!(
            p.detail.is_none(),
            "a narrow terminal must not squeeze out a detail pane"
        );
        assert_eq!(
            p.list.width,
            MIN_TWO_PANE_WIDTH - 1,
            "the list fills the whole width"
        );
        assert_eq!(p.list.height, 22);
        // The boundary value itself still gets two panes.
        assert!(layout(area(MIN_TWO_PANE_WIDTH, 24)).detail.is_some());
    }

    /// The single-pane skeleton: a full-width body, one row each for status bar and keys.
    #[test]
    fn a_single_pane_layout_gives_the_body_the_whole_width() {
        let p = layout_single(area(120, 30));
        assert!(p.detail.is_none());
        assert_eq!((p.list.x, p.list.width), (0, 120));
        assert_eq!(p.list.height, 28);
        assert_eq!(p.status.y, 0);
        assert_eq!(p.footer.y, 29);
        // The same shape on a narrow terminal — it never splits into panes anyway.
        let narrow = layout_single(area(40, 10));
        assert_eq!((narrow.list.x, narrow.list.width), (0, 40));
        assert!(narrow.detail.is_none());
    }

    /// The status bar carries only what needs action, and a zero takes up no room.
    #[test]
    fn the_status_bar_shows_only_what_needs_doing() {
        let quiet = Status {
            title: "agit".into(),
            identity: Some("nana @ agent-git.com".into()),
            ..Default::default()
        };
        let txt = quiet.line().to_string();
        assert!(txt.contains("nana @ agent-git.com"));
        assert!(
            !txt.contains("unnamed"),
            "a zero unnamed count takes up no room: {txt}"
        );

        let busy = Status {
            counters: Counters { unnamed: 2 },
            ..quiet
        };
        assert!(busy.line().to_string().contains("2 unnamed"));
    }

    #[test]
    fn the_shared_rc_snapshot_fills_only_an_unspecified_status() {
        assert_eq!(resolve_rc_status(None, Some(true)), Some(true));
        assert_eq!(resolve_rc_status(Some(false), Some(true)), Some(false));
        assert_eq!(resolve_rc_status(None, None), None);
    }

    /// Being signed out must be said on the status bar — recording a version needs an account
    /// name, and it cannot be filled in afterwards.
    #[test]
    fn being_signed_out_is_visible_before_you_start() {
        let s = Status {
            title: "agit".into(),
            identity: None,
            ..Default::default()
        };
        assert!(s.line().to_string().contains("not signed in"));
    }

    /// A real render: content lands on the rows it belongs on, and spills onto no others.
    #[test]
    fn the_frame_renders_where_the_layout_says() {
        let mut term = Terminal::new(TestBackend::new(100, 10)).unwrap();
        let status = Status {
            title: "agit".into(),
            identity: Some("nana @ hub".into()),
            counters: Counters { unnamed: 2 },
            ..Default::default()
        };
        term.draw(|f| {
            let p = layout(f.area());
            render_status(f, p.status, &status);
            f.render_widget(pane("sessions"), p.list);
            f.render_widget(pane("conversation"), p.detail.unwrap());
            render_footer(f, p.footer, "q quit");
        })
        .unwrap();

        let rows = rows_of(term.backend());
        assert!(
            rows[0].contains("agit"),
            "the status bar is the first row: {:?}",
            rows[0]
        );
        assert!(rows[0].contains("nana @ hub"));
        assert!(rows[0].contains("2 unnamed"));
        assert!(
            rows[1].contains("sessions"),
            "the list box comes next: {:?}",
            rows[1]
        );
        assert!(
            rows[1].contains("conversation"),
            "the detail box starts on that same row"
        );
        assert!(
            rows[9].contains("q quit"),
            "the keys are the last row: {:?}",
            rows[9]
        );
        // The keys must not appear on any other row.
        assert_eq!(rows.iter().filter(|r| r.contains("q quit")).count(), 1);
    }

    /// A real render on a narrow terminal: one box only, and no horizontal overflow.
    #[test]
    fn a_narrow_frame_draws_a_single_pane() {
        let mut term = Terminal::new(TestBackend::new(60, 8)).unwrap();
        term.draw(|f| {
            let p = layout(f.area());
            assert!(p.detail.is_none());
            f.render_widget(pane("sessions"), p.list);
        })
        .unwrap();
        let rows = rows_of(term.backend());
        assert!(rows[1].contains("sessions"));
        assert!(!rows[1].contains("conversation"));
        assert!(rows.iter().all(|r| r.chars().count() == 60));
    }

    /// Width counts **columns**, not characters.
    ///
    /// This pins a failure that shows up only on Chinese rows: budget by character count and a
    /// row of CJK takes twice the columns, so the renderer cuts the time and the tags off the end
    /// of the row, and nothing errors.
    #[test]
    fn width_is_measured_in_columns_not_characters() {
        assert_eq!(cols("abc"), 3);
        // CJK width fixture: the Chinese literals in this test are what it measures against.
        assert_eq!(cols("重构"), 4, "two CJK characters occupy four columns");
        assert_eq!(
            "重构".chars().count(),
            2,
            "the character count is only 2 — the trap"
        );

        // Truncation is by column too: `重构错误` is 8 columns, so cutting to 5 leaves room for
        // two characters plus the ellipsis.
        let cut = truncate_cols("重构错误", 5);
        assert_eq!(cut, "重构…");
        assert!(
            cols(&cut) <= 5,
            "a truncated string is really within the width: {cut}"
        );
        // Half a character does not go in; better one column short than one column over.
        let odd = truncate_cols("重构错误", 4);
        assert!(cols(&odd) <= 4, "{odd}");
        // Wide enough returns the string unchanged, with no gratuitous ellipsis.
        assert_eq!(truncate_cols("重构", 4), "重构");
        assert_eq!(truncate_cols("abc", 10), "abc");
        // Zero columns cannot carry even the ellipsis — `…` occupies a column itself.
        assert_eq!(truncate_cols("重构", 0), "");
        assert_eq!(truncate_cols("", 0), "");
    }

    /// The last-resort truncation: no row may exceed the columns it was given.
    ///
    /// This pins the **end** of the yield order — the fields that never yield have a floor of
    /// their own once added up, and below that width no field is left to yield. Without this
    /// step, "a row never exceeds the width it was given" holds only at the widths the tests
    /// picked.
    #[test]
    fn a_line_never_exceeds_the_columns_it_was_given() {
        let line = Line::from(vec![
            Span::raw("#14 3f2a1bc"),
            // CJK width fixture: a Chinese span is what makes the column arithmetic bite.
            Span::raw(" 修掉退款重试"),
            Span::raw("  6m ago"),
        ]);
        for max in [0usize, 1, 3, 8, 11, 12, 20, 40] {
            let out = clamp_line(line.clone(), max);
            assert!(
                cols(&out.to_string()) <= max,
                "a row of {max} columns draws {} columns: {}",
                cols(&out.to_string()),
                out
            );
        }
        // Wide enough leaves every character alone.
        let wide = clamp_line(line.clone(), 100);
        assert_eq!(wide.to_string(), line.to_string());
    }

    /// Padding counts columns too. `{:<n}` pads by character, which doubles the width of a
    /// Chinese name.
    #[test]
    fn padding_is_measured_in_columns_too() {
        assert_eq!(cols(&pad_cols("abc", 6)), 6);
        // CJK width fixture: a 12-column Chinese name padded to 24 columns takes 12 spaces, not
        // the 12 characters `{:<24}` appends.
        assert_eq!(cols(&pad_cols("重构错误处理层", 24)), 24);
        assert_eq!(
            format!("{:<24}", "重构错误处理层").chars().count(),
            24,
            "the character count looks right — that is the trap"
        );
        assert_eq!(cols(&format!("{:<24}", "重构错误处理层")), 31);
        // Already wide enough is left alone.
        assert_eq!(pad_cols("重构", 2), "重构");
    }

    #[test]
    fn filtering_is_case_insensitive_and_clears_on_close() {
        let mut f = Filter::default();
        assert!(f.matches("anything"), "an empty query allows every row");
        f.open();
        for c in "Refund".chars() {
            f.push(c);
        }
        assert!(f.matches("payments/refund-fix"));
        assert!(!f.matches("infra/deploy"));
        assert_eq!(f.hint().as_deref(), Some("/Refund"));
        f.pop();
        assert_eq!(f.query(), "Refun");
        // The input closes but the query stays: the screen must still show that a filter is on.
        f.blur();
        assert!(!f.is_active());
        assert_eq!(f.query(), "Refun");
        assert_eq!(
            f.hint().as_deref(),
            Some("/Refun"),
            "a filter that outlives focus must stay visible"
        );
        // Closing clears: a query kept but not shown makes the user think entries were lost.
        f.close();
        assert!(!f.is_active());
        assert_eq!(f.query(), "");
        assert!(f.hint().is_none());
    }

    fn rows_of(backend: &TestBackend) -> Vec<String> {
        let buf = backend.buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }
}
