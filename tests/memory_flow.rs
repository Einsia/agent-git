//! Memory's landing places, walked from the CLI entry point: `memory sync` collects new files in
//! the runtime directory into the session branch and places the branch's files into a mirror
//! subdirectory of the runtime directory; `memory status` names what is not in main yet;
//! `distill -y` carries them into main, after which status reports everything in sync. The
//! repository's privacy policy allow-lists its memory files, since memory leaves the machine only
//! where the policy lets it, and a distill of a file it keeps here is refused before any question. An implementation that treats the runtime directory as the branch
//! itself (rather than a live copy) fails on "no local file moves"; one that writes main on its
//! own fails on "main does not have it before distill".

use agit::domain::meta::{self, Meta};
use agit::domain::repo::Repo;
use std::path::{Path, PathBuf};
use std::{fs, process::Command};

const REPO: &str = "drh/qa";

/// An Agent repo: `memory/team.md` on main, session line `s1` recording the claude-code runtime
/// and the working directory `work`; plus a fake Claude memory directory (located through
/// `CLAUDE_CONFIG_DIR`).
fn fixture(tmp: &Path) -> (PathBuf, Repo, PathBuf, PathBuf) {
    let home = tmp.join("home");
    let work = tmp.join("work");
    fs::create_dir_all(&work).unwrap();
    let repo = Repo::init(&home.join("repos").join(REPO)).unwrap();
    repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
    meta::write(repo.root(), &Meta::new_file_line()).unwrap();
    fs::create_dir_all(repo.root().join("memory")).unwrap();
    fs::write(
        repo.root().join("memory/team.md"),
        "---\nname: team\ndescription: shared\n---\nuid, not user_id\n",
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("agit: init").unwrap();

    let mut snap = Meta::new_session_line(
        "claude-code".into(),
        work.canonicalize().unwrap().to_string_lossy().into_owned(),
    );
    snap.session = format!("{}{}", meta::ID_PREFIX, "c".repeat(meta::ID_HEX_LEN));
    meta::write(repo.root(), &snap).unwrap();
    let mut privacy = agit::domain::privacy::PrivacyPolicy::default();
    privacy.memory_allow.push("**/*.md".into());
    privacy.save(&repo).unwrap();
    repo.git(&["switch", "-q", "-c", "s1"]).unwrap();
    repo.add_all().unwrap();
    repo.commit("agit: new session s1").unwrap();
    repo.switch("main").unwrap();
    meta::write(repo.root(), &Meta::new_file_line()).unwrap();

    let claude = tmp.join("claude-config");
    let project = agit::infra::runtime_memory::encode_project(&work.canonicalize().unwrap());
    let mem = claude.join("projects").join(project).join("memory");
    fs::create_dir_all(&mem).unwrap();
    (home, repo, claude, mem)
}

fn agit(home: &Path, claude: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_agit"))
        .args(args)
        .current_dir(home)
        .env("AGIT_HOME", home)
        .env("CLAUDE_CONFIG_DIR", claude)
        .env_remove("AGIT_SESSION")
        .output()
        .unwrap()
}

fn ok(out: &std::process::Output) -> String {
    assert!(
        out.status.success(),
        "stderr: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn memory_flows_runtime_to_branch_and_branch_to_main_only_on_distill() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo, claude, mem) = fixture(tmp.path());
    let into = format!("{REPO}@s1");
    agit::domain::privacy::PrivacyPolicy::default()
        .save(&repo)
        .unwrap();
    let policy_path = agit::domain::privacy::PrivacyPolicy::path(&repo).unwrap();
    let policy_bytes = fs::read(&policy_path).unwrap();
    fs::write(&policy_path, b"{ invalid policy").unwrap();
    let unreadable = agit(&home, &claude, &["memory", "status", "--into", &into]);
    assert!(!unreadable.status.success(), "{unreadable:?}");
    assert!(
        String::from_utf8_lossy(&unreadable.stderr)
            .contains("cannot read repository memory privacy policy"),
        "{unreadable:?}"
    );
    let display = String::from_utf8_lossy(&unreadable.stdout);
    assert!(
        !display.contains("your memory allowlist for drh/qa on this computer is empty"),
        "{display}"
    );
    assert!(!display.contains("eligible for collection"), "{display}");
    assert_eq!(fs::read(&policy_path).unwrap(), b"{ invalid policy");
    fs::write(&policy_path, policy_bytes).unwrap();
    let status = ok(&agit(
        &home,
        &claude,
        &["memory", "status", "--into", &into],
    ));
    assert!(
        status.contains("your memory allowlist for drh/qa on this computer is empty"),
        "{status}"
    );
    assert!(!status.contains("eligible for collection"), "{status}");
    fs::write(mem.join("mine.md"), "---\nname: mine\n---\nlocal fact\n").unwrap();
    fs::write(
        mem.join("MEMORY.md"),
        "# Memory Index\n\n- [mine](mine.md)\n",
    )
    .unwrap();

    let denied = agit(&home, &claude, &["memory", "sync", "--into", &into]);
    ok(&denied);
    let refusal = String::from_utf8_lossy(&denied.stderr);
    assert!(
        refusal.contains("agit privacy policy allow-memory --repo drh/qa -- mine.md"),
        "{refusal}"
    );
    assert!(repo.show("refs/heads/s1", "memory/mine.md").is_none());
    let status = ok(&agit(
        &home,
        &claude,
        &["memory", "status", "--into", &into],
    ));
    assert!(
        status.contains("not collected (privacy policy)"),
        "{status}"
    );
    assert!(
        status.contains("allow-memory --repo drh/qa -- mine.md"),
        "{status}"
    );
    assert!(!status.contains("eligible for collection"), "{status}");
    assert!(!status.contains("everything in sync"), "{status}");
    let allowed = ok(&agit(
        &home,
        &claude,
        &[
            "privacy",
            "policy",
            "allow-memory",
            "--repo",
            REPO,
            "--",
            "mine.md",
        ],
    ));
    // Allowing names what it covers now, by name only.
    assert!(allowed.contains("it covers mine.md"), "{allowed}");
    assert!(!allowed.contains("local fact"), "{allowed}");
    let status = ok(&agit(
        &home,
        &claude,
        &["memory", "status", "--into", &into],
    ));
    assert!(status.contains("eligible for collection"), "{status}");
    assert!(
        !status.contains("your memory allowlist for drh/qa on this computer is empty"),
        "{status}"
    );

    // sync: the authorized local file enters the branch; the branch's team.md (inherited from main)
    // enters the mirror subdirectory. MEMORY.md stays the person's: agit writes no section into it.
    ok(&agit(&home, &claude, &["memory", "sync", "--into", &into]));
    assert!(repo.show("refs/heads/s1", "memory/mine.md").is_some());
    assert!(
        repo.show("refs/heads/main", "memory/mine.md").is_none(),
        "main never moves by itself"
    );
    assert!(mem.join("agit/drh/qa/s1/team.md").is_file());
    let index = fs::read_to_string(mem.join("MEMORY.md")).unwrap();
    assert!(
        index.contains("- [mine](mine.md)"),
        "local lines untouched: {index}"
    );
    assert!(!index.contains("agit/drh/qa/s1/team.md"), "{index}");
    assert_eq!(
        fs::read_to_string(mem.join("mine.md")).unwrap(),
        "---\nname: mine\n---\nlocal fact\n"
    );

    // status: mine.md is not in main yet.
    let status = ok(&agit(
        &home,
        &claude,
        &["memory", "status", "--into", &into],
    ));
    assert!(status.contains("mine.md"), "{status}");
    assert!(status.contains("not in main"), "{status}");
    assert!(
        status.contains("1 change on `s1` not yet in main"),
        "{status}"
    );

    // A file the person's allowlist keeps on this computer is refused before any question and
    // before anything is written, with the command that allows it.
    agit::domain::privacy::PrivacyPolicy::default()
        .save(&repo)
        .unwrap();
    let refused = agit(&home, &claude, &["distill", "--into", &into]);
    assert_eq!(refused.status.code(), Some(7), "{refused:?}");
    let refusal = String::from_utf8_lossy(&refused.stderr);
    assert!(
        refusal.contains(
            "your memory allowlist for drh/qa on this computer is empty: memory files stay on this computer until you allow them. `agit privacy policy allow-memory --repo drh/qa -- mine.md` would let agit collect mine.md; it is then saved to drh/qa and uploaded with the branch."
        ),
        "{refusal}"
    );
    assert!(!refusal.contains("confirmation"), "{refusal}");
    assert!(repo.show("refs/heads/main", "memory/mine.md").is_none());
    ok(&agit(
        &home,
        &claude,
        &[
            "privacy",
            "policy",
            "allow-memory",
            "--repo",
            REPO,
            "--",
            "mine.md",
        ],
    ));

    // With no terminal to confirm at, distill stops and names the terminal; no line suggests the
    // flag that would also overwrite others' lines unasked.
    let unconfirmed = agit(&home, &claude, &["distill", "--into", &into]);
    assert!(!unconfirmed.status.success(), "{unconfirmed:?}");
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&unconfirmed.stdout),
        String::from_utf8_lossy(&unconfirmed.stderr)
    );
    assert!(
        said.contains(
            "distilling needs confirmation; run it at a terminal to confirm each change."
        ),
        "{said}"
    );
    for line in said.lines() {
        assert!(!line.contains("-y"), "{line}");
    }
    assert!(repo.show("refs/heads/main", "memory/mine.md").is_none());

    // distill -y: into main, after which everything is in sync.
    let distilled = agit(&home, &claude, &["distill", "-y", "--into", &into]);
    ok(&distilled);
    let hint = String::from_utf8_lossy(&distilled.stderr);
    // Read by a program, not a person, the push is offered for when the user asks, not as a step.
    assert!(
        hint.contains(&format!(
            "when the user asks to publish it: `agit push {REPO}@main`"
        )),
        "{hint}"
    );
    // Distill publishes nothing; the session branch, holding the same memory and not on the Hub,
    // is named with its own push.
    assert!(
        hint.contains(&format!(
            "`s1`, which holds this memory too, isn't on the Hub yet: `agit push {REPO}@s1`"
        )),
        "{hint}"
    );
    // The result names the repository whose main it is, which may not be the team's.
    let said = format!("{}{hint}", String::from_utf8_lossy(&distilled.stdout));
    assert!(said.contains(&format!("into {REPO}'s main (")), "{said}");
    assert_eq!(
        repo.git_bytes(&["show", "refs/heads/main:memory/mine.md"])
            .unwrap(),
        b"---\nname: mine\n---\nlocal fact\n"
    );
    assert!(
        meta::read_at_ref(&repo, "refs/heads/main")
            .unwrap()
            .is_file_line()
    );
    let status = ok(&agit(
        &home,
        &claude,
        &["memory", "status", "--into", &into],
    ));
    assert!(status.contains("everything in sync"), "{status}");
}

