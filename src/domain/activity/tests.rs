use super::*;
use crate::domain::repo::Repo;
use crate::domain::{
    meta::{self, Meta},
    storage, transcript,
};
use serde_json::json;

fn save(
    repo: &Repo,
    log: &str,
    sid: &str,
    author: &str,
    date: &str,
    layout: meta::LayoutVersion,
    turn: u32,
) -> String {
    let mut metadata = Meta::new(sid.into(), "claude-code".into(), "/fixture".into());
    metadata.layout = layout;
    metadata.turn = Some(turn);
    std::fs::create_dir_all(repo.root().join("session")).unwrap();
    std::fs::write(
        repo.root().join(meta::FILE),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    match layout {
        meta::LayoutVersion::V0 => {
            std::fs::write(repo.root().join(meta::LEGACY_LOG_FILE), log).unwrap()
        }
        meta::LayoutVersion::V1 => storage::write_snapshot(repo.root(), log, log).unwrap(),
    }
    commit(repo, author, date)
}

fn commit(repo: &Repo, author: &str, date: &str) -> String {
    repo.add_all().unwrap();
    let out = std::process::Command::new("git")
        .current_dir(repo.root())
        .args(["commit", "-qm", "Save conversation"])
        .env("GIT_AUTHOR_NAME", author)
        .env("GIT_AUTHOR_EMAIL", format!("{author}@example.com"))
        .env("GIT_COMMITTER_NAME", "Recorder")
        .env("GIT_COMMITTER_EMAIL", "recorder@example.com")
        .env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", date)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    repo.git(&["rev-parse", "HEAD"]).unwrap().trim().into()
}

fn record(sid: &str, role: &str, text: &str) -> String {
    transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":role,"message":{"role":role,"content":text}})
        ),
        "claude-code",
        sid,
    )
}

fn query() -> Query {
    Query {
        author: "ALICE".into(),
        since: "2026-10-01".into(),
        before: "2026-10-03".into(),
        session: None,
        cursor: None,
        limit: Some(1),
        assistant: Assistant::All,
    }
}

/// Hub attribution as a fixture: the supplied facts, filtered the way the Hub filters them.
#[derive(Default)]
struct Fixed {
    facts: Facts,
    aliases: Vec<Alias>,
    accepted: BTreeSet<String>,
}

impl Attribution for Fixed {
    fn aliases(&mut self, sessions: &[String]) -> Result<Vec<Alias>> {
        Ok(self
            .aliases
            .iter()
            .filter(|alias| sessions.contains(&alias.logical) || sessions.contains(&alias.public))
            .cloned()
            .collect())
    }

    fn facts(&mut self, request: &FactsRequest) -> Result<Facts> {
        let (from, before) = (instant(&request.from)?, instant(&request.before)?);
        let mut facts = self.facts.clone();
        facts.publications.retain(|publication| {
            let at = reported(&publication.published_at).unwrap();
            at >= from
                && at < before
                && (request.sessions.is_empty()
                    || request.sessions.contains(&publication.session)
                    || request.sessions.contains(&publication.public_session))
        });
        Ok(facts)
    }

    fn missing(&mut self, commits: &[String]) -> Result<Reason> {
        let mut missing = Reason::default();
        for commit in commits.iter().filter(|c| !self.accepted.contains(*c)) {
            missing.note(commit);
        }
        Ok(missing)
    }
}

fn read(repo: &Repo, attribution: &mut Fixed, query: &Query) -> Result<Page> {
    page(
        &mut local::LocalStore::new(repo),
        attribution,
        "alice/work",
        "local-fixture",
        query,
    )
}

/// Every page of a query; `between` runs after the first page.
fn pages(
    repo: &Repo,
    attribution: &mut Fixed,
    mut query: Query,
    mut between: impl FnMut(),
) -> Vec<Page> {
    let mut result = Vec::new();
    loop {
        let page = read(repo, attribution, &query).unwrap();
        assert!(page.has_more || page.next_cursor.is_none());
        let next = page.next_cursor.clone();
        assert_ne!(query.cursor, next, "every continuation makes progress");
        result.push(page);
        if next.is_none() {
            return result;
        }
        if result.len() == 1 {
            between();
        }
        query.cursor = next;
    }
}

fn texts(pages: &[Page]) -> Vec<(String, String)> {
    pages
        .iter()
        .flat_map(|page| &page.contributions)
        .flat_map(|c| {
            c.messages
                .iter()
                .map(|m| (c.commit.clone(), m.text.clone()))
        })
        .collect()
}

