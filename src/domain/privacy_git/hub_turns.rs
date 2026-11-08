//! Turns another computer published on top of the published form of a branch's tip, brought onto
//! the branch as the Hub holds them.

use super::*;
use crate::hub::git::RemoteRefs;

/// What [`bring_over`] found for a Hub tip the branch doesn't hold.
pub(crate) enum NewerTurns {
    /// The Hub tip doesn't add turns on top of the published form of the branch's tip itself.
    None,
    /// It does, but they can't be brought over as the Hub holds them.
    Refused,
    /// Local commits that hold them, ready to put on the branch.
    Built(BroughtOver),
    /// They were brought over and recorded, and the branch is still below them: `head` is the
    /// built tip, `turns` above the branch's tip, each recorded as published by the Hub's turn.
    Recorded { head: String, turns: usize },
}

/// Local commits built from the Hub's newer turns, each the published form of one of them.
pub(crate) struct BroughtOver {
    branch: String,
    /// The built tip.
    pub head: String,
    /// How many turns it adds.
    pub turns: usize,
    candidate: accepted::Candidate,
}

impl BroughtOver {
    /// Records each built commit as published by the Hub turn it holds, so the branch's tip reads
    /// as the Hub's and the next push sends only what follows it.
    pub(crate) fn record(&self, repo: &Repo) -> Result<()> {
        let identity = crate::hub::identity::read(repo)?
            .context("this checkout has no Hub identity to record the Hub's turns under")?;
        let mut ledger = accepted::Ledger::open(repo, &identity)?;
        ledger.prepare(
            &BTreeMap::from([(self.branch.clone(), self.candidate.clone())]),
            &BTreeSet::new(),
        )?;
        ledger.reconcile(&RemoteRefs {
            refs: BTreeMap::from([(
                format!("refs/heads/{}", self.branch),
                self.candidate.public_head.clone(),
            )]),
            ..Default::default()
        })
    }
}

/// The turns the Hub's tip `remote` of `branch` adds on top of the published form of `local`,
/// built as local commits on `local`. `remote` is at most `limit` commits from that published
/// form. Commits above `local` that a bring-over recorded and the branch wasn't moved onto stand
/// for `local`: the turns are built on them, and the branch moves over both.
///
/// Each built commit is the commit below it with the turn's added events byte for byte, its LOG
/// and VIEW extended by the turn's, the turn's metadata, and the turn's identity, dates and
/// message. The local turns keep their bytes; nothing the Hub holds is restored.
///
/// Only a plain session's turns are built, each with one parent, adding events and extending LOG
/// and VIEW only. And only when this computer's plain publication of each built commit, on top of
/// the turn below it, gives exactly the turn's LOG and VIEW: then the Hub holds each one as its
/// published form, and the next push carries them as they are. A turn this computer's rules would
/// publish differently is [`NewerTurns::Refused`]; so is anything else, rather than a guess.
pub(crate) fn bring_over(
    repo: &Repo,
    branch: &str,
    local: &str,
    remote: &str,
    limit: usize,
) -> Result<NewerTurns> {
    let Some(identity) = crate::hub::identity::read(repo).ok().flatten() else {
        return Ok(NewerTurns::None);
    };
    let Ok(ledger) = accepted::Ledger::open(repo, &identity) else {
        return Ok(NewerTurns::None);
    };
    let mut turns = Vec::new();
    let mut single_parents = true;
    let mut at = remote.to_owned();
    let base = loop {
        if let Some(entry) = ledger
            .accepted(branch)
            .find(|entry| entry.public == at && entry.source != entry.public)
        {
            break entry.clone();
        }
        if turns.len() >= limit {
            return Ok(NewerTurns::None);
        }
        let parents = repo.git(&["rev-list", "--parents", "-n", "1", &at])?;
        let mut parents = parents.split_whitespace().skip(1);
        let Some(parent) = parents.next() else {
            return Ok(NewerTurns::None);
        };
        single_parents &= parents.next().is_none();
        turns.push(at);
        at = parent.to_owned();
    };
    // What the Hub's turns go on: the branch's tip, or a commit above it that a bring-over recorded
    // and the branch wasn't moved onto, by commits each recorded as a bring-over records them.
    let (below, recorded) = if base.source == local {
        (local.to_owned(), 0)
    } else {
        match recorded_above(|at| ledger.get(branch, at), local, &base.source, limit) {
            Some(recorded) => (base.source.clone(), recorded),
            // The published form of an older local commit: this computer has turns of its own
            // that aren't uploaded, so the session continued in two places.
            None => return Ok(NewerTurns::None),
        }
    };
    if turns.is_empty() {
        return Ok(if recorded > 0 {
            NewerTurns::Recorded {
                head: below,
                turns: recorded,
            }
        } else {
            NewerTurns::None
        });
    }
    if !single_parents || !base.plain || base.recipient.is_some() || base.delivered {
        return Ok(NewerTurns::Refused);
    }
    turns.reverse();
    let local_meta = meta::read_at_ref_result(repo, &below)?
        .context("the branch's tip has no session metadata")?;
    let policy = effective_policy(repo, &[])?.digest()?;
    let mut mappings = BTreeMap::new();
    let mut source_parent = below;
    let mut public_parent = base.public.clone();
    for turn in &turns {
        let Some(built) = build(repo, &local_meta, &source_parent, &public_parent, turn)? else {
            return Ok(NewerTurns::Refused);
        };
        let Some(published) =
            sessions::plain_published(repo, &built, &source_parent, &public_parent)?
        else {
            return Ok(NewerTurns::Refused);
        };
        for sequence in [meta::LOG_FILE, meta::VIEW_FILE] {
            if blob(repo, &published, sequence)? != blob(repo, turn, sequence)? {
                return Ok(NewerTurns::Refused);
            }
        }
        mappings.insert(
            built.clone(),
            accepted::Mapping {
                source: built.clone(),
                public: turn.clone(),
                source_parents: vec![source_parent],
                public_parents: vec![public_parent],
                store: base.store.clone(),
                source_session: local_meta.session.clone(),
                public_session: local_meta.session.clone(),
                policy: policy.clone(),
                recipient: None,
                plain: true,
                delivered: false,
            },
        );
        source_parent = built;
        public_parent = turn.clone();
    }
    Ok(NewerTurns::Built(BroughtOver {
        branch: branch.to_owned(),
        turns: turns.len(),
        candidate: accepted::Candidate {
            source_head: source_parent.clone(),
            public_head: remote.to_owned(),
            mappings,
        },
        head: source_parent,
    }))
}

