//! The naming inbox: decide which unclaimed runtime sessions enter version control.
//!
//! The screen owns only the decision. Adoption suspends the terminal and goes through
//! [`crate::commands::import`], while ignore persists through [`crate::domain::link`]. Skip is
//! deliberately process-local: it moves on for this visit without making a future decision for
//! the user.
//!
//! # No transcript parsing
//!
//! Candidates come from the Sessions screen's already assembled rows, and repositories come from
//! the same batched scan as `agit new`. The first frame therefore opens no transcript. The only
//! operation that does is an adoption the user explicitly submits, inside `import`.

use super::{repos, sessions};
use crate::domain::store::Store;
use crate::tui::widgets;
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use std::collections::HashSet;
use std::io::Write as _;
use std::path::Path;

/// A runtime session's complete store identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identity {
    pub runtime: String,
    pub session_id: String,
}

impl Identity {
    fn of(row: &sessions::Row) -> Option<Identity> {
        Some(Identity {
            runtime: row.runtime.clone(),
            session_id: row.session_id.clone()?,
        })
    }
}

/// The arguments selected in the inbox. Execution stays outside the alternate screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportChoice {
    pub identity: Identity,
    pub slug: String,
    pub branch: String,
}

impl ImportChoice {
    fn args(&self) -> crate::commands::import::Args {
        crate::commands::import::Args {
            session: Some(self.identity.session_id.clone()),
            name: None,
            from: Some(self.identity.runtime.clone()),
            link_only: false,
            repo: Some(format!("{}@{}", self.slug, self.branch)),
            branch: None,
            onto: None,
            privacy: false,
        }
    }
}

/// One exit from the inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Adopt(ImportChoice),
    /// Every remaining item was skipped for this visit.
    Done,
    /// Quit the resident TUI entirely.
    Quit,
}

/// Whether the current rows contain a naming decision that has not been deferred in this visit.
pub fn has_pending(rows: &[sessions::Row], deferred: &HashSet<Identity>) -> bool {
    rows.iter().any(|row| {
        row.badge == sessions::Badge::Unnamed
            && Identity::of(row).is_some_and(|id| !deferred.contains(&id))
    })
}

/// Run one inbox pass. Persistent ignores are written immediately; adoption returns to the
/// resident shell so it can suspend the terminal before invoking the command layer.
pub fn run(
    rows: &[sessions::Row],
    cwd: &Path,
    deferred: &mut HashSet<Identity>,
    focus: Option<&Identity>,
) -> crate::Result<Outcome> {
    let repos = repos::collect(crate::commands::new::DEFAULT_FROM);
    let preferred = crate::commands::context::repo_for(cwd).ok();
    let repo_index = preferred
        .as_deref()
        .and_then(|slug| repos.iter().position(|repo| repo.slug() == slug))
        .unwrap_or(0);
    run_loop(rows, &repos, repo_index, cwd, deferred, focus)
}

/// Run the selected import on the normal screen, then wait before taking the terminal back.
pub fn execute_import(
    guard: &mut crate::tui::term::Guard,
    choice: &ImportChoice,
) -> crate::Result<crate::ExitCode> {
    guard.suspend()?;
    let outcome = crate::commands::import::run(choice.args());
    let waited = wait_for_return();
    widgets::refresh_rc_status();
    let resumed = guard.resume();
    // Terminal restoration wins over command propagation: if taking the terminal back fails,
    // continuing the resident loop would draw into an ordinary screen in an unknown mode.
    resumed?;
    waited?;
    outcome
}

fn wait_for_return() -> crate::Result<()> {
    print!("\npress Enter to return to agit.");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(())
}

fn candidates<'a>(
    rows: &'a [sessions::Row],
    deferred: &HashSet<Identity>,
) -> Vec<&'a sessions::Row> {
    rows.iter()
        .filter(|row| row.badge == sessions::Badge::Unnamed)
        .filter(|row| Identity::of(row).is_some_and(|id| !deferred.contains(&id)))
        .collect()
}