#[test]
fn a_branch_without_a_runtime_memory_dir_still_reports_and_distills() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo, claude, _mem) = fixture(tmp.path());
    // Turn s1 into a codex session: no per-project memory directory on disk.
    let refname = "refs/heads/s1";
    let mut snap = meta::read_at_ref(&repo, refname).unwrap();
    snap.runtime = "codex".into();
    let tree = agit::commands::plumbing::tree_apply_owned(
        &repo,
        refname,
        vec![(
            meta::FILE.into(),
            Some(meta::to_text(&snap).unwrap().into_bytes()),
        )],
    )
    .unwrap();
    let head = repo.git(&["rev-parse", refname]).unwrap();
    let c = agit::commands::plumbing::commit_tree(&repo, &tree, &[head.trim()], "codex").unwrap();
    repo.git(&["update-ref", refname, &c]).unwrap();

    let into = format!("{REPO}@s1");
    let out = ok(&agit(&home, &claude, &["memory", "sync", "--into", &into]));
    assert!(out.contains("no per-project memory directory"), "{out}");
    let status = ok(&agit(
        &home,
        &claude,
        &["memory", "status", "--into", &into],
    ));
    assert!(status.contains("everything in sync"), "{status}");
}

/// `agit new`'s start line names the repository and commit its memory comes from and how many
/// files the new session has as placing came out, 0 said out loud: a placing that fails is said to
/// have failed, never counted as the source's files. A runtime agit gives no memory to is told it
/// gets none (CLI #502).
#[test]
fn agit_new_names_the_commit_its_memory_comes_from_and_how_many_files() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo, claude, mem) = fixture(tmp.path());
    let work = tmp.path().join("work");
    let start = |args: &[&str]| {
        ok(&Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(&work)
            .env("AGIT_HOME", &home)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env_remove("AGIT_SESSION")
            .output()
            .unwrap())
    };
    let short = |rev: &str| repo.git(&["rev-parse", rev]).unwrap().trim()[..9].to_string();
    let main = short("main");

    let claude_start = start(&["new", REPO, "-b", "fresh", "--no-launch"]);
    assert!(
        claude_start.contains(&format!(
            "new session {REPO} @ fresh (memory: 1 file from {REPO}@{main}, main on this computer)"
        )),
        "{claude_start}"
    );
    assert!(claude_start.contains("memory: 1 file →"), "{claude_start}");

    let codex_start = start(&["new", REPO, "-b", "cx", "--as", "codex", "--no-launch"]);
    assert!(
        codex_start.contains(&format!(
            "new session {REPO} @ cx (memory: 1 file from {REPO}@{main}, main on this computer; \
             given to Codex as an index when agit starts it)"
        )),
        "{codex_start}"
    );

    let opencode_start = start(&["new", REPO, "-b", "oc", "--as", "opencode", "--no-launch"]);
    assert!(
        opencode_start.contains(&format!(
            "new session {REPO} @ oc (memory: none; OpenCode doesn't get agit's memory)"
        )),
        "{opencode_start}"
    );

    // A memory folder that can't be written: the line counts nothing placed.
    fs::remove_dir_all(&mem).unwrap();
    fs::write(&mem, "not a folder").unwrap();
    let blocked = start(&["new", REPO, "-b", "blocked", "--no-launch"]);
    assert!(
        blocked.contains(&format!(
            "new session {REPO} @ blocked (memory from {REPO}@{main}, main on this computer couldn't be placed in full; see the note below)"
        )) && !blocked.contains("memory: 1 file"),
        "{blocked}"
    );
    fs::remove_file(&mem).unwrap();
    fs::create_dir_all(&mem).unwrap();

    repo.git(&["rm", "-q", "memory/team.md"]).unwrap();
    repo.commit("agit: no team memory").unwrap();
    let emptied = short("main");
    let empty_start = start(&["new", REPO, "-b", "empty", "--no-launch"]);
    assert!(
        empty_start.contains(&format!(
            "new session {REPO} @ empty (memory: 0 files; {REPO}@{emptied}, main on this computer, has none)"
        )),
        "{empty_start}"
    );
    assert!(!empty_start.contains("memory: 1 file"), "{empty_start}");
}