/// The local commit holding `turn` on top of `parent`, whose published form is `turn`'s parent
/// `public_parent`. `None` when `turn` does more than add events of the session and extend LOG and
/// VIEW with them.
fn build(
    repo: &Repo,
    local: &Meta,
    parent: &str,
    public_parent: &str,
    turn: &str,
) -> Result<Option<String>> {
    for commit in [parent, public_parent, turn] {
        for sequence in [meta::LOG_FILE, meta::VIEW_FILE] {
            if blob(repo, commit, sequence)?.is_none() {
                return Ok(None);
            }
        }
    }
    let changes = repo.git(&[
        "diff-tree",
        "-r",
        "--no-renames",
        "--name-status",
        public_parent,
        turn,
    ])?;
    let mut edits = Vec::new();
    let mut added = BTreeSet::new();
    for change in changes.lines() {
        let Some((status, path)) = change.split_once('\t') else {
            return Ok(None);
        };
        match (status, path) {
            ("M", meta::LOG_FILE | meta::VIEW_FILE | meta::FILE) => {}
            ("A", path) if path.starts_with(&format!("{}/", meta::EVENTS_DIR)) => {
                edits.push((
                    path.to_owned(),
                    Some(repo.git_bytes_result(&[
                        "cat-file",
                        "blob",
                        &format!("{turn}:{path}"),
                    ])?),
                ));
                added.insert(path.to_owned());
            }
            _ => return Ok(None),
        }
    }
    let ids = |commit: &str, sequence: &str| -> Result<Vec<String>> {
        Ok(String::from_utf8(repo.git_bytes_result(&[
            "cat-file",
            "blob",
            &format!("{commit}:{sequence}"),
        ])?)?
        .lines()
        .map(str::to_owned)
        .collect())
    };
    let mut appended_log = Vec::new();
    for sequence in [meta::LOG_FILE, meta::VIEW_FILE] {
        let below = ids(public_parent, sequence)?;
        let above = ids(turn, sequence)?;
        let Some(appended) = above.strip_prefix(below.as_slice()) else {
            return Ok(None);
        };
        if sequence == meta::LOG_FILE {
            appended_log = appended.to_vec();
        } else if !appended.iter().all(|id| appended_log.contains(id)) {
            // A VIEW naming an event from below would name one the local LOG holds in another form.
            return Ok(None);
        }
        for id in appended {
            let path = meta::event_path(id)?;
            if !added.contains(&path)
                && repo
                    .git_opt(&["cat-file", "-e", &format!("{parent}:{path}")])
                    .is_none()
            {
                return Ok(None);
            }
        }
        let mut extended = ids(parent, sequence)?;
        extended.extend(appended.iter().cloned());
        edits.push((
            sequence.to_owned(),
            Some(crate::domain::storage::sequence_text(&extended)?.into_bytes()),
        ));
    }
    let Some(mut turn_meta) = meta::read_at_ref_result(repo, turn)? else {
        return Ok(None);
    };
    if turn_meta.session != local.session || !turn_meta.is_session_line() {
        return Ok(None);
    }
    // What plain publication withholds or replaces stays as this computer recorded it.
    turn_meta.session_privacy = local.session_privacy.clone();
    turn_meta.fork_origin = local.fork_origin.clone();
    turn_meta.source_ref = local.source_ref.clone();
    turn_meta.source_commit = local.source_commit.clone();
    edits.push((
        meta::FILE.to_owned(),
        Some(meta::to_text(&turn_meta)?.into_bytes()),
    ));
    let tree = crate::commands::plumbing::tree_apply_owned(repo, parent, edits)?;
    let original = read(repo, &["cat-file", "commit", turn], MAX_COMMIT)?;
    Ok(Some(write_object(
        repo,
        "commit",
        &commit_body(&tree, &[parent.to_owned()], &original)?,
    )?))
}