fn validate(
    candidate: &sessions::Row,
    repo: Option<&repos::Row>,
    branch: &str,
) -> Result<ImportChoice, String> {
    if candidate.live {
        return Err(
            "this session still looks active. exit it in its own terminal before adopting it."
                .into(),
        );
    }
    let repo = repo.ok_or_else(|| {
        "there is no local agit repo to adopt into. quit and run `agit init <name>` first."
            .to_string()
    })?;
    let branch = branch.trim();
    if branch.is_empty() {
        return Err("type a branch name first.".into());
    }
    crate::domain::repo::valid_branch_name(branch).map_err(|error| format!("{error:#}"))?;
    let slug = repo.slug();
    crate::commands::target::branch_only(&format!("{slug}@{branch}"))
        .map_err(|error| format!("{error:#}"))?;
    if repo.branches.iter().any(|existing| existing == branch) {
        return Err(format!(
            "`{branch}` already exists in {} — choose a new session branch.",
            slug
        ));
    }
    Ok(ImportChoice {
        identity: Identity::of(candidate)
            .ok_or_else(|| "this row has no runtime session identity.".to_string())?,
        slug,
        branch: branch.to_string(),
    })
}

fn run_loop(
    rows: &[sessions::Row],
    repos: &[repos::Row],
    mut repo_index: usize,
    cwd: &Path,
    deferred: &mut HashSet<Identity>,
    focus: Option<&Identity>,
) -> crate::Result<Outcome> {
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut state = ListState::default();
    let initial = candidates(rows, deferred);
    let selected = focus
        .and_then(|wanted| {
            initial
                .iter()
                .position(|row| Identity::of(row).as_ref() == Some(wanted))
        })
        .unwrap_or(0);
    state.select((!initial.is_empty()).then_some(selected));
    if !repos.is_empty() {
        repo_index %= repos.len();
    }
    let mut branch = String::new();
    let mut editing = false;
    let mut notice: Option<String> = None;

    loop {
        let view = candidates(rows, deferred);
        if view.is_empty() {
            return Ok(Outcome::Done);
        }
        if state.selected().unwrap_or(0) >= view.len() {
            state.select(Some(view.len() - 1));
        }
        let repo = (!repos.is_empty()).then(|| &repos[repo_index]);
        term.draw(|frame| {
            draw(
                frame,
                &view,
                &mut state,
                repo,
                &branch,
                editing,
                notice.as_deref(),
            )
        })?;

        let Some(key) = crate::tui::term::next_key()? else {
            continue;
        };
        notice = None;
        if editing {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(Outcome::Quit);
                }
                KeyCode::Esc => editing = false,
                KeyCode::Backspace => {
                    branch.pop();
                }
                KeyCode::Enter => {
                    let candidate = state.selected().and_then(|index| view.get(index)).copied();
                    let Some(candidate) = candidate else { continue };
                    match validate(candidate, repo, &branch) {
                        Ok(choice) => return Ok(Outcome::Adopt(choice)),
                        Err(error) => notice = Some(error),
                    }
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    branch.push(ch);
                }
                _ => {}
            }
            continue;
        }

        let n = view.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(Outcome::Quit),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(Outcome::Quit);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let index = state.selected().unwrap_or(0);
                state.select(Some((index + 1).min(n - 1)));
                branch.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let index = state.selected().unwrap_or(0);
                state.select(Some(index.saturating_sub(1)));
                branch.clear();
            }
            KeyCode::Tab if !repos.is_empty() => {
                repo_index = (repo_index + 1) % repos.len();
            }
            KeyCode::BackTab if !repos.is_empty() => {
                repo_index = repo_index.checked_sub(1).unwrap_or(repos.len() - 1);
            }
            KeyCode::Char('e') => editing = true,
            KeyCode::Enter => {
                if branch.is_empty() {
                    editing = true;
                    continue;
                }
                let candidate = state.selected().and_then(|index| view.get(index)).copied();
                let Some(candidate) = candidate else { continue };
                match validate(candidate, repo, &branch) {
                    Ok(choice) => return Ok(Outcome::Adopt(choice)),
                    Err(error) => notice = Some(error),
                }
            }
            KeyCode::Char('s') => {
                if let Some(identity) = state
                    .selected()
                    .and_then(|index| view.get(index))
                    .and_then(|row| Identity::of(row))
                {
                    deferred.insert(identity);
                    branch.clear();
                }
            }
            KeyCode::Char('x') => {
                let selected = state.selected().and_then(|index| view.get(index)).copied();
                let Some(candidate) = selected else { continue };
                let Some(identity) = Identity::of(candidate) else {
                    notice = Some("this row has no runtime session identity.".into());
                    continue;
                };
                let store = Store::open_or_init();
                match store.and_then(|store| {
                    crate::domain::link::dismiss_naming(
                        &store,
                        &identity.runtime,
                        &identity.session_id,
                        Some(cwd),
                    )
                }) {
                    Ok(_) => {
                        deferred.insert(identity);
                        branch.clear();
                    }
                    Err(error) => notice = Some(format!("cannot ignore this session: {error:#}")),
                }
            }
            _ => {}
        }
    }
}

