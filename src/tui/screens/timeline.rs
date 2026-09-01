//! The Timeline screen (bare `agit log`): turn-by-turn history, `Tab` switches to the
//! branch-level view.
//!
//! # Fetching does not happen here
//!
//! Both views take their material from [`crate::commands::log`]'s `turns()` / `branch_rows()`
//! — **the same one the text rendering uses**. Fetched separately, the two drift apart sooner or
//! later on things like "where `#n` starts counting" and "which commit the opening prompt comes
//! from", and that kind of drift has no symptom: both sides look right.
//!
//! # Performance (`docs/07_tui.md` §3.3 / §4.1)
//!
//! The turn-by-turn view spends three git processes on the whole history (one `log`, one meta
//! batch, one tag table). The branch-level view costs several more git calls per branch, so it
//! fetches **on demand**: press no `Tab` and it costs nothing. `/` filtering acts only on rows
//! **already fetched** — a filter that triggers a full recompute is the easiest trap on this
//! screen.
//!
//! Neither view opens the live transcript.

use crate::commands::log::{BranchRow, Turn};
use crate::domain::meta::Kind;
use crate::domain::repo::Repo;
use crate::tui::widgets::{self, Filter};
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

/// Which view is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Turns,
    Branches,
}

impl View {
    fn flip(self) -> View {
        match self {
            View::Turns => View::Branches,
            View::Branches => View::Turns,
        }
    }
}

/// What to do after one keystroke. Split out so that "which key does what in which view" can be
/// asserted without starting a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Stay on this screen and keep drawing.
    Stay,
    /// Switch views.
    Switch(View),
    /// Switch to another branch's turn-by-turn history (Enter in the branch-level view).
    OpenBranch {
        name: String,
        head: String,
    },
    /// The user selected a turn: open the transcript screen to read it.
    ///
    /// It carries **the index into the current filtered view**, not a sha: the transcript screen
    /// lists exactly that view, and only an index answers "back to the row I was on when the read
    /// ends".
    OpenTurn(usize),
    Quit,
}

/// Below this width the list is not drawn; one sentence takes its place.
///
/// The yield order guarantees "the message, the tags and the time may yield; `#n` / sha / kind /
/// the turn count / `↑↓` may not"; [`widgets::clamp_line`] guarantees "a row never crosses the
/// frame". At a width too small even for the fields that never yield, the two **cannot both
/// hold** — the last-resort truncation cuts from the end of the row into the locating fields and
/// the divergence warning.
///
/// A screenful of rows that look selectable but cannot be identified or cited is worse than
/// saying "too narrow": the reader assumes they saw everything. So the whole list yields.
const MIN_USABLE_WIDTH: u16 = 44;

/// Columns the frame and the highlight symbol take: one for each side border, two for `"▸ "`.
///
/// ratatui reserves the highlight symbol's place on **every** row, not only the selected one —
/// leave those two columns out of the budget and a CJK row lands exactly on the right border.
const BORDER_AND_MARKER: usize = 4;

/// One turn-by-turn row: `#n` ordinal, kind, short sha, message, time, tags.
///
/// A pure function: it touches neither the terminal nor git. Where the width does not suffice it
/// is the **message** that is cut; `#n` and kind do not move — those two locate the turn, the
/// message is only a hint.
pub fn turn_line(t: &Turn, width: u16) -> Line<'static> {
    // The turn ordinal is printed only on the commit that actually settled that turn; the rest
    // stay blank — so the number in this column and what `<ref>#n` resolves to are the same
    // commit. Blank is not empty: the placeholder still occupies four columns, otherwise the sha
    // column jumps left and right between numbered and unnumbered rows.
    let label = match t.turn {
        Some(n) => format!("#{n:>3}"),
        None => "    ".into(),
    };
    let head = format!("{label} {} {:<6} ", t.short, kind_word(t.kind));
    let mut ago = format!("  {}", crate::ui::ago(t.at));
    let mut tags = if t.tags.is_empty() {
        String::new()
    } else {
        format!("  ⌂ {}", t.tags.join(","))
    };

    // Yield order: the message goes first, then the tags, and the time last.
    //
    // `#n` / sha / kind never yield — they are what gets `agit show` to this turn; the message is
    // only a hint. Everything is budgeted in **columns**: a CJK row has half as many characters
    // as columns, so a character budget overflows the row, and the renderer cuts the overflow
    // from the **end** — which takes exactly the fields that most need to stay. The width given
    // here is the **frame** width; the content gets [`BORDER_AND_MARKER`] columns less.
    let budget = (width as usize).saturating_sub(widgets::cols(&head) + BORDER_AND_MARKER);
    if widgets::cols(&ago) + widgets::cols(&tags) > budget {
        tags.clear();
    }
    if widgets::cols(&ago) > budget {
        ago.clear();
    }
    let room = budget.saturating_sub(widgets::cols(&ago) + widgets::cols(&tags));
    // Under four columns, drop it entirely: a message reduced to an ellipsis carries nothing and
    // still takes the space.
    let subject = if room >= 4 {
        widgets::truncate_cols(&t.subject, room)
    } else {
        String::new()
    };

    // The yield order has an end: `head` itself never yields, and below its width every step
    // above has already yielded everything. The row still must not cross the frame, so there is a
    // backstop (see [`widgets::clamp_line`]).
    widgets::clamp_line(
        Line::from(vec![
            Span::styled(head, Style::default().fg(kind_color(t.kind))),
            Span::raw(subject),
            Span::styled(ago, theme::muted()),
            Span::styled(tags, Style::default().fg(theme::ACCENT)),
        ]),
        (width as usize).saturating_sub(BORDER_AND_MARKER),
    )
}