/// How many commits `head` is above `local`, when each of them has one parent and `recorded`
/// names a published form of it that a bring-over writes: plain, for no recipient, not a delivery.
/// At most `limit`. Any other record, an encrypted publication's for one, leaves the branch as
/// it is.
fn recorded_above<'a>(
    recorded: impl Fn(&str) -> Option<&'a accepted::Mapping>,
    local: &str,
    head: &str,
    limit: usize,
) -> Option<usize> {
    let mut turns = 0;
    let mut at = head.to_owned();
    while at != local {
        let entry = recorded(&at)
            .filter(|entry| entry.plain && entry.recipient.is_none() && !entry.delivered)?;
        let [parent] = entry.source_parents.as_slice() else {
            return None;
        };
        turns += 1;
        if turns > limit {
            return None;
        }
        at = parent.clone();
    }
    (turns > 0).then_some(turns)
}

fn blob(repo: &Repo, commit: &str, path: &str) -> Result<Option<String>> {
    Ok(repo.git_opt(&["rev-parse", "--verify", &format!("{commit}:{path}")]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(source: &str, parent: &str) -> accepted::Mapping {
        accepted::Mapping {
            source: source.into(),
            public: format!("{source}-public"),
            source_parents: vec![parent.into()],
            public_parents: vec![format!("{parent}-public")],
            store: "0".repeat(64),
            source_session: "session".into(),
            public_session: "session".into(),
            policy: format!("sha256:{}", "0".repeat(64)),
            recipient: None,
            plain: true,
            delivered: false,
        }
    }

    /// A branch is moved up only over records a bring-over writes. An encrypted publication's
    /// records, or a delivery's, above the branch leave it where it is: taking them would move the
    /// branch onto a session published encrypted, which a bring-over refuses. A walk that reads
    /// any record moves it.
    #[test]
    fn only_plain_records_move_a_branch_up() {
        let chain = |change: &dyn Fn(&mut accepted::Mapping)| {
            let mut above = mapping("b", "a");
            change(&mut above);
            BTreeMap::from([
                ("a".to_owned(), mapping("a", "local")),
                ("b".to_owned(), above),
            ])
        };
        let walk = |records: &BTreeMap<String, accepted::Mapping>| {
            recorded_above(|at| records.get(at), "local", "b", 8)
        };
        assert_eq!(walk(&chain(&|_| {})), Some(2));
        assert_eq!(walk(&chain(&|entry| entry.plain = false)), None);
        assert_eq!(
            walk(&chain(&|entry| {
                entry.plain = false;
                entry.recipient = Some(format!("sha256:{}", "1".repeat(64)));
            })),
            None
        );
        assert_eq!(walk(&chain(&|entry| entry.delivered = true)), None);
    }
}
