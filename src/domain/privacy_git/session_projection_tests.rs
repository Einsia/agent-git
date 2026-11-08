use super::*;
use serde_json::{Value, json};

const SESSION: &str = "agit-eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

fn line(source: &str, content: Value) -> String {
    storage::envelope_line(&Envelope {
        source: source.into(),
        session_id: SESSION.into(),
        object_hash: crate::domain::transcript::object_hash(&content),
        content,
    })
}

fn claude(content: Value) -> String {
    line("claude-code", content)
}

fn git(
    root: &std::path::Path,
    args: &[&str],
    work: Option<(&std::path::Path, &std::path::Path)>,
) -> String {
    let mut command = std::process::Command::new("git");
    command
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(["-c", "commit.gpgsign=false"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        );
    match work {
        Some((tree, index)) => {
            command
                .current_dir(tree)
                .env("GIT_DIR", root.join(".git"))
                .env("GIT_WORK_TREE", tree)
                .env("GIT_INDEX_FILE", index);
        }
        None => {
            command.arg("-C").arg(root);
        }
    }
    let output = command.args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// A repository whose commits are whole session snapshots, each built from nothing so that its
/// tree holds exactly the events, and the extra files, it is given.
struct History {
    root: tempfile::TempDir,
    staging: tempfile::TempDir,
    commits: Vec<String>,
}

impl History {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"], None);
        Self {
            root,
            staging: tempfile::tempdir().unwrap(),
            commits: Vec::new(),
        }
    }

    fn source(&self) -> Repo {
        Repo::at(self.root.path()).local_objects_only()
    }

    /// A snapshot of `log` and `view` with `parents`, holding `extra` files beside its events and
    /// without the events `missing` names.
    fn commit(
        &mut self,
        parents: &[&str],
        log: &[String],
        view: &[String],
        extra: &[(String, String)],
        missing: &[&String],
    ) -> String {
        let work = self.staging.path().join(self.commits.len().to_string());
        std::fs::create_dir_all(&work).unwrap();
        storage::write_snapshot(&work, &log.concat(), &view.concat()).unwrap();
        meta::write(
            &work,
            &meta::Meta::new(SESSION.into(), "claude-code".into(), String::new()),
        )
        .unwrap();
        for (path, text) in extra {
            let path = work.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        for line in missing {
            let id = storage::event_id(line).unwrap();
            std::fs::remove_file(work.join(meta::event_path(&id).unwrap())).unwrap();
        }
        let index = self.staging.path().join("index");
        let _ = std::fs::remove_file(&index);
        git(self.root.path(), &["add", "-A", "."], Some((&work, &index)));
        let tree = git(self.root.path(), &["write-tree"], Some((&work, &index)));
        let mut args = vec!["commit-tree", tree.as_str(), "-m", "turn"];
        for parent in parents {
            args.extend(["-p", parent]);
        }
        let oid = git(self.root.path(), &args, None);
        self.commits.push(oid.clone());
        oid
    }
}

/// Projects `commits` in order with one projector, as one publication run does, checks each
/// against the whole-LOG projection, and returns the last one's public commit.
fn assert_equivalent(history: &History, commits: &[(String, Vec<String>)]) -> Option<String> {
    let source = history.source();
    let mut projector = Projector::default();
    let mut rechecked = Rechecked::default();
    let mut last = None;
    for (oid, parents) in commits {
        let projected = projector
            .project(&source, oid, parents, &Published::default(), &mut rechecked)
            .unwrap();
        let whole = conversation_commit(
            &source,
            oid,
            parents,
            &Published::default(),
            Carried::default(),
            &mut Rechecked::default(),
        )
        .unwrap();
        assert_eq!(projected, whole, "snapshot {oid}");
        last = projected.commit;
    }
    last
}

/// A long session's snapshots, projected one after another as a publication introduces them, are
/// each published exactly as projecting their whole transcript publishes them: the same commit
/// (and so the same tree, LOG, VIEW and event objects) and the same records reported as left
/// out. The history exercises every input the record rule carries from one record to the next,
/// across commit boundaries:
/// - a left-out record whose kept child arrives in a later commit and is linked to an ancestor;
/// - an OpenCode part whose role comes from a header an earlier commit recorded;
/// - a record rewritten by the rule, and one holding a left-out placeholder;
/// - a duplicate line between records that change how it is published, where its first occurrence
///   names it;
/// - a VIEW that rewinds;
/// - two children of one commit, and a merge of them;
/// - a rewritten LOG, and a revert that leaves a named event's object behind;
/// - event objects the LOG never names, in a step that continues and in one that starts again,
///   and a commit that adds one;
/// - a tool step's context this device's delivery records vouch for, published once as the typed
///   message it was, and an identical context after it, which the spent record no longer vouches
///   for.
///
/// A projector that started each commit's rule afresh, continued from a sibling or across a
/// rewritten LOG, kept a duplicate's last occurrence, tracked the tree's event objects from the
/// appended records alone, or spent delivery records afresh at each commit or not at all publishes
/// something here that the whole-LOG projection does not.
#[test]
fn every_snapshot_is_projected_as_its_whole_transcript_is() {
    let home = tempfile::tempdir().unwrap();
    crate::rc::with_agit_home(
        home.path(),
        every_snapshot_is_projected_as_its_whole_transcript_is_in,
    );
}

fn every_snapshot_is_projected_as_its_whole_transcript_is_in() {
    use crate::domain::privacy::{projector::UNREADABLE_TEXT, service::OMITTED_TEXT};
    use crate::rc::step_delivery;
    const CONVERSATION: &str = "4e6c2b1a-0d0e-4c43-9a51-8f0e2f4a7b10";
    const WORDS: &str = "also update the README";
    let sent = step_delivery::framed(WORDS);
    step_delivery::record(CONVERSATION, "client-1", &sent).unwrap();
    let context = |uuid: &str| {
        claude(
            json!({"type":"attachment","uuid":uuid,"parentUuid":"a","sessionId":CONVERSATION,
            "timestamp":"2026-11-06T08:00:00.000Z","attachment":{"type":"hook_additional_context",
            "hookName":"tool.call","hookEvent":"PostToolUse","toolUseID":"toolu_1-context",
            "content":[sent]}}),
        )
    };
    let mut history = History::new();
    let mut log = vec![
        claude(
            json!({"type":"user","uuid":"a","parentUuid":null,"message":{"role":"user","content":"hi"}}),
        ),
        claude(
            json!({"type":"attachment","uuid":"b","parentUuid":"a","attachment":{"type":"instructions"}}),
        ),
        claude(
            json!({"type":"assistant","uuid":"c","parentUuid":"b","message":{"role":"assistant","content":"hello"}}),
        ),
    ];
    let mut order = Vec::new();
    let mut tip = history.commit(&[], &log, &log, &[], &[]);
    order.push((tip.clone(), Vec::new()));
    let unnamed = (
        "events/0/0/0/0/0000000000000000000000000000000000000000".to_owned(),
        "not an event\n".to_owned(),
    );
    let added = (
        "events/f/f/f/f/ffffffffffffffffffffffffffffffffffffffff".to_owned(),
        "added later\n".to_owned(),
    );
    for turn in 0..200usize {
        let parent = tip.clone();
        if turn == 101 {
            // A revert: the parent's last record leaves the LOG, and its object stays in the tree.
            let reverted = log.pop().unwrap();
            let path = meta::event_path(&storage::event_id(&reverted).unwrap()).unwrap();
            tip = history.commit(&[&parent], &log, &log, &[(path, reverted)], &[]);
            order.push((tip.clone(), vec![parent]));
            continue;
        }
        let uuid = |suffix: &str| format!("u{turn}{suffix}");
        let mut view_back = 0;
        match turn % 10 {
            0 => log.push(claude(json!({"type":"user","uuid":uuid("r"),"parentUuid":"a",
                "message":{"role":"user","content":format!("rename it <system-reminder>note {turn}</system-reminder>")}}))),
            1 => log.push(claude(json!({"type":"system","uuid":uuid("s"),"parentUuid":"a","subtype":"turn_duration"}))),
            2 => log.push(claude(json!({"type":"assistant","uuid":uuid("k"),"parentUuid":format!("u{}s", turn - 1),
                "message":{"role":"assistant","content":format!("answer {turn}")}}))),
            3 => log.push(line("opencode", json!({"id":format!("m{turn}"),"kind":"message","session_id":"ses",
                "time_created":1,"data":{"role":"assistant"}}))),
            4 => log.push(line("opencode", json!({"id":format!("p{turn}"),"kind":"part","message_id":format!("m{}", turn - 1),
                "data":{"type":"text","text":format!("reply {turn}")}}))),
            5 => {
                log.push(claude(json!({"type":"user","uuid":uuid("o"),"parentUuid":"a",
                    "message":{"role":"user","content":format!("{OMITTED_TEXT} {turn}")}})));
                log.push(claude(json!({"type":"user","uuid":uuid("n"),"parentUuid":"a",
                    "message":{"role":"user","content":format!("{UNREADABLE_TEXT} {turn}")}})));
            }
            6 => {
                // The same part under a header that turns from the user's into the assistant's:
                // its first occurrence is left out, its second is kept.
                let header = |role: &str| line("opencode", json!({"id":format!("d{turn}"),"kind":"message",
                    "session_id":"ses","time_created":1,"data":{"role":role}}));
                let part = line("opencode", json!({"id":format!("q{turn}"),"kind":"part","message_id":format!("d{turn}"),
                    "data":{"type":"text","text":"same words"}}));
                log.extend([header("user"), part.clone(), header("assistant"), part]);
            }
            7 => {
                log.push(claude(json!({"type":"user","uuid":uuid("v"),"parentUuid":"a","message":{"role":"user","content":"rewind"}})));
                view_back = 2;
            }
            _ => log.push(claude(json!({"type":"user","uuid":uuid("n"),"parentUuid":"a",
                "message":{"role":"user","content":format!("turn {turn}")}}))),
        }
        if turn == 30 || turn == 31 {
            log.push(context(&uuid("x")));
        }
        if turn == 100 {
            // A LOG rewritten in the middle.
            log.remove(5);
        }
        let view = log[..log.len() - view_back].to_vec();
        let mut extra = Vec::new();
        if (120..140).contains(&turn) {
            extra.push(unnamed.clone());
        }
        if (130..140).contains(&turn) {
            extra.push(added.clone());
        }
        if turn == 50 {
            // Two children of one commit, published in one run, then their merge.
            let mut left = log.clone();
            left.push(claude(json!({"type":"user","uuid":"left","parentUuid":"a","message":{"role":"user","content":"left"}})));
            let one = history.commit(&[&parent], &left, &left, &[unnamed.clone()], &[]);
            let two = history.commit(&[&parent], &log, &view, &[], &[]);
            order.push((one.clone(), vec![parent.clone()]));
            order.push((two.clone(), vec![parent.clone()]));
            log.push(claude(json!({"type":"user","uuid":"merged","parentUuid":"a","message":{"role":"user","content":"merged"}})));
            tip = history.commit(&[&one, &two], &log, &log, &[unnamed.clone()], &[]);
            order.push((tip.clone(), vec![one, two]));
            continue;
        }
        tip = history.commit(&[&parent], &log, &view, &extra, &[]);
        order.push((tip.clone(), vec![parent]));
    }
    let last =
        assert_equivalent(&history, &order).expect("records are published other than as recorded");
    // The person's words go out once, as the typed message; the second context stays back.
    let carrying = git(
        history.root.path(),
        &["grep", "-l", "-F", WORDS, &last, "--", "events"],
        None,
    );
    assert_eq!(carrying.lines().count(), 1, "{carrying}");
}

/// An event the LOG names whose object the tree lacks refuses the projection, in a step that
/// continues and in one that starts again, as it refuses the whole-LOG projection: neither leaves
/// it out nor skips it. So does an event the parent already held whose object the child removed
/// or replaced: a projector that continued from what it read in the parent would publish the
/// parent's copy, where the whole-LOG projection reads the child's tree.
#[test]
fn a_missing_event_refuses_the_projection_as_it_refuses_the_whole_transcript() {
    let mut history = History::new();
    let first = claude(
        json!({"type":"user","uuid":"a","parentUuid":null,"message":{"role":"user","content":"hi"}}),
    );
    let second = claude(
        json!({"type":"user","uuid":"b","parentUuid":"a","message":{"role":"user","content":"next"}}),
    );
    let base = history.commit(&[], &[first.clone()], &[first.clone()], &[], &[]);
    let both = [first.clone(), second.clone()];
    let broken = history.commit(&[&base], &both, &both, &[], &[&second]);
    let source = history.source();
    let parents = vec![base.clone()];
    let mut rechecked = Rechecked::default();
    assert!(
        conversation_commit(
            &source,
            &broken,
            &parents,
            &Published::default(),
            Carried::default(),
            &mut rechecked
        )
        .is_err()
    );
    let mut continuing = Projector::default();
    continuing
        .project(&source, &base, &[], &Published::default(), &mut rechecked)
        .unwrap();
    assert!(
        continuing
            .project(
                &source,
                &broken,
                &parents,
                &Published::default(),
                &mut rechecked
            )
            .is_err()
    );
    assert!(
        Projector::default()
            .project(
                &source,
                &broken,
                &parents,
                &Published::default(),
                &mut rechecked
            )
            .is_err()
    );

    // An event the parent held, removed from the child's tree or replaced there by other bytes,
    // while the child keeps the parent's LOG.
    let held = meta::event_path(&storage::event_id(&first).unwrap()).unwrap();
    let removed = history.commit(&[&base], &both, &both, &[], &[&first]);
    let replaced = history.commit(
        &[&base],
        &both,
        &both,
        &[(
            held,
            claude(json!({"type":"user","uuid":"x","message":{"role":"user","content":"other"}})),
        )],
        &[],
    );
    let source = history.source();
    for child in [removed, replaced] {
        let whole = conversation_commit(
            &source,
            &child,
            &parents,
            &Published::default(),
            Carried::default(),
            &mut rechecked,
        );
        let mut continuing = Projector::default();
        continuing
            .project(&source, &base, &[], &Published::default(), &mut rechecked)
            .unwrap();
        let continued = continuing.project(
            &source,
            &child,
            &parents,
            &Published::default(),
            &mut rechecked,
        );
        assert!(
            whole.is_err(),
            "the whole-LOG projection reads the child's tree"
        );
        assert!(continued.is_err(), "a continued step reads it too");
    }
}

/// A difference that removes one event object and adds another, from a parent at the cap, leaves
/// the count to the full listing: the child holds no more than its parent. A count kept against
/// the parent's set, which the removal leaves stale, refuses a snapshot the full listing accepts.
#[test]
fn an_addition_at_the_cap_leaves_the_count_to_the_full_listing() {
    let mut history = History::new();
    let first =
        claude(json!({"type":"user","uuid":"u1","message":{"role":"user","content":"deploy"}}));
    let dropped = claude(
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"text","text":"Done."}]}}),
    );
    let added = claude(
        json!({"type":"user","uuid":"u2","parentUuid":"u1","message":{"role":"user","content":"again"}}),
    );
    let parent_log = vec![first.clone(), dropped];
    let parent = history.commit(&[], &parent_log, &parent_log, &[], &[]);
    let child_log = vec![first, added];
    let child = history.commit(&[&parent], &child_log, &child_log, &[], &[]);
    let held: BTreeSet<String> = git(
        history.root.path(),
        &[
            "ls-tree",
            "-r",
            "--name-only",
            &parent,
            "--",
            meta::EVENTS_DIR,
        ],
        None,
    )
    .lines()
    .map(str::to_owned)
    .collect();
    assert_eq!(held.len(), 2);
    let source = history.source();
    assert_eq!(held_after(&source, held, &parent, &child, 2).unwrap(), None);
}

