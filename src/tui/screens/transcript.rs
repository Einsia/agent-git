//! Transcript screen: renders a stretch of a session as a conversation.
//!
//! Two entry points share this screen:
//!
//! * `agit show --tui` — lists the sessions in a repo (`commands::show` enters here);
//! * Enter in the Timeline — lists that branch turn by turn, with the selected turn's
//!   conversation on the right.
//!
//! They differ only in **what the left pane lists** and **where the text comes from**, so both
//! merge into one [`Entry`] list: rendering, keys and the cache each exist once.
//!
//! # Rules this screen holds to
//!
//! 1. **Terminal state goes through RAII**, not "remember to call it at the end of the
//!    function" — panics and early returns both have to be covered.
//!    [`crate::tui::term::Guard`] also shows the cursor again; a guard that only does
//!    `LeaveAlternateScreen` hands back a terminal with no cursor.
//! 2. **Only `KeyEventKind::Press` counts**, or one keypress moves two rows in a Windows
//!    terminal. [`crate::tui::term::next_key`] guarantees this in one place.
//! 3. **Transcripts parse on demand and are cached**: reparsing a large file on every cursor
//!    move hangs the screen.

use crate::domain::meta;
use crate::domain::repo::Repo;
use crate::tui::widgets::{self, Filter};
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Margin;
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use std::path::PathBuf;
use std::time::SystemTime;

/// How many characters of each message survive when a conversation is rendered.
///
/// Not the same number as the default for `agit show`'s line rendering: that one feeds a pipe
/// and may run arbitrarily long, while here every cursor move re-renders, and one over-long
/// message packs the whole screen into a wall.
const MESSAGE_CHARS: usize = 1500;

/// Which half a narrow terminal is showing.
///
/// With no room for two panes, what this screen is for — **reading** — has nowhere to land.
/// The `MIN_TWO_PANE_WIDTH` trade-off is written as "one pane plus one Enter to expand"; this
/// is that Enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    List,
    Conversation,
}

/// One row in the left pane, and the source of the text in the right one.
#[derive(Debug, Clone)]
pub struct Entry {
    /// The row's main identity: a session id prefix, or `#n` plus a short sha.
    pub label: String,
    /// The supporting line: the runtime, or this turn's message.
    pub note: String,
    pub when: SystemTime,
    source: Source,
}

#[derive(Debug, Clone)]
enum Source {
    /// A session inside a repo: the transcript is an **envelope** sitting in some worktree, so
    /// it is materialized first and the envelope unwrapped after.
    Worktree { root: PathBuf, runtime: String },
    /// The transcript of one session branch in a repo: materialized at that branch's own ref,
    /// never depending on which branch happens to be checked out — the main checkout stays on
    /// `main`, so reading through the checkout always yields `main`'s LOG.
    Branch {
        repo: PathBuf,
        branch: String,
        runtime: String,
    },
    /// The **runtime-native** transcript a store link points at: one file, read directly.
    ///
    /// Kept apart from [`Source::Worktree`] for a reason, not out of tidiness: that path goes
    /// through `materialize_worktree` looking for AgentGit's storage layout, while a native
    /// transcript's parent is the runtime's own session directory, which has none — the symptom
    /// is every row reporting that it cannot read the LOG.
    Native { path: PathBuf, runtime: String },
    /// One turn: that turn's LOG events, taken from the repo's object store.
    ///
    /// It takes **this turn**, not the whole VIEW at this point — the key table in
    /// `docs/07_tui.md` §3.3 reads "Enter shows this turn", and it goes through the same
    /// function as `agit show <ref>#n` ([`crate::commands::show::turn_envelopes`]), so the two
    /// never diverge.
    Turn {
        repo: PathBuf,
        sha: String,
        /// The turn ordinal. `None` = this commit settled no turn (birth, fork, file, merge),
        /// so there is no "this turn" to read — not empty, absent.
        turn: Option<u32>,
    },
}

impl Entry {
    /// The text the filter matches against.
    pub fn haystack(&self) -> String {
        format!("{} {}", self.label, self.note)
    }
}