/// Columns the branch-name field takes on a wide terminal. Row alignment rests on it.
const BRANCH_NAME_COLS: usize = 24;

/// How narrow the name field may get on a narrow terminal. Narrower and the branch is no longer
/// recognizable.
const MIN_BRANCH_NAME_COLS: usize = 6;

/// One branch-level row.
///
/// Isomorphic to [`turn_line`]: budgeted in **columns**, with the yield order running from
/// weakest to strongest — gist → time → `[file line]` → the name field narrows. **The turn count
/// and `↑↓` come last** — everything before them has to have yielded before they are cut at all,
/// and what cuts them then is [`widgets::clamp_line`].
///
/// `↑↓` comes last because it is the divergence warning: a branch that has diverged from the
/// remote goes unhandled when the reader cannot see those two arrows. `[file line]` only says
/// that Enter leads to a history with no turns, so it may yield.
pub fn branch_line(b: &BranchRow, width: u16) -> Line<'static> {
    let turns = format!(" {:>4} turns", b.turns);
    let ab = if b.ahead_behind.is_empty() {
        String::new()
    } else {
        format!("  ↑↓ {}", b.ahead_behind)
    };

    let budget = (width as usize).saturating_sub(BORDER_AND_MARKER);
    // Subtract the two that never yield first; the rest is handed out in order.
    let must = widgets::cols(&turns) + widgets::cols(&ab);
    let name_cols = BRANCH_NAME_COLS
        .min(budget.saturating_sub(must))
        .max(MIN_BRANCH_NAME_COLS);
    // Pad by **column**: `{:<24}` pads by character, and a 12-character CJK name is already 24
    // columns; padded to 24 characters it becomes 36 columns and pushes everything after it past
    // the frame.
    let name = widgets::pad_cols(&widgets::truncate_cols(&b.name, name_cols), name_cols);

    // The rest is handed out in order; what cannot be afforded is the field that yields.
    let mut left = budget.saturating_sub(must + widgets::cols(&name));
    let mut afford = |want: usize| {
        let ok = left >= want;
        if ok {
            left -= want;
        }
        ok
    };

    const FILE_TAG: &str = " [file line]";
    let file = if b.file_line && afford(widgets::cols(FILE_TAG)) {
        FILE_TAG
    } else {
        ""
    };
    let stamp = format!(" · {}", b.when);
    let when = if !b.when.is_empty() && afford(widgets::cols(&stamp)) {
        stamp
    } else {
        String::new()
    };
    // The `  “”` wrapper around the gist is 4 columns; with no room for even one character the
    // whole gist is dropped.
    let gist = if b.gist.is_empty() || left < 6 {
        String::new()
    } else {
        format!("  “{}”", widgets::truncate_cols(&b.gist, left - 4))
    };

    let mut spans = vec![
        Span::raw(name),
        Span::styled(format!("{turns}{when}"), theme::muted()),
    ];
    if !gist.is_empty() {
        spans.push(Span::raw(gist));
    }
    if !file.is_empty() {
        spans.push(Span::styled(file, theme::muted()));
    }
    if !ab.is_empty() {
        spans.push(Span::styled(ab, Style::default().fg(theme::WARN)));
    }
    // Once the name field hits its floor there is nothing left to yield; below that
    // [`widgets::clamp_line`] is the backstop.
    widgets::clamp_line(Line::from(spans), budget)
}

