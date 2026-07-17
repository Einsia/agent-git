//! v3 end-to-end: dual-repo model + scope routing + WorkspaceRevision pairing + secret defense for session dumps.
//! The fact/evidence approach has been deprecated and removed.

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_agit");

struct Repo {
    dir: tempfile::TempDir,
}

impl Repo {
    fn new() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let r = Repo { dir };
        r.sh("git init -q -b main .");
        r.sh("git config user.name dev && git config user.email d@x.com");
        r.sh("git config commit.gpgsign false");
        r.write("app.ts", "export const x = 1;\n");
        r.sh("git add -A && git commit -qm seed");
        // `--agent` is required non-interactively: an agent is named for what it knows, so agit will
        // not invent a label from the directory — which here would be the tempdir's `.tmpXXXXXX`.
        assert_eq!(r.agit(&["init", "--agent", "testmemory"]).0, 0, "init should succeed");
        r
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }
    /// The resolved agent's store — **not** a path a test may hardcode. A store is keyed by the
    /// agent's identity at `$AGIT_HOME/agents/<aid>/`, so the only way to find it is to ask agit
    /// which agent this repo resolves to, exactly as a user would. `agit a <git…>` is plain git on
    /// that store, so git itself answers.
    fn agent(&self) -> PathBuf {
        let (code, out, err) = self.agit(&["a", "rev-parse", "--show-toplevel"]);
        assert_eq!(code, 0, "could not resolve this repo's agent store: {err}");
        PathBuf::from(out.trim())
    }
    /// Write into the resolved store, the way `snap` would.
    fn write_agent(&self, rel: &str, content: &str) {
        let p = self.agent().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    /// Find a captured session by runtime + filename, under EITHER store layout — `sessions/<rt>/` or
    /// `sessions/<env-slug>/<rt>/`.
    ///
    /// The per-environment partition exists because one agent works in many repos and its memory of
    /// each must stay tellable apart; it is landing separately from this cutover. What these tests
    /// assert is that the session reached the right AGENT, which is the same claim under either
    /// layout — so they should not fail, or pass, on the strength of where inside the store it sits.
    fn captured(&self, rt: &str, file: &str) -> bool {
        fn walk(dir: &Path, want: &Path) -> bool {
            std::fs::read_dir(dir).into_iter().flatten().flatten().any(|e| {
                let p = e.path();
                p.ends_with(want) || (p.is_dir() && walk(&p, want))
            })
        }
        walk(&self.agent().join("sessions"), &PathBuf::from(rt).join(file))
    }
    /// Every session byte in the store, wherever it sits.
    fn all_session_text(&self) -> String {
        fn walk(dir: &Path, out: &mut String) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                match p.is_dir() {
                    true => walk(&p, out),
                    false => out.push_str(&std::fs::read_to_string(&p).unwrap_or_default()),
                }
            }
        }
        let mut s = String::new();
        walk(&self.agent().join("sessions"), &mut s);
        s
    }

    /// Every process this suite spawns goes through here. agit resolves ~/.claude, ~/.codex and its own
    /// home from the environment, so an un-isolated test reads — and can write — the developer's real
    /// session stores. Per-invocation env only: `std::env::set_var` is process-global and would race
    /// across parallel tests.
    fn cmd(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.current_dir(self.path())
            .env("HOME", self.path())
            .env("AGIT_HOME", self.path().join("agit-home"));
        c
    }
    fn sh(&self, cmd: &str) -> String {
        let o = self.cmd("sh").arg("-c").arg(cmd).output().unwrap();
        String::from_utf8_lossy(&o.stdout).to_string()
    }
    fn write(&self, rel: &str, content: &str) {
        let p = self.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }
    fn agit(&self, args: &[&str]) -> (i32, String, String) {
        self.agit_env(&[], args)
    }
    /// `envs` is applied last, so a test that needs a specific HOME (a fake runtime dump) overrides the
    /// isolated default rather than fighting it.
    fn agit_env(&self, envs: &[(&str, &str)], args: &[&str]) -> (i32, String, String) {
        let mut c = self.cmd(BIN);
        c.args(args);
        for (k, v) in envs {
            c.env(k, v);
        }
        let o = c.output().unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).to_string(),
            String::from_utf8_lossy(&o.stderr).to_string(),
        )
    }
    fn git_env(&self, args: &[&str]) -> String {
        self.git_at(self.path(), args)
    }
    fn git_agent(&self, args: &[&str]) -> String {
        self.git_at(&self.agent(), args)
    }
    fn git_at(&self, root: &Path, args: &[&str]) -> String {
        let mut c = self.cmd("git");
        c.arg("-C").arg(root).args(args);
        String::from_utf8_lossy(&c.output().unwrap().stdout).trim().to_string()
    }
}

// ─────────────────────────── init / storage model ───────────────────────────

#[test]
fn init_binds_an_identified_store_outside_the_code_repo() {
    let r = Repo::new();
    assert!(r.agent().join(".git").exists(), "the store should be a standalone git repo");
    assert!(r.agent().join("sessions").exists(), "should have the sessions/ skeleton");
    // no more fact machinery
    assert!(!r.agent().join("state/facts").exists(), "state/facts should have been removed");
    assert!(r.git_env(&["config", "--get", "merge.agit.driver"]).is_empty(), "the fact merge driver is no longer registered");
    // .agit/ holds this environment's local state (workspace log, watcher pid/log) and stays out of
    // the code history. Asked about a path *inside* it: the `.agit/` pattern carries a trailing slash,
    // so it only matches a directory that exists, and init no longer creates one — the store moved out.
    assert!(r.sh("git check-ignore .agit/workspace/log.jsonl; echo $?").contains('0'));
    // secret hooks are installed
    assert!(r.agent().join(".git/hooks/pre-commit").exists());
    assert!(r.agent().join(".git/hooks/pre-push").exists());

    // The cutover, and the whole point of it: the store is keyed by the agent's IDENTITY, under
    // $AGIT_HOME — NOT nested in the code repo. A nested store is welded to one environment, which is
    // exactly what stopped an agent carrying its memory into another repo.
    let store = r.agent();
    assert!(
        store.starts_with(r.path().join("agit-home").join("agents")),
        "the store must live at $AGIT_HOME/agents/<aid>/, got {}",
        store.display()
    );
    assert!(
        store.file_name().unwrap().to_str().unwrap().starts_with("agt_"),
        "the store directory must BE the aid, so rename/publish never move it: {}",
        store.display()
    );
    assert!(!r.path().join(".agit/agent").exists(), "the legacy nested store must not be created");
    assert!(!r.path().join(".agit/store").exists(), "the .agit/store pointer file is gone");

    // The binding is COMMITTED — this is what makes a teammate's clone work.
    let binding = std::fs::read_to_string(r.path().join(".agit.toml")).expect(".agit.toml should be written");
    assert!(binding.contains("id     = \"agt_"), "the binding must record the aid: {binding}");
    assert!(
        r.sh("git check-ignore .agit.toml; echo $?").contains('1'),
        "the binding must NOT be gitignored — an uncommitted binding tells a teammate nothing"
    );
}

