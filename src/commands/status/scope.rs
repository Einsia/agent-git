//! Which repositories one status run reports on, and the one-row-per-repository summary `--all`
//! puts before the full report.
//!
//! The summary reads what the report already reads (claims, upload outcomes, waits and holds),
//! never a transcript, so a computer with a hundred repositories pays for it once.

use crate::domain::link;

/// The repositories a status run covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Scope {
    /// Every repository on this computer.
    Machine,
    /// One repository, as `owner/name`.
    Repository(String),
}

impl Scope {
    /// Whether the repository `owner/name` is reported.
    pub(super) fn covers(&self, repository: &str) -> bool {
        match self {
            Scope::Machine => true,
            Scope::Repository(only) => only == repository,
        }
    }

    /// Whether `owner/name@branch` is reported.
    pub(super) fn covers_target(&self, target: &str) -> bool {
        self.covers(repository_of_target(target))
    }

    /// Whether a claim is reported: one with no recorded repository only when every repository is.
    pub(super) fn covers_link(&self, link: &link::Link) -> bool {
        match repository_of(link) {
            Some(repository) => self.covers(&repository),
            None => *self == Scope::Machine,
        }
    }
}

/// The `owner/name` of `owner/name@branch`. A repository name never holds `@`.
pub(super) fn repository_of_target(target: &str) -> &str {
    target
        .split_once('@')
        .map_or(target, |(repository, _)| repository)
}

/// The `owner/name` a claim records, if it records both.
pub(super) fn repository_of(link: &link::Link) -> Option<String> {
    Some(format!("{}/{}", link.owner.as_ref()?, link.agent.as_ref()?))
}

/// Moves the claims `scope` covers to the front, each part in its order, and returns how many
/// lead. The list itself stays whole: a page's inventory recheck compares every claim.
pub(super) fn lead(links: &mut Vec<link::Link>, scope: &Scope) -> usize {
    let (mut covered, mut rest): (Vec<_>, Vec<_>) = std::mem::take(links)
        .into_iter()
        .partition(|link| scope.covers_link(link));
    let leading = covered.len();
    covered.append(&mut rest);
    *links = covered;
    leading
}

/// One repository on this computer, as the summary shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(super) struct Repository {
    pub repository: String,
    /// The current folder saves to it.
    pub here: bool,
    /// Active conversations this computer saved to it.
    pub conversations: usize,
    /// Items the report lists for it that need the person: uploads that didn't go through or
    /// wait, held branches, turns kept back by an upload setting.
    pub attention: usize,
}

/// One summary row per repository in `repositories`: the one the current folder saves to first,
/// then those that need attention, the most first, then by how many conversations they hold.
/// `attention` names a repository once for each item; one this computer has no checkout of
/// still gets a row, since its items need the person all the same.
pub(super) fn summarize(
    repositories: &[String],
    links: &[link::Link],
    attention: &[String],
    here: Option<&str>,
) -> Vec<Repository> {
    let mut rows: std::collections::BTreeMap<&str, Repository> = repositories
        .iter()
        .chain(attention)
        .map(|repository| {
            (
                repository.as_str(),
                Repository {
                    repository: repository.clone(),
                    here: here == Some(repository.as_str()),
                    conversations: 0,
                    attention: 0,
                },
            )
        })
        .collect();
    for link in links.iter().filter(|link| link.is_active()) {
        if let Some(repository) = repository_of(link)
            && let Some(row) = rows.get_mut(repository.as_str())
        {
            row.conversations += 1;
        }
    }
    for repository in attention {
        if let Some(row) = rows.get_mut(repository.as_str()) {
            row.attention += 1;
        }
    }
    let mut rows: Vec<Repository> = rows.into_values().collect();
    rows.sort_by(|a, b| {
        b.here
            .cmp(&a.here)
            .then(b.attention.cmp(&a.attention))
            .then(b.conversations.cmp(&a.conversations))
            .then(a.repository.cmp(&b.repository))
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(owner: &str, agent: &str, session: &str) -> link::Link {
        let mut claim = link::Link::new("claude-code", session, None);
        claim.owner = Some(owner.into());
        claim.agent = Some(agent.into());
        claim.branch = Some(format!("b-{session}"));
        claim
    }

    #[test]
    fn a_repository_scope_covers_its_targets_and_claims_only() {
        let scope = Scope::Repository("alice/notes".into());
        assert!(scope.covers_target("alice/notes@topic"));
        assert!(!scope.covers_target("alice/notes-old@topic"));
        assert!(!scope.covers_target("bob/notes@topic"));
        assert!(scope.covers_link(&claim("alice", "notes", "a")));
        assert!(!scope.covers_link(&claim("alice", "other", "b")));
        // A claim with no recorded repository belongs to no one repository.
        assert!(!scope.covers_link(&link::Link::new("codex", "c", None)));
        assert!(Scope::Machine.covers_link(&link::Link::new("codex", "c", None)));
    }

    #[test]
    fn leading_claims_keep_their_order_and_the_list_stays_whole() {
        let mut links = vec![
            claim("alice", "other", "1"),
            claim("alice", "notes", "2"),
            claim("bob", "x", "3"),
            claim("alice", "notes", "4"),
        ];
        let leading = lead(&mut links, &Scope::Repository("alice/notes".into()));
        assert_eq!(leading, 2);
        let order: Vec<_> = links.iter().map(|link| link.session_id.as_str()).collect();
        assert_eq!(order, ["2", "4", "1", "3"]);
    }

    #[test]
    fn the_summary_puts_here_first_then_attention_then_size() {
        let repositories = ["alice/big", "alice/here", "alice/quiet", "team/wiki"]
            .map(String::from)
            .to_vec();
        let mut links = vec![
            claim("alice", "big", "1"),
            claim("alice", "big", "2"),
            claim("alice", "big", "3"),
            claim("alice", "here", "4"),
            claim("team", "wiki", "5"),
        ];
        let mut superseded = claim("alice", "quiet", "6");
        superseded.superseded_by = Some("claude-code/7".into());
        links.push(superseded);
        let attention = ["team/wiki", "team/wiki", "alice/gone"].map(String::from);
        let rows = summarize(&repositories, &links, &attention, Some("alice/here"));
        let order: Vec<_> = rows
            .iter()
            .map(|row| (row.repository.as_str(), row.conversations, row.attention))
            .collect();
        assert_eq!(
            order,
            [
                ("alice/here", 1, 0),
                ("team/wiki", 1, 2),
                ("alice/gone", 0, 1),
                ("alice/big", 3, 0),
                ("alice/quiet", 0, 0),
            ]
        );
        assert!(rows[0].here);
        assert!(rows[1..].iter().all(|row| !row.here));
    }
}
