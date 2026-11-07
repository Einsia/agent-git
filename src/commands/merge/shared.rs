//! The part of a merge source's LOG that is provably the target's own history.
//!
//! A source that continues the target's history carries that history again at the head of its
//! LOG: byte for byte when the two share a commit, or as the Hub's plain copy when the source
//! grew from what the target published. Appended again, those records read as new turns. Only a
//! prefix something proves is left out; with no proof the whole source LOG goes in, as it always
//! has.

use crate::domain::{meta, privacy_git, repo::Repo, storage};

/// What proves a source LOG's prefix is the target's own history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Proof {
    /// The two histories share this commit, whose LOG both extend.
    Common(String),
    /// The source grew from this Hub commit, the accepted plain publication of the target's
    /// `source` commit.
    Published { public: String, source: String },
}

/// The source LOG's leading lines that are the target's own history.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    /// For each leading source line, in order, the target's line it stands for.
    pub originals: Vec<String>,
    pub proof: Option<Proof>,
}

impl Shared {
    /// How many of the source LOG's leading lines are the target's own.
    pub fn lines(&self) -> usize {
        self.originals.len()
    }
}

/// A LOG's records, one complete line each.
pub(crate) fn lines(log: &str) -> Vec<String> {
    log.split_inclusive('\n')
        .map(|line| {
            let mut line = line.to_owned();
            if !line.ends_with('\n') {
                line.push('\n');
            }
            line
        })
        .collect()
}

fn log_of(holders: &[&Repo], commit: &str) -> Option<Vec<String>> {
    holders.iter().find_map(|holder| {
        holder.git_opt(&["cat-file", "-e", &format!("{commit}^{{commit}}")])?;
        storage::materialize_at(holder.root(), commit, meta::LOG_FILE)
            .ok()
            .map(|log| lines(&log))
    })
}

fn is_ancestor(repo: &Repo, ancestor: &str, head: &str) -> bool {
    repo.git_opt(&["merge-base", "--is-ancestor", ancestor, head])
        .is_some()
}

/// The longest prefix of `source_log` (the LOG at `source_head`) proven to be the history of the
/// target `branch` at `target_head`, whose LOG is `target_log`. `base` is the histories' common
/// commit, empty when they share none.
#[allow(clippy::too_many_arguments)]
pub(crate) fn shared_prefix(
    target: &Repo,
    source: &Repo,
    branch: &str,
    target_head: &str,
    source_head: &str,
    base: &str,
    target_log: &[String],
    source_log: &[String],
) -> Shared {
    let mut best = Shared::default();
    if !base.is_empty()
        && let Some(common) = log_of(&[target, source], base)
        && target_log.starts_with(&common)
        && source_log.starts_with(&common)
    {
        best = Shared {
            originals: common,
            proof: Some(Proof::Common(base.to_owned())),
        };
    }
    for (public, local) in privacy_git::published_copies(target, branch) {
        if !is_ancestor(source, &public, source_head) || !is_ancestor(target, &local, target_head) {
            continue;
        }
        let (Some(copy), Some(own)) = (
            log_of(&[source, target], &public),
            log_of(&[target], &local),
        ) else {
            continue;
        };
        if copy.len() <= best.lines()
            || !source_log.starts_with(&copy)
            || !target_log.starts_with(&own)
        {
            continue;
        }
        // The source holds the Hub's copy it grew from, whether or not the target fetched it.
        let Ok(Some(pairs)) =
            privacy_git::published_events_from(target, &[target, source], branch, &public)
        else {
            continue;
        };
        // Each public event's original, found in order among the target's own lines.
        let mut rest = own.iter();
        let originals = pairs
            .iter()
            .map(|(_, id)| {
                rest.find(|line| storage::canonical_event_id(line) == *id)
                    .cloned()
            })
            .collect::<Option<Vec<_>>>();
        if let Some(originals) = originals.filter(|found| found.len() == copy.len()) {
            best = Shared {
                originals,
                proof: Some(Proof::Published {
                    public,
                    source: local,
                }),
            };
        }
    }
    best
}