/// `agit new` tells its conversation of the team memory it placed on the runtime's own command
/// line and writes nothing into `MEMORY.md`, which every conversation started in the folder reads,
/// whatever its repository, and `agit resume` tells a saved conversation the same way. Sections an
/// older agit wrote, another repository's included, are taken out by the start and by Claude
/// Code's session-start hook, and every other line of the file stays as it was. A Codex start
/// without agit's hooks says it starts without its team memory.
#[test]
fn a_conversation_is_told_its_own_memory_at_launch_and_memory_md_keeps_only_the_persons_lines() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _repo, claude, mem) = fixture(tmp.path());
    let work = tmp.path().join("work").canonicalize().unwrap();
    let codex_home = tmp.path().join("codex");
    fs::create_dir_all(&codex_home).unwrap();
    // A saved sign-in: an import records who wrote the version. The Hub is never reached.
    let hub = "http://127.0.0.1:1";
    agit::infra::credentials::save_at(
        &home.join("credentials").join(format!(
            "{}.json",
            agit::infra::config::hub_host_key(hub).unwrap()
        )),
        &agit::infra::credentials::HubCredential {
            account_id: None,
            username: "drh".into(),
            email: None,
            hub: Some(hub.into()),
            access_token: "SYNTHETIC".into(),
            refresh_token: "SYNTHETIC".into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_expires_at: "2099-01-01T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = |args: &[&str], input: Option<&str>| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(&work)
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", hub)
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env("CODEX_HOME", &codex_home)
            .env_remove("AGIT_SESSION")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(
            &mut child.stdin.take().unwrap(),
            input.unwrap_or_default().as_bytes(),
        )
        .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };
    let mine = "# Memory Index\n\n- [mine](mine.md) — my own notes\n";
    let old = format!(
        "{mine}\n<!-- agit:memory:begin other/repo@old -->\n- [codename](agit/other/repo/old/codename.md) — OLDOTTER\n<!-- agit:memory:end -->\n\n<!-- agit:memory:begin {REPO}@s0 -->\n- [team](agit/drh/qa/s0/team.md)\n<!-- agit:memory:end -->\n"
    );
    fs::write(mem.join("MEMORY.md"), &old).unwrap();

    let started = run(&["new", REPO, "-b", "told", "--no-launch"], None);
    let copy = mem.join("agit/drh/qa/told/team.md");
    assert!(copy.is_file());
    assert!(
        started.contains("claude --append-system-prompt")
            && started
                .contains("Team memory of drh/qa@told, placed by agit for this conversation.")
            && started.contains(&copy.display().to_string())
            && !started.contains("OLDOTTER")
            && !started.contains("other/repo"),
        "{started}"
    );
    assert_eq!(fs::read_to_string(mem.join("MEMORY.md")).unwrap(), mine);

    // A section an older agit wrote after this version's sweep is taken out by the folder's next
    // session-start hook, for the conversations started there after it.
    fs::write(mem.join("MEMORY.md"), &old).unwrap();
    let event = serde_json::json!({
        "session_id": "11111111-2222-4333-8444-555555555555",
        "cwd": work,
        "hook_event_name": "SessionStart",
        "source": "startup",
    })
    .to_string();
    run(&["hooks", "ingest"], Some(&event));
    assert_eq!(fs::read_to_string(mem.join("MEMORY.md")).unwrap(), mine);

    // `agit resume` of a conversation saved here tells it the same way.
    let sid = "6f0c9a3e-1b2d-4c5e-8f70-a1b2c3d4e5f6";
    let cwd = work.to_string_lossy();
    let lines = format!(
        "{}\n{}\n",
        serde_json::json!({
            "type": "user", "sessionId": sid, "cwd": cwd, "uuid": format!("{sid}-u1"),
            "timestamp": "2026-08-29T00:00:01.000Z",
            "message": {"role": "user", "content": "What does the team call user ids?"}
        }),
        serde_json::json!({
            "type": "assistant", "sessionId": sid, "cwd": cwd, "uuid": format!("{sid}-a1"),
            "timestamp": "2026-08-29T00:00:01.500Z",
            "message": {"role": "assistant", "stop_reason": "end_turn",
                "content": [{"type": "text", "text": "uid, not user_id."}]}
        })
    );
    fs::write(mem.parent().unwrap().join(format!("{sid}.jsonl")), lines).unwrap();
    let into = format!("{REPO}@imp");
    run(
        &[
            "import",
            sid,
            "--from",
            "claude-code",
            "--into",
            &into,
            "--independent",
        ],
        None,
    );
    let resumed = run(&["resume", &into, "--no-launch"], None);
    assert!(
        resumed.contains("--append-system-prompt")
            && resumed.contains("Team memory of drh/qa@imp, placed by agit for this conversation.")
            && resumed.contains(&mem.join("agit/drh/qa/imp/team.md").display().to_string()),
        "{resumed}"
    );
    assert_eq!(fs::read_to_string(mem.join("MEMORY.md")).unwrap(), mine);

    let codex = run(
        &["new", REPO, "-b", "cx", "--as", "codex", "--no-launch"],
        None,
    );
    assert!(
        codex.contains("Codex is told of its team memory by agit's hooks, which aren't set up here, so this conversation starts without it")
            && codex.contains("`agit setup --hooks` sets them up"),
        "{codex}"
    );
}