fn kind_word(k: Kind) -> &'static str {
    match k {
        Kind::Turn => "turn",
        Kind::Merge => "merge",
        Kind::View => "view",
        Kind::File => "file",
    }
}

fn kind_color(k: Kind) -> Color {
    match k {
        Kind::Turn => theme::ACCENT,
        Kind::Merge => theme::WARN,
        _ => theme::MUTED,
    }
}

/// The detail strip under the selected row (the separator row in `docs/07_tui.md` §3.3).
///
/// It carries only what **does not fit in the row and is worth seeing**: the code anchor and the
/// milestone. With neither, it says so rather than leaving a blank.
pub fn strip_text(t: Option<&Turn>) -> String {
    let Some(t) = t else {
        return "nothing matches this filter.".into();
    };
    let mut parts = Vec::new();
    if let Some(c) = &t.code {
        parts.push(format!("code {c}"));
    }
    if let Some(m) = &t.milestone {
        parts.push(format!("★ {m}"));
    }
    if parts.is_empty() {
        format!("{} · no code anchor on this turn", t.short)
    } else {
        parts.join("   ")
    }
}

/// All the state this screen holds.
struct Screen {
    slug: String,
    branch: String,
    turns: Vec<Turn>,
    /// The branch-level view fetches **on demand**: press no `Tab` and not one git call is spent.
    branches: Option<Vec<BranchRow>>,
    view: View,
}

/// Open the Timeline. Returns only once the user presses q.
pub fn run(repo: &Repo, slug: &str, branch: &str, head: &str) -> crate::CmdResultAlias {
    // Failing to read the history is an error and is reported at the command boundary — flattened
    // into an empty list, this screen says "no turns yet" and exits normally, and one corrupt spot
    // makes the whole history vanish with no symptom.
    let turns = crate::commands::log::turns(repo, head, usize::MAX, None, None, None, &[])?;
    if turns.is_empty() {
        // Zero candidates never enter an empty shell, the same as the other screens.
        println!("no turns on {slug} @ {branch} yet.");
        crate::ui::hint("a session gets its first turn when it settles — see `agit commit`");
        return Ok(crate::ExitCode::Ok);
    }
    let mut screen = Screen {
        slug: slug.to_string(),
        branch: branch.to_string(),
        turns,
        branches: None,
        view: View::Turns,
    };
    let guard = crate::tui::term::Guard::enter()?;
    let out = event_loop(repo, &mut screen);
    // Give the terminal back before letting the result (an error above all) propagate: those
    // words belong on the normal screen, not in the alt screen — whatever is written there goes
    // away the moment the alt screen exits.
    drop(guard);
    out
}

fn event_loop(repo: &Repo, screen: &mut Screen) -> crate::CmdResultAlias {
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut state = ListState::default();
    let mut filter = Filter::default();
    let mut notice: Option<String> = None;
    // The turn-by-turn view opens on the newest turn — that is what someone opening log
    // wants to see.
    state.select(Some(0));

    loop {
        let turns: Vec<&Turn> = screen
            .turns
            .iter()
            .rev()
            .filter(|t| filter.matches(&turn_haystack(t)))
            .collect();
        let branches: Vec<&BranchRow> = screen
            .branches
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter(|b| filter.matches(&branch_haystack(b)))
            .collect();
        let len = match screen.view {
            View::Turns => turns.len(),
            View::Branches => branches.len(),
        };
        if state.selected().unwrap_or(0) >= len {
            state.select((len > 0).then(|| len - 1));
        }
        term.draw(|f| {
            draw(
                f,
                screen,
                &turns,
                &branches,
                &mut state,
                &filter,
                notice.as_deref(),
            )
        })?;

        let Some(key) = crate::tui::term::next_key()? else {
            continue;
        };
        if filter.is_active() {
            match key.code {
                KeyCode::Esc => filter.close(),
                KeyCode::Enter => filter.blur(),
                KeyCode::Backspace => filter.pop(),
                KeyCode::Char(c) => filter.push(c),
                _ => {}
            }
            continue;
        }
        notice = None;
        let step = decide(
            key.code,
            key.modifiers,
            screen.view,
            state.selected(),
            &turns,
            &branches,
        );
        match step {
            Step::Quit => return Ok(crate::ExitCode::Ok),
            Step::Stay => {
                move_cursor(key.code, &mut state, len, &mut filter);
            }
            Step::Switch(next) => {
                if next == View::Branches && screen.branches.is_none() {
                    // Only the first `Tab` pays this cost.
                    screen.branches = Some(crate::commands::log::branch_rows(repo));
                }
                screen.view = next;
                state.select(Some(0));
            }
            Step::OpenBranch { name, head } => {
                let rows =
                    crate::commands::log::turns(repo, &head, usize::MAX, None, None, None, &[])?;
                if rows.is_empty() {
                    notice = Some(format!("{name} has no turns yet."));
                    continue;
                }
                screen.branch = name;
                screen.turns = rows;
                screen.view = View::Turns;
                state.select(Some(0));
            }
            Step::OpenTurn(i) => {
                // The transcript screen runs inside the **same** alt screen (see
                // `read_turns_inline`): the guard is still ours, so the read comes back here with
                // the cursor and the filter untouched.
                let shown: Vec<Turn> = turns.iter().map(|t| (*t).clone()).collect();
                crate::tui::screens::transcript::read_turns_inline(repo, &shown, i)?;
                // Full redraw on the way back: the transcript screen painted over the same
                // surface while this `Terminal`'s previous frame still holds what was there
                // before — without a clear it fills in only the cells that differ from that
                // frame, and what the transcript screen left behind stays in the gaps.
                term.clear()?;
            }
        }
    }
}