/// Pins the selection contract: committer time inside `[since, before)`, the author's own
/// commits, only text appended since the first parent, and no tool, thinking, synthetic or
/// foreign-session records. A reader that returned cumulative LOGs would repeat earlier turns; one
/// that filtered by raw `role` would leak tool results stored as user records.
///
/// Pagination is keyset-based: a save that lands after the cursor extends the result. A cursor
/// bound to a snapshot fingerprint would reject the continuation instead.
#[test]
fn saved_contributions_use_committer_time_and_continue_across_later_saves() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repo::init(temp.path()).unwrap();
    let sid = format!("agit-{}", "a".repeat(40));
    let foreign = format!("agit-{}", "b".repeat(40));
    let mut log = record(&sid, "user", "before range");
    save(
        &repo,
        &log,
        &sid,
        "alice",
        "2026-09-30T12:00:00Z",
        meta::LayoutVersion::V0,
        1,
    );
    log += &record(&sid, "user", "repeated request");
    log += &record(&sid, "assistant", "answer one");
    let first = save(
        &repo,
        &log,
        &sid,
        "alice",
        "2026-10-01T00:00:00Z",
        meta::LayoutVersion::V1,
        2,
    );
    log += &record(&sid, "user", "bob's request");
    save(
        &repo,
        &log,
        &sid,
        "bob",
        "2026-10-01T01:00:00Z",
        meta::LayoutVersion::V1,
        3,
    );
    log += &record(&sid, "user", "repeated request");
    log += &record(&foreign, "user", "foreign inherited prompt");
    log += &transcript::wrap_lines(
        &format!(
            "{}\n{}\n{}\n",
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private thinking"},{"type":"text","text":"answer two"},{"type":"tool_use","id":"t1","name":"shell","input":{"command":"tool secret"}}]}}),
            json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"tool output"}]}}),
            json!({"type":"user","agit":"merge_summary","message":{"role":"user","content":"synthetic summary"}})
        ),
        "claude-code",
        &sid,
    );
    // Shares the first save's time; turn order keeps the batch in conversation order.
    let latest = save(
        &repo,
        &log,
        &sid,
        "alice",
        "2026-10-01T00:00:00Z",
        meta::LayoutVersion::V1,
        4,
    );
    log += &record(&sid, "user", "at exclusive end");
    save(
        &repo,
        &log,
        &sid,
        "alice",
        "2026-10-03T00:00:00Z",
        meta::LayoutVersion::V1,
        5,
    );
    let mut late = String::new();
    let pages = pages(&repo, &mut Fixed::default(), query(), || {
        log += &record(&sid, "user", "saved while paginating");
        late = save(
            &repo,
            &log,
            &sid,
            "alice",
            "2026-10-02T00:00:00Z",
            meta::LayoutVersion::V1,
            6,
        );
    });
    assert!(pages.iter().all(|page| !page.incomplete));
    assert_eq!(
        texts(&pages),
        [
            (first.clone(), "repeated request".into()),
            (first.clone(), "answer one".into()),
            (latest.clone(), "repeated request".into()),
            (latest.clone(), "answer two".into()),
            (late, "saved while paginating".into()),
        ]
    );
    let contribution = &pages[0].contributions[0];
    assert_eq!(contribution.saved_at, "2026-10-01T00:00:00Z");
    assert_eq!(contribution.time_basis, TimeBasis::CommittedAt);
    assert_eq!(contribution.turn, Some(2));
    assert_eq!(contribution.messages[0].role, "user");

    repo.git(&["checkout", "-qb", "forked-session"]).unwrap();
    let forked_sid = format!("agit-{}", "d".repeat(40));
    log += &record(&forked_sid, "user", "fork contribution");
    save(
        &repo,
        &log,
        &forked_sid,
        "alice",
        "2026-10-02T12:00:00Z",
        meta::LayoutVersion::V1,
        1,
    );
    let mut fork = query();
    fork.session = Some("forked-session".into());
    fork.limit = Some(200);
    let fork = read(&repo, &mut Fixed::default(), &fork).unwrap();
    assert!(!fork.incomplete);
    assert_eq!(fork.contributions.len(), 1);
    assert_eq!(fork.contributions[0].messages[0].text, "fork contribution");

    let mut by_email = query();
    by_email.author = "alice@example.com".into();
    by_email.session = Some(sid);
    let page = read(&repo, &mut Fixed::default(), &by_email).unwrap();
    by_email.cursor = page.next_cursor;
    by_email.author = "bob".into();
    assert!(
        read(&repo, &mut Fixed::default(), &by_email)
            .unwrap_err()
            .is::<RequestError>(),
        "a cursor cannot change its filters"
    );
}