/// The first agit process of a version, here a Claude Code session-start hook, takes agit's
/// sections out of every Claude Code memory index under the configuration folder, the person's
/// lines kept, not only its own folder's. It records the version only after handling every index:
/// one too large to read leaves it unrecorded, and the next process that prepares local state
/// sweeps again. Once the version is recorded, its later processes leave the indexes alone. A sweep that cleaned only the hook's
/// folder, recorded the version past an index it skipped, or ran on every process fails here.
#[test]
fn a_versions_first_agit_process_sweeps_every_memory_index_once() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, _repo, claude, mem) = fixture(tmp.path());
    let work = tmp.path().join("work").canonicalize().unwrap();
    let mine = "# Memory Index\n\n- [notes](notes.md) — personal notes\n";
    let old = format!(
        "{mine}\n<!-- agit:memory:begin other/repo@old -->\n- [codename](agit/other/repo/old/codename.md) — OLDOTTER\n<!-- agit:memory:end -->\n"
    );
    let elsewhere = claude.join("projects").join("-elsewhere").join("memory");
    let large = claude.join("projects").join("-large").join("memory");
    let oversized = format!("{}\n{old}", "x".repeat(1 << 20));
    for (dir, text) in [(&mem, &old), (&elsewhere, &old), (&large, &oversized)] {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("MEMORY.md"), text).unwrap();
    }
    let run = |args: &[&str], input: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_agit"))
            .args(args)
            .current_dir(&work)
            .env("AGIT_HOME", &home)
            .env("AGIT_HUB_URL", "http://127.0.0.1:1")
            .env("CLAUDE_CONFIG_DIR", &claude)
            .env_remove("AGIT_SESSION")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(&mut child.stdin.take().unwrap(), input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    let event = serde_json::json!({
        "session_id": "11111111-2222-4333-8444-555555555555",
        "cwd": work,
        "hook_event_name": "SessionStart",
        "source": "startup",
    })
    .to_string();
    let read = |dir: &Path| fs::read_to_string(dir.join("MEMORY.md")).unwrap();
    let swept = home.join("memory-swept");

    run(&["hooks", "ingest"], &event);
    assert_eq!(read(&mem), mine);
    assert_eq!(read(&elsewhere), mine);
    assert_eq!(read(&large), oversized);
    assert!(!swept.exists());

    fs::write(large.join("MEMORY.md"), mine).unwrap();
    fs::write(elsewhere.join("MEMORY.md"), &old).unwrap();
    run(&["config", "--list"], "");
    assert_eq!(read(&elsewhere), mine);
    let version = ok(&run(&["--version"], ""));
    assert_eq!(
        fs::read_to_string(&swept).unwrap(),
        version.trim().strip_prefix("agit ").unwrap()
    );

    fs::write(elsewhere.join("MEMORY.md"), &old).unwrap();
    run(&["config", "--list"], "");
    assert_eq!(read(&elsewhere), old);
}