/// An agent is a memory, "named for what it knows" — not for the directory it happened to be
/// initialised in, which it outlives and which everyone's `~/code/api` shares. So a non-interactive
/// `init` with no `--agent` must refuse rather than invent a label, exactly as resolution refuses to
/// guess which memory you meant.
#[test]
fn init_will_not_name_an_agent_after_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let r = Repo { dir };
    r.sh("git init -q -b main .");
    r.sh("git config user.name dev && git config user.email d@x.com && git config commit.gpgsign false");
    r.write("app.ts", "x\n");
    r.sh("git add -A && git commit -qm seed");

    let (code, out, err) = r.agit(&["init"]);
    let said = format!("{out}{err}");
    assert_ne!(code, 0, "init must not mint an agent nobody named: {said}");
    assert!(said.contains("will not name one for you"), "{said}");
    assert!(said.contains("agit init --agent <name>"), "must name the fix: {said}");
    assert!(said.contains("agit a clone"), "and the other real answer: {said}");
    // Nothing may be left behind by the refusal.
    let dirname = r.path().file_name().unwrap().to_str().unwrap().to_string();
    let (_, listed, _) = r.agit(&["a", "list"]);
    assert!(!listed.contains(&dirname), "no agent may be named after the directory: {listed}");
    assert!(!r.path().join(".agit.toml").exists(), "and nothing may be bound");

    // Named explicitly, it works — the name was a decision, not a guess.
    assert_eq!(r.agit(&["init", "--agent", "payments-api"]).0, 0);
    let (_, listed, _) = r.agit(&["a", "list"]);
    assert!(listed.contains("payments-api"), "{listed}");
}

#[test]
fn init_is_idempotent() {
    let r = Repo::new();
    let head1 = r.git_agent(&["rev-parse", "HEAD"]);
    assert_eq!(r.agit(&["init"]).0, 0);
    assert_eq!(r.git_agent(&["rev-parse", "HEAD"]), head1, "re-running init adds no new commit");
}

/// Isolation lock. This suite drives the real binary, which resolves ~/.claude, ~/.codex and its own
/// home from the environment — un-isolated, `cargo test` reads (and could write) the developer's real
/// session stores. agit's own "where did I look?" error is the proof of which HOME it used.
#[test]
fn spawned_agit_resolves_the_isolated_home_not_the_developers() {
    let r = Repo::new();
    let (_, out, err) = r.agit(&["-a", "snap", "--from", "claude-code"]);
    let isolated = format!("{}/.claude/projects", r.path().display());
    // snap names the directory it looked under (the source line), whether or not it exists — that path
    // is the proof of which HOME agit resolved.
    let seen = format!("{out}{err}");
    assert!(seen.contains(&isolated), "snap should look under the isolated HOME ({isolated}), got:\n{seen}");
}

// ─────────────────────── scope routing (key ambiguity) ───────────────────────

#[test]
fn default_scope_is_transparent_git_on_code_repo() {
    let r = Repo::new();
    let (code, out, _) = r.agit(&["status", "--short"]);
    assert_eq!(code, 0);
    assert!(!out.contains(".agit/"), "agit status should not expose .agit/ local state:\n{out}");
    // `.agit.toml` is the exception, and deliberately so: it is the COMMITTED binding, so it must
    // show up as untracked until you commit it. A binding nobody commits tells a teammate nothing.
    assert!(out.contains(".agit.toml"), "the binding must be visible to commit:\n{out}");
}

#[test]
fn agit_dash_a_targets_agent_store() {
    let r = Repo::new();
    r.write_agent("notes.md", "hi\n");
    assert_eq!(r.agit(&["-a", "add", "-A"]).0, 0);
    assert_eq!(r.agit(&["-a", "commit", "-m", "agent scope"]).0, 0);
    assert_eq!(r.git_agent(&["log", "-1", "--format=%s"]), "agent scope");
    assert_eq!(r.git_env(&["log", "-1", "--format=%s"]), "seed", "the code repo should not gain an extra commit");
}

/// Ambiguity called out by the PRD: the -a in `agit commit -a` is a git flag, not a scope switch.
#[test]
fn commit_dash_a_is_git_flag_not_scope() {
    let r = Repo::new();
    let agent_before = r.git_agent(&["rev-list", "--count", "HEAD"]);
    r.write("app.ts", "export const x = 2;\n");
    let (code, _, err) = r.agit(&["commit", "-a", "-m", "code via -a"]);
    assert_eq!(code, 0, "commit -a should act on the code repo: {err}");
    assert_eq!(r.git_env(&["log", "-1", "--format=%s"]), "code via -a");
    assert_eq!(r.git_agent(&["rev-list", "--count", "HEAD"]), agent_before, "should not touch the Agent Store");
}