/// What one keystroke means in the current view. **A pure function** — the key map is a
/// criterion, not rendering.
pub fn decide(
    code: KeyCode,
    mods: KeyModifiers,
    view: View,
    selected: Option<usize>,
    turns: &[&Turn],
    branches: &[&BranchRow],
) -> Step {
    match code {
        KeyCode::Char('q') | KeyCode::Esc => Step::Quit,
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => Step::Quit,
        KeyCode::Tab | KeyCode::BackTab => Step::Switch(view.flip()),
        KeyCode::Enter => match (view, selected) {
            (View::Turns, Some(i)) if i < turns.len() => Step::OpenTurn(i),
            (View::Branches, Some(i)) => branches
                .get(i)
                .map(|b| Step::OpenBranch {
                    name: b.name.clone(),
                    head: b.head.clone(),
                })
                .unwrap_or(Step::Stay),
            _ => Step::Stay,
        },
        _ => Step::Stay,
    }
}

fn move_cursor(code: KeyCode, state: &mut ListState, len: usize, filter: &mut Filter) {
    let last = len.saturating_sub(1);
    match code {
        KeyCode::Char('/') => filter.open(),
        KeyCode::Down | KeyCode::Char('j') => {
            state.select(Some((state.selected().unwrap_or(0) + 1).min(last)));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.select(Some(state.selected().unwrap_or(0).saturating_sub(1)));
        }
        KeyCode::Char('g') | KeyCode::Home => state.select(Some(0)),
        KeyCode::Char('G') | KeyCode::End => state.select(Some(last)),
        KeyCode::PageDown => state.select(Some((state.selected().unwrap_or(0) + 10).min(last))),
        KeyCode::PageUp => {
            state.select(Some(state.selected().unwrap_or(0).saturating_sub(10)));
        }
        _ => {}
    }
}

fn turn_haystack(t: &Turn) -> String {
    format!(
        "{} {} {} {}",
        t.short,
        kind_word(t.kind),
        t.subject,
        t.tags.join(" ")
    )
}

fn branch_haystack(b: &BranchRow) -> String {
    format!("{} {}", b.name, b.gist)
}