/// Pins chunking and completeness across pages: long text splits on UTF-8 boundaries with byte
/// offsets that concatenate back to the message, and an unreadable contribution on an earlier
/// page keeps `incomplete` set on the last one. A per-page flag would let a reader that only
/// inspects the final page report a partial result as complete.
#[test]
fn message_chunks_preserve_utf8_and_incompleteness_spans_pages() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repo::init(temp.path()).unwrap();
    let broken = format!("agit-{}", "e".repeat(40));
    let mut metadata = Meta::new(broken, "claude-code".into(), "/fixture".into());
    metadata.turn = Some(1);
    std::fs::create_dir_all(repo.root().join("session")).unwrap();
    std::fs::write(
        repo.root().join(meta::FILE),
        serde_json::to_vec(&metadata).unwrap(),
    )
    .unwrap();
    std::fs::write(repo.root().join(meta::LOG_FILE), "broken\n").unwrap();
    let unreadable = commit(&repo, "alice", "2026-10-01T00:00:00Z");
    let sid = format!("agit-{}", "c".repeat(40));
    // Fixture data exercises UTF-8 chunk boundaries.
    let text = "你好".repeat(60_000);
    save(
        &repo,
        &record(&sid, "assistant", &text),
        &sid,
        "alice",
        "2026-10-01T01:00:00Z",
        meta::LayoutVersion::V1,
        1,
    );
    let pages = pages(&repo, &mut Fixed::default(), query(), || {
        repo.git(&["branch", "another-session"]).unwrap();
    });
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].reasons["unreadable_commit"].examples, [unreadable]);
    let last = pages.last().unwrap();
    assert!(last.incomplete);
    assert_eq!(last.reasons["unreadable_commit"].count, 1);
    let chunks: Vec<_> = pages
        .iter()
        .flat_map(|page| &page.contributions)
        .flat_map(|c| &c.messages)
        .collect();
    assert_eq!(chunks.len(), 2);
    assert!(!chunks[0].text_complete);
    assert!(chunks[1].text_complete);
    assert_eq!(chunks[1].text_offset, chunks[0].text.len());
    assert_eq!(format!("{}{}", chunks[0].text, chunks[1].text), text);
}

pub(crate) fn assert_unattributed_projection(repo: &Repo, commit: &str) {
    let missing = read(repo, &mut Fixed::default(), &query()).unwrap();
    assert!(missing.incomplete);
    assert!(
        missing.reasons["missing_publication_metadata"]
            .examples
            .iter()
            .any(|id| id == commit)
    );
    assert!(missing.contributions.is_empty());
}