/// Acceptance §13.1, PRD #1 takeover: bob does `git clone <code repo>` then `agit init`, and the memory
/// alice published is here — cloned by init, not by a manual `track`, with alice's exact aid. Two
/// isolated HOMEs share a bare repo standing in for the hub; `publish`/`track` clone over a local path.
#[test]
fn a_teammate_takes_over_with_git_clone_then_agit_init() {
    let root = tempfile::tempdir().unwrap();
    let hub = root.path().join("frontend.git"); // the "hub": a bare repo publish pushes to
    // A PLAIN bare repo, HEAD defaulting to `master` while agit's store is on `main` — the exact
    // dangling-HEAD case a self-hosted `git init --bare` hub creates. clone_in must recover by checking
    // out the branch the remote actually has; an earlier version of this test used --initial-branch=main
    // and so tested AROUND the bug the takeover has to survive.
    let st = Command::new("git")
        .args(["-c", "init.defaultBranch=master", "init", "-q", "--bare"])
        .arg(&hub).status().unwrap();
    assert!(st.success());

    let run = |home: &Path, cwd: &Path, args: &[&str]| -> (i32, String, String) {
        let o = Command::new(BIN)
            .current_dir(cwd)
            .env("HOME", home)
            .env("AGIT_HOME", home.join(".agit"))
            .args(args)
            .output()
            .unwrap();
        (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into(), String::from_utf8_lossy(&o.stderr).into())
    };
    let git = |cwd: &Path, args: &[&str]| {
        Command::new("git").current_dir(cwd)
            .args(["-c", "user.name=t", "-c", "user.email=t@e.com", "-c", "commit.gpgsign=false"])
            .args(args).output().unwrap();
    };

    // ── alice: mint, point the store at the hub and push (records the remote into .agit.toml), commit ──
    let alice = root.path().join("alice");
    let web = alice.join("web");
    std::fs::create_dir_all(&web).unwrap();
    git(&web, &["init", "-q", "-b", "main", "."]);
    std::fs::write(web.join("f"), "x").unwrap();
    git(&web, &["add", "-A"]);
    git(&web, &["commit", "-qm", "seed"]);
    assert_eq!(run(&alice, &web, &["init", "--agent", "frontend"]).0, 0);
    assert_eq!(run(&alice, &web, &["a", "remote", "add", "origin", hub.to_str().unwrap()]).0, 0, "remote add");
    let (c, _, e) = run(&alice, &web, &["a", "push", "-u", "origin", "HEAD"]);
    assert_eq!(c, 0, "push failed: {e}");
    git(&web, &["add", ".agit.toml"]);
    git(&web, &["commit", "-qm", "declare frontend"]);
    let alice_aid = std::fs::read_to_string(web.join(".agit.toml")).unwrap()
        .lines().find(|l| l.trim_start().starts_with("id")).unwrap().to_string();

    // ── bob: a clean machine. git clone the CODE repo, then a single `agit init`. ──
    let bob = root.path().join("bob");
    std::fs::create_dir_all(&bob).unwrap();
    let code = bob.join("code");
    Command::new("git").args(["clone", "-q"]).arg(&web).arg(&code).output().unwrap();

    let (ic, iout, ierr) = run(&bob, &code, &["init"]);
    assert_eq!(ic, 0, "bob's init should clone the declared agent, not error: {ierr}");
    assert!(iout.contains("cloned frontend"), "init should say it cloned the agent: {iout}{ierr}");

    // same agent, not a namesake: bob's frontend carries alice's aid.
    let (_, binfo, _) = run(&bob, &code, &["a", "info", "frontend"]);
    let bob_aid = binfo.lines().find(|l| l.starts_with("aid")).unwrap();
    let alice_aid_val = alice_aid.split('"').nth(1).unwrap();
    assert!(bob_aid.contains(alice_aid_val), "bob got a namesake, not alice's agent: {bob_aid} vs {alice_aid_val}");
}

/// `agit clone <url>` is git's clone, on the code repo. It used to mean "clone the team's Agent Store
/// into `<env>/.agit/agent`" — shadowing git's own verb, and after the cutover building a store at a
/// path nothing resolves. The transparent wrapper must also be no PICKIER than git: clone is run from
/// outside a repo by definition, so requiring one would reject the command whose job is to make one.
#[test]
fn clone_is_gits_clone_and_works_outside_a_repo() {
    let src = Repo::new();
    src.write("f.txt", "hello\n");
    src.sh("git add -A && git commit -qm seed2");

    let out = tempfile::tempdir().unwrap();
    let mut c = Command::new(BIN);
    c.current_dir(out.path())
        .env("HOME", out.path())
        .env("AGIT_HOME", out.path().join("agit-home"))
        .args(["clone", &src.path().to_string_lossy(), "cloned"]);
    let o = c.output().unwrap();
    assert_eq!(o.status.code(), Some(0), "agit clone should clone the CODE repo: {}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(
        std::fs::read_to_string(out.path().join("cloned/f.txt")).unwrap(),
        "hello\n",
        "it must be git's clone of the code repo"
    );
    assert!(
        !out.path().join("cloned/.agit/agent").exists(),
        "it must not build an Agent Store at a path the resolver cannot reach"
    );
}

// ─────────────────── `agit a` — the subcommand that replaces `-a` ───────────────────

/// The whole point of the subcommand: `agit a <git-verb>` reaches the store exactly as `agit -a` did.
///
/// `log` is now session-aware by default, so this routes through the `--raw` escape hatch — which is
/// the thing being guarded here: that `agit a <git-verb>` still reaches the STORE's git (its history),
/// not the code repo's. `--raw` drops the flag and hands the rest to real git passthrough.
#[test]
fn agit_a_subcommand_runs_git_on_the_agent_store() {
    let r = Repo::new();
    for verb in ["a", "agent"] {
        let (code, out, err) = r.agit(&[verb, "log", "--raw", "-1", "--format=%s"]);
        assert_eq!(code, 0, "`agit {verb} log --raw` should reach the store: {err}");
        assert!(
            out.contains("agit: mint agent"),
            "`agit {verb} log --raw` should show the store's history, not the code repo's: {out}"
        );
    }
}

/// `track` is the management verb precisely so `add` stays git's. `agit a add -A` must stage in the store.
#[test]
fn agit_a_add_is_git_add_not_a_management_verb() {
    let r = Repo::new();
    r.write_agent("notes.md", "hi\n");
    let (code, _, err) = r.agit(&["a", "add", "-A"]);
    assert_eq!(code, 0, "`agit a add -A` must be git-add on the store: {err}");
    assert_eq!(r.agit(&["a", "commit", "-m", "via subcommand"]).0, 0);
    assert_eq!(r.git_agent(&["log", "-1", "--format=%s"]), "via subcommand");
    assert_eq!(r.git_env(&["log", "-1", "--format=%s"]), "seed", "the code repo must not gain a commit");
}

/// `show` is git's; `info` is the management verb. Neither may be confused for the other.
#[test]
fn agit_a_info_is_management_while_show_stays_git() {
    let r = Repo::new();
    let (code, _, err) = r.agit(&["a", "info"]);
    assert_ne!(code, 0, "`agit a info` is a management verb, not git");
    assert!(err.contains("agit agent info"), "should be handled by agit, not handed to git: {err}");
    assert!(!err.contains("not a git command"), "`info` must never reach git: {err}");

    let (code, out, _) = r.agit(&["a", "show", "--stat", "--format=%s"]);
    assert_eq!(code, 0, "`git show` must stay reachable through `agit a show`");
    assert!(out.contains("agit: mint agent"), "`agit a show` should be git-show on the store: {out}");
}

/// Every closed-set verb is recognized, so none of them silently becomes a git invocation.
#[test]
fn management_verbs_are_a_closed_set_and_never_reach_git() {
    let r = Repo::new();
    for verb in ["list", "switch", "info", "rename", "rebind"] {
        let (_, out, err) = r.agit(&["a", verb]);
        let said = format!("{out}{err}");
        // The invariant is that the verb is agit's, not git's. This used to assert the stub's
        // "not implemented yet" text, which made the STUB the spec: implementing `list` broke it
        // even though routing — the thing being guarded — was never touched.
        assert!(
            !said.contains("is not a git command"),
            "`agit a {verb}` fell through to git, which is exactly what the closed set prevents: {said}"
        );
        assert!(
            !said.trim().is_empty(),
            "`agit a {verb}` said nothing at all — it must either act or explain itself"
        );
    }
    // The negative control, without which the assertions above pass for a build that routes NOTHING:
    // a verb outside the closed set MUST still reach git, because `agit a <git-verb>` is the feature.
    let (_, _, err) = r.agit(&["a", "bogusverb"]);
    assert!(
        err.contains("is not a git command"),
        "a verb outside the closed set must reach git, or `agit a log` would stop working: {err}"
    );
}