/// Renders a session as conversation text. **Slow** — the caller must cache it (rule 3 in the
/// module comment).
fn text_of(entry: &Entry) -> String {
    match &entry.source {
        Source::Worktree { root, runtime } => {
            let Ok(envelopes) = crate::domain::storage::materialize_worktree(root, meta::LOG_FILE)
            else {
                return "(can’t read this session’s LOG)".into();
            };
            render(
                &crate::domain::transcript::unwrap_lossy(&envelopes).0,
                runtime,
            )
        }
        Source::Branch {
            repo,
            branch,
            runtime,
        } => {
            let Ok(envelopes) = crate::domain::storage::materialize_at(
                repo,
                &format!("refs/heads/{branch}"),
                meta::LOG_FILE,
            ) else {
                return "(can’t read this session’s LOG)".into();
            };
            render(
                &crate::domain::transcript::unwrap_lossy(&envelopes).0,
                runtime,
            )
        }
        Source::Native { path, runtime } => match std::fs::read_to_string(path) {
            Ok(text) => render(&text, runtime),
            Err(e) => format!("(can’t read {}: {e})", path.display()),
        },
        Source::Turn { repo, sha, turn } => {
            let Some(repo) = Repo::open(repo) else {
                return "(this repository is gone)".into();
            };
            let Some(turn) = *turn else {
                return "(this commit did not settle a turn — nothing to read)".into();
            };
            match crate::commands::show::turn_envelopes(&repo, sha, turn) {
                Ok(events) => {
                    let (text, _) = crate::domain::transcript::unwrap_lossy(&events.concat());
                    render(&text, "")
                }
                // An unreadable turn says so. A birth commit, a merge, a commit on the file
                // line can all carry no turn events; that is not an error, and it must not be
                // dressed up as an empty turn either.
                Err(e) => format!("(no turn {turn} to read at this point: {e:#})"),
            }
        }
    }
}

/// Raw text → parse → conversation. An unrecognized runtime is guessed from the content; when
/// the guess fails, the raw text is handed back.
fn render(text: &str, hint: &str) -> String {
    let rt =
        crate::adapter::infer_runtime(text).unwrap_or(if hint.is_empty() { "codex" } else { hint });
    let rendered = crate::adapter::get(rt)
        .and_then(|a| a.parse(text))
        .map(|parsed| crate::ui::transcript::render_transcript(&parsed, MESSAGE_CHARS))
        .unwrap_or_default();
    if !rendered.trim().is_empty() {
        return rendered;
    }
    // Two ways to get here: the parse errored, or it "succeeded" while recognizing no event at
    // all (that is what an adapter returns for text it does not know, **without an error**).
    // Neither may render blank: blank reads as "this turn has no content", when the truth is
    // that we could not read it.
    if text.trim().is_empty() {
        "(nothing recorded at this point)".into()
    } else {
        text.to_string()
    }
}

/// Backs out the worktree root that holds a transcript path.
///
/// This list comes only from a repo, so the path is always an envelope transcript, but the two
/// layouts sit at different depths: v0 at `<root>/session/log.jsonl` (back out two levels), v1
/// at `<root>/session/` (back out one). Off by one level materializes into a directory that
/// does not exist, and all that reaches the reader is that the LOG cannot be read — which looks
/// like broken data, not like arithmetic on a path.
fn worktree_root(path: &std::path::Path) -> PathBuf {
    let up = if path.ends_with(meta::LEGACY_LOG_FILE) {
        path.parent().and_then(|p| p.parent())
    } else {
        path.parent()
    };
    up.unwrap_or(path).to_path_buf()
}

/// `agit show --tui --agent <slug>`: lists the sessions in that repo (transcripts are
/// envelopes).
pub fn browse_repo(
    repo: &Repo,
    sessions: &[crate::domain::session::Stored],
    start: usize,
) -> crate::CmdResultAlias {
    browse_with(sessions, start, |s| repo_source(repo, s))
}

/// Where one session in a repo listing is read from: with a branch, through that branch's ref;
/// without one (the legacy form), by backing out the worktree from the transcript path.
///
/// Every row carries its own branch, while `path` is the same placeholder in the main checkout
/// for all of them — reading by path shows the main checkout's LOG whichever branch is
/// selected.
fn repo_source(repo: &Repo, s: &crate::domain::session::Stored) -> Source {
    match &s.branch {
        Some(branch) => Source::Branch {
            repo: repo.root().to_path_buf(),
            branch: branch.clone(),
            runtime: s.runtime.clone(),
        },
        None => Source::Worktree {
            root: worktree_root(&s.path),
            runtime: s.runtime.clone(),
        },
    }
}

