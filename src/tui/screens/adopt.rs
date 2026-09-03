//! The standalone `agit import` screen: choose one unmanaged runtime session and its destination.
//!
//! The screen makes no adoption decision of its own. It returns ordinary import arguments, leaves
//! the alternate screen, and lets [`crate::commands::import`] perform every permission, lineage,
//! branch and settlement check on the normal command path.
//!
//! # Bounded previews
//!
//! Codex hands the opening prompt over in its index. Claude does not, so the selected row alone is
//! parsed on demand and cached. Moving the cursor may parse one more candidate; gathering and
//! filtering never opens a transcript, and the work never scales with candidates that were not
//! inspected.

use super::{repos, sessions};
use crate::adapter;
use crate::domain::link;
use crate::tui::widgets::{self, Filter};
use crate::ui::theme;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The explicit arguments the screen gives back to `agit import`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    pub runtime: String,
    pub session_id: String,
    /// `None` is the `--link-only` path; otherwise the repo and new branch are both explicit.
    pub destination: Option<(String, String)>,
    pub link_only: bool,
}

#[derive(Debug, Clone)]
struct Candidate {
    runtime: String,
    session_id: String,
    path: PathBuf,
    gist: Option<String>,
    last_active: SystemTime,
    live: bool,
}

impl Candidate {
    fn key(&self) -> (String, String) {
        (self.runtime.clone(), self.session_id.clone())
    }

    fn haystack(&self, preview: Option<&str>) -> String {
        format!(
            "{} {} {}",
            self.runtime,
            self.session_id,
            preview.or(self.gist.as_deref()).unwrap_or_default()
        )
    }
}

/// Gather unmanaged sessions in this directory without opening their transcripts.
fn collect(cwd: &Path) -> Vec<Candidate> {
    let store = crate::domain::store::Store::open_or_init().ok();
    let links = store.as_ref().map(link::list).unwrap_or_default();
    let managed: HashSet<(&str, &str)> = links
        .iter()
        .filter(|item| item.agent.is_some() && item.branch.is_some())
        .map(|item| (item.source.as_str(), item.session_id.as_str()))
        .collect();
    let ignored: HashSet<(&str, &str)> = links
        .iter()
        .filter(|item| item.naming_ignored)
        .map(|item| (item.source.as_str(), item.session_id.as_str()))
        .collect();
    let now = SystemTime::now();
    let mut out = Vec::new();
    for runtime in adapter::RUNTIMES {
        let Ok(adapter) = adapter::get(runtime) else {
            continue;
        };
        for session in adapter.sessions_for(cwd).unwrap_or_default() {
            let identity = (session.runtime, session.id.as_str());
            if managed.contains(&identity)
                || ignored.contains(&identity)
                || !sessions::worth_naming(session.runtime, &session.path, session.gist.as_deref())
            {
                continue;
            }
            out.push(Candidate {
                runtime: session.runtime.to_string(),
                session_id: session.id,
                path: session.path,
                gist: session.gist,
                last_active: session.mtime,
                live: sessions::is_live(session.mtime, now),
            });
        }
    }
    out.sort_by_key(|candidate| std::cmp::Reverse(candidate.last_active));
    out
}

/// Open the picker, returning only after the normal screen owns the terminal again.
pub fn pick(cwd: &Path) -> crate::Result<Option<Picked>> {
    let candidates = collect(cwd);
    if candidates.is_empty() {
        println!(
            "{}",
            crate::ui::dim(&format!(
                "no unadopted sessions under {}.",
                crate::ui::tilde(cwd)
            ))
        );
        crate::ui::hint(
            "session ran in another directory? give the id directly: agit import <session-id> -n <name>",
        );
        return Ok(None);
    }

    let repos = repos::collect(crate::commands::new::DEFAULT_FROM);
    let preferred = crate::commands::context::repo_for(cwd).ok();
    let repo_index = preferred
        .as_deref()
        .and_then(|slug| repos.iter().position(|repo| repo.slug() == slug))
        .unwrap_or(0);
    widgets::refresh_rc_status();
    let picked = {
        let mut guard = crate::tui::term::Guard::enter()?;
        let outcome = run_loop(&candidates, &repos, repo_index);
        guard.suspend()?;
        outcome?
    };
    Ok(picked)
}

fn preview(candidate: &Candidate) -> String {
    candidate
        .gist
        .as_deref()
        .filter(|gist| !gist.trim().is_empty())
        .map(|gist| crate::ui::truncate(gist, 72))
        .unwrap_or_else(|| {
            crate::commands::import::gist_for(
                &candidate.runtime,
                &candidate.session_id,
                &candidate.path,
            )
        })
}