/// `-a` keeps working while the docs and demo scripts still say it — silently.
#[test]
fn dash_a_remains_a_silent_deprecated_alias() {
    let r = Repo::new();
    // `log --raw` is the passthrough escape hatch (log is session-aware by default); the alias must
    // route it to the STORE's git just like `agit a`.
    let (code, out, err) = r.agit(&["-a", "log", "--raw", "-1", "--format=%s"]);
    assert_eq!(code, 0, "`agit -a log --raw` must keep working: {err}");
    assert!(out.contains("agit: mint agent"), "{out}");
    assert!(!err.to_lowercase().contains("deprecat"), "the alias must print nothing yet: {err}");
}

// ─────────────────────── WorkspaceRevision pairing ───────────────────────

#[test]
fn agent_commit_generates_workspace_revision() {
    let r = Repo::new();
    r.write_agent("notes.md", "x\n");
    r.agit(&["-a", "add", "-A"]);
    r.agit(&["-a", "commit", "-m", "c"]);
    let head = r.path().join(".agit/workspace/HEAD.json");
    assert!(head.exists(), "an agent commit should generate a WorkspaceRevision");
    let json = std::fs::read_to_string(&head).unwrap();
    assert!(json.contains("agent_rev") && json.contains("head_commit") && json.contains("stash_tree"));
    assert!(json.contains(&r.git_agent(&["rev-parse", "HEAD"])));
}

#[test]
fn env_commit_also_pairs() {
    let r = Repo::new();
    r.write("app.ts", "export const x = 3;\n");
    r.agit(&["commit", "-am", "code moved"]);
    let log = r.path().join(".agit/workspace/log.jsonl");
    assert!(log.exists());
    assert!(std::fs::read_to_string(&log).unwrap().contains("env:commit"));
}

#[test]
fn environment_state_captures_dirty_worktree() {
    let r = Repo::new();
    r.write("scratch.txt", "未跟踪\n");
    r.write_agent("notes.md", "x\n");
    r.agit(&["-a", "add", "-A"]);
    r.agit(&["-a", "commit", "-m", "pair while dirty"]);
    let json = std::fs::read_to_string(r.path().join(".agit/workspace/HEAD.json")).unwrap();
    assert!(json.contains("\"dirty\": true"), "{json}");
}

// ─────────────────── secret defense for session dumps ───────────────────

#[test]
fn secret_in_session_blocked_by_precommit() {
    let r = Repo::new();
    // simulate a post-sync session dump that carries a real secret
    r.write_agent(
        "sessions/claude-code/s.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n",
    );
    r.agit(&["-a", "add", "-A"]);
    let (code, _, err) = r.agit(&["-a", "commit", "-m", "leak"]);
    assert_ne!(code, 0, "committing a session that contains a secret should be blocked");
    assert!(err.contains("suspected secrets") || err.contains("aws"), "{err}");
}

#[test]
fn scan_covers_sessions_but_ignores_uuid_noise() {
    let r = Repo::new();
    // a high-entropy UUID/requestId should not false-positive; a real AWS key should be reported
    r.write_agent(
        "sessions/claude-code/s.jsonl",
        "{\"uuid\":\"7c48816b-6fa5-42f7-9fff-bbeea20ff632\",\"requestId\":\"req_a8Xk92mFqLp3\"}\n\
         {\"content\":\"AKIAIOSFODNN7EXAMPLE\"}\n",
    );
    let (code, _, err) = r.agit(&["-a", "scan"]);
    assert_ne!(code, 0, "a real secret should be reported");
    assert!(err.contains("aws-access-key-id"), "{err}");
    assert!(!err.contains("high-entropy"), "a UUID/requestId inside a session should not be false-flagged by entropy detection:\n{err}");
}

/// Regression: pre-commit must scan **the blob in the index**, not the working tree.
/// Stage a version that carries a secret, then revert the working tree to a clean version (without re-staging); the commit must still be blocked --
/// otherwise the secret lands in the repo while the hook reads the clean working tree and lets it through (the old behavior).
#[test]
fn staged_secret_blocked_even_after_worktree_cleaned() {
    let r = Repo::new();
    let p = "sessions/claude-code/s.jsonl";
    r.write_agent(p, "{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}\n"); // the secret version
    r.agit(&["-a", "add", "-A"]); // stage the secret blob
    r.write_agent(p, "{\"content\":\"clean\"}\n"); // revert the working tree to clean, without re-staging
    let (code, _, err) = r.agit(&["-a", "commit", "-m", "sneaky"]);
    assert_ne!(code, 0, "a staged secret should be blocked even if the working tree is already clean: {err}");
    assert!(err.contains("suspected secrets") || err.contains("aws"), "{err}");
    // and the clean working-tree version should have no hits at all (proving we scan the index, not the disk)
    assert!(!err.contains("clean"));
}


/// Regression: codex sync filters by session_meta.cwd -- it syncs only this project's rollouts,
/// and never pulls in another project's sessions (the privacy bottom line).
#[test]
fn codex_sync_only_pulls_matching_project() {
    let r = Repo::new();
    let top = r.sh("git rev-parse --show-toplevel").trim().to_string();
    let home = r.path().join("fakehome");
    let day = home.join(".codex/sessions/2026/07/15");
    std::fs::create_dir_all(&day).unwrap();
    // this project's rollout (cwd == repo root)
    std::fs::write(
        day.join("rollout-2026-07-15T00-00-00-aaaa-mine.jsonl"),
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"mineid\",\"cwd\":\"{top}\",\"git\":{{\"branch\":\"main\"}}}}}}\n\
             {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"MINE work\"}}}}\n"
        ),
    )
    .unwrap();
    // another project's rollout (different cwd) -- should not be synced
    std::fs::write(
        day.join("rollout-2026-07-15T01-00-00-bbbb-other.jsonl"),
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"otherid\",\"cwd\":\"/some/other/proj\",\"git\":{\"branch\":\"x\"}}}\n\
         {\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"OTHER secret\"}}\n",
    )
    .unwrap();
    // fork/resume: the 1st session_meta is this project, the 2nd embeds a parent session from **another project**.
    // The whole file must be skipped -- otherwise the parent session's content leaks into this project's store and gets pushed to collaborators.
    std::fs::write(
        day.join("rollout-2026-07-15T02-00-00-cccc-fork.jsonl"),
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"forkid\",\"cwd\":\"{top}\",\"git\":{{\"branch\":\"main\"}}}}}}\n\
             {{\"type\":\"session_meta\",\"payload\":{{\"id\":\"parentid\",\"cwd\":\"/some/other/proj\",\"git\":{{\"branch\":\"x\"}}}}}}\n\
             {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"PARENT leaked secret\"}}}}\n"
        ),
    )
    .unwrap();

    let (code, out, err) = r.agit_env(&[("HOME", home.to_str().unwrap())], &["-a", "snap", "--from", "codex"]);
    assert_eq!(code, 0, "codex sync should succeed: {err}");
    assert!(out.contains("matched 1 rollouts"), "should match only this project's 1 rollout (the fork must be skipped):\n{out}");

    assert!(r.captured("codex", "mineid.jsonl"), "this project's session should be written to disk");
    assert!(!r.captured("codex", "otherid.jsonl"), "another project's session should never be synced");
    assert!(!r.captured("codex", "forkid.jsonl"), "a fork that contains a foreign-project session should not be synced at all");
    // Double insurance, and the privacy bottom line: another project's content must not appear
    // ANYWHERE in the store — read the whole tree, not one directory, so the claim cannot be narrowed
    // by where the layout happens to put a file.
    let all = r.all_session_text();
    assert!(all.contains("MINE work"));
    assert!(!all.contains("OTHER secret"), "another project's content leaked");
    assert!(!all.contains("PARENT leaked secret"), "the parent-project session inside the fork leaked");
}