/// A PEM private key in a runtime memory file reaches the branch only as one placeholder that
/// covers the block from BEGIN through END, and a runtime directory that lacks the file gets the
/// key back from that placeholder; a block without its END line keeps its file off the branch
/// with a "suspected secret" warning. A projection that works line by line replaces the BEGIN
/// line and publishes the key body, and a restore that works line by line cannot expand one
/// placeholder into several lines; one that guesses where an unterminated block ends publishes
/// part of the key or swallows the rest of the file.
#[test]
fn a_private_key_in_memory_is_replaced_as_one_block_or_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo, claude, mem) = fixture(tmp.path());
    let into = format!("{REPO}@s1");
    let body = [
        "MIIEowIBAAKCAQEAsYnThEtIcKeYbOdYlInEoNe0123456789abcdefghijklmnop",
        "qrstuvwxyzSYNTHETICkeyBODYlineTWO9876543210ABCDEFGHIJKLMNOPQRSTUV",
        "WXYZsyntheticKEYbodyLINEthree0246813579zyxwvutsrqponmlkjihgfedcb==",
    ];
    let key = format!(
        "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----",
        body.join("\n")
    );
    let closed = format!("---\nname: mine\n---\nstaging key:\n{key}\nafter the key\n");
    fs::write(mem.join("mine.md"), &closed).unwrap();
    let open = format!(
        "---\nname: open\n---\n-----BEGIN OPENSSH PRIVATE KEY-----\n{}\n",
        body.join("\n")
    );
    fs::write(mem.join("open.md"), &open).unwrap();

    let synced = agit(&home, &claude, &["memory", "sync", "--into", &into]);
    ok(&synced);
    let saved = repo.show("refs/heads/s1", "memory/mine.md").unwrap();
    for line in body {
        assert!(!saved.contains(line), "{saved}");
    }
    assert!(!saved.contains("-----"), "{saved}");
    let lines: Vec<_> = saved.lines().collect();
    let at = lines
        .iter()
        .position(|line| *line == "staging key:")
        .unwrap();
    assert_eq!(lines.get(at + 2), Some(&"after the key"), "{saved}");
    assert!(repo.show("refs/heads/s1", "memory/open.md").is_none());
    let warnings = String::from_utf8_lossy(&synced.stderr);
    assert!(
        warnings.contains("memory/open.md") && warnings.contains("suspected secret"),
        "{warnings}"
    );
    assert_eq!(fs::read_to_string(mem.join("mine.md")).unwrap(), closed);
    assert_eq!(fs::read_to_string(mem.join("open.md")).unwrap(), open);

    let fresh = tmp.path().join("claude-fresh");
    let project = agit::infra::runtime_memory::encode_project(
        &tmp.path().join("work").canonicalize().unwrap(),
    );
    let fresh_mem = fresh.join("projects").join(project).join("memory");
    fs::create_dir_all(&fresh_mem).unwrap();
    ok(&agit(&home, &fresh, &["memory", "sync", "--into", &into]));
    assert_eq!(
        fs::read_to_string(fresh_mem.join("agit/drh/qa/s1/mine.md")).unwrap(),
        closed
    );
}