/// `agit show --tui` (no agent named): lists the native transcripts the store links point at.
pub fn browse_native(
    sessions: &[crate::domain::session::Stored],
    start: usize,
) -> crate::CmdResultAlias {
    browse_with(sessions, start, |s| Source::Native {
        path: s.path.clone(),
        runtime: s.runtime.clone(),
    })
}

fn browse_with(
    sessions: &[crate::domain::session::Stored],
    start: usize,
    source: impl Fn(&crate::domain::session::Stored) -> Source,
) -> crate::CmdResultAlias {
    let entries: Vec<Entry> = sessions
        .iter()
        .map(|s| Entry {
            label: s.id.chars().take(8).collect(),
            note: s.runtime.clone(),
            when: s.mtime,
            source: source(s),
        })
        .collect();
    run(entries, start, "sessions")
}

/// Enter in the Timeline: lists that branch turn by turn, with the selected turn's
/// conversation on the right.
///
/// **Takes no terminal** — the caller's (the Timeline's) guard is still up, so this screen runs
/// inside the same alternate screen. Another `Guard::enter()` pushes one more alternate screen,
/// and leaving that one wipes what the Timeline drew. Reading returns straight to the Timeline,
/// cursor position and filter intact.
pub fn read_turns_inline(
    repo: &Repo,
    turns: &[crate::commands::log::Turn],
    start: usize,
) -> crate::Result<()> {
    let entries: Vec<Entry> = turns
        .iter()
        .map(|t| Entry {
            label: match t.turn {
                Some(n) => format!("#{n:>3} {}", t.short),
                None => format!("     {}", t.short),
            },
            note: t.subject.clone(),
            when: t.at,
            source: Source::Turn {
                repo: repo.root().to_path_buf(),
                sha: t.short.clone(),
                turn: t.turn,
            },
        })
        .collect();
    if entries.is_empty() {
        return Ok(());
    }
    event_loop(&entries, start, "turns")?;
    Ok(())
}

/// List plus conversation, until the reader presses q.
fn run(entries: Vec<Entry>, start: usize, title: &str) -> crate::CmdResultAlias {
    if entries.is_empty() {
        println!("nothing to read here.");
        return Ok(crate::ExitCode::Ok);
    }
    let guard = crate::tui::term::Guard::enter()?;
    let out = event_loop(&entries, start, title);
    // Give the terminal back before letting the result propagate: those words belong on the
    // normal screen.
    drop(guard);
    out
}