// ─────────────────── runtime parity: claude-code and codex are peers ───────────────────

/// Write a codex rollout owned by this project into `<home>/.codex/sessions/...`.
fn seed_codex_rollout(r: &Repo, home: &Path, id: &str, msg: &str) {
    let top = r.sh("git rev-parse --show-toplevel").trim().to_string();
    let day = home.join(".codex/sessions/2026/07/15");
    std::fs::create_dir_all(&day).unwrap();
    std::fs::write(
        day.join(format!("rollout-2026-07-15T00-00-00-{id}.jsonl")),
        format!(
            "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{top}\",\"git\":{{\"branch\":\"main\"}}}}}}\n\
             {{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\",\"message\":\"{msg}\"}}}}\n"
        ),
    )
    .unwrap();
}

/// The framing bug, as behaviour: claude was hard-coded as snap's default, so a codex-only project
/// silently snapped nothing and failed with "has this project not been run in Claude Code yet?".
/// With only codex sessions present, a bare `snap` must capture codex without being told.
#[test]
fn snap_captures_codex_without_being_told_when_it_is_the_only_runtime() {
    let r = Repo::new();
    let home = r.path().join("codexhome");
    seed_codex_rollout(&r, &home, "onlycodex", "codex work");

    let (code, out, err) = r.agit_env(&[("HOME", home.to_str().unwrap())], &["a", "snap"]);
    assert_eq!(code, 0, "a bare snap must find codex on its own: {err}{out}");
    assert!(out.contains("codex"), "should report capturing codex: {out}");
    assert!(r.captured("codex", "onlycodex.jsonl"), "codex session should be mirrored into the store");
    assert!(!err.contains("Claude Code"), "must not fail over a runtime this project never used: {err}");
}

/// With no --from and both runtimes holding sessions, snap captures BOTH — it never silently picks one.
#[test]
fn snap_with_no_from_captures_both_runtimes() {
    let r = Repo::new();
    let home = r.path().join("bothhome");
    seed_codex_rollout(&r, &home, "cx", "codex side");
    // a claude transcript this project owns
    let top = r.sh("git rev-parse --show-toplevel").trim().to_string();
    let slug: String = top.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let sdir = home.join(".claude/projects").join(&slug);
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(
        sdir.join("cc.jsonl"),
        format!("{{\"type\":\"user\",\"sessionId\":\"cc\",\"cwd\":\"{top}\",\"message\":{{\"content\":\"claude side\"}}}}\n"),
    )
    .unwrap();

    let (code, out, err) = r.agit_env(&[("HOME", home.to_str().unwrap())], &["a", "snap"]);
    assert_eq!(code, 0, "snap should capture both: {err}");
    assert!(r.captured("codex", "cx.jsonl"), "codex must be captured: {out}");
    assert!(r.captured("claude-code", "cc.jsonl"), "claude-code must be captured: {out}");
}

/// No sessions in either runtime: the error names them as peers, alphabetically, and never singles
/// claude out as the one that was missing.
#[test]
fn snap_with_no_sessions_anywhere_names_both_runtimes() {
    let r = Repo::new();
    let (code, _, err) = r.agit(&["a", "snap"]);
    assert_ne!(code, 0);
    assert!(err.contains("claude-code, codex"), "should name both runtimes in one breath: {err}");
}

/// `harness apply` rewrites the project's own .mcp.json / .claude, so with both runtimes captured and
/// no --from it must refuse and name them — not quietly apply claude's.
#[test]
fn harness_apply_refuses_to_guess_between_both_runtimes() {
    let r = Repo::new();
    for rt in ["claude-code", "codex"] {
        r.write_agent(&format!("harness/{rt}/project/mcp.json"), "{}\n");
    }
    let (code, out, err) = r.agit(&["harness", "apply"]);
    assert_ne!(code, 0, "must not pick a runtime on its own: {out}");
    assert!(err.contains("claude-code") && err.contains("codex"), "should name both as peers: {err}");
    assert!(err.contains("--from"), "should say how to disambiguate: {err}");
}

// ─────────────────────── passthrough fidelity ───────────────────────

#[test]
fn passthrough_propagates_git_exit_code() {
    let r = Repo::new();
    let (code, _, _) = r.agit(&["rev-parse", "does-not-exist"]);
    assert_ne!(code, 0);
    assert_ne!(code, 2, "passthrough should propagate git's exit code");
}

// ─────────────────────── smart agent-scope verbs ───────────────────────

/// `sync` is the back-compat alias for the dialogue merge, and it must be scope-gated exactly like
/// `merge`: only the Agent scope runs it. In the Environment scope `agit sync` is not agit's verb —
/// it passes through to git and must NEVER trigger the dialogue merge (the ungated-alias bug).
#[test]
fn sync_alias_is_scope_gated_like_merge() {
    let r = Repo::new();

    // Agent scope: `a sync` reaches the same command as `a merge`. With no target both print the same
    // usage and exit 2 — proof `sync` routes to merge_cmd, symmetric with `merge`.
    let (mc, mo, me) = r.agit(&["a", "merge"]);
    let (sc, so, se) = r.agit(&["a", "sync"]);
    assert_eq!(mc, 2, "`a merge` with no target exits 2: {me}");
    assert!(me.contains("agit a merge <target>"), "`a merge` prints the merge usage: {me}");
    assert_eq!((sc, &so, &se), (mc, &mo, &me), "`a sync` must behave exactly like `a merge`");

    // Environment scope: `agit sync` passes through to git — it must not run the dialogue merge (no
    // merge usage on either stream), which is precisely what the ungated alias used to do.
    let (_ec, eo, ee) = r.agit(&["sync"]);
    assert!(
        !eo.contains("agit a merge <target>") && !ee.contains("agit a merge <target>"),
        "`agit sync` in the environment must pass through to git, not run the dialogue merge: {eo}{ee}"
    );
}