fn draw(
    frame: &mut Frame,
    view: &[&sessions::Row],
    state: &mut ListState,
    repo: Option<&repos::Row>,
    branch: &str,
    editing: bool,
    notice: Option<&str>,
) {
    let panes = widgets::layout(frame.area());
    widgets::render_status(
        frame,
        panes.status,
        &widgets::Status {
            title: "agit name".into(),
            identity: crate::infra::credentials::current_user()
                .map(|user| format!("{user} @ {}", crate::infra::config::hub_url())),
            rc_online: None,
            counters: widgets::Counters {
                unnamed: view.len(),
            },
        },
    );
    let list_area = widgets::list_area_with_notice(frame, panes, notice);

    let items: Vec<ListItem> = view
        .iter()
        .map(|row| ListItem::new(row_lines(row)))
        .collect();
    frame.render_stateful_widget(
        List::new(items)
            .block(widgets::pane(&format!("sessions to name ({})", view.len())))
            .highlight_style(theme::selected())
            .highlight_symbol("▸ "),
        list_area,
        state,
    );

    if let Some(area) = panes.detail {
        let selected = state.selected().and_then(|index| view.get(index)).copied();
        frame.render_widget(
            Paragraph::new(detail_text(selected, repo, branch, editing, notice))
                .block(widgets::pane("adopt"))
                .wrap(Wrap { trim: false }),
            area,
        );
    }
    widgets::render_footer(
        frame,
        panes.footer,
        if editing {
            "type branch   enter adopt   esc stop editing"
        } else {
            "↑↓ session   tab repo   enter name   s skip   x ignore   q quit"
        },
    );
}

fn row_lines(row: &sessions::Row) -> Vec<Line<'static>> {
    let id = row
        .session_id
        .as_deref()
        .map(crate::domain::link::short)
        .unwrap_or_default();
    let mut lines = vec![Line::from(format!("{}  {id}", row.runtime))];
    if let Some(gist) = &row.gist {
        lines.push(Line::from(Span::styled(
            format!("  {}", crate::ui::truncate(gist, 52)),
            Style::default().fg(theme::MUTED),
        )));
    }
    lines
}

