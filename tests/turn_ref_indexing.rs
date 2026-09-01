//! `<ref>#n` and `agit log`'s left column are one numbering: both count only the commits that
//! settled a turn.
//!
//! A real branch's first-parent chain also carries birth, claim, fork-identity, file and merge
//! commits, none of which take a turn ordinal; the latter four inherit the ordinal carried by
//! HEAD. If `log` printed the inherited value, one number would point at several commits; if `#n`
//! counted by position, `fork` / `show` would silently land on a different commit. This pins both
//! sides at once, from the CLI.

use agit::domain::meta::{self, Meta};
use agit::domain::repo::Repo;
use agit::domain::{storage, transcript};
use sha2::{Digest, Sha256};
use std::{fs, process::Command};

fn claim() -> String {
    format!("{}{}", meta::ID_PREFIX, "a".repeat(meta::ID_HEX_LEN))
}

fn settle_turn(r: &Repo, turn: u32) {
    let mut log = r.show("HEAD", meta::LOG_FILE).unwrap_or_default();
    if !log.is_empty() && !log.ends_with('\n') {
        log.push('\n');
    }
    let raw = format!(
        "{{\"type\":\"user\",\"sessionId\":\"s1\",\"message\":{{\"role\":\"user\",\"content\":\"PROMPT-{turn}\"}}}}\n\
         {{\"type\":\"assistant\",\"sessionId\":\"s1\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"REPLY-{turn}\"}}]}}}}\n"
    );
    log.push_str(&transcript::wrap_lines(&raw, "claude-code", &claim()));
    storage::write_snapshot(r.root(), &log, &log).unwrap();
    let mut m = Meta::new(claim(), "claude-code".into(), "/w".into());
    m.turn = Some(turn);
    meta::write(r.root(), &m).unwrap();
    r.add_all().unwrap();
    assert!(r.commit(&format!("agit: turn {turn}")).unwrap());
}

fn inherit_commit(r: &Repo, kind: meta::Kind, message: &str) {
    let mut m = meta::read_at_ref(r, "HEAD").unwrap();
    m.kind = kind;
    meta::write(r.root(), &m).unwrap();
    fs::write(r.root().join("AGENTS.md"), format!("# {message}\n")).unwrap();
    r.add_all().unwrap();
    assert!(r.commit(message).unwrap());
}

/// main (init) → s1 (claim + turns 1..3, turn 4 later) → f1 (forked at turn 3; turn 4, file,
/// merge s1).
fn forked_history(root: &std::path::Path) -> Repo {
    let r = Repo::init(root).unwrap();
    r.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::ensure_session_dir(r.root()).unwrap();
    meta::write(r.root(), &Meta::new_file_line()).unwrap();
    fs::write(r.root().join("AGENTS.md"), "# shared\n").unwrap();
    r.add_all().unwrap();
    assert!(r.commit("agit: init").unwrap());

    r.git(&["checkout", "-q", "-b", "s1"]).unwrap();
    meta::write(
        r.root(),
        &Meta::new_session_line("claude-code".into(), "/w".into()),
    )
    .unwrap();
    storage::write_snapshot(r.root(), "", "").unwrap();
    r.add_all().unwrap();
    assert!(r.commit("agit: claim session line s1").unwrap());
    for turn in 1..=3 {
        settle_turn(&r, turn);
    }
    r.git(&["checkout", "-q", "-b", "f1"]).unwrap();
    inherit_commit(&r, meta::Kind::File, "agit: fork f1 from s1");
    settle_turn(&r, 4);
    inherit_commit(&r, meta::Kind::File, "add note");
    r.git(&["checkout", "-q", "s1"]).unwrap();
    settle_turn(&r, 4);
    r.git(&["checkout", "-q", "f1"]).unwrap();
    r.git(&["merge", "-s", "ours", "--no-commit", "s1"])
        .unwrap();
    inherit_commit(&r, meta::Kind::Merge, "agit: merge s1 into f1");
    r
}

fn turn_sha(r: &Repo, branch: &str, turn: u32) -> String {
    r.git(&["log", "--first-parent", "--format=%H%x00%s", branch])
        .unwrap()
        .lines()
        .find_map(|l| {
            let (sha, subject) = l.split_once('\0')?;
            (subject == format!("agit: turn {turn}")).then(|| sha.to_string())
        })
        .unwrap()
}

#[test]
fn log_show_and_fork_agree_on_turn_numbers() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    fs::create_dir_all(&work).unwrap();
    let repo = forked_history(&home.join("repos/drh/qa"));

    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(&work)
            .env("AGIT_HOME", &home)
            .env_remove("AGIT_SESSION")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };

    // ① log: only a turn commit carries `#n`, each turn ordinal appears once, and every other
    // commit leaves the left column blank.
    let log = run(&["log", "drh/qa@f1", "--oneline"]);
    let numbered: Vec<&str> = log.lines().filter(|l| l.starts_with('#')).collect();
    assert_eq!(numbered.len(), 4, "{log}");
    for (i, line) in numbered.iter().enumerate() {
        let turn = i as u32 + 1;
        assert!(line.starts_with(&format!("#{turn:>3} ")), "{line}");
        assert!(line.contains("[turn ]"), "{line}");
        assert!(line.ends_with(&format!("agit: turn {turn}")), "{line}");
    }
    let unnumbered: Vec<&str> = log.lines().filter(|l| l.starts_with("    ")).collect();
    assert_eq!(unnumbered.len(), 5, "{log}");
    assert!(
        unnumbered
            .iter()
            .any(|l| l.contains("agit: fork f1 from s1"))
    );
    assert!(unnumbered.iter().any(|l| l.contains("[merge]")));

    // ② show `<ref>#n`: prints that turn's conversation, not the nth commit on the chain and not
    // a blank.
    let shown = run(&["show", "drh/qa@f1#3"]);
    assert!(
        shown.contains("PROMPT-3") && shown.contains("REPLY-3"),
        "{shown}"
    );
    assert!(
        !shown.contains("PROMPT-2") && !shown.contains("PROMPT-4"),
        "{shown}"
    );
    let last = run(&["show", "drh/qa@f1#-1"]);
    assert!(last.contains("PROMPT-4"), "{last}");

    // ③ fork `<ref>#n`: the new branch grows on the commit of turn n.
    run(&["fork", "drh/qa@f1#2", "-b", "from-turn-2"]);
    let parent = repo
        .git(&["rev-parse", "refs/heads/from-turn-2^"])
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(parent, turn_sha(&repo, "f1", 2));

    // ④ The branch view lists local branches: a repo with no remote still has branches, and must
    // not read as "no branches yet".
    let branches = run(&["log", "drh/qa"]);
    for b in ["main", "s1", "f1", "from-turn-2"] {
        assert!(
            branches.lines().any(|l| l.starts_with(b)),
            "{b} missing:\n{branches}"
        );
    }

    // ⑤ In a workspace bound to a directory with no pinned branch, a bare branch name renders the
    // VIEW, not a session link in the store.
    let canonical = work.canonicalize().unwrap();
    let id = &hex::encode(Sha256::digest(canonical.to_string_lossy().as_bytes()))[..16];
    let workspace_dir = home.join("workspaces");
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::write(
        workspace_dir.join(format!("{id}.json")),
        serde_json::to_vec(&serde_json::json!({ "dir": canonical, "repo": "drh/qa" })).unwrap(),
    )
    .unwrap();
    let bare = run(&["show", "s1"]);
    for turn in 1..=4 {
        assert!(bare.contains(&format!("PROMPT-{turn}")), "{bare}");
    }
}