#[test]
fn a_pull_fast_forwards_but_refuses_to_textually_merge_diverged_sessions() {
    let r = Repo::new();

    // A bare "hub" to share the store through, and the store pointed at it (smart push records the
    // binding and sets the upstream).
    r.sh("git init -q --bare -b main hub.git");
    let hub_path = r.path().join("hub.git");
    let hub = hub_path.to_str().unwrap();
    assert_eq!(r.agit(&["a", "remote", "add", "origin", hub]).0, 0, "remote add");
    assert_eq!(r.agit(&["a", "push", "-u", "origin", "main"]).0, 0, "initial push");

    // A teammate clones the hub and pushes a session; we hold no local commits of our own yet.
    r.sh(&format!("git clone -q {hub} team"));
    r.sh("cd team && git config user.name t && git config user.email t@x && git config commit.gpgsign false");
    r.sh("cd team && git commit -q --allow-empty -m 'teammate session' && git push -q origin main");

    // Fast-forward case: strictly behind → `agit a pull` succeeds and takes the new session.
    let (code, _o, err) = r.agit(&["a", "pull"]);
    assert_eq!(code, 0, "a pull that only needs a fast-forward must succeed: {err}");

    // Divergence case: both sides now commit.
    r.sh("cd team && git commit -q --allow-empty -m 'teammate again' && git push -q origin main");
    r.git_agent(&["commit", "--allow-empty", "--no-verify", "-m", "our local session"]);

    let (code, _o, err) = r.agit(&["a", "pull"]);
    assert_ne!(code, 0, "a diverged pull must NOT textually splice transcripts");
    assert!(err.contains("agit a merge"), "it must route to the dialogue merge: {err}");
    assert!(err.to_lowercase().contains("diverg"), "and explain why: {err}");

    // And no textual merge happened: HEAD is still a single-parent commit, not a merge.
    let parents = r.git_agent(&["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert_eq!(parents.split_whitespace().count(), 2, "HEAD must not be a merge commit: {parents}");
}

/// A minimal but valid Claude session: two lines carrying `sid` and a distinctive note.
fn claude_session(sid: &str, note: &str) -> String {
    format!(
        "{{\"type\":\"user\",\"sessionId\":\"{sid}\",\"uuid\":\"u1\",\"cwd\":\"/proj\",\"message\":{{\"role\":\"user\",\"content\":\"go\"}}}}\n\
         {{\"type\":\"assistant\",\"sessionId\":\"{sid}\",\"uuid\":\"u2\",\"parentUuid\":\"u1\",\"cwd\":\"/proj\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"{note}\"}}]}}}}\n"
    )
}

/// Concatenate every installed Claude session (`~/.claude/projects/<slug>/<id>.jsonl`, HOME being the
/// repo dir in these tests) — that is where a spliced session lands, ready to resume.
fn installed_claude_sessions(r: &Repo) -> String {
    fn walk(dir: &Path, out: &mut String) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().map(|x| x == "jsonl").unwrap_or(false) {
                out.push_str(&std::fs::read_to_string(&p).unwrap_or_default());
            }
        }
    }
    let mut s = String::new();
    walk(&r.path().join(".claude/projects"), &mut s);
    s
}

/// `--splice` is the no-model, no-runtime-CLI merge: it combines both sides' sessions into one new
/// resumable session instead of running a dialogue. This runs the whole path end to end — with neither
/// a model nor `claude`/`codex` installed — and asserts the combined session carries both sides' work.
#[test]
fn a_merge_splice_combines_both_sides_without_a_model() {
    let r = Repo::new();

    // A base commit both sides fork from, so the merge is grounded in a shared ancestor.
    r.git_agent(&["commit", "--allow-empty", "--no-verify", "-m", "base"]);

    // A peer branch carrying a diverged Claude session that HEAD does not have.
    r.git_agent(&["checkout", "-q", "-b", "peer"]);
    r.write_agent("sessions/claude-code/team.jsonl", &claude_session("TEAM", "team found the deadlock"));
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "team session"]);

    // Back on main, our own diverged Claude session.
    r.git_agent(&["checkout", "-q", "main"]);
    r.write_agent("sessions/claude-code/local.jsonl", &claude_session("LOCAL", "local fixed the parser"));
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "local session"]);

    let (code, out, err) = r.agit(&["a", "merge", "peer", "--from", "claude-code", "--splice"]);
    assert_eq!(code, 0, "splice must succeed with no model and no claude/codex installed: {err}{out}");
    assert!(out.contains("splice (no model)"), "it announces the no-model path: {out}");
    assert!(out.contains("resume"), "it prints how to resume the combined session: {out}");

    // The combined session is a real installed transcript carrying BOTH sides' work.
    let combined = installed_claude_sessions(&r);
    assert!(combined.contains("local fixed the parser"), "carries the local side: {combined}");
    assert!(combined.contains("team found the deadlock"), "carries the peer side: {combined}");
    // And it is valid JSONL, not a glued-together text blob.
    for line in combined.lines().filter(|l| !l.trim().is_empty()) {
        serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|_| panic!("resumable line is valid json: {line}"));
    }
}

#[test]
fn a_fetch_reports_incoming_sessions_without_integrating() {
    let r = Repo::new();
    r.sh("git init -q --bare -b main hub.git");
    let hub_path = r.path().join("hub.git");
    let hub = hub_path.to_str().unwrap();
    assert_eq!(r.agit(&["a", "remote", "add", "origin", hub]).0, 0, "remote add");
    assert_eq!(r.agit(&["a", "push", "-u", "origin", "main"]).0, 0, "initial push");

    // A teammate clones, adds a real codex session, and pushes it.
    r.sh(&format!("git clone -q {hub} team"));
    r.sh("cd team && git config user.name t && git config user.email t@x && git config commit.gpgsign false");
    r.sh("mkdir -p team/sessions/codex && printf '{}\\n' > team/sessions/codex/sess1.jsonl");
    r.sh("cd team && git add -A && git commit -q -m 'a codex session' && git push -q origin main");

    let (code, out, err) = r.agit(&["a", "fetch"]);
    assert_eq!(code, 0, "fetch should succeed: {err}");
    assert!(out.contains("new session"), "fetch reports incoming sessions in session terms: {out}");
    assert!(out.contains("codex"), "and attributes the runtime: {out}");

    // Fetch must not integrate — no working-tree change, the session is not present locally.
    assert!(
        !r.agent().join("sessions/codex/sess1.jsonl").exists(),
        "fetch only advances remote-tracking refs; it must not touch the working tree"
    );
}