fn validate(
    candidate: &Candidate,
    repo: Option<&repos::Row>,
    branch: &str,
    link_only: bool,
    signed_in: bool,
) -> Result<Picked, String> {
    if candidate.live {
        return Err(
            "this session still looks active. exit it in its own terminal before adopting it."
                .into(),
        );
    }
    if link_only {
        return Ok(Picked {
            runtime: candidate.runtime.clone(),
            session_id: candidate.session_id.clone(),
            destination: None,
            link_only: true,
        });
    }
    if !signed_in {
        return Err(
            "recording the first version needs a sign-in. choose link-only, or quit and run `agit login`."
                .into(),
        );
    }
    let repo = repo.ok_or_else(|| {
        "there is no local agit repo to adopt into. choose link-only, or quit and run `agit init <name>`."
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
            "`{branch}` already exists in {slug} — choose a new session branch."
        ));
    }
    Ok(Picked {
        runtime: candidate.runtime.clone(),
        session_id: candidate.session_id.clone(),
        destination: Some((slug, branch.to_string())),
        link_only: false,
    })
}

fn run_loop(
    candidates: &[Candidate],
    repos: &[repos::Row],
    mut repo_index: usize,
) -> crate::Result<Option<Picked>> {
    let mut term = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut state = ListState::default();
    state.select(Some(0));
    if !repos.is_empty() {
        repo_index %= repos.len();
    }
    let mut filter = Filter::default();
    let mut previews: HashMap<(String, String), String> = HashMap::new();
    let mut branch = String::new();
    let mut editing = false;
    let mut link_only = false;
    let mut notice: Option<String> = None;
    let signed_in = crate::infra::credentials::current_user().is_some();

    loop {
        let view: Vec<&Candidate> = candidates
            .iter()
            .filter(|candidate| {
                let cached = previews.get(&candidate.key()).map(String::as_str);
                filter.matches(&candidate.haystack(cached))
            })
            .collect();
        if state.selected().unwrap_or(0) >= view.len() {
            state.select((!view.is_empty()).then_some(view.len().saturating_sub(1)));
        }
        if let Some(candidate) = state.selected().and_then(|index| view.get(index)).copied() {
            previews
                .entry(candidate.key())
                .or_insert_with(|| preview(candidate));
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
                link_only,
                &filter,
                &previews,
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
                KeyCode::Char(ch) => filter.push(ch),
                _ => {}
            }
            continue;
        }
        notice = None;
        if editing {
            match key.code {
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(None);
                }
                KeyCode::Esc => editing = false,
                KeyCode::Backspace => {
                    branch.pop();
                }
                KeyCode::Enter => {
                    let candidate = state.selected().and_then(|index| view.get(index)).copied();
                    let Some(candidate) = candidate else {
                        continue;
                    };
                    match validate(candidate, repo, &branch, link_only, signed_in) {
                        Ok(picked) => return Ok(Some(picked)),
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

        let count = view.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Ok(None);
            }
            KeyCode::Char('/') => filter.open(),
            KeyCode::Down | KeyCode::Char('j') => {
                let index = state.selected().unwrap_or(0);
                state.select(Some((index + 1).min(count.saturating_sub(1))));
                branch.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                let index = state.selected().unwrap_or(0);
                state.select(Some(index.saturating_sub(1)));
                branch.clear();
            }
            KeyCode::Char('g') | KeyCode::Home => {
                state.select(Some(0));
                branch.clear();
            }
            KeyCode::Char('G') | KeyCode::End => {
                state.select(Some(count.saturating_sub(1)));
                branch.clear();
            }
            KeyCode::Tab if !repos.is_empty() && !link_only => {
                repo_index = (repo_index + 1) % repos.len();
            }
            KeyCode::BackTab if !repos.is_empty() && !link_only => {
                repo_index = repo_index.checked_sub(1).unwrap_or(repos.len() - 1);
            }
            KeyCode::Char('l') | KeyCode::Char(' ') => link_only = !link_only,
            KeyCode::Char('e') if !link_only => editing = true,
            KeyCode::Enter => {
                let candidate = state.selected().and_then(|index| view.get(index)).copied();
                let Some(candidate) = candidate else {
                    continue;
                };
                if !link_only && branch.is_empty() {
                    editing = true;
                    continue;
                }
                match validate(candidate, repo, &branch, link_only, signed_in) {
                    Ok(picked) => return Ok(Some(picked)),
                    Err(error) => notice = Some(error),
                }
            }
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(
    frame: &mut Frame,
    view: &[&Candidate],
    state: &mut ListState,
    repo: Option<&repos::Row>,
    branch: &str,
    editing: bool,
    link_only: bool,
    filter: &Filter,
    previews: &HashMap<(String, String), String>,
    notice: Option<&str>,
) {
    let panes = widgets::layout(frame.area());
    widgets::render_status(
        frame,
        panes.status,
        &widgets::Status {
            title: "agit import".into(),
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
        .map(|candidate| {
            let preview = previews.get(&candidate.key()).map(String::as_str);
            ListItem::new(row_lines(candidate, preview))
        })
        .collect();
    let title = if filter.query().is_empty() {
        format!("sessions ({})", view.len())
    } else {
        format!("sessions ({}) /{}", view.len(), filter.query())
    };
    frame.render_stateful_widget(
        List::new(items)
            .block(widgets::pane(&title))
            .highlight_style(theme::selected())
            .highlight_symbol("▸ "),
        list_area,
        state,
    );
    if let Some(area) = panes.detail {
        let selected = state.selected().and_then(|index| view.get(index)).copied();
        let preview = selected.and_then(|candidate| previews.get(&candidate.key()));
        frame.render_widget(
            Paragraph::new(detail_text(
                selected,
                repo,
                branch,
                editing,
                link_only,
                preview.map(String::as_str),
                notice,
            ))
            .block(widgets::pane("destination"))
            .wrap(Wrap { trim: false }),
            area,
        );
    }
    widgets::render_footer(
        frame,
        panes.footer,
        if editing {
            "type branch   enter import   esc stop editing"
        } else {
            "↑↓ session   tab repo   enter name   l link-only   / filter   q quit"
        },
    );
}

fn row_lines(candidate: &Candidate, preview: Option<&str>) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(format!(
        "{}  {}  {}",
        candidate.runtime,
        link::short(&candidate.session_id),
        crate::ui::ago(candidate.last_active)
    ))];
    if let Some(preview) = preview {
        lines.push(Line::from(Span::styled(
            format!("  {}", crate::ui::truncate(preview, 56)),
            Style::default().fg(theme::MUTED),
        )));
    }
    lines
}

fn detail_text(
    candidate: Option<&Candidate>,
    repo: Option<&repos::Row>,
    branch: &str,
    editing: bool,
    link_only: bool,
    preview: Option<&str>,
    notice: Option<&str>,
) -> String {
    let Some(candidate) = candidate else {
        return "no session matches the filter.".into();
    };
    let mut out = String::new();
    if let Some(notice) = notice {
        out.push_str(notice);
        out.push_str("\n\n");
    }
    out.push_str(&format!("runtime    {}\n", candidate.runtime));
    out.push_str(&format!(
        "session    {}\n",
        link::short(&candidate.session_id)
    ));
    out.push_str(&format!(
        "active     {}\n",
        crate::ui::ago(candidate.last_active)
    ));
    if let Some(preview) = preview {
        out.push_str(&format!("\n{preview}\n"));
    }
    out.push('\n');
    out.push_str(&format!(
        "link only  {}  (l toggles)\n",
        if link_only { "[x]" } else { "[ ]" }
    ));
    if link_only {
        out.push_str("           records the link without a first version\n");
    } else {
        match repo {
            Some(repo) => {
                out.push_str(&format!("repo       {}  (Tab changes repo)\n", repo.slug()));
                if repo.read_only {
                    out.push_str("           read-only checkout\n");
                }
            }
            None => out.push_str("repo       no local agit repo\n"),
        }
        let cursor = if editing { "_" } else { "" };
        out.push_str(&format!(
            "branch     {}{cursor}\n",
            if branch.is_empty() {
                "<press Enter to type>"
            } else {
                branch
            }
        ));
    }
    if candidate.live {
        out.push_str("\nthis session still looks active; adoption waits until it exits.\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn candidate(live: bool) -> Candidate {
        Candidate {
            runtime: "codex".into(),
            session_id: "aaaaaaaa-0000-4000-8000-000000000001".into(),
            path: "/tmp/session.jsonl".into(),
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
    fn link_only_needs_no_repo_or_branch_but_still_preserves_identity() {
        let picked = validate(&candidate(false), None, "", true, false).unwrap();
        assert_eq!(picked.runtime, "codex");
        assert_eq!(picked.session_id, "aaaaaaaa-0000-4000-8000-000000000001");
        assert!(picked.destination.is_none());
        assert!(picked.link_only);
    }

    #[test]
    fn a_versioned_import_requires_a_new_valid_branch() {
        let target = repo(&["taken"]);
        assert!(validate(&candidate(false), Some(&target), "", false, true).is_err());
        assert!(
            validate(
                &candidate(false),
                Some(&target),
                "agit-version",
                false,
                true
            )
            .is_err()
        );
        let duplicate =
            validate(&candidate(false), Some(&target), "taken", false, true).unwrap_err();
        assert!(duplicate.contains("already exists"), "{duplicate}");

        let picked = validate(&candidate(false), Some(&target), "fresh", false, true).unwrap();
        assert_eq!(
            picked.destination,
            Some(("nana/payments".into(), "fresh".into()))
        );
    }

    #[test]
    fn an_active_session_is_not_adopted_from_another_terminal() {
        assert!(validate(&candidate(true), None, "", true, false).is_err());
    }

    #[test]
    fn the_frame_exposes_session_destination_and_link_only_paths() {
        use ratatui::backend::TestBackend;
        let rows = [candidate(false)];
        let view = vec![&rows[0]];
        let target = repo(&[]);
        let mut state = ListState::default();
        state.select(Some(0));
        let mut previews = HashMap::new();
        previews.insert(rows[0].key(), "fix the retry path".into());
        let mut term = Terminal::new(TestBackend::new(120, 15)).unwrap();
        term.draw(|frame| {
            draw(
                frame,
                &view,
                &mut state,
                Some(&target),
                "retry-fix",
                false,
                false,
                &Filter::default(),
                &previews,
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
            "agit import",
            "codex",
            "fix the retry path",
            "nana/payments",
            "retry-fix",
            "link-only",
        ] {
            assert!(
                text.contains(expected),
                "missing `{expected}` from frame: {text}"
            );
        }
    }
}