/// Memory reaches a branch only as UTF-8 text that passed protection in full. A file larger than
/// one protection unit is protected window by window and lands with its credentials replaced: one
/// whose command a window edge separates from its user flag by more than a fixed context, and a
/// run with too many findings for one scan, after which the rest of the file still lands. A file
/// that is not UTF-8 stays local whatever it holds; a file withheld while the worker cannot run is
/// collected, unchanged, by the next sync that can protect it; and a session branch file that
/// holds the body and END line of a key whose BEGIN line was replaced on its own does not distill
/// into main. An implementation that published what the worker did not process or could not read,
/// carried a fixed context into the next window, kept growing a window that had too many findings,
/// held a large file back for good, recorded a withheld file as seen, ended an omitted part inside
/// a private key it opened, or looked only for BEGIN lines would put a credential on a branch, never collect the file, drop it, or carry the key
/// body into main.
#[test]
fn memory_that_did_not_pass_protection_stays_off_every_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo, claude, mem) = fixture(tmp.path());
    let into = format!("{REPO}@s1");
    const MIB: usize = 1024 * 1024;
    let filler = "a filler line that carries this file past one protection unit\n";
    let mut big = String::from("---\nname: big\n---\naws AKIA4X7QZ2M5RT6VW3JH\n");
    while big.len() < 3 * MIB + MIB / 2 {
        big.push_str(filler);
    }
    // The first window's cut lands on the line of spaces, after the command and before its flag.
    big.push_str("curl https://deploy.example.invalid/upload");
    big.push_str(&" ".repeat(24 * 1024));
    big.push('\n');
    big.push_str(&" ".repeat(MIB));
    big.push_str("-u 'deployuser:weakpass77'\n");
    while big.len() < 6 * MIB {
        big.push_str(filler);
    }
    // A private key opens just before a run too crowded for one scan and ends long after it, so
    // the part omitted for the crowding ends inside the key's body.
    big.push_str("-----BEGIN RSA PRIVATE KEY-----\n");
    for _ in 0..2100 {
        big.push_str("aws AKIA4X7QZ2M5RT6VW3JH\n");
    }
    let key_line = "Zq9SyntheticKeyBodyLine0123456789abcdefghijklmnopqrstuvwxyzABCDEFG\n";
    let key_start = big.len();
    while big.len() < key_start + MIB / 2 {
        big.push_str(key_line);
    }
    big.push_str("-----END RSA PRIVATE KEY-----\n");
    while big.len() <= 8 * MIB {
        big.push_str(filler);
    }
    big.push_str("the closing fact\n");
    fs::write(mem.join("big.md"), &big).unwrap();
    fs::write(
        mem.join("latin.md"),
        b"---\nname: latin\n---\ncaf\xe9 aws AKIA4X7QZ2M5RT6VW3JH\n",
    )
    .unwrap();
    let synced = agit(&home, &claude, &["memory", "sync", "--into", &into]);
    ok(&synced);
    let warnings = String::from_utf8_lossy(&synced.stderr);
    let stored = repo
        .show("refs/heads/s1", "memory/big.md")
        .expect("a large memory file is collected");
    assert!(!stored.contains("AKIA4X7QZ2M5RT6VW3JH"));
    assert!(!stored.contains("weakpass77"));
    assert!(
        !stored.contains("Zq9SyntheticKeyBodyLine"),
        "the key body behind the omitted part landed"
    );
    assert!(
        stored.trim_end().ends_with("the closing fact"),
        "{}",
        &stored[stored.len().saturating_sub(400)..]
    );
    // Only the crowded part is omitted, with at most the window after it, whose context the crowded
    // findings still fill; the rest of the file lands.
    let lost = big.matches(filler).count() - stored.matches(filler).count();
    assert!(
        lost <= 2 * 64 * 1024 / filler.len() + 1,
        "{lost} lines lost"
    );
    assert!(repo.show("refs/heads/s1", "memory/latin.md").is_none());
    assert!(
        warnings.contains("memory/latin.md: not UTF-8 text"),
        "{warnings}"
    );

    // A file withheld while the worker cannot run is collected, unchanged, once it can again. A
    // plain file where the privacy directory belongs keeps the worker from protecting anything.
    let privacy = home.join("privacy");
    let aside = home.join("privacy.aside");
    let existed = privacy.exists();
    if existed {
        fs::rename(&privacy, &aside).unwrap();
    }
    fs::write(&privacy, "not a directory").unwrap();
    fs::write(
        mem.join("later.md"),
        "---\nname: later\n---\na later fact\n",
    )
    .unwrap();
    let blocked = agit(&home, &claude, &["memory", "sync", "--into", &into]);
    fs::remove_file(&privacy).unwrap();
    if existed {
        fs::rename(&aside, &privacy).unwrap();
    }
    ok(&blocked);
    assert!(
        String::from_utf8_lossy(&blocked.stderr).contains("memory/later.md: privacy check pending"),
        "{blocked:?}"
    );
    assert!(repo.show("refs/heads/s1", "memory/later.md").is_none());
    ok(&agit(&home, &claude, &["memory", "sync", "--into", &into]));
    assert!(repo.show("refs/heads/s1", "memory/later.md").is_some());

    let session = Repo::open(home.join("worktrees").join(REPO).join("s1")).unwrap();
    fs::write(
        session.root().join("memory/leaked.md"),
        "staging key:\n[replaced header]\nMIIEowIBAAKCAQEAsYnThEtIcKeYbOdYlInEoNe0123456789abcdefghij\n-----END RSA PRIVATE KEY-----\n",
    )
    .unwrap();
    session.add_all().unwrap();
    session.commit("agit: memory sync (1 changes)").unwrap();
    let distilled = agit(&home, &claude, &["distill", "-y", "--into", &into]);
    assert!(!distilled.status.success(), "{distilled:?}");
    let refusal = String::from_utf8_lossy(&distilled.stderr);
    assert!(
        refusal.contains("memory/leaked.md") && refusal.contains("suspected secret"),
        "{refusal}"
    );
    assert!(repo.show("refs/heads/main", "memory/leaked.md").is_none());
}