#[test]
fn a_init_mints_an_agent_and_a_switch_selects_it() {
    let r = Repo::new();

    // `agit a init <name>` is the git-native mint (was `new`): a store with its own identity.
    let (code, out, err) = r.agit(&["a", "init", "backend"]);
    assert_eq!(code, 0, "agit a init should mint: {err}");
    assert!(out.contains("minted backend"), "should report the mint: {out}");

    // With two agents, `agit a switch` (was `use`) picks which one this worktree resolves to.
    let (code, _o, err) = r.agit(&["a", "switch", "backend"]);
    assert_eq!(code, 0, "agit a switch should select the agent: {err}");
    let (_, info, _) = r.agit(&["a", "info", "backend"]);
    assert!(info.contains("backend"), "switched agent should resolve: {info}");

    // A bare name (no `<name>`) is an error, not a silent no-op.
    assert_ne!(r.agit(&["a", "init"]).0, 0, "init needs a name");
}

#[test]
fn snap_from_a_never_run_runtime_exits_zero_for_both_peers() {
    let r = Repo::new();
    // HOME is the repo dir, so neither runtime has a session dump. Both peers must behave the same:
    // nothing to mirror, exit 0 — not one erroring (claude-code) and the other succeeding (codex).
    for rt in ["claude-code", "codex"] {
        let (code, out, err) = r.agit(&["snap", "--from", rt]);
        assert_eq!(code, 0, "snap --from {rt} with no sessions must exit 0: out={out} err={err}");
    }
}

#[test]
fn a_rebind_new_id_with_a_bad_name_errors_and_leaves_the_active_agent_alone() {
    let r = Repo::new();
    // The store the repo's (active) agent resolves to. Its directory name IS the aid.
    let before = r.agent();
    let aid_before = before.file_name().unwrap().to_str().unwrap().to_string();

    // Re-mint MOVES the store and rewrites its identity. A typo'd name must error, never re-mint the
    // active agent by falling through.
    let (code, _out, err) = r.agit(&["a", "rebind", "no-such-agent", "--new-id"]);
    assert_ne!(code, 0, "a bad name must error, not re-mint whatever is active");
    assert!(err.contains("no-such-agent"), "the error must name the bad selector: {err}");

    // The active agent is untouched: same store, same aid, still on disk.
    let after = r.agent();
    assert_eq!(after, before, "the active agent's store must not have moved");
    assert_eq!(after.file_name().unwrap().to_str().unwrap(), aid_before, "its aid must be unchanged");
    assert!(before.exists(), "the original store must still exist");
}

// ─────────────────────── `agit a status` — the per-repo overview ───────────────────────

/// `agit a status` names this repo's agents, marks the active one, and reports where the active store
/// stands against its remote — no upstream yet, then unpushed after a local commit.
#[test]
fn a_status_lists_this_repos_agents_and_the_active_stores_upstream_position() {
    let r = Repo::new();

    let (code, out, err) = r.agit(&["a", "status"]);
    assert_eq!(code, 0, "status must succeed: {err}");
    assert!(out.contains("AGENT") && out.contains("SESSIONS"), "renders the overview table: {out}");
    assert!(out.contains("testmemory"), "names this repo's agent: {out}");
    assert!(out.contains("(active)"), "marks which agent is active here: {out}");
    assert!(out.contains("no upstream"), "a store never pushed has no upstream to report: {out}");

    // Give the store a real upstream, push, then make a local commit it hasn't pushed.
    r.sh("git init -q --bare -b main hub.git");
    let hub_path = r.path().join("hub.git");
    let hub = hub_path.to_str().unwrap();
    assert_eq!(r.agit(&["a", "remote", "add", "origin", hub]).0, 0, "remote add");
    assert_eq!(r.agit(&["a", "push", "-u", "origin", "main"]).0, 0, "initial push");
    r.git_agent(&["commit", "--allow-empty", "--no-verify", "-m", "local work"]);

    let (code, out, err) = r.agit(&["a", "status"]);
    assert_eq!(code, 0, "status must succeed with an upstream: {err}");
    assert!(out.contains("unpushed"), "status surfaces the unpushed commit (ahead of origin): {out}");
}

// ─────────────────────── session-aware `agit a log` / `agit a diff` ───────────────────────

/// A minimal Claude transcript carrying a real user prompt, an `Edit` tool call, and a timestamp (so
/// recency ordering is deterministic).
fn session_with(prompt: &str, edit_file: &str, ts: &str) -> String {
    format!(
        "{{\"type\":\"user\",\"sessionId\":\"s\",\"uuid\":\"u1\",\"timestamp\":\"{ts}\",\"cwd\":\"/code/web\",\"message\":{{\"role\":\"user\",\"content\":\"{prompt}\"}}}}\n\
         {{\"type\":\"assistant\",\"sessionId\":\"s\",\"uuid\":\"u2\",\"parentUuid\":\"u1\",\"timestamp\":\"{ts}\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Edit\",\"input\":{{\"file_path\":\"{edit_file}\"}}}}]}}}}\n"
    )
}

/// `agit a log` renders the SESSION timeline (prompt gist + tool activity, most recent first), not the
/// store's git history — and `--raw` is the escape hatch back to `git log`.
#[test]
fn a_log_renders_the_session_timeline_and_raw_falls_back_to_git() {
    let r = Repo::new();
    r.write_agent(
        "sessions/claude-code/older.jsonl",
        &session_with("build the login form", "/code/web/login.rs", "2026-07-10T09:00:00Z"),
    );
    r.write_agent(
        "sessions/claude-code/newer.jsonl",
        &session_with("add a rate limiter", "/code/web/limit.rs", "2026-07-16T09:00:00Z"),
    );

    let (code, out, err) = r.agit(&["a", "log"]);
    assert_eq!(code, 0, "the session-aware log must succeed: {err}");
    assert!(out.contains("build the login form"), "shows a session's opening prompt: {out}");
    assert!(out.contains("add a rate limiter"), "shows the other session's prompt: {out}");
    assert!(out.contains("tool call"), "summarizes tool activity: {out}");
    assert!(out.contains("edited"), "names the edited file(s): {out}");
    assert!(!out.contains("agit: mint agent"), "the session view is NOT the store's git log: {out}");
    // Most recent first: the newer session's prompt appears before the older one's.
    let newer = out.find("add a rate limiter").unwrap();
    let older = out.find("build the login form").unwrap();
    assert!(newer < older, "sessions must be listed most-recent first: {out}");

    // The escape hatch: `--raw` drops the flag and hands the rest to real git.
    let (code, raw, err) = r.agit(&["a", "log", "--raw", "-1", "--format=%s"]);
    assert_eq!(code, 0, "`a log --raw` must fall back to git: {err}");
    assert!(raw.contains("agit: mint agent"), "`a log --raw` shows the store's git history: {raw}");
    assert!(!raw.contains("rate limiter"), "the raw git log is not the session view: {raw}");
}