/// Real publication output retains source Git headers and a separate logical session identity.
/// Attribution must come from accepted Hub records: without them the projected commits make the
/// result incomplete rather than empty-and-complete, and with them the message reports the
/// logical session, the saving account and the acceptance time.
pub(crate) fn assert_encrypted_publication_activity(
    repo: &Repo,
    manifest: &crate::domain::session_privacy::SessionPublicationManifest,
) {
    use crate::domain::session_privacy::{CommitterAttribution, PublishedSnapshot};
    let mut attribution = Fixed {
        facts: Facts {
            account: Some(Account {
                id: "writer-bob".into(),
                username: "bob".into(),
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let mut accepted = BTreeSet::new();
    for entry in &manifest.snapshots {
        if let PublishedSnapshot::SessionOwnerV2 { snapshot, .. } = entry {
            let binding = &snapshot.binding;
            accepted.insert(snapshot.published_commit.clone());
            attribution.aliases.push(Alias {
                logical: binding.session_id.clone(),
                public: binding.published_session_id.clone(),
            });
            if let CommitterAttribution::Account { account_id } = &binding.committer
                && account_id == "writer-bob"
            {
                attribution.facts.publications.push(Publication {
                    commit: snapshot.published_commit.clone(),
                    session: binding.session_id.clone(),
                    public_session: binding.published_session_id.clone(),
                    source_commit: binding.source_commit.clone(),
                    account_id: account_id.clone(),
                    username: Some("bob".into()),
                    published_at: "2026-10-01T12:00:00+00:00".into(),
                });
            }
        }
    }
    attribution
        .facts
        .publications
        .sort_by(|a, b| a.commit.cmp(&b.commit));
    let mut query = query();
    query.author = "writer-bob".into();
    query.limit = Some(50);
    let page = |attribution: &mut Fixed, query: &Query| {
        super::page(
            &mut local::LocalStore::new(repo),
            attribution,
            "alice/work",
            "encrypted",
            query,
        )
        .unwrap()
    };
    let publication = attribution.facts.publications[0].clone();
    assert_unattributed_projection(repo, &publication.commit);
    attribution.accepted = accepted;
    query.session = Some(format!("agit-{}", "b".repeat(40)));
    let result = page(&mut attribution, &query);
    assert!(!result.incomplete, "{:?}", result.reasons);
    assert_eq!(result.contributions.len(), 1);
    let contribution = &result.contributions[0];
    assert_eq!(contribution.messages.len(), 1);
    assert_eq!(contribution.messages[0].text, "Private session Alice");
    assert_eq!(contribution.session, query.session.unwrap());
    assert_eq!(contribution.time_basis, TimeBasis::PublishedAt);
    assert_eq!(contribution.saved_at, "2026-10-01T12:00:00Z");
    assert_eq!(
        contribution.author_account_id.as_deref(),
        Some("writer-bob")
    );

    // Acceptance remains authoritative even when the source author's identity matches the query.
    query.session = Some(publication.public_session);
    query.author = "original@example.invalid".into();
    query.since = "1970-01-01".into();
    query.before = "2100-01-01".into();
    let unclaimed = page(&mut Fixed::default(), &query);
    assert!(unclaimed.incomplete);
    assert!(unclaimed.contributions.is_empty());
}

/// Pins record-window continuation: a contribution larger than one read window is emitted in
/// order across pages without skipping the commit, repeating its parent's records, or emitting
/// tool results and inherited compaction context.
///
/// With `Assistant::Last`, commentary before a tool call is dropped even though the answer that
/// follows it lies in a later read window; deciding message by message within one window would
/// keep the commentary as if it ended the turn.
#[test]
fn oversized_contributions_continue_inside_the_commit_without_repeating_tools_or_context() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Repo::init(temp.path()).unwrap();
    let sid = format!("agit-{}", "c".repeat(40));
    let mut log = record(&sid, "user", &"Historical evidence. ".repeat(450_000));
    save(
        &repo,
        &log,
        &sid,
        "alice",
        "2026-09-01T00:00:00Z",
        meta::LayoutVersion::V1,
        1,
    );
    log += &record(&sid, "user", "New request");
    log += &record(&sid, "assistant", "Checking the logs first.");
    log += &transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"large","content":"x".repeat(9 * 1024 * 1024)}
            ]}})
        ),
        "claude-code",
        &sid,
    );
    log += &transcript::wrap_lines(
        &format!(
            "{}\n",
            json!({"type":"compacted","payload":{"replacement_history":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Inherited context"}]}
            ]}})
        ),
        "codex",
        &sid,
    );
    let answer = "Long answer. ".repeat(30_000);
    log += &record(&sid, "assistant", &answer);
    save(
        &repo,
        &log,
        &sid,
        "alice",
        "2026-10-01T00:00:00Z",
        meta::LayoutVersion::V1,
        2,
    );
    for (assistant, leading) in [
        (
            Assistant::All,
            vec![("New request", 2), ("Checking the logs first.", 3)],
        ),
        (Assistant::Last, vec![("New request", 2)]),
    ] {
        let mut query = query();
        query.assistant = assistant;
        let pages = pages(&repo, &mut Fixed::default(), query, || {});
        assert!(pages.iter().all(|page| !page.incomplete));
        let output: Vec<_> = pages
            .iter()
            .flat_map(|page| &page.contributions)
            .flat_map(|c| &c.messages)
            .collect();
        let (head, tail) = output.split_at(leading.len());
        assert_eq!(
            head.iter()
                .map(|m| (m.text.as_str(), m.line))
                .collect::<Vec<_>>(),
            leading,
            "{assistant:?}"
        );
        let mut restored = String::new();
        for message in tail {
            assert_eq!(message.role, "assistant");
            assert_eq!(message.line, 6);
            assert_eq!(message.text_offset, restored.len());
            restored.push_str(&message.text);
        }
        assert_eq!(restored, answer);
        assert!(tail.last().unwrap().text_complete);
    }
}