/// A session line `name` forked from main's tip, as `agit new` leaves one.
fn fork_session(repo: &Repo, name: &str, id: char) {
    let mut snap = Meta::new_session_line("claude-code".into(), "/work".into());
    snap.session = format!(
        "{}{}",
        meta::ID_PREFIX,
        id.to_string().repeat(meta::ID_HEX_LEN)
    );
    meta::write(repo.root(), &snap).unwrap();
    repo.git(&["switch", "-q", "-c", name]).unwrap();
    repo.add_all().unwrap();
    repo.commit(&format!("agit: new session {name}")).unwrap();
    repo.switch("main").unwrap();
    meta::write(repo.root(), &Meta::new_file_line()).unwrap();
}

/// `text` settled as `memory/team.md` on session line `branch`.
fn settle_team(repo: &Repo, branch: &str, text: &str) {
    let refname = format!("refs/heads/{branch}");
    let tree = agit::commands::plumbing::tree_apply_owned(
        repo,
        &refname,
        vec![("memory/team.md".into(), Some(text.as_bytes().to_vec()))],
    )
    .unwrap();
    let head = repo.git(&["rev-parse", &refname]).unwrap();
    let c = agit::commands::plumbing::commit_tree(repo, &tree, &[head.trim()], "settle").unwrap();
    repo.git(&["update-ref", &refname, &c]).unwrap();
}

/// Two sessions forked from one main distill the same memory file. Changes to different lines
/// both reach main, and a distill of a file the session left alone carries nothing. Where the
/// second session changed a line the first one also changed, its distill stops with nothing
/// written, shows the lines and names who changed them on main, with the terminal controls and
/// bidi overrides the other person put in their line and name made inert; after it settles a
/// third text,
/// `--take-mine` lands that text, its own later edit of a line it distilled before, and the first
/// session's other lines. A distill that compares with main alone undoes a colleague's change; one
/// whose fork point never moves names the session's own earlier distill as the clash; one that
/// stops on every rerun leaves no way to land the third text; an overwrite of the whole file drops
/// the first session's other line; one that prints the other person's bytes raw lets them
/// rewrite this person's terminal.
#[test]
fn two_sessions_distilling_one_file_keep_both_changes_or_stop() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo, claude, _mem) = fixture(tmp.path());
    let lines = |description: &str, ids: &str, naming: &str| {
        format!(
            "---\nname: team\ndescription: {description}\n---\nids: {ids}\ntime: UTC everywhere\nlogs: JSON lines\nnaming: {naming}\n"
        )
    };
    let main_text = || {
        String::from_utf8(
            repo.git_bytes(&["show", "refs/heads/main:memory/team.md"])
                .unwrap(),
        )
        .unwrap()
    };
    let main_tip = || repo.git(&["rev-parse", "refs/heads/main"]).unwrap();
    fs::write(
        repo.root().join("memory/team.md"),
        lines("shared", "uid, not user_id", "snake_case"),
    )
    .unwrap();
    repo.add_all().unwrap();
    repo.commit("team.md grows").unwrap();
    fork_session(&repo, "pat", 'a');
    fork_session(&repo, "quinn", 'b');

    // Set by the other person: a bidi override and an escape sequence inside their name and line.
    let pat_ids = "uid, never user_id\u{1b}]52;c;cGF0\u{7}\u{202e}";
    repo.git(&["config", "user.name", "Pat\u{202e}\u{1b}[2J Lee"])
        .unwrap();
    repo.git(&["config", "user.email", "pat@example.com"])
        .unwrap();
    settle_team(
        &repo,
        "pat",
        &lines("shared", pat_ids, "snake_case, no abbreviations"),
    );
    ok(&agit(
        &home,
        &claude,
        &["distill", "-y", "--into", "drh/qa@pat"],
    ));
    repo.git(&["config", "user.name", "Quinn"]).unwrap();
    repo.git(&["config", "user.email", "quinn@example.com"])
        .unwrap();
    settle_team(
        &repo,
        "quinn",
        &lines("shared by the whole team", "uid, not user_id", "snake_case"),
    );
    ok(&agit(
        &home,
        &claude,
        &["distill", "-y", "--into", "drh/qa@quinn"],
    ));
    assert_eq!(
        main_text(),
        lines(
            "shared by the whole team",
            pat_ids,
            "snake_case, no abbreviations"
        )
    );
    let merged = main_tip();
    ok(&agit(
        &home,
        &claude,
        &["distill", "-y", "--into", "drh/qa@pat"],
    ));
    assert_eq!(main_tip(), merged, "pat left team.md alone since");

    settle_team(
        &repo,
        "quinn",
        &lines("shared by everyone", "account_id", "snake_case"),
    );
    let stopped = agit(&home, &claude, &["distill", "-y", "--into", "drh/qa@quinn"]);
    assert_eq!(stopped.status.code(), Some(4), "{stopped:?}");
    let report = String::from_utf8_lossy(&stopped.stderr);
    for expected in [
        "memory/team.md: main and `quinn` changed the same lines; nothing was distilled",
        "changed on main by Pat  [2J Lee (",
        "<<<<<<< main",
        "ids: uid, never user_id ]52;c;cGF0",
        "||||||| fork point",
        "ids: uid, not user_id",
        "ids: account_id",
        "`agit distill --take-mine=team.md`",
    ] {
        assert!(report.contains(expected), "{expected}: {report}");
    }
    assert!(!report.contains("Quinn"), "{report}");
    assert!(
        !report.contains(['\u{1b}', '\u{7}', '\u{202e}']),
        "{report:?}"
    );
    assert_eq!(main_tip(), merged);

    settle_team(
        &repo,
        "quinn",
        &lines("shared by everyone", "uid as account_id", "snake_case"),
    );
    let taken = agit(
        &home,
        &claude,
        &[
            "distill",
            "-y",
            "--take-mine",
            "team.md",
            "--into",
            "drh/qa@quinn",
        ],
    );
    ok(&taken);
    let notice = String::from_utf8_lossy(&taken.stderr);
    assert!(
        notice.contains("your lines replace the change Pat  [2J Lee ("),
        "{notice}"
    );
    assert!(
        !notice.contains(['\u{1b}', '\u{7}', '\u{202e}']),
        "{notice:?}"
    );
    assert_eq!(
        main_text(),
        lines(
            "shared by everyone",
            "uid as account_id",
            "snake_case, no abbreviations"
        )
    );
}