/// `agit a diff` shows the session-level change between two refs — the prompts and edits ADDED, not a
/// line-by-line diff of the jsonl — and `--raw` falls back to `git diff`.
#[test]
fn a_diff_shows_session_level_changes_and_raw_falls_back_to_git() {
    let r = Repo::new();
    // A new session captured in one commit, so HEAD~1..HEAD is exactly "this session was added".
    r.write_agent(
        "sessions/claude-code/health.jsonl",
        &session_with("add the health endpoint", "/code/web/health.rs", "2026-07-16T10:00:00Z"),
    );
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "captured a session"]);

    let (code, out, err) = r.agit(&["a", "diff", "HEAD~1", "HEAD"]);
    assert_eq!(code, 0, "the session-aware diff must succeed: {err}");
    assert!(out.contains("new session"), "labels the added session: {out}");
    assert!(out.contains("add the health endpoint"), "shows the prompt added: {out}");
    assert!(out.contains("health.rs"), "shows the edit added: {out}");
    assert!(!out.contains("sessionId"), "the session view is NOT the raw jsonl diff: {out}");

    // No refs → a bare diff still runs (defaults to HEAD~1..HEAD here) and finds the same change.
    let (code, bare, err) = r.agit(&["a", "diff"]);
    assert_eq!(code, 0, "a bare `a diff` must resolve a default range: {err}");
    assert!(bare.contains("add the health endpoint"), "the default range covers the new session: {bare}");

    // `--raw` is git's byte-level diff, which DOES carry the jsonl.
    let (code, raw, err) = r.agit(&["a", "diff", "--raw", "HEAD~1", "HEAD"]);
    assert_eq!(code, 0, "`a diff --raw` must fall back to git: {err}");
    assert!(raw.contains("sessionId"), "`a diff --raw` is the byte-level git diff: {raw}");
}

// ─────────────────────── `agit a rebind` — binding-repair e2e coverage ───────────────────────

/// A stale but well-formed aid, standing in for the identity a committed binding still records after a
/// remote was recreated under the same name with a different store.
const STALE_AID: &str = "agt_00000000-0000-7000-8000-000000000000";

/// Overwrite `.agit.toml` so this repo's binding claims the name `frontend` maps to a DIFFERENT aid than
/// the store actually carries — exactly the "recreated remote, same name, new identity" case the aid
/// integrity check exists to catch. `remote` is written verbatim (credential and all).
fn corrupt_binding_to(r: &Repo, remote: &str) {
    r.write(
        ".agit.toml",
        &format!(
            "version = 1\n\n[[agent]]\nid = \"{STALE_AID}\"\nname = \"frontend\"\nremote = \"{remote}\"\n\n[defaults]\nagent = \"frontend\"\n"
        ),
    );
}

/// The DEFAULT rebind path (no `--remote`): a recreated remote changing the aid trips the integrity
/// error that names `agit a rebind`; rebind then repairs the binding to a single entry carrying the
/// store's real aid, recording the store's EXISTING origin credential-stripped.
#[test]
fn a_rebind_repairs_the_binding_and_strips_the_stores_credentialed_origin() {
    let r = Repo::new();
    // One agent, named `frontend`, so "exactly one entry" is literal. Its store's real aid:
    assert_eq!(r.agit(&["a", "rename", "testmemory", "frontend"]).0, 0, "rename to frontend");
    let real_aid = r.agent().file_name().unwrap().to_str().unwrap().to_string();

    // The store has a credentialed origin (what a push would have set).
    let origin = "https://alice:tok_secret123@ex.com/frontend.git";
    assert_eq!(r.agit(&["a", "remote", "add", "origin", origin]).0, 0, "remote add");

    // The binding now records a DIFFERENT aid for `frontend` → the integrity check must refuse and name
    // rebind.
    corrupt_binding_to(&r, origin);
    let (code, _o, err) = r.agit(&["a", "switch", "frontend"]);
    assert_ne!(code, 0, "a recreated-remote aid mismatch must refuse, not silently rebind");
    assert!(err.contains("rebind"), "the integrity error must name `agit a rebind`: {err}");

    // Repair: the default path rewrites the binding to the store's real identity and records its origin.
    let (code, _o, err) = r.agit(&["a", "rebind", "frontend"]);
    assert_eq!(code, 0, "rebind must repair the binding: {err}");

    let toml = std::fs::read_to_string(r.path().join(".agit.toml")).unwrap();
    assert_eq!(toml.matches("[[agent]]").count(), 1, "exactly one entry, no duplicate name/aid: {toml}");
    assert!(toml.contains(&real_aid), "the binding now carries the store's real aid: {toml}");
    assert!(!toml.contains(STALE_AID), "the stale aid must be gone: {toml}");
    assert!(toml.contains("https://ex.com/frontend.git"), "records the credential-stripped locator: {toml}");
    assert!(!toml.contains("tok_secret123"), "a token must never land in the committed binding: {toml}");

    // …and the mismatch is gone: the same command that refused now succeeds.
    assert_eq!(r.agit(&["a", "switch", "frontend"]).0, 0, "the binding resolves cleanly after rebind");
}

/// The `--remote` rebind path: point the binding at a NEW recreated remote, adopting the store's real
/// identity and recording the new URL credential-stripped, still as a single entry.
#[test]
fn a_rebind_remote_repoints_the_binding_credential_stripped() {
    let r = Repo::new();
    assert_eq!(r.agit(&["a", "rename", "testmemory", "frontend"]).0, 0, "rename to frontend");
    let real_aid = r.agent().file_name().unwrap().to_str().unwrap().to_string();

    // The committed binding records a stale aid + old remote for `frontend`.
    corrupt_binding_to(&r, "https://ex.com/old-frontend.git");
    let (code, _o, err) = r.agit(&["a", "switch", "frontend"]);
    assert_ne!(code, 0, "the stale aid must refuse");
    assert!(err.contains("rebind"), "and name rebind: {err}");

    // Repair, repointing at a NEW credentialed remote.
    let new_remote = "https://bob:tok_other456@newhub.com/frontend.git";
    let (code, _o, err) = r.agit(&["a", "rebind", "frontend", "--remote", new_remote]);
    assert_eq!(code, 0, "rebind --remote must succeed: {err}");

    let toml = std::fs::read_to_string(r.path().join(".agit.toml")).unwrap();
    assert_eq!(toml.matches("[[agent]]").count(), 1, "one entry only, no dup: {toml}");
    assert!(toml.contains(&real_aid), "adopts the store's real aid: {toml}");
    assert!(!toml.contains(STALE_AID), "the stale aid is dropped: {toml}");
    assert!(toml.contains("https://newhub.com/frontend.git"), "repointed at the new remote, stripped: {toml}");
    assert!(!toml.contains("tok_other456"), "no credential in the committed binding: {toml}");
    // The store's own origin was updated to the new remote too.
    assert_eq!(r.git_agent(&["remote", "get-url", "origin"]), new_remote, "the store's origin is repointed");
    assert_eq!(r.agit(&["a", "switch", "frontend"]).0, 0, "resolves cleanly after the remote rebind");
}