fn draw(
    f: &mut Frame,
    screen: &Screen,
    turns: &[&Turn],
    branches: &[&BranchRow],
    state: &mut ListState,
    filter: &Filter,
    notice: Option<&str>,
) {
    let panes = widgets::layout_single(f.area());
    if f.area().width < MIN_USABLE_WIDTH {
        f.render_widget(
            Paragraph::new(format!(
                "terminal too narrow — {MIN_USABLE_WIDTH} columns are needed to \
                 identify a turn and show whether a branch diverged"
            ))
            .wrap(ratatui::widgets::Wrap { trim: false })
            .block(widgets::pane("agit log")),
            f.area(),
        );
        return;
    }
    // The list leaves one detail-strip row under it (the separator row in `docs/07_tui.md`
    // §3.3).
    let split = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(panes.list);
    widgets::render_status(
        f,
        panes.status,
        &widgets::Status {
            title: "agit log".into(),
            identity: Some(format!("{} @ {}", screen.slug, screen.branch)),
            rc_online: None,
            counters: widgets::Counters::default(),
        },
    );

    let width = split[0].width;
    let items: Vec<ListItem> = match screen.view {
        View::Turns => turns
            .iter()
            .map(|t| ListItem::new(turn_line(t, width)))
            .collect(),
        View::Branches => branches
            .iter()
            .map(|b| ListItem::new(branch_line(b, width)))
            .collect(),
    };
    let count = items.len();
    let title = match (screen.view, filter.hint()) {
        (View::Turns, Some(q)) => format!("turns  {q}"),
        (View::Turns, None) => format!("turns ({count})"),
        (View::Branches, Some(q)) => format!("branches  {q}"),
        (View::Branches, None) => format!("branches ({count})"),
    };
    f.render_stateful_widget(
        List::new(items)
            .block(widgets::pane(&title))
            .highlight_style(theme::selected())
            .highlight_symbol("▸ "),
        split[0],
        state,
    );

    let strip = match notice {
        Some(n) => n.to_string(),
        None => match screen.view {
            View::Turns => strip_text(state.selected().and_then(|i| turns.get(i)).copied()),
            View::Branches => "kind: turn/merge/view/file   enter opens that branch’s turns".into(),
        },
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            format!(
                " {}",
                widgets::truncate_cols(&strip, width.saturating_sub(2) as usize)
            ),
            theme::muted(),
        ))),
        split[1],
    );

    widgets::render_footer(
        f,
        panes.footer,
        if filter.is_active() {
            "type to filter   enter apply   esc cancel"
        } else {
            match screen.view {
                View::Turns => "↑↓ move   enter read   tab branches   / filter   q quit",
                View::Branches => "↑↓ move   enter turns   tab timeline   / filter   q quit",
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn turn(n: u32, subject: &str) -> Turn {
        Turn {
            turn: Some(n),
            short: format!("{n:0>9}"),
            kind: Kind::Turn,
            subject: subject.into(),
            tags: vec![],
            code: None,
            milestone: None,
            at: std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
        }
    }

    fn branch(name: &str) -> BranchRow {
        BranchRow {
            name: name.into(),
            head: format!("refs/heads/{name}"),
            turns: 3,
            when: "2 hours ago".into(),
            // CJK width fixture (AGENTS.md exception iii): the column arithmetic in these
            // rows is checked against a gist of wide characters.
            gist: "修掉退款重试".into(),
            file_line: false,
            ahead_behind: String::new(),
        }
    }

    /// `Tab` flips between the two views; two presses land back where it started.
    #[test]
    fn tab_flips_between_the_two_views_and_back() {
        let (t, b) = (vec![], vec![]);
        assert_eq!(
            decide(KeyCode::Tab, KeyModifiers::NONE, View::Turns, None, &t, &b),
            Step::Switch(View::Branches)
        );
        assert_eq!(
            decide(
                KeyCode::Tab,
                KeyModifiers::NONE,
                View::Branches,
                None,
                &t,
                &b
            ),
            Step::Switch(View::Turns)
        );
    }

    /// Enter means a different thing in each view, and both speak for **the selected row**.
    #[test]
    fn enter_means_a_different_thing_in_each_view() {
        let turns = [turn(2, "second"), turn(1, "first")];
        let refs: Vec<&Turn> = turns.iter().collect();
        let mut remote = branch("spike-idx");
        remote.head = "refs/remotes/origin/spike-idx".into();
        let rows = [branch("refund-fix"), remote];
        let brefs: Vec<&BranchRow> = rows.iter().collect();

        assert_eq!(
            decide(
                KeyCode::Enter,
                KeyModifiers::NONE,
                View::Turns,
                Some(1),
                &refs,
                &brefs
            ),
            Step::OpenTurn(1),
            "the index carried back is the index into the view the transcript screen lists"
        );
        assert_eq!(
            decide(
                KeyCode::Enter,
                KeyModifiers::NONE,
                View::Branches,
                Some(1),
                &refs,
                &brefs
            ),
            Step::OpenBranch {
                name: "spike-idx".into(),
                head: "refs/remotes/origin/spike-idx".into(),
            }
        );
        // A selection that does not exist (filtered to empty) must neither panic nor open the
        // wrong thing.
        assert_eq!(
            decide(
                KeyCode::Enter,
                KeyModifiers::NONE,
                View::Turns,
                Some(9),
                &refs,
                &brefs
            ),
            Step::Stay
        );
    }

    #[test]
    fn quitting_takes_the_usual_three_keys() {
        let (t, b) = (vec![], vec![]);
        for (code, mods) in [
            (KeyCode::Char('q'), KeyModifiers::NONE),
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(decide(code, mods, View::Turns, None, &t, &b), Step::Quit);
        }
        // Cursor keys do not change screen state; movement is left to `move_cursor`.
        assert_eq!(
            decide(KeyCode::Down, KeyModifiers::NONE, View::Turns, None, &t, &b),
            Step::Stay
        );
    }

    /// This pins that a narrow width cuts the **message** while `#n`, the sha and the kind lose
    /// not one character.
    ///
    /// Those three locate the turn — they are what gets `agit show` to it; the message is only a
    /// hint. A plausible wrong implementation cuts from the end of the row, which takes the time
    /// and the tags first and the message last: exactly backwards.
    #[test]
    fn a_narrow_row_gives_up_the_message_not_the_identifiers() {
        let mut t = turn(
            14,
            "fix the refund retry idempotency key, add a regression test and a note",
        );
        t.tags = vec!["v0.3".into()];
        let wide = turn_line(&t, 120).to_string();
        assert!(wide.contains("# 14"), "{wide}");
        assert!(wide.contains(&t.short), "{wide}");
        assert!(wide.contains("turn"), "{wide}");
        assert!(wide.contains("⌂ v0.3"), "{wide}");
        assert!(
            wide.contains("regression test"),
            "a wide row keeps the message: {wide}"
        );

        let narrow = turn_line(&t, 44).to_string();
        assert!(
            narrow.contains("# 14"),
            "the ordinal must not be cut: {narrow}"
        );
        assert!(
            narrow.contains(&t.short),
            "the sha must not be cut: {narrow}"
        );
        assert!(
            narrow.contains("⌂ v0.3"),
            "the tags must not be cut: {narrow}"
        );
        assert!(
            !narrow.contains("regression test"),
            "the message is what gets cut: {narrow}"
        );
    }

    /// This pins that a whole row stays inside the **columns** it was given.
    ///
    /// CJK rows are the entire reason for it: on a character budget `turn_line` believes the row
    /// fits while the renderer cuts by column — from the **end**, so `ago` and the tags go first.
    /// On a 120-column terminal a CJK subject pushes `9d ago` outside the frame.
    #[test]
    fn a_cjk_row_still_fits_the_columns_it_was_given() {
        let mut t = turn(
            14,
            "上传自己：从真实 Session 出发，对 Agent 说把你自己上传到 AgentGitHub",
        );
        t.tags = vec!["v0.3".into()];
        // The sweep starts where **even the fields that never yield do not fit**: from 40 columns
        // up the threshold is already crossed, and the widths that overflow most easily go
        // untested.
        for width in [10u16, 20, 26, 30, 40, 60, 80, 100, 120] {
            let line = turn_line(&t, width).to_string();
            let used = widgets::cols(&line);
            // The frame and the highlight symbol stay out of the budget, otherwise a CJK row
            // lands exactly on the right border.
            let room = width as usize - BORDER_AND_MARKER;
            assert!(
                used <= room,
                "a {width}-column row drew {used} columns (only {room} available): {line}"
            );
        }
        // A pure-ASCII row must be neither shortened nor over-wide.
        let ascii = turn_line(&turn(14, &"x".repeat(200)), 100).to_string();
        assert!(widgets::cols(&ascii) <= 100 - BORDER_AND_MARKER, "{ascii}");
        assert!(ascii.contains("# 14"), "{ascii}");
    }

    /// This pins that the file line is marked: it claims no session, and Enter leads to a history
    /// with no turns.
    #[test]
    fn a_file_line_says_so_and_a_diverged_branch_shows_its_counts() {
        let plain = branch_line(&branch("refund-fix"), 120).to_string();
        assert!(!plain.contains("file line"), "{plain}");
        assert!(
            !plain.contains("↑↓"),
            "a branch in sync must not take that space: {plain}"
        );

        let odd = branch_line(
            &BranchRow {
                file_line: true,
                ahead_behind: "3/1".into(),
                ..branch("main")
            },
            120,
        )
        .to_string();
        assert!(odd.contains("[file line]"), "{odd}");
        assert!(odd.contains("↑↓ 3/1"), "{odd}");
    }

    /// This pins that a branch row does not go over-wide either, and that `↑↓` comes last in the
    /// yield order.
    ///
    /// Two places at once: at the data layer `gist` is truncated to 60 **characters**
    /// (`ui::truncate` in `branch_rows` — that is the text-output constraint and does not move),
    /// and 60 CJK characters are 120 columns; pad the name field with `{:<24}` and it pads by
    /// character, so a 12-character CJK name stretches to 36 columns. Together those would push a
    /// row to 165 columns — and what gets pushed out is exactly `[file line]` and `↑↓`, and `↑↓` is the
    /// divergence warning: unseen means unhandled.
    #[test]
    fn a_cjk_branch_row_keeps_its_divergence_warning() {
        // CJK width fixtures: the column arithmetic below is exactly what this test pins.
        let wide = BranchRow {
            name: "重构错误处理层".into(), // 14 columns
            gist: "重".repeat(60),         // 120 columns
            file_line: true,
            ahead_behind: "3/2".into(),
            ..branch("x")
        };
        // "never overflows" is **unconditional**, so the sweep starts where even the fields that
        // never yield do not fit — from 40 columns up the threshold is already crossed and the
        // widths that overflow most easily go untested.
        for width in [10u16, 20, 29, 30, 40, 60, 80, 100, 120, 200] {
            let line = branch_line(&wide, width).to_string();
            let room = width as usize - BORDER_AND_MARKER;
            let used = widgets::cols(&line);
            assert!(
                used <= room,
                "a {width}-column row drew {used} columns: {line}"
            );
        }
        // "the fields that come last are still there" is **conditional**: the width has to hold
        // them. Below it no field can yield any further, the last-resort truncation takes over,
        // and it cuts from the end of the row — `↑↓` goes first. The two guarantees hold over
        // different ranges; in one loop, one of them would look stronger than it is.
        for width in [30u16, 40, 60, 80, 100, 120, 200] {
            let line = branch_line(&wide, width).to_string();
            assert!(
                line.contains("↑↓ 3/2"),
                "{width} columns keep the divergence warning: {line}"
            );
            assert!(
                line.contains("3 turns"),
                "{width} columns keep the turn count: {line}"
            );
            assert!(
                line.contains("重构"),
                "{width} columns keep the name readable: {line}"
            );
        }
        // `[file line]` comes before `↑↓` in the yield order, so it goes first at the narrowest
        // width.
        assert!(!branch_line(&wide, 40).to_string().contains("[file line]"));
        assert!(branch_line(&wide, 60).to_string().contains("[file line]"));
        // On a wide terminal the gist really is shown, rather than uniformly dropped for safety.
        let roomy = branch_line(&wide, 200).to_string();
        assert!(
            roomy.contains('“'),
            "a wide terminal shows the gist: {roomy}"
        );
        // The name field pads by column: the turn count right after a CJK name still lines up.
        let aligned = branch_line(&wide, 200).to_string();
        let ascii = branch_line(
            &BranchRow {
                name: "abc".into(),
                ..wide.clone()
            },
            200,
        )
        .to_string();
        assert_eq!(
            widgets::cols(aligned.split("turns").next().unwrap()),
            widgets::cols(ascii.split("turns").next().unwrap()),
            "the turn count lands in the same column for a CJK row and an ASCII row"
        );
    }

    /// This pins that the detail strip carries only what does not fit in the row, and that with
    /// nothing to carry it says so instead of leaving a blank.
    #[test]
    fn the_detail_strip_says_when_there_is_nothing_to_say() {
        let bare = strip_text(Some(&turn(1, "x")));
        assert!(bare.contains("no code anchor"), "{bare}");
        let rich = strip_text(Some(&Turn {
            code: Some("git@h:nana/payments.git@1839e61".into()),
            milestone: Some("v0.3 shipped".into()),
            ..turn(1, "x")
        }));
        assert!(
            rich.contains("code git@h:nana/payments.git@1839e61"),
            "{rich}"
        );
        assert!(rich.contains("★ v0.3 shipped"), "{rich}");
        assert_eq!(strip_text(None), "nothing matches this filter.");
    }

    /// A real render: the newest turn is on top and the key hints follow the view.
    #[test]
    fn the_frame_puts_the_newest_turn_first() {
        let screen = Screen {
            slug: "nana/payments".into(),
            branch: "refund-fix".into(),
            turns: vec![turn(1, "oldest"), turn(2, "middle"), turn(3, "newest")],
            branches: None,
            view: View::Turns,
        };
        // In the event loop `turns` is filtered in reverse order; the same is done here.
        let ordered: Vec<&Turn> = screen.turns.iter().rev().collect();
        let mut state = ListState::default();
        state.select(Some(0));
        let mut term = Terminal::new(TestBackend::new(100, 10)).unwrap();
        term.draw(|f| {
            draw(
                f,
                &screen,
                &ordered,
                &[],
                &mut state,
                &Filter::default(),
                None,
            )
        })
        .unwrap();
        let rows = rows_of(term.backend());
        assert!(rows[0].contains("agit log"), "{:?}", rows[0]);
        assert!(
            rows[0].contains("nana/payments @ refund-fix"),
            "{:?}",
            rows[0]
        );
        assert!(rows[1].contains("turns (3)"), "{:?}", rows[1]);
        assert!(
            rows[2].contains("newest"),
            "the newest turn is on top: {:?}",
            rows[2]
        );
        assert!(rows[4].contains("oldest"), "{:?}", rows[4]);
        assert!(rows[9].contains("tab branches"), "{:?}", rows[9]);
    }

    /// This pins that a terminal too narrow **says so** instead of handing over a screenful of
    /// unidentifiable rows.
    ///
    /// The last-resort truncation makes "never crosses the frame" hold unconditionally; the cost
    /// is that it cuts from the end of the row — straight into the locating fields and the
    /// divergence warning. At this width the two invariants cannot both hold, so the whole list
    /// yields: a screenful of rows that look selectable but cannot be cited is worse than the one
    /// sentence "too narrow".
    #[test]
    fn a_terminal_too_narrow_to_identify_a_turn_says_so() {
        let screen = Screen {
            slug: "nana/payments".into(),
            branch: "refund-fix".into(),
            // CJK width fixture (AGENTS.md exception iii): the narrow widths swept below only
            // bite while the subject costs two columns per character.
            turns: vec![turn(14, "修掉退款重试")],
            branches: None,
            view: View::Turns,
        };
        let ordered: Vec<&Turn> = screen.turns.iter().collect();
        let paint = |w: u16| {
            let mut state = ListState::default();
            state.select(Some(0));
            let mut term = Terminal::new(TestBackend::new(w, 10)).unwrap();
            term.draw(|f| {
                draw(
                    f,
                    &screen,
                    &ordered,
                    &[],
                    &mut state,
                    &Filter::default(),
                    None,
                )
            })
            .unwrap();
            rows_of(term.backend()).join("\n")
        };

        let narrow = paint(MIN_USABLE_WIDTH - 1);
        assert!(narrow.contains("too narrow"), "{narrow}");
        assert!(
            !narrow.contains("#"),
            "unidentifiable rows are not drawn: {narrow}"
        );

        // At exactly the usable width it draws as usual, with the locating fields present.
        let ok = paint(MIN_USABLE_WIDTH);
        assert!(!ok.contains("too narrow"), "{ok}");
        assert!(ok.contains("# 14"), "{ok}");
    }

    /// This pins that the branch view relabels its keys — `enter` here opens that branch's
    /// turn-by-turn history.
    #[test]
    fn the_branch_view_relabels_its_keys() {
        let rows = vec![branch("refund-fix")];
        let screen = Screen {
            slug: "nana/payments".into(),
            branch: "refund-fix".into(),
            turns: vec![],
            branches: Some(rows.clone()),
            view: View::Branches,
        };
        let brefs: Vec<&BranchRow> = rows.iter().collect();
        let mut state = ListState::default();
        state.select(Some(0));
        let mut term = Terminal::new(TestBackend::new(100, 10)).unwrap();
        term.draw(|f| {
            draw(
                f,
                &screen,
                &[],
                &brefs,
                &mut state,
                &Filter::default(),
                None,
            )
        })
        .unwrap();
        let out = rows_of(term.backend());
        assert!(out[1].contains("branches (1)"), "{:?}", out[1]);
        assert!(out[2].contains("refund-fix"), "{:?}", out[2]);
        assert!(out[9].contains("enter turns"), "{:?}", out[9]);
        assert!(out[9].contains("tab timeline"), "{:?}", out[9]);
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