/// A file named for distill that has nothing to carry says which way: one the branch never got,
/// with the allowlist sentence and what allowing does when the policy keeps it on this computer,
/// or one unchanged against main. "already matches main, or never existed" left a person whose
/// memory was written to another folder, and whose allowlist was empty, guessing which.
#[test]
fn distill_says_why_a_file_has_nothing_to_carry() {
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo, claude, _mem) = fixture(tmp.path());
    let into = format!("{REPO}@s1");
    let refused = |file: &str| {
        let out = agit(&home, &claude, &["distill", file, "--into", &into]);
        assert_eq!(out.status.code(), Some(3), "{out:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };

    let unchanged = refused("team.md");
    assert!(
        unchanged.contains(
            "memory/team.md on `s1` already matches main, or hasn't changed since `s1` left main: there is nothing to carry."
        ),
        "{unchanged}"
    );

    let absent = refused("decisions.md");
    assert!(
        absent.contains(
            "memory/decisions.md isn't on `s1`: no turn collected it from the runtime's memory folder."
        ),
        "{absent}"
    );
    assert!(
        absent.contains("agit memory status --into")
            && absent.contains("drh/qa@s1")
            && absent.contains("shows where it is"),
        "{absent}"
    );
    assert!(!absent.contains("allow-memory"), "{absent}");

    agit::domain::privacy::PrivacyPolicy::default()
        .save(&repo)
        .unwrap();
    let kept = refused("decisions.md");
    assert!(
        kept.contains(
            "memory/decisions.md isn't on `s1`: no turn collected it from the runtime's memory folder."
        ),
        "{kept}"
    );
    assert!(
        kept.contains(
            "your memory allowlist for drh/qa on this computer is empty: memory files stay on this computer until you allow them. `agit privacy policy allow-memory --repo drh/qa -- decisions.md` would let agit collect decisions.md; it is then saved to drh/qa and uploaded with the branch."
        ),
        "{kept}"
    );
    assert!(
        repo.show("refs/heads/main", "memory/decisions.md")
            .is_none()
    );
}

/// Memory a session recorded reaches main through distill with a credential in it replaced, even
/// a value a session branch name or a tag name keeps as it was typed: collecting memory is the
/// step that still protects it.
#[test]
fn memory_distilled_into_main_has_its_credentials_replaced() {
    const TOKEN: &str = "ghp_R7kQ2mXv9LpZ4tNc8WjF3bHy6sVd1aGe5uKr";
    let tmp = tempfile::tempdir().unwrap();
    let (home, repo, claude, _mem) = fixture(tmp.path());
    settle_team(
        &repo,
        "s1",
        &format!(
            "---\nname: team\ndescription: shared\n---\nuid, not user_id\ndeploy with {TOKEN}\n"
        ),
    );
    ok(&agit(
        &home,
        &claude,
        &["distill", "-y", "--into", &format!("{REPO}@s1")],
    ));
    let distilled = repo
        .show("refs/heads/main", "memory/team.md")
        .expect("the session's memory reaches main");
    assert!(distilled.contains("deploy with "), "{distilled}");
    assert!(!distilled.contains(TOKEN), "{distilled}");
}