fn event_loop(entries: &[Entry], start: usize, title: &str) -> crate::CmdResultAlias {
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    // Clear once before taking over the screen.
    //
    // ratatui writes only the cells that differ from **its own** previous frame, and a freshly
    // built `Terminal` has a blank one — so every cell that should be a space here counts as
    // unchanged and is skipped, and the screen underneath (the Timeline) shows through the
    // gaps. This screen stacks onto the Timeline's alternate screen (see `read_turns_inline`);
    // without the clear the two are printed over each other.
    term.clear()?;
    let mut state = ListState::default();
    state.select(Some(start.min(entries.len() - 1)));
    let mut filter = Filter::default();
    // Parse on demand and cache: reparsing a large file on every cursor move hangs the screen.
    // The key is **the row's own identity**, not its index — the filter changes indices, so an
    // index key crosses one row with another.
    let mut cache: std::collections::HashMap<String, String> = Default::default();
    let mut scroll: u16 = 0;
    // Only meaningful in a narrow terminal: with both panes up the conversation is always
    // visible.
    let mut focus = Focus::List;

    loop {
        let view: Vec<&Entry> = entries
            .iter()
            .filter(|e| filter.matches(&e.haystack()))
            .collect();
        if state.selected().unwrap_or(0) >= view.len() {
            state.select((!view.is_empty()).then(|| view.len() - 1));
            scroll = 0;
        }
        // Nothing cached for this row means **draw a frame first, then read**.
        //
        // Reading a turn is not instant (it costs what `agit show <ref>#n` costs, in
        // `merge::turn_lines` and in materialization). Reading before drawing leaves the screen
        // frozen on the blank it just cleared, which reads as a hang — while the screen has a
        // way to say it is reading.
        if let Some(e) = state.selected().and_then(|i| view.get(i))
            && !cache.contains_key(&e.label)
        {
            term.draw(|f| {
                draw(
                    f,
                    &Pane {
                        title,
                        filter: &filter,
                        cache: &cache,
                        scroll,
                        focus,
                    },
                    &view,
                    &mut state,
                )
            })?;
            let text = text_of(e);
            cache.insert(e.label.clone(), text);
        }
        term.draw(|f| {
            draw(
                f,
                &Pane {
                    title,
                    filter: &filter,
                    cache: &cache,
                    scroll,
                    focus,
                },
                &view,
                &mut state,
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
        let n = view.len();
        let before = state.selected();
        match key.code {
            // In a narrow terminal Esc collapses the conversation first and only quits on the
            // next press — expanding is one step, so is going back.
            KeyCode::Esc if focus == Focus::Conversation => focus = Focus::List,
            KeyCode::Char('q') | KeyCode::Esc => return Ok(crate::ExitCode::Ok),
            // With room for two panes the conversation is always up and this key changes
            // nothing.
            KeyCode::Enter => focus = Focus::Conversation,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(crate::ExitCode::Ok);
            }
            KeyCode::Char('/') => filter.open(),
            KeyCode::Down | KeyCode::Char('j') => {
                state.select(Some(
                    (state.selected().unwrap_or(0) + 1).min(n.saturating_sub(1)),
                ));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.select(Some(state.selected().unwrap_or(0).saturating_sub(1)));
            }
            KeyCode::Char('g') | KeyCode::Home => state.select(Some(0)),
            KeyCode::Char('G') | KeyCode::End => state.select(Some(n.saturating_sub(1))),
            // The conversation itself scrolls — a session runs well past one screen, and
            // content that cannot be reached is content this screen does not carry.
            KeyCode::PageDown | KeyCode::Char('f') => scroll = scroll.saturating_add(10),
            KeyCode::PageUp | KeyCode::Char('b') => scroll = scroll.saturating_sub(10),
            _ => {}
        }
        // A new row starts from the top, not at the previous row's scroll position.
        if state.selected() != before {
            scroll = 0;
        }
    }
}

/// How far down the conversation pane can scroll.
///
/// Counted in the lines **the renderer itself produces** (`Paragraph::line_count`), not in the
/// newlines of the source text. One source line of tool output or JSON routinely wraps into
/// dozens of screen lines; counting newlines yields "no scrolling needed", and everything below
/// it is pushed out of the viewport and can never be reached.
///
/// The question goes to the same `Paragraph` configuration: ratatui holds the only copy of the
/// wrapping rules, a second copy drifts apart from it sooner or later, and the symptom of that
/// drift is exactly the kind of bug nobody reports — the last lines are missing.
fn max_scroll(paragraph: &Paragraph<'_>, area: Rect) -> u16 {
    let inner = area.inner(Margin::new(1, 1));
    let lines = paragraph.line_count(inner.width) as u16;
    lines.saturating_sub(inner.height)
}

/// The interface state one frame needs.
///
/// Gathering it into a struct is not only about writing fewer parameters: once `draw` takes
/// many arguments, a call site is a run of interchangeable positional values, and the compiler
/// does not stop the wrong order.
struct Pane<'a> {
    title: &'a str,
    filter: &'a Filter,
    cache: &'a std::collections::HashMap<String, String>,
    scroll: u16,
    focus: Focus,
}

fn draw(f: &mut Frame, pane: &Pane<'_>, view: &[&Entry], state: &mut ListState) {
    let Pane {
        title,
        filter,
        cache,
        scroll,
        focus,
    } = *pane;
    let panes = widgets::layout(f.area());
    // Narrow terminal: the body is one block, and focus decides whether it holds the list or
    // the conversation. Skipping the conversation whenever two panes do not fit leaves a table
    // of contents that cannot be read.
    let narrow = panes.detail.is_none();
    let reading = narrow && focus == Focus::Conversation;
    widgets::render_status(
        f,
        panes.status,
        &widgets::Status {
            title: "agit show".into(),
            identity: Some(format!("{} {}", view.len(), title)),
            rc_online: None,
            counters: widgets::Counters::default(),
        },
    );

    let items: Vec<ListItem> = view
        .iter()
        .map(|e| {
            ListItem::new(vec![
                Line::from(Span::styled(
                    e.label.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                Line::from(Span::styled(
                    format!(
                        "  {}",
                        widgets::truncate_cols(
                            &e.note,
                            panes.list.width.saturating_sub(4) as usize
                        )
                    ),
                    theme::muted(),
                )),
            ])
        })
        .collect();
    let list_title = match filter.hint() {
        Some(q) => format!("{title}  {q}"),
        None => format!("{title} ({})", view.len()),
    };
    if !reading {
        f.render_stateful_widget(
            List::new(items)
                .block(widgets::pane(&list_title))
                .highlight_style(theme::selected())
                .highlight_symbol("▸ "),
            panes.list,
            state,
        );
    }

    // A wide terminal uses the right pane; a narrow one, once expanded, uses the whole body.
    if let Some(area) = panes.detail.or(reading.then_some(panes.list)) {
        // "nothing selected" and "not read yet" are two different things and must not be
        // conflated into one line: the former is a filter that matched nothing, the latter is
        // this screen working. Saying the wrong one costs the reader believing the interface is
        // broken.
        let text = match state.selected().and_then(|i| view.get(i)) {
            None => "nothing matches this filter.",
            Some(e) => cache
                .get(&e.label)
                .map(String::as_str)
                .unwrap_or("reading…"),
        };
        let body = Paragraph::new(text)
            .block(widgets::pane("conversation"))
            .wrap(Wrap { trim: false });
        let capped = scroll.min(max_scroll(&body, area));
        f.render_widget(body.scroll((capped, 0)), area);
    }

    widgets::render_footer(
        f,
        panes.footer,
        match (filter.is_active(), narrow, reading) {
            (true, ..) => "type to filter   enter apply   esc cancel",
            // On a narrow terminal, how to see the content is the first thing this screen
            // says.
            (_, true, false) => "↑↓ move   enter read   / filter   q quit",
            (_, true, true) => "f/b scroll   esc back   q quit",
            (_, false, _) => "↑↓ move   f/b scroll   / filter   q quit",
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn entry(label: &str, note: &str) -> Entry {
        Entry {
            label: label.into(),
            note: note.into(),
            when: SystemTime::UNIX_EPOCH,
            source: Source::Turn {
                repo: PathBuf::from("/nope"),
                sha: "deadbeef".into(),
                turn: Some(1),
            },
        }
    }

    /// This pins the scroll cap to the line count **after rendering**, not to the newlines in
    /// the source text.
    ///
    /// The shape it watches is the worst one: a single very long source line (tool output,
    /// JSON, a stretch of Markdown) wrapping into dozens of screen lines. Counting newlines
    /// says "one line, no scrolling", and everything below it is pushed out of the viewport and
    /// can never be reached — not merely a screen that stops a few lines short.
    #[test]
    fn scrolling_reaches_content_pushed_down_by_one_very_long_line() {
        let area = Rect::new(0, 0, 24, 10); // content area 22 columns × 8 rows
        let para = |t: &'static str| {
            Paragraph::new(t)
                .block(widgets::pane("conversation"))
                .wrap(Wrap { trim: false })
        };

        // Under one screen: nothing to scroll.
        assert_eq!(max_scroll(&para("one\ntwo\n"), area), 0);

        // One source line, dozens of screens of content: counting newlines yields 0, and that
        // is what this test blocks.
        let long: &'static str = Box::leak("x".repeat(22 * 30).into_boxed_str());
        let cap = max_scroll(&para(long), area);
        assert!(cap > 0, "content below a wrapped line must be reachable");
        assert!(
            cap >= 30 - 8,
            "the cap must cover the height after wrapping; got {cap}"
        );

        // A pane shorter than its own border must not underflow.
        assert_eq!(max_scroll(&para("one"), Rect::new(0, 0, 24, 1)), 0);
    }

    /// This pins that an unreadable point says so instead of going blank.
    ///
    /// Blank reads as "this turn has no content", while the truth is "we cannot read it" — two
    /// different things.
    #[test]
    fn an_unreadable_point_says_so_instead_of_going_blank() {
        let text = text_of(&entry("#1 deadbeef", "x"));
        assert!(!text.trim().is_empty(), "blank reads as an empty turn");
        assert!(text.contains("gone") || text.contains("no turn"), "{text}");
    }

    /// This pins that unparsable text is handed back verbatim rather than turning into blank.
    #[test]
    fn text_that_cannot_be_parsed_is_shown_raw() {
        let raw = "this is not any runtime's transcript format";
        assert!(render(raw, "").contains(raw));
        assert!(render("", "").contains("nothing recorded"));
    }

    /// This pins the cache key to **the row's own identity**, not to its index.
    ///
    /// With an index key, one `/` filter changes the view and the same index points at another
    /// row — the right pane then shows the conversation cached for a different row, with no
    /// symptom.
    #[test]
    fn the_cache_is_keyed_by_the_row_not_by_its_position() {
        let mut cache: std::collections::HashMap<String, String> = Default::default();
        let (a, b) = (entry("#1 aaa", "first"), entry("#2 bbb", "second"));
        cache.insert(a.label.clone(), "conversation A".into());
        cache.insert(b.label.clone(), "conversation B".into());
        // After filtering, b is row 0; looked up by identity it is still B.
        assert_eq!(cache.get(&b.label).unwrap(), "conversation B");
        assert_ne!(a.label, b.label);
    }

    /// This pins that every session branch in the listing reads **its own** transcript while
    /// the main checkout sits on `main`.
    ///
    /// Every row's `path` is the same placeholder in the main checkout; reading by path shows
    /// the main checkout's LOG whichever row is selected.
    #[test]
    fn each_session_branch_reads_its_own_transcript_while_main_is_checked_out() {
        use crate::domain::meta::Meta;
        use crate::domain::repo::Repo;
        use crate::domain::{session, storage, transcript};
        let d = tempfile::tempdir().unwrap();
        let r = Repo::init(&d.path().join("a")).unwrap();
        r.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(r.root(), &Meta::new_file_line()).unwrap();
        r.add_all().unwrap();
        r.commit("init").unwrap();
        for (branch, digit, prompt) in [("s1", "1", "PROMPT-ONE"), ("s2", "2", "PROMPT-TWO")] {
            r.git(&["checkout", "--quiet", "-b", branch, "main"])
                .unwrap();
            let claim = format!("{}{}", meta::ID_PREFIX, digit.repeat(meta::ID_HEX_LEN));
            let line = format!(
                "{{\"type\":\"user\",\"sessionId\":\"{branch}\",\"message\":{{\"role\":\"user\",\"content\":\"{prompt}\"}}}}\n"
            );
            let env = transcript::wrap_lines(&line, "claude-code", &claim);
            meta::ensure_session_dir(r.root()).unwrap();
            storage::write_snapshot(r.root(), &env, &env).unwrap();
            meta::write(
                r.root(),
                &Meta::new(claim, "claude-code".into(), "/r".into()),
            )
            .unwrap();
            r.add_all().unwrap();
            r.commit(branch).unwrap();
        }
        r.git(&["checkout", "--quiet", "main"]).unwrap();

        let sessions = session::list(&r);
        assert_eq!(sessions.len(), 2);
        for s in &sessions {
            let (own, other) = match s.branch.as_deref() {
                Some("s1") => ("PROMPT-ONE", "PROMPT-TWO"),
                Some("s2") => ("PROMPT-TWO", "PROMPT-ONE"),
                b => panic!("unexpected branch {b:?}"),
            };
            let text = text_of(&Entry {
                label: s.id.clone(),
                note: String::new(),
                when: s.mtime,
                source: repo_source(&r, s),
            });
            assert!(text.contains(own), "{own}: {text}");
            assert!(!text.contains(other), "{own} must not show {other}: {text}");
        }
    }

    /// This pins how many levels are backed out for v0 and v1 transcripts, which sit at
    /// different depths.
    ///
    /// Off by one level materializes into a directory that does not exist, and all the reader
    /// sees is that the LOG cannot be read — which looks like broken data, not like arithmetic
    /// on a path.
    #[test]
    fn the_worktree_root_backs_out_the_right_number_of_levels() {
        // v1: `<root>/session/`
        assert_eq!(
            worktree_root(&PathBuf::from("/w/repo/session")),
            PathBuf::from("/w/repo")
        );
        // v0: `<root>/session/log.jsonl`
        assert_eq!(
            worktree_root(&PathBuf::from(format!("/w/repo/{}", meta::LEGACY_LOG_FILE))),
            PathBuf::from("/w/repo")
        );
        // With no parent directory: no panic, and no empty path.
        assert_eq!(worktree_root(&PathBuf::from("/")), PathBuf::from("/"));
    }

    /// This pins that a narrow terminal can still reach the transcript.
    ///
    /// With no room for two panes the detail pane is `None`, and rendering only the list would
    /// leave a table of contents that cannot be opened — while reading is the reason this
    /// screen exists. Enter expands, Esc collapses.
    #[test]
    fn a_narrow_terminal_can_still_open_the_conversation() {
        // CJK width fixture (AGENTS.md exception iii): a wide-character subject is what fills the
        // narrow frame this case paints into.
        let entries = [entry("#14 3f2a1bc", "修掉退款重试的幂等键")];
        let view: Vec<&Entry> = entries.iter().collect();
        let mut cache: std::collections::HashMap<String, String> = Default::default();
        cache.insert(entries[0].label.clone(), "you: fix it\nagent: ok".into());
        let mut state = ListState::default();
        state.select(Some(0));

        let paint = |focus: Focus, state: &mut ListState| {
            let mut term =
                Terminal::new(TestBackend::new(widgets::MIN_TWO_PANE_WIDTH - 20, 12)).unwrap();
            term.draw(|f| {
                draw(
                    f,
                    &Pane {
                        title: "turns",
                        filter: &Filter::default(),
                        cache: &cache,
                        scroll: 0,
                        focus,
                    },
                    &view,
                    state,
                )
            })
            .unwrap();
            let buf = term.backend().buffer().clone();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let listing = paint(Focus::List, &mut state);
        assert!(listing.contains("#14 3f2a1bc"), "{listing}");
        assert!(
            listing.contains("enter read"),
            "a narrow terminal must first say how to read: {listing}"
        );

        let reading = paint(Focus::Conversation, &mut state);
        assert!(
            reading.contains("you:"),
            "expanding must show the content: {reading}"
        );
        assert!(
            reading.contains("esc back"),
            "it must also say how to go back: {reading}"
        );
    }

    /// This pins a real frame: both panes, the titles and the key line.
    #[test]
    fn the_frame_shows_the_list_and_the_conversation() {
        // CJK width fixture (AGENTS.md exception iii): the two rows pair a wide-character subject
        // with an ASCII one, so the frame is drawn with both.
        let entries = [
            entry("#14 3f2a1bc", "修掉退款重试的幂等键"),
            entry("#13 9c8e442", "add a regression test"),
        ];
        let view: Vec<&Entry> = entries.iter().collect();
        let mut cache: std::collections::HashMap<String, String> = Default::default();
        cache.insert(entries[0].label.clone(), "you: fix it\nagent: ok".into());
        let mut state = ListState::default();
        state.select(Some(0));
        let mut term = Terminal::new(TestBackend::new(100, 12)).unwrap();
        term.draw(|f| {
            draw(
                f,
                &Pane {
                    title: "turns",
                    filter: &Filter::default(),
                    cache: &cache,
                    scroll: 0,
                    focus: Focus::List,
                },
                &view,
                &mut state,
            )
        })
        .unwrap();
        let rows: Vec<String> = {
            let buf = term.backend().buffer();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect()
        };
        assert!(rows[0].contains("agit show"), "{:?}", rows[0]);
        assert!(rows[0].contains("2 turns"), "{:?}", rows[0]);
        assert!(rows[1].contains("turns (2)"), "{:?}", rows[1]);
        assert!(rows[1].contains("conversation"), "{:?}", rows[1]);
        assert!(rows[2].contains("#14 3f2a1bc"), "{:?}", rows[2]);
        // The assertion is only that the right pane draws the selected row; it does not count
        // how wide characters land in the buffer's cells.
        assert!(
            rows[2].contains("you:"),
            "the right pane is the selection: {:?}",
            rows[2]
        );
        assert!(rows[11].contains("f/b scroll"), "{:?}", rows[11]);
    }
}