/// A projector continues only from the commit it projected last. A snapshot whose LOG extends
/// that commit's LOG, but whose parent is another commit, starts again and takes the lines its
/// own public parent published. Here that public parent published a record in another form than
/// the recorded one, as protection under a later rule writes it; a projector that continued from
/// the commit it held would publish the recorded form instead.
#[test]
fn a_projector_continues_only_from_the_commit_it_projected_last() {
    let mut history = History::new();
    let first = claude(
        json!({"type":"user","uuid":"u1","message":{"role":"user","content":"deploy with the value"}}),
    );
    let reply = claude(
        json!({"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"text","text":"Done."}]}}),
    );
    let more = claude(
        json!({"type":"user","uuid":"u2","parentUuid":"a1","message":{"role":"user","content":"next"}}),
    );
    let base_log = vec![first.clone()];
    let base = history.commit(&[], &base_log, &base_log, &[], &[]);
    let held_log = vec![first.clone(), reply.clone()];
    let held = history.commit(&[&base], &held_log, &held_log, &[], &[]);
    let sibling_log = vec![first.clone(), reply.clone(), more.clone()];
    let sibling = history.commit(&[&base], &sibling_log, &sibling_log, &[], &[]);
    let published_first = claude(
        json!({"type":"user","uuid":"u1","message":{"role":"user","content":"deploy with ‹secret:blocked:abcdefghijkl›"}}),
    );
    let public_log = vec![published_first.clone()];
    let public = history.commit(&[], &public_log, &public_log, &[], &[]);
    let source = history.source();
    let published_id = storage::event_id(&published_first).unwrap();
    let published = Published {
        ids: [published_id.clone()].into_iter().collect(),
        prefix: Some(Prefix {
            source_lines: 1,
            public: vec![published_id],
            holder: &source,
            commit: public,
        }),
    };

    let mut projector = Projector::default();
    let mut rechecked = Rechecked::default();
    for (oid, parents) in [(&base, vec![]), (&held, vec![base.clone()])] {
        projector
            .project(
                &source,
                oid,
                &parents,
                &Published::default(),
                &mut rechecked,
            )
            .unwrap();
    }
    let parents = vec![base.clone()];
    let projected = projector
        .project(&source, &sibling, &parents, &published, &mut rechecked)
        .unwrap();
    let whole = conversation_commit(
        &source,
        &sibling,
        &parents,
        &published,
        Carried::default(),
        &mut Rechecked::default(),
    )
    .unwrap();
    assert_eq!(projected, whole);
    let commit = projected.commit.expect("the snapshot is rewritten");
    let limit = storage::MAX_MATERIALIZED_BYTES;
    let (log, _) = storage::materialize_pair_local(source.root(), &commit, limit, limit).unwrap();
    assert!(log.contains("‹secret:blocked:abcdefghijkl›"), "{log}");
}