fn detail_text(
    row: Option<&sessions::Row>,
    repo: Option<&repos::Row>,
    branch: &str,
    editing: bool,
    notice: Option<&str>,
) -> String {
    let Some(row) = row else {
        return "no session is waiting for a name.".into();
    };
    let mut out = String::new();
    if let Some(notice) = notice {
        out.push_str(notice);
        out.push_str("\n\n");
    }
    out.push_str(&format!("runtime  {}\n", row.runtime));
    if let Some(id) = &row.session_id {
        out.push_str(&format!("session  {}\n", crate::domain::link::short(id)));
    }
    out.push_str(&format!("active   {}\n", crate::ui::ago(row.last_active)));
    if let Some(gist) = &row.gist {
        out.push_str(&format!("\n{gist}\n"));
    }
    out.push('\n');
    match repo {
        Some(repo) => {
            out.push_str(&format!("repo     {}  (Tab changes repo)\n", repo.slug()));
            if repo.read_only {
                out.push_str("         read-only checkout\n");
            }
        }
        None => out.push_str("repo     no local agit repo\n"),
    }
    let cursor = if editing { "_" } else { "" };
    out.push_str(&format!(
        "branch   {}{cursor}\n",
        if branch.is_empty() {
            "<press Enter to type>"
        } else {
            branch
        }
    ));
    if row.live {
        out.push_str("\nthis session still looks active; adoption waits until it exits.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn session(runtime: &str, id: &str, live: bool) -> sessions::Row {
        sessions::Row {
            badge: sessions::Badge::Unnamed,
            slug: None,
            branch: None,
            runtime: runtime.into(),
            session_id: Some(id.into()),
            gist: Some("fix the retry path".into()),
            last_active: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            live,
        }
    }

    fn repo(branches: &[&str]) -> repos::Row {
        repos::Row {
            owner: "nana".into(),
            name: "payments".into(),
            path: "/repo".into(),
            sessions: branches.len(),
            from_ref: "main".into(),
            from_line: Some(crate::domain::meta::Line::File),
            branches: branches.iter().map(|branch| (*branch).into()).collect(),
            read_only: false,
        }
    }

    #[test]
    fn skip_is_local_to_one_visit_and_runtime_is_part_of_identity() {
        let rows = vec![
            session("codex", "same", false),
            session("claude-code", "same", false),
        ];
        let mut deferred = HashSet::new();
        deferred.insert(Identity {
            runtime: "codex".into(),
            session_id: "same".into(),
        });

        let visible = candidates(&rows, &deferred);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].runtime, "claude-code");
        assert!(has_pending(&rows, &deferred));
        deferred.clear();
        assert_eq!(candidates(&rows, &deferred).len(), 2);
    }

    #[test]
    fn adoption_preserves_runtime_and_uses_the_complete_destination() {
        let choice = validate(
            &session("codex", "ABC", false),
            Some(&repo(&[])),
            "retry-fix",
        )
        .unwrap();
        let args = choice.args();
        assert_eq!(args.session.as_deref(), Some("ABC"));
        assert_eq!(args.from.as_deref(), Some("codex"));
        assert_eq!(args.repo.as_deref(), Some("nana/payments@retry-fix"));
        assert!(args.name.is_none());
        assert!(args.branch.is_none());
    }

    #[test]
    fn adoption_blocks_active_invalid_and_existing_branches() {
        let target = repo(&["taken"]);
        assert!(validate(&session("codex", "A", true), Some(&target), "fresh").is_err());
        assert!(validate(&session("codex", "A", false), Some(&target), "agit-version").is_err());
        assert!(validate(&session("codex", "A", false), Some(&target), "topic#2").is_err());
        let duplicate =
            validate(&session("codex", "A", false), Some(&target), "taken").unwrap_err();
        assert!(duplicate.contains("already exists"), "{duplicate}");
    }

    #[test]
    fn the_frame_exposes_all_three_decisions_and_the_destination() {
        use ratatui::backend::TestBackend;
        let rows = [session("codex", "ABC", false)];
        let view = vec![&rows[0]];
        let target = repo(&[]);
        let mut state = ListState::default();
        state.select(Some(0));
        let mut term = Terminal::new(TestBackend::new(110, 14)).unwrap();
        term.draw(|frame| {
            draw(
                frame,
                &view,
                &mut state,
                Some(&target),
                "retry-fix",
                false,
                None,
            )
        })
        .unwrap();
        let buffer = term.backend().buffer();
        let text = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "agit name",
            "sessions to name",
            "nana/payments",
            "retry-fix",
            "s skip",
            "x ignore",
        ] {
            assert!(
                text.contains(expected),
                "missing `{expected}` from frame: {text}"
            );
        }
    }
}
