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
    assert!(said.contains("agit a track"), "and the other real answer: {said}");
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

/// A repo from before identity gets ONE actionable error, from whatever the user typed — not a
/// second, silently empty store beside the memory it already has.
#[test]
fn a_store_that_predates_identity_is_refused_with_one_actionable_error() {
    let dir = tempfile::tempdir().unwrap();
    let r = Repo { dir };
    r.sh("git init -q -b main .");
    r.sh("git config user.name dev && git config user.email d@x.com && git config commit.gpgsign false");
    r.write("app.ts", "x\n");
    r.sh("git add -A && git commit -qm seed");
    // exactly what agit scaffolded before identity: the nested store with the shared placeholder
    r.write(".agit/agent/agent.toml", "# Agent identity\nid = \"unnamed-agent\"\n");
    r.write(".agit/agent/sessions/claude-code/old.jsonl", "{\"type\":\"user\"}\n");
    r.sh("cd .agit/agent && git init -q -b main . && git add -A && \
          git -c user.name=a -c user.email=a@x -c commit.gpgsign=false commit -qm 'agit: initialize Agent Store'");

    for args in [vec!["a", "log"], vec!["start"], vec!["init"]] {
        let (code, out, err) = r.agit(&args);
        let said = format!("{out}{err}");
        assert_ne!(code, 0, "`agit {}` must not silently succeed on a legacy repo", args.join(" "));
        assert!(said.contains("predates agent identity"), "`agit {}`: {said}", args.join(" "));
        assert!(said.contains("agit a import"), "`agit {}` must name the fix: {said}", args.join(" "));
    }
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
    let (_, _, err) = r.agit(&["-a", "snap", "--from", "claude-code"]);
    let isolated = format!("{}/.claude/projects", r.path().display());
    assert!(err.contains(&isolated), "snap should look under the isolated HOME ({isolated}), got:\n{err}");
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

// ─────────────────── `agit a` — the subcommand that replaces `-a` ───────────────────

/// The whole point of the subcommand: `agit a <git-verb>` reaches the store exactly as `agit -a` did.
#[test]
fn agit_a_subcommand_runs_git_on_the_agent_store() {
    let r = Repo::new();
    for verb in ["a", "agent"] {
        let (code, out, err) = r.agit(&[verb, "log", "-1", "--format=%s"]);
        assert_eq!(code, 0, "`agit {verb} log` should reach the store: {err}");
        assert!(
            out.contains("agit: mint agent"),
            "`agit {verb} log` should show the store's history, not the code repo's: {out}"
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
    for verb in ["list", "use", "new", "track", "info", "rename", "publish", "rebind", "import"] {
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

/// `-a` keeps working while the docs, demo scripts and install-shadow.sh still say it — silently.
#[test]
fn dash_a_remains_a_silent_deprecated_alias() {
    let r = Repo::new();
    let (code, out, err) = r.agit(&["-a", "log", "-1", "--format=%s"]);
    assert_eq!(code, 0, "`agit -a log` must keep working: {err}");
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

    let cdir = r.agent().join("sessions/codex");
    assert!(cdir.join("mineid.jsonl").exists(), "this project's session should be written to disk");
    assert!(!cdir.join("otherid.jsonl").exists(), "another project's session should never be synced");
    assert!(!cdir.join("forkid.jsonl").exists(), "a fork that contains a foreign-project session should not be synced at all");
    // double insurance: another project's content should not appear anywhere in the codex directory
    let mut all = String::new();
    for e in std::fs::read_dir(&cdir).unwrap() {
        all.push_str(&std::fs::read_to_string(e.unwrap().path()).unwrap());
    }
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
    assert!(r.agent().join("sessions/codex/onlycodex.jsonl").exists(), "codex session should be mirrored into the store");
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
    assert!(r.agent().join("sessions/codex/cx.jsonl").exists(), "codex must be captured: {out}");
    assert!(r.agent().join("sessions/claude-code/cc.jsonl").exists(), "claude-code must be captured: {out}");
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
