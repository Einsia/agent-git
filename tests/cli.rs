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
    assert!(said.contains("no agent name"), "{said}");
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

/// `agit clone <known-local-agent-name>` is smart: a bare name that resolves to an agent this machine
/// already has (or the committed binding declares) is ADOPTED via the agent path, not raw-git-cloned into
/// a directory named after it. The redirect note names what happened and how to force the raw clone.
#[test]
fn clone_of_a_known_local_agent_name_routes_to_the_agent_path() {
    let r = Repo::new(); // mints + binds "testmemory" here
    let (code, out, err) = r.agit(&["clone", "testmemory"]);
    assert_eq!(code, 0, "clone of a known agent name should adopt it, not fail: {err}");
    assert!(err.contains("detected an agit agent store"), "must print the redirect note: {err}");
    assert!(out.contains("cloned testmemory"), "must adopt the agent by identity: {out}");
}

/// `--git` forces the raw passthrough git clone, even for a target that WOULD redirect. It is stripped
/// and handed to git verbatim BEFORE any hub probe, so git clones a local path `testmemory` (which does
/// not exist → git's own error) and the smart redirect never fires.
#[test]
fn clone_git_flag_forces_raw_passthrough_and_never_redirects() {
    let r = Repo::new();
    // Without --git the same target redirects (proven above); with --git it must not.
    let (code, _out, err) = r.agit(&["clone", "--git", "testmemory"]);
    assert_ne!(code, 0, "raw git clone of a nonexistent local path must fail with git's own error");
    assert!(!err.contains("detected an agit agent store"), "--git must force passthrough, not redirect: {err}");
}

/// `agit a clone <empty-store>` (a store created but never pushed to) must NOT die with the raw
/// `agent.toml … No such file (os error 2)`. It surfaces a clear, actionable message; and `--init` mints
/// a fresh agent into the empty store and pushes, so the once-empty store becomes a real, adoptable agent.
#[test]
fn agent_clone_of_an_empty_store_is_actionable_and_init_mints_into_it() {
    let r = Repo::new();
    let bare = r.path().join("empty.git");
    Command::new("git").args(["init", "--bare", "-q", "-b", "main"]).arg(&bare).status().unwrap();
    let bare_url = bare.to_str().unwrap();

    // Plain adopt of an empty store → the clear message, not the raw os-error.
    let (code, _out, err) = r.agit(&["a", "clone", bare_url]);
    assert_ne!(code, 0, "adopting an empty store must fail cleanly");
    assert!(err.contains("empty store"), "must name the empty-store case: {err}");
    assert!(err.contains("--init"), "must point at --init: {err}");
    assert!(!err.contains("os error") && !err.contains("No such file"), "must not surface the raw os-error: {err}");

    // --init mints a fresh agent into it and pushes.
    let (ic, iout, ierr) = r.agit(&["a", "clone", "--init", bare_url]);
    assert_eq!(ic, 0, "--init should mint+push into the empty store: {ierr}");
    assert!(iout.contains("minted") && iout.contains("empty store"), "reports the minted agent: {iout}");

    // The store now publishes a real identity on main — it is adoptable.
    let toml = Command::new("git").arg("-C").arg(&bare).args(["show", "main:agent.toml"]).output().unwrap();
    assert!(String::from_utf8_lossy(&toml.stdout).contains("agt_"), "the once-empty store now carries an identity");
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


/// The non-bypassable gate: `agit a commit` scans the staged index ITSELF, before delegating to git.
/// A secret must be blocked even with git's pre-commit hook removed — the whole point is that agit's
/// own commit path cannot be slipped past with `--no-verify` (or a wiped hook), unlike bare `git commit`.
#[test]
fn secret_blocked_through_agit_commit_even_with_git_hook_removed() {
    let r = Repo::new();
    // Rip out git's own hooks so ONLY the in-wrapper gate can be doing the blocking.
    std::fs::remove_file(r.agent().join(".git/hooks/pre-commit")).ok();
    std::fs::remove_file(r.agent().join(".git/hooks/pre-push")).ok();

    r.write_agent(
        "sessions/claude-code/s.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n",
    );
    r.agit(&["-a", "add", "-A"]);
    let (code, _, err) = r.agit(&["-a", "commit", "-m", "leak via agit"]);
    assert_ne!(code, 0, "the wrapper gate must block a staged secret with no git hook present: {err}");
    assert!(err.contains("secret gate") || err.contains("aws"), "{err}");
    // Nothing was committed: the store's HEAD still points at the mint commit, not the leak.
    let head_subject = r.git_agent(&["log", "-1", "--format=%s"]);
    assert!(head_subject.starts_with("agit: mint agent"), "the secret commit must not exist: {head_subject}");
}

/// Regression (bypass hole): `agit a commit -a` stages tracked modifications AT COMMIT TIME, after a
/// pre-commit index scan, and --no-verify skips git's hook. So the wrapper must pre-stage what -a will
/// add and scan THAT. Before the fix, `agit a commit -a` committed a secret with exit 0, strictly less
/// safe than bare git.
#[test]
fn secret_blocked_through_agit_commit_dash_a() {
    let r = Repo::new();
    std::fs::remove_file(r.agent().join(".git/hooks/pre-commit")).ok();
    // A clean, TRACKED session first (so a later `-a` stages its modification).
    r.write_agent("sessions/claude-code/s.jsonl", "{\"type\":\"user\",\"message\":{\"content\":\"clean\"}}\n");
    r.agit(&["-a", "add", "-A"]);
    assert_eq!(r.agit(&["-a", "commit", "-m", "base"]).0, 0, "a clean base commit succeeds");
    // Now modify it to carry a secret and try to sneak it in with -a.
    r.write_agent("sessions/claude-code/s.jsonl", "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n");
    let (code, _out, err) = r.agit(&["-a", "commit", "-a", "-m", "sneak via -a"]);
    assert_ne!(code, 0, "agit a commit -a must gate what it stages at commit time: {err}");
    assert!(
        !r.git_agent(&["show", "HEAD:sessions/claude-code/s.jsonl"]).contains("AKIA"),
        "the secret staged by -a must not reach a commit"
    );
}

/// Regression (bypass hole): `agit a commit <pathspec>` stages the named path at commit time; the
/// wrapper must stage and scan the pathspec, not just the pre-existing index.
#[test]
fn secret_blocked_through_agit_commit_pathspec() {
    let r = Repo::new();
    std::fs::remove_file(r.agent().join(".git/hooks/pre-commit")).ok();
    r.write_agent("sessions/claude-code/s.jsonl", "{\"type\":\"user\",\"message\":{\"content\":\"clean\"}}\n");
    r.agit(&["-a", "add", "-A"]);
    assert_eq!(r.agit(&["-a", "commit", "-m", "base"]).0, 0, "a clean base commit succeeds");
    // Modify to carry a secret, then commit it by pathspec (git stages the path at commit time).
    r.write_agent("sessions/claude-code/s.jsonl", "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n");
    let (code, _out, err) = r.agit(&["-a", "commit", "-m", "sneak", "sessions/claude-code/s.jsonl"]);
    assert_ne!(code, 0, "agit a commit <pathspec> must gate the path it stages: {err}");
    assert!(
        !r.git_agent(&["show", "HEAD:sessions/claude-code/s.jsonl"]).contains("AKIA"),
        "the secret staged by the pathspec must not reach a commit"
    );
}

/// The LEGIBLE escape valve: the gate is refusable, but the exit is visible and auditable — not a
/// silent bypass. `AGIT_ALLOW_SECRETS=1` lets the same commit through AND discloses that it did.
#[test]
fn visible_override_lets_a_secret_commit_through_and_discloses_it() {
    let r = Repo::new();
    std::fs::remove_file(r.agent().join(".git/hooks/pre-commit")).ok();

    r.write_agent(
        "sessions/claude-code/s.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n",
    );
    r.agit(&["-a", "add", "-A"]);
    let (code, _out, err) = r.agit_env(&[("AGIT_ALLOW_SECRETS", "1")], &["-a", "commit", "-m", "explicit override"]);
    assert_eq!(code, 0, "the disclosed override must let the commit through: {err}");
    // It committed…
    assert_eq!(r.git_agent(&["log", "-1", "--format=%s"]), "explicit override");
    // …and it said so — the escape is on the record, not hidden.
    assert!(err.contains("AGIT_ALLOW_SECRETS"), "the override must be disclosed on stderr: {err}");
    assert!(err.to_lowercase().contains("bypass"), "the disclosure must name what it did: {err}");
}

/// Root-cause regression: claude-code stamps `bridgeSessionId:"cse_..."` into EVERY transcript. The
/// `cse_...` value is high-entropy, so before the fix the generic entropy rule flagged it and the gate
/// REFUSED every real claude-code session. It must now commit cleanly — the key is a session id, exempt.
#[test]
fn real_claude_code_bridge_session_id_does_not_trip_the_gate() {
    let r = Repo::new();
    std::fs::remove_file(r.agent().join(".git/hooks/pre-commit")).ok();
    r.write_agent(
        "sessions/claude-code/s.jsonl",
        "{\"type\":\"user\",\"bridgeSessionId\":\"cse_Zx8Z4pQ1mV7bLr3TnW2yJhKd5Fs6Gc9A\",\"message\":{\"content\":\"hello\"}}\n",
    );
    r.agit(&["-a", "add", "-A"]);
    let (code, _out, err) = r.agit(&["-a", "commit", "-m", "real claude-code session"]);
    assert_eq!(code, 0, "bridgeSessionId must not be flagged as a secret: {err}");
    assert_eq!(r.git_agent(&["log", "-1", "--format=%s"]), "real claude-code session");
}

/// Legibility: a blocked commit must say IN WORDS that no commit was created, so the snap→commit→push
/// flow never looks like success when the gate actually refused.
#[test]
fn blocked_commit_states_no_commit_was_created() {
    let r = Repo::new();
    std::fs::remove_file(r.agent().join(".git/hooks/pre-commit")).ok();
    r.write_agent(
        "sessions/claude-code/s.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n",
    );
    r.agit(&["-a", "add", "-A"]);
    let (code, _out, err) = r.agit(&["-a", "commit", "-m", "leak"]);
    assert_ne!(code, 0, "a staged secret must block: {err}");
    assert!(err.contains("No commit created"), "the block must state nothing was committed: {err}");
}

/// The push gate: `agit a push` scans the store tree before touching the remote, so a committed secret
/// cannot be published even though the commit that carried it slipped in with `--no-verify`. Bare `git
/// push` fires only the pre-push hook, which `--no-verify` skips — agit's push must not.
#[test]
fn secret_blocked_through_agit_push_before_touching_the_remote() {
    let r = Repo::new();
    r.sh("git init -q --bare -b main hub.git");
    let hub_path = r.path().join("hub.git");
    let hub = hub_path.to_str().unwrap();
    assert_eq!(r.agit(&["a", "remote", "add", "origin", hub]).0, 0, "remote add");

    // Commit a secret into the store WITHOUT the gate — raw git, bypassing the hook — then try to push it.
    r.write_agent(
        "sessions/claude-code/s.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n",
    );
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "snuck a secret in"]);

    let (code, _out, err) = r.agit(&["a", "push", "-u", "origin", "main"]);
    assert_ne!(code, 0, "the push gate must refuse to publish a committed secret: {err}");
    assert!(err.contains("secret gate") || err.contains("aws"), "{err}");
    // The remote never received the branch: the gate ran BEFORE git push.
    assert!(
        r.git_at(&hub_path, &["rev-parse", "--verify", "refs/heads/main"]).is_empty(),
        "nothing may have reached the remote: {err}"
    );

    // The disclosed override publishes it — legibly.
    let (code, _o, err) = r.agit_env(&[("AGIT_ALLOW_SECRETS", "yes")], &["a", "push", "-u", "origin", "main"]);
    assert_eq!(code, 0, "the disclosed override must let the push through: {err}");
    assert!(err.contains("AGIT_ALLOW_SECRETS"), "the override must be disclosed: {err}");
}

/// The range gate is the whole point of scanning what's PUBLISHED, not the working tree: a secret
/// committed with a raw `git commit --no-verify` and then DELETED from the working tree leaves the tree
/// clean, but the committed blob still ships on push. A working-tree scan sees nothing; the range scan
/// reads the blob out of the pushed commit and blocks. The disclosed override still publishes it.
#[test]
fn committed_then_deleted_secret_is_caught_by_the_push_range_scan() {
    let r = Repo::new();
    r.sh("git init -q --bare -b main hub.git");
    let hub_path = r.path().join("hub.git");
    let hub = hub_path.to_str().unwrap();
    assert_eq!(r.agit(&["a", "remote", "add", "origin", hub]).0, 0, "remote add");

    // Sneak a secret in with a raw no-verify commit, then delete it from the working tree and commit the
    // deletion. The store's working tree is now clean — a whole-tree scan would find nothing.
    r.write_agent(
        "sessions/claude-code/leak.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n",
    );
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "snuck a secret in"]);
    std::fs::remove_file(r.agent().join("sessions/claude-code/leak.jsonl")).unwrap();
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "deleted it from the working tree"]);
    // Prove the working tree really is clean: only the whole-tree scan-clean case matters here.
    assert!(
        !r.agent().join("sessions/claude-code/leak.jsonl").exists(),
        "the secret file must be gone from the working tree"
    );

    // The range scan reads the committed blob out of the range being pushed and refuses.
    let (code, _out, err) = r.agit(&["a", "push", "-u", "origin", "main"]);
    assert_ne!(code, 0, "the range scan must catch a committed-then-deleted secret: {err}");
    assert!(err.contains("secret gate") || err.contains("aws"), "{err}");
    assert!(
        r.git_at(&hub_path, &["rev-parse", "--verify", "refs/heads/main"]).is_empty(),
        "nothing may have reached the remote: {err}"
    );

    // The disclosed override still publishes it — legibly.
    let (code, _o, err) = r.agit_env(&[("AGIT_ALLOW_SECRETS", "1")], &["a", "push", "-u", "origin", "main"]);
    assert_eq!(code, 0, "the disclosed override must let the push through: {err}");
    assert!(err.contains("AGIT_ALLOW_SECRETS"), "the override must be disclosed: {err}");
}

/// Regression: the range gate scans the SOURCE ref of the refspec being pushed, not always HEAD. A
/// secret on a NON-HEAD branch pushed by name (`agit a push origin leak`) must be caught, or a HEAD-only
/// scan would let it slip out.
#[test]
fn push_of_a_non_head_branch_scans_that_branch_not_head() {
    let r = Repo::new();
    r.sh("git init -q --bare -b main hub.git");
    let hub_path = r.path().join("hub.git");
    let hub = hub_path.to_str().unwrap();
    assert_eq!(r.agit(&["a", "remote", "add", "origin", hub]).0, 0, "remote add");
    assert_eq!(r.agit(&["a", "push", "-u", "origin", "main"]).0, 0, "clean main pushes");

    // A secret on a side branch that is NOT reachable from HEAD.
    r.git_agent(&["checkout", "-q", "-b", "leak"]);
    r.write_agent(
        "sessions/claude-code/s.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n",
    );
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "secret on a side branch"]);
    r.git_agent(&["checkout", "-q", "main"]); // HEAD is back on clean main; leak is non-HEAD

    // Pushing the side branch by name must scan `leak` (not HEAD) and refuse.
    let (code, _o, err) = r.agit(&["a", "push", "origin", "leak"]);
    assert_ne!(code, 0, "pushing a non-HEAD branch must scan THAT branch: {err}");
    assert!(err.contains("secret gate") || err.contains("aws"), "{err}");
    assert!(
        r.git_at(&hub_path, &["rev-parse", "--verify", "refs/heads/leak"]).is_empty(),
        "the secret branch must not have reached the remote: {err}"
    );
}

/// A clean committed range publishes with no friction: the range gate reads the new session blobs, finds
/// nothing, and the push reaches the remote.
#[test]
fn clean_committed_range_pushes_fine() {
    let r = Repo::new();
    r.sh("git init -q --bare -b main hub.git");
    let hub_path = r.path().join("hub.git");
    let hub = hub_path.to_str().unwrap();
    assert_eq!(r.agit(&["a", "remote", "add", "origin", hub]).0, 0, "remote add");

    r.write_agent(
        "sessions/claude-code/s.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"claude found the deadlock\"}}\n",
    );
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "a clean session"]);

    let (code, _out, err) = r.agit(&["a", "push", "-u", "origin", "main"]);
    assert_eq!(code, 0, "a clean committed range must push: {err}");
    assert!(
        !r.git_at(&hub_path, &["rev-parse", "--verify", "refs/heads/main"]).is_empty(),
        "the branch must have reached the remote"
    );
}

/// The range is what's NEW to the remote, not the whole history: a secret that was ALREADY published
/// must not re-block every later push. After the secret lands on the remote (via the disclosed override),
/// a subsequent CLEAN commit pushes without the gate re-scanning — and re-blocking on — the old history.
#[test]
fn already_pushed_secret_is_not_rescanned_on_the_next_push() {
    let r = Repo::new();
    r.sh("git init -q --bare -b main hub.git");
    let hub_path = r.path().join("hub.git");
    let hub = hub_path.to_str().unwrap();
    assert_eq!(r.agit(&["a", "remote", "add", "origin", hub]).0, 0, "remote add");

    // A secret commit, published with the disclosed override — the remote now HAS this history.
    r.write_agent(
        "sessions/claude-code/leak.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"key AKIAIOSFODNN7EXAMPLE\"}}\n",
    );
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "the secret, published once"]);
    let (code, _o, err) = r.agit_env(&[("AGIT_ALLOW_SECRETS", "1")], &["a", "push", "-u", "origin", "main"]);
    assert_eq!(code, 0, "the override must land the secret on the remote: {err}");

    // Now a clean commit on top. The range is only this new commit; the already-pushed secret is NOT in
    // it, so the push must NOT be blocked (a whole-history rescan would wrongly re-flag the old blob).
    r.write_agent(
        "sessions/claude-code/clean.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"just a normal follow-up\"}}\n",
    );
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "a clean follow-up"]);

    let (code, _out, err) = r.agit(&["a", "push", "origin", "main"]);
    assert_eq!(
        code, 0,
        "the next clean push must not be re-blocked by already-published history: {err}"
    );
    // Both commits are now on the remote.
    assert_eq!(
        r.git_at(&hub_path, &["log", "-1", "--format=%s", "main"]),
        "a clean follow-up",
        "the clean follow-up must have reached the remote"
    );
}

/// The pragma / allowlist escape stays intact through the wrapper gate — a false positive marked with
/// `agit:allow-secret` on the line still commits, exactly as it did through the git hook.
#[test]
fn inline_pragma_still_lets_a_flagged_line_commit_through_the_wrapper_gate() {
    let r = Repo::new();
    std::fs::remove_file(r.agent().join(".git/hooks/pre-commit")).ok();
    r.write_agent(
        "sessions/claude-code/s.jsonl",
        "{\"type\":\"user\",\"message\":{\"content\":\"doc example AKIAIOSFODNN7EXAMPLE agit:allow-secret\"}}\n",
    );
    r.agit(&["-a", "add", "-A"]);
    let (code, _o, err) = r.agit(&["-a", "commit", "-m", "flagged false positive"]);
    assert_eq!(code, 0, "a line carrying the allow pragma must not be gated: {err}");
    assert_eq!(r.git_agent(&["log", "-1", "--format=%s"]), "flagged false positive");
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

/// A session captured on ANOTHER path resumes HERE: agit installs it under the CURRENT environment
/// (rewriting the transcript's cwd), not the captured path, so `claude --resume` (which resolves a
/// session by the dir you run it in) can find it. Regression for the collaboration/merge "No
/// conversation found" break, where a teammate's session (their path) could not be resumed on your
/// machine because it installed under the capture path's slug.
#[test]
fn resume_installs_under_the_current_env_not_the_captured_cwd() {
    let r = Repo::new();
    r.agit(&["init", "--agent", "demo"]);
    let store = r.agit(&["a", "rev-parse", "--show-toplevel"]).1.trim().to_string();
    let foreign = "/home/someone-else/their-repo";
    let sdir = std::path::Path::new(&store).join("sessions/claude-code");
    std::fs::create_dir_all(&sdir).unwrap();
    let sess = sdir.join("teammate.jsonl");
    std::fs::write(
        &sess,
        format!("{{\"type\":\"user\",\"sessionId\":\"tm\",\"uuid\":\"u1\",\"cwd\":\"{foreign}\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n"),
    )
    .unwrap();
    let (code, out, err) = r.agit(&["resume", sess.to_str().unwrap(), "--as", "claude-code"]);
    assert_eq!(code, 0, "resume should succeed: {err}{out}");
    let installed = installed_claude_sessions(&r);
    let here = r.path().to_string_lossy().to_string();
    assert!(installed.contains(&here), "the transcript cwd is rewritten to the current env, not the capture path: {}", &installed[..installed.len().min(300)]);
    assert!(!installed.contains(foreign), "the captured foreign cwd must be gone after resume here: {}", &installed[..installed.len().min(300)]);
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

// ───────────── `agit a snap` mirrors AND commits (gated), in one step ─────────────

/// The behaviour change: a manual `agit a snap` of a clean session now COMMITS what it mirrored,
/// matching the watch daemon. Before, snap only mirrored and printed a separate commit hint; now the
/// store's HEAD advances with an `auto-snap` commit and the tree is left clean.
#[test]
fn snap_of_a_clean_session_commits_what_it_mirrored() {
    let r = Repo::new();
    let home = r.path().join("cchome");
    seed_claude_session(&r, &home, "clean1", "honest work");

    let before = r.git_agent(&["rev-parse", "HEAD"]);
    let (code, out, err) = r.agit_env(&[("HOME", home.to_str().unwrap())], &["a", "snap"]);
    assert_eq!(code, 0, "snap must succeed: {err}{out}");

    let after = r.git_agent(&["rev-parse", "HEAD"]);
    assert_ne!(before, after, "snap must create a commit — HEAD must advance: {out}{err}");
    let subject = r.git_agent(&["log", "-1", "--format=%s"]);
    assert!(subject.starts_with("auto-snap claude-code"), "the commit must be the auto-snap: {subject}");
    // Committed, not merely mirrored: no separate `agit a commit` step is left dangling.
    assert!(r.captured("claude-code", "clean1.jsonl"), "the session must be mirrored: {out}");
    assert!(
        r.git_agent(&["status", "--porcelain"]).is_empty(),
        "snap must leave a clean tree, not staged/untracked work waiting for a commit: {out}"
    );
    // And it must NOT print the old mirror-only commit hint.
    assert!(!out.contains("agit a commit"), "snap no longer tells the user to commit separately: {out}");
}

/// A snap of a session that trips the secret gate must NOT commit: the dump is mirrored to disk and
/// the block is disclosed, but held out of history — exactly commit_snap's contract. And the snap must
/// EXIT NON-ZERO, so a scripted `snap && push` (and `set -e`) stops here rather than pushing a store
/// whose latest capture was held back — consistent with `agit a scan`, which exits non-zero on the same
/// secret. The disclosed AGIT_ALLOW_SECRETS override then commits it (and restores exit 0).
#[test]
fn snap_of_a_secret_session_is_mirrored_but_not_committed() {
    let r = Repo::new();
    let home = r.path().join("cchome");
    seed_claude_session(&r, &home, "leak1", "key AKIAIOSFODNN7EXAMPLE");

    let before = r.git_agent(&["rev-parse", "HEAD"]);
    let (code, _out, err) = r.agit_env(&[("HOME", home.to_str().unwrap())], &["a", "snap"]);
    assert_ne!(code, 0, "a blocked snap must exit non-zero so `snap && push` cannot proceed: {err}");
    assert_eq!(
        r.git_agent(&["rev-parse", "HEAD"]),
        before,
        "a suspected secret must NOT be committed — HEAD must not move: {err}"
    );
    assert!(err.contains("not committed"), "the block must be disclosed in words: {err}");
    assert!(r.captured("claude-code", "leak1.jsonl"), "the dump is still mirrored to disk regardless: {err}");

    // The disclosed override commits it.
    let (code, _o, err) =
        r.agit_env(&[("HOME", home.to_str().unwrap()), ("AGIT_ALLOW_SECRETS", "1")], &["a", "snap"]);
    assert_eq!(code, 0, "override snap must succeed: {err}");
    assert_ne!(
        r.git_agent(&["rev-parse", "HEAD"]),
        before,
        "AGIT_ALLOW_SECRETS=1 must let the held-back commit through: {err}"
    );
    assert!(r.git_agent(&["log", "-1", "--format=%s"]).starts_with("auto-snap claude-code"));
}

/// Idempotent: re-snapping with nothing new must make NO commit. The store's coalesce (nothing staged
/// → no-op) means snap never writes an empty commit.
#[test]
fn snap_with_nothing_changed_makes_no_new_commit() {
    let r = Repo::new();
    let home = r.path().join("cchome");
    seed_claude_session(&r, &home, "once", "work");

    assert_eq!(r.agit_env(&[("HOME", home.to_str().unwrap())], &["a", "snap"]).0, 0, "first snap");
    let head = r.git_agent(&["rev-parse", "HEAD"]);

    // Second snap, same dump, nothing new to capture.
    let (code, out, err) = r.agit_env(&[("HOME", home.to_str().unwrap())], &["a", "snap"]);
    assert_eq!(code, 0, "a no-op snap must still exit 0: {err}{out}");
    assert_eq!(
        r.git_agent(&["rev-parse", "HEAD"]),
        head,
        "a nothing-changed snap must not create an empty commit: {out}"
    );
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

/// A bare `agit a push` fans out to EVERY configured remote (a shared origin + a personal hub), records
/// both into the committed binding with origin primary, and a per-remote failure is reported but never
/// aborts the others — the command's exit follows the PRIMARY (origin) push.
#[test]
fn push_fans_out_to_all_remotes_and_a_failure_is_non_fatal() {
    let r = Repo::new();

    // Two bare "hubs": origin (the shared anchor) and hub (a personal central hub).
    r.sh("git init -q --bare -b main origin.git");
    r.sh("git init -q --bare -b main hub.git");
    let origin = r.path().join("origin.git");
    let hub = r.path().join("hub.git");
    assert_eq!(r.agit(&["a", "remote", "add", "origin", origin.to_str().unwrap()]).0, 0);
    assert_eq!(r.agit(&["a", "remote", "add", "hub", hub.to_str().unwrap()]).0, 0);

    // Bare push → fan out. Both bare repos must receive the store's `main`.
    let (code, out, err) = r.agit(&["a", "push"]);
    assert_eq!(code, 0, "the fan-out push must succeed on the primary: {err}");
    assert!(out.contains("pushed origin") && out.contains("pushed hub"), "both remotes reported: {out}");
    // The fan-out announces each target by name BEFORE pushing, so a multi-remote push is never a surprise.
    assert!(out.contains("pushing to origin") && out.contains("pushing to hub"), "each target announced: {out}");
    let head = r.git_agent(&["rev-parse", "HEAD"]);
    assert_eq!(r.git_at(&origin, &["rev-parse", "main"]), head, "origin must have the branch");
    assert_eq!(r.git_at(&hub, &["rev-parse", "main"]), head, "hub must have the branch");

    // The binding records BOTH remotes, origin marked primary (sub-table form for 2+ remotes).
    let binding = std::fs::read_to_string(r.path().join(".agit.toml")).unwrap();
    assert!(binding.contains("[[agent.remote]]"), "multi-remote uses sub-tables:\n{binding}");
    assert!(binding.contains("name    = \"origin\""), "origin recorded:\n{binding}");
    assert!(binding.contains("name    = \"hub\""), "hub recorded:\n{binding}");
    assert!(binding.contains("primary = true"), "the anchor is marked primary:\n{binding}");

    // Repoint hub at a path that cannot receive a push, add a commit, and fan out again: hub's push is
    // rejected (reported, not fatal), origin still gets the commit, and the command still exits 0.
    assert_eq!(r.agit(&["a", "remote", "set-url", "hub", "/nonexistent/nope.git"]).0, 0);
    r.git_agent(&["commit", "--allow-empty", "--no-verify", "-m", "another session"]);
    let head2 = r.git_agent(&["rev-parse", "HEAD"]);
    let (code, out, err) = r.agit(&["a", "push"]);
    assert_eq!(code, 0, "a non-primary rejection must NOT change the exit (origin still succeeded): {err}");
    assert!(out.contains("pushed origin"), "origin still pushed: {out}");
    assert!(err.contains("push to hub rejected") && err.contains("not fatal"), "hub failure is reported, not fatal: {err}");
    assert_eq!(r.git_at(&origin, &["rev-parse", "main"]), head2, "origin advanced despite hub failing");

    // `--to <name>` targets a single remote. Give origin an upstream first (git-style passthrough,
    // route (b) — preserved verbatim), then a targeted push to it is the command's anchor and does NOT
    // fan out (no per-remote "pushed …" lines).
    assert_eq!(r.agit(&["a", "push", "-u", "origin", "main"]).0, 0, "route (b) passthrough sets origin upstream");
    let (code, out, err) = r.agit(&["a", "push", "--to", "origin"]);
    assert_eq!(code, 0, "a targeted push to a reachable remote must succeed: {err}");
    assert!(!out.contains("pushed origin"), "a targeted push must not fan out: {out}");

    // `--to <name> <refspec…>` must PASS the positional refspec through to the targeted push — not drop
    // it. A `HEAD:refs/heads/review` refspec creates a `review` branch on origin that a bare
    // `git push origin` (upstream = main) would never create, so its existence proves the passthrough.
    let head3 = r.git_agent(&["rev-parse", "HEAD"]);
    let (code, _out, err) = r.agit(&["a", "push", "--to", "origin", "HEAD:refs/heads/review"]);
    assert_eq!(code, 0, "a targeted push with a refspec must succeed: {err}");
    assert_eq!(
        r.git_at(&origin, &["rev-parse", "review"]),
        head3,
        "the refspec must be forwarded — origin must now carry the `review` branch"
    );

    // `--to` given as the FINAL arg with no following value is a usage error, never a silent fall-through
    // to the fan-out (which would push to every remote — the opposite of a targeted push).
    let (code, _out, err) = r.agit(&["a", "push", "--to"]);
    assert_ne!(code, 0, "a bare --to must error, not fan out");
    assert!(err.contains("--to needs a remote name"), "the error must name the missing value: {err}");
}

/// `agit a push <url>` names ONLY a URL and NO refspec, against a fresh `main` with no upstream. It must
/// DEFAULT the refspec to the current branch (so git carries `main`), not fall through to a bare
/// `git push` that dies with 'The current branch main has no upstream branch'.
#[test]
fn push_url_with_no_refspec_pushes_the_current_branch() {
    let r = Repo::new();
    r.sh("git init -q --bare -b main hub.git");
    let hub_path = r.path().join("hub.git");
    let hub = hub_path.to_str().unwrap();

    // No `remote add` at all: the URL is the only positional, and there is no upstream.
    let (code, out, err) = r.agit(&["a", "push", hub]);
    assert_eq!(code, 0, "push <url> with no refspec must push the current branch: {out}{err}");
    assert!(!err.contains("no upstream"), "the default refspec must avoid git's no-upstream error: {err}");
    let head = r.git_agent(&["rev-parse", "HEAD"]);
    assert_eq!(r.git_at(&hub_path, &["rev-parse", "main"]), head, "the bare repo received the current branch");
}

/// `agit a push origin` (a remote, no refspec, no upstream) likewise defaults to the current branch; and
/// `-u` sets the upstream tracking ref while a plain push does not.
#[test]
fn push_remote_with_no_refspec_defaults_to_current_branch_and_u_sets_upstream() {
    // (1) Plain `push <remote>` with no refspec: pushes main, sets NO upstream.
    let r = Repo::new();
    r.sh("git init -q --bare -b main hub.git");
    let hub_path = r.path().join("hub.git");
    assert_eq!(r.agit(&["a", "remote", "add", "origin", hub_path.to_str().unwrap()]).0, 0, "remote add");
    let (code, out, err) = r.agit(&["a", "push", "origin"]);
    assert_eq!(code, 0, "push <remote> with no refspec must push the current branch: {out}{err}");
    assert!(!err.contains("no upstream"), "the default refspec must avoid git's no-upstream error: {err}");
    let head = r.git_agent(&["rev-parse", "HEAD"]);
    assert_eq!(r.git_at(&hub_path, &["rev-parse", "main"]), head, "the bare repo received main");
    assert!(r.git_agent(&["config", "--get", "branch.main.merge"]).is_empty(), "a plain push sets no upstream");

    // (2) `-u` sets the upstream tracking ref (still no refspec typed).
    let r2 = Repo::new();
    r2.sh("git init -q --bare -b main hub.git");
    let hub2 = r2.path().join("hub.git");
    assert_eq!(r2.agit(&["a", "remote", "add", "origin", hub2.to_str().unwrap()]).0, 0, "remote add");
    let (code, out, err) = r2.agit(&["a", "push", "-u", "origin"]);
    assert_eq!(code, 0, "push -u <remote> with no refspec must succeed: {out}{err}");
    assert_eq!(r2.git_agent(&["config", "--get", "branch.main.merge"]), "refs/heads/main", "-u set the upstream branch");
    assert_eq!(r2.git_agent(&["config", "--get", "branch.main.remote"]), "origin", "-u set the upstream remote");
}

/// A one-connection-per-request HTTP stub that answers EVERY request with 401: `/api/me` gets the
/// agit-hub anonymous shape (so the client positively identifies it as a hub AND sees the credential
/// rejected), and git's `/info/refs` fetch gets a plain 401 so `git push` fails with an auth error.
struct StubHub {
    addr: String,
    _handle: std::thread::JoinHandle<()>,
}

fn stub_hub() -> StubHub {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
            let (ctype, body) = if path.starts_with("/api/me") {
                ("application/json", r#"{"error":"not logged in"}"#)
            } else {
                ("text/plain", "credentials required")
            };
            let resp = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(resp.as_bytes());
        }
    });
    StubHub { addr, _handle: handle }
}

/// When a push to a positively-identified agit-hub FAILS because the credential is rejected, agit prints
/// a one-line hint pointing at the web `/tokens` page (the client cannot run the server-side
/// `agit-hub token add`). The hint fires ONLY on an auth failure to a hub — never on a non-hub failure.
#[test]
fn hub_auth_failure_prints_the_write_token_hint() {
    let hub = stub_hub();
    let r = Repo::new();
    let url = format!("http://{}/alice/frontend.git", hub.addr);
    assert_eq!(r.agit(&["a", "remote", "add", "origin", &url]).0, 0, "remote add");

    // GIT_TERMINAL_PROMPT=0 so git fails fast on the 401 instead of trying to prompt for a credential.
    let (code, out, err) = r.agit_env(&[("GIT_TERMINAL_PROMPT", "0")], &["a", "push", "origin"]);
    let all = format!("{out}{err}");
    assert_ne!(code, 0, "a push to a hub that rejects the credential must fail: {all}");
    assert!(all.contains("WRITE TOKEN"), "the hint names the write token: {all}");
    assert!(
        all.contains(&format!("http://{}/tokens", hub.addr)),
        "the hint points at the hub web /tokens page: {all}"
    );

    // A failure to a NON-hub target (a local path) must NOT print the hub hint.
    assert_eq!(r.agit(&["a", "remote", "set-url", "origin", "/nonexistent/nope.git"]).0, 0, "repoint origin");
    let (code, out, err) = r.agit_env(&[("GIT_TERMINAL_PROMPT", "0")], &["a", "push", "origin"]);
    let all = format!("{out}{err}");
    assert_ne!(code, 0, "the local push still fails");
    assert!(!all.contains("WRITE TOKEN"), "no hub hint for a non-hub failure: {all}");
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

/// A peer branch that is a DIFFERENT agent: same store, but its `agent.toml` carries another aid, so a
/// merge against it is DialogueOnly (no fuse). That isolates the merged-session capture from the git
/// fuse — the ONLY commit the merge makes is the merged session itself.
fn make_diverged_peer_branch(r: &Repo, peer_note: &str, local_note: &str) {
    // A base commit both sides fork from, so the merge is grounded in a shared ancestor.
    r.git_agent(&["commit", "--allow-empty", "--no-verify", "-m", "base"]);
    // The peer branch: a different agent (its own aid) carrying a diverged session HEAD does not have.
    r.git_agent(&["checkout", "-q", "-b", "peer"]);
    r.write_agent(
        "agent.toml",
        "[agent]\nid      = \"agt_peer-different-0001\"\nname    = \"peer-agent\"\ncreated = \"2026-01-01T00:00:00Z\"\n",
    );
    r.write_agent("sessions/claude-code/team.jsonl", &claude_session("TEAM", peer_note));
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "team session"]);
    // Back on main (our identity + our own diverged session).
    r.git_agent(&["checkout", "-q", "main"]);
    r.write_agent("sessions/claude-code/local.jsonl", &claude_session("LOCAL", local_note));
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "local session"]);
}

/// The newest session in the store by RECORDED recency (max sidecar `last_activity`), and its transcript
/// — the same thing `latest_session` resolves. Returns "" when no session carries a sidecar.
fn latest_recorded_session_text(r: &Repo) -> String {
    fn walk(dir: &Path, out: &mut Vec<(String, PathBuf)>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.file_name().map(|n| n.to_string_lossy().ends_with(".agit.json")).unwrap_or(false) {
                if let Ok(txt) = std::fs::read_to_string(&p) {
                    if let Some(la) = serde_json::from_str::<serde_json::Value>(&txt)
                        .ok()
                        .and_then(|v| v.get("last_activity").and_then(|x| x.as_str()).map(str::to_string))
                    {
                        out.push((la, p.with_extension("").with_extension("jsonl")));
                    }
                }
            }
        }
    }
    let mut found = Vec::new();
    walk(&r.agent().join("sessions"), &mut found);
    found.sort();
    found.last().map(|(_, p)| std::fs::read_to_string(p).unwrap_or_default()).unwrap_or_default()
}

/// After a successful `agit a merge --splice`, the reconciled MERGED session is captured into the store
/// and becomes the agent's LATEST/default — a merge produces a commit, git-style. No manual resume+snap:
/// the store's HEAD advances and the newest recorded session IS the merged one carrying both sides.
#[test]
fn a_merge_splice_captures_the_merged_session_as_the_latest_and_commits_it() {
    let r = Repo::new();
    make_diverged_peer_branch(&r, "team found the deadlock", "local fixed the parser");

    let head_before = r.git_agent(&["rev-parse", "HEAD"]);
    let (code, out, err) = r.agit(&["a", "merge", "peer", "--from", "claude-code", "--splice"]);
    assert_eq!(code, 0, "a clean splice merge must exit 0: {err}{out}");
    assert!(out.contains("splice (no model)"), "still the no-model path: {out}");

    // The merge produced a commit: HEAD advanced (DialogueOnly, so the only commit is the merged session).
    let head_after = r.git_agent(&["rev-parse", "HEAD"]);
    assert_ne!(head_before, head_after, "the merge must commit the merged session; HEAD must advance: {out}");

    // The newest recorded session IS the merged one, carrying BOTH sides — resume-ready as the default.
    let latest = latest_recorded_session_text(&r);
    assert!(latest.contains("local fixed the parser"), "latest carries the local side: {latest}");
    assert!(latest.contains("team found the deadlock"), "latest carries the peer side: {latest}");

    // And it is genuinely committed to history (tracked), not just written to the working tree.
    let tracked = r.git_agent(&["ls-files", "--", "sessions"]);
    let merged_committed = tracked.lines().any(|f| {
        f.ends_with(".jsonl")
            && r.git_agent(&["show", &format!("HEAD:{f}")]).contains("team found the deadlock")
    });
    assert!(merged_committed, "the merged session must be committed into the store: {tracked}");
}

/// Same-agent divergence (a `peer` branch that KEEPS this agent's aid) resolves to Fuse mode: the merge
/// must fast-forward the histories AND capture the merged session, exiting 0. Regression for an ordering
/// bug: capturing the merged session BEFORE the fuse advanced main and turned the fuse into a divergent
/// 3-way merge that exited 1. Only DialogueOnly splice (no fuse) was covered, so this path slipped.
#[test]
fn a_merge_splice_same_agent_fuses_and_exits_clean() {
    let r = Repo::new();
    // main: our own session.
    r.write_agent("sessions/claude-code/local.jsonl", &claude_session("LOCAL", "local fixed the parser"));
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "local session"]);
    // A `peer` branch that KEEPS the same agent.toml (same aid, so Fuse) and adds its own session,
    // branched here so it is strictly ahead of main.
    r.git_agent(&["checkout", "-q", "-b", "peer"]);
    r.write_agent("sessions/claude-code/team.jsonl", &claude_session("TEAM", "peer found the deadlock"));
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "peer session"]);
    r.git_agent(&["checkout", "-q", "main"]);

    let head_before = r.git_agent(&["rev-parse", "HEAD"]);
    let (code, out, err) = r.agit(&["a", "merge", "peer", "--from", "claude-code", "--splice"]);
    assert_eq!(code, 0, "a same-agent (Fuse) splice merge must exit 0, not conflict on its own capture: {err}{out}");
    assert!(out.to_lowercase().contains("same agent") || out.to_lowercase().contains("fus"), "must be Fuse mode: {out}");
    let head_after = r.git_agent(&["rev-parse", "HEAD"]);
    assert_ne!(head_before, head_after, "HEAD must advance (fuse + the merged-session commit): {out}");
    let latest = latest_recorded_session_text(&r);
    assert!(
        latest.contains("local fixed the parser") && latest.contains("peer found the deadlock"),
        "the latest session is the merged one carrying both sides: {latest}"
    );
}

/// The merge writes EXACTLY the one merged session into the store, never a blanket snap. The runtime
/// install (`~/.claude/projects`) and, on the dialogue path, the temporary revived A/B copies must NOT
/// leak into the store. DialogueOnly here, so the peer's own `team.jsonl` also stays out of main.
#[test]
fn a_merge_splice_captures_only_the_merged_session_not_the_runtime_or_the_peer() {
    let r = Repo::new();
    make_diverged_peer_branch(&r, "team found the deadlock", "local fixed the parser");

    let (code, _out, err) = r.agit(&["a", "merge", "peer", "--from", "claude-code", "--splice"]);
    assert_eq!(code, 0, "splice must succeed: {err}");

    let tracked = r.git_agent(&["ls-files", "--", "sessions"]);
    // Exactly ONE partitioned merged session was added on top of our own pre-existing flat local session.
    let partitioned: Vec<&str> = tracked
        .lines()
        .filter(|f| f.ends_with(".jsonl") && !f.starts_with("sessions/claude-code/"))
        .collect();
    assert_eq!(partitioned.len(), 1, "exactly one merged session must be captured, not a blanket snap: {tracked}");
    // The peer is a different agent (DialogueOnly): its team.jsonl must not be fused into main's history.
    assert!(
        !tracked.contains("team.jsonl"),
        "DialogueOnly must keep the peer's own session out of this agent's store: {tracked}"
    );
}

/// The merge's secret gate is IDENTICAL to snap's: a merged transcript carrying a secret is mirrored to
/// disk but held OUT of history, the merge exits non-zero, and the store's HEAD does not advance for it.
#[test]
fn a_merge_splice_gates_a_merged_secret_out_of_history() {
    let r = Repo::new();
    // The peer's session carries a real AWS key; spliced into the merged session, committing it to main
    // would introduce the secret to this agent's history — the gate must refuse.
    make_diverged_peer_branch(&r, "aws key AKIAIOSFODNN7EXAMPLE from the peer", "local fixed the parser");

    let head_before = r.git_agent(&["rev-parse", "HEAD"]);
    let (code, out, err) = r.agit(&["a", "merge", "peer", "--from", "claude-code", "--splice"]);
    assert_ne!(code, 0, "a merged transcript carrying a secret must fail the merge: {out}");
    assert!(err.contains("secret") || err.contains("suspected") || err.contains("AKIA"), "it discloses the block: {err}{out}");

    // HEAD did not advance: the merged session was kept out of history.
    assert_eq!(r.git_agent(&["rev-parse", "HEAD"]), head_before, "the store's HEAD must not advance for a gated merge");
    let tracked = r.git_agent(&["ls-files", "--", "sessions"]);
    let leaked = tracked.lines().any(|f| f.ends_with(".jsonl") && r.git_agent(&["show", &format!("HEAD:{f}")]).contains("AKIAIOSFODNN7EXAMPLE"));
    assert!(!leaked, "the secret must not be committed anywhere in the store: {tracked}");
}

/// `--dry-run` is a pure preview: it reports what the merge WOULD do (target, mode, each side's sessions,
/// whether the histories would fuse) and returns 0 WITHOUT spending the model, installing a session,
/// writing a transcript/ledger, or fusing the histories. Proven end to end — no `claude`/`codex` is
/// installed (a real dialogue merge would demand one), the store gains no `sessions/sync` dir, no session
/// is installed, and main's tip is untouched (no fuse commit).
#[test]
fn a_merge_dry_run_previews_without_spending_the_model_or_touching_the_store() {
    let r = Repo::new();

    // A base commit both sides fork from, so the merge is grounded in a shared ancestor.
    r.git_agent(&["commit", "--allow-empty", "--no-verify", "-m", "base"]);

    // A peer branch carrying a diverged Claude session that HEAD does not have.
    r.git_agent(&["checkout", "-q", "-b", "peer"]);
    r.write_agent("sessions/claude-code/team.jsonl", &claude_session("TEAM", "team found the deadlock"));
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "team session"]);

    // Back on main, our own diverged Claude session. main keeps this exact tip through a dry run.
    r.git_agent(&["checkout", "-q", "main"]);
    r.write_agent("sessions/claude-code/local.jsonl", &claude_session("LOCAL", "local fixed the parser"));
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "local session"]);
    let main_before = r.git_agent(&["rev-parse", "main"]);

    let (code, out, err) = r.agit(&["a", "merge", "peer", "--from", "claude-code", "--dry-run"]);
    assert_eq!(code, 0, "dry-run must exit 0 with no model and no claude/codex installed: {err}{out}");
    assert!(out.contains("dry run"), "it banners the preview: {out}");
    assert!(out.to_lowercase().contains("no model"), "it promises no model spend: {out}");
    assert!(out.contains("peer"), "it names the target: {out}");
    assert!(out.contains("session(s)"), "it reports each side's session inventory: {out}");

    // Nothing was installed: a real merge would install revived sessions under ~/.claude/projects.
    assert!(installed_claude_sessions(&r).is_empty(), "a dry run must install no session: it spends nothing");
    // No transcript and no decision ledger were written — both would land in <store>/sessions/sync/.
    assert!(!r.agent().join("sessions/sync").exists(), "a dry run must write no transcript or ledger");
    // And the histories were NOT fused, even though this is the same agent (Fuse mode).
    assert_eq!(r.git_agent(&["rev-parse", "main"]), main_before, "main's tip must be untouched: no fuse commit");
    // The peer branch is likewise untouched, and no stray worktree was left behind.
    assert!(r.git_agent(&["worktree", "list"]).lines().count() <= 1, "a dry run must leave no worktree behind");
}

/// `--preview` is the spelled-out alias for `--dry-run`: same pure preview, same exit 0, same no-op.
#[test]
fn a_merge_preview_is_an_alias_for_dry_run() {
    let r = Repo::new();
    r.git_agent(&["commit", "--allow-empty", "--no-verify", "-m", "base"]);
    r.git_agent(&["checkout", "-q", "-b", "peer"]);
    r.write_agent("sessions/claude-code/team.jsonl", &claude_session("TEAM", "peer work"));
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "team session"]);
    r.git_agent(&["checkout", "-q", "main"]);
    r.write_agent("sessions/claude-code/local.jsonl", &claude_session("LOCAL", "local work"));
    r.git_agent(&["add", "-A"]);
    r.git_agent(&["commit", "--no-verify", "-m", "local session"]);

    let (code, out, err) = r.agit(&["a", "merge", "peer", "--from", "claude-code", "--preview"]);
    assert_eq!(code, 0, "--preview must behave exactly like --dry-run: {err}{out}");
    assert!(out.contains("dry run"), "the alias reaches the same preview: {out}");
    assert!(!r.agent().join("sessions/sync").exists(), "the alias installs and writes nothing either");
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
// ─────────────────────────── provenance: sessions tied to their producer ───────────────────────────

/// Seed a claude transcript this project owns, into `<home>/.claude/projects/<slug>/`.
fn seed_claude_session(r: &Repo, home: &Path, id: &str, msg: &str) {
    let top = r.sh("git rev-parse --show-toplevel").trim().to_string();
    let slug: String = top.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    let sdir = home.join(".claude/projects").join(&slug);
    std::fs::create_dir_all(&sdir).unwrap();
    std::fs::write(
        sdir.join(format!("{id}.jsonl")),
        format!("{{\"type\":\"user\",\"sessionId\":\"{id}\",\"cwd\":\"{top}\",\"message\":{{\"content\":\"{msg}\"}}}}\n"),
    )
    .unwrap();
}

/// The captured transcript on disk in the store, under either layout.
fn captured_transcript(r: &Repo, rt: &str, file: &str) -> PathBuf {
    fn walk(dir: &Path, want: &Path) -> Option<PathBuf> {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.ends_with(want) {
                return Some(p);
            }
            if p.is_dir() {
                if let Some(found) = walk(&p, want) {
                    return Some(found);
                }
            }
        }
        None
    }
    walk(&r.agent().join("sessions"), &PathBuf::from(rt).join(file))
        .unwrap_or_else(|| panic!("{rt}/{file} was not captured into the store"))
}

/// End to end: snap signs the captured session into its committed sidecar, and `provenance verify`
/// confirms the signature. This is the whole client-side loop — sign on capture, self-verify later.
#[test]
fn a_snapped_session_is_signed_and_verifies() {
    let r = Repo::new();
    let home = r.path().join("cchome");
    seed_claude_session(&r, &home, "sess1", "provenance work");

    let (code, _out, err) = r.agit_env(&[("HOME", home.to_str().unwrap())], &["a", "snap"]);
    assert_eq!(code, 0, "snap must succeed: {err}");

    let transcript = captured_transcript(&r, "claude-code", "sess1.jsonl");
    // The sidecar beside it must carry a provenance block with a pubkey and signature.
    let sidecar = transcript.with_extension("agit.json");
    let side = std::fs::read_to_string(&sidecar).expect("sidecar must exist");
    assert!(side.contains("\"provenance\""), "the sidecar must carry provenance: {side}");
    assert!(side.contains("\"sig\""), "provenance must carry a signature: {side}");
    assert!(side.contains("\"pubkey\""), "provenance must carry a pubkey: {side}");

    let (code, out, err) = r.agit(&["provenance", "verify", transcript.to_str().unwrap()]);
    assert_eq!(code, 0, "a signed, intact session must verify (exit 0): {err}{out}");
    assert!(out.contains("verified"), "must report verified: {out}");
    assert!(!out.contains("UNVERIFIED"), "an intact session is not a failure: {out}");
}

/// Tamper the committed transcript after it was signed: verification must fail loudly (non-zero) and
/// name the reason as a content change, not silently pass.
#[test]
fn a_tampered_session_fails_verification_end_to_end() {
    let r = Repo::new();
    let home = r.path().join("cchome");
    seed_claude_session(&r, &home, "sess2", "honest work");
    assert_eq!(r.agit_env(&[("HOME", home.to_str().unwrap())], &["a", "snap"]).0, 0);

    let transcript = captured_transcript(&r, "claude-code", "sess2.jsonl");
    // Edit the stored transcript — the signature was over its original bytes.
    let mut content = std::fs::read_to_string(&transcript).unwrap();
    content.push_str("{\"type\":\"user\",\"message\":{\"content\":\"forged line\"}}\n");
    std::fs::write(&transcript, content).unwrap();

    let (code, out, _err) = r.agit(&["provenance", "verify", transcript.to_str().unwrap()]);
    assert_eq!(code, 1, "a tampered session must fail verification: {out}");
    assert!(out.contains("UNVERIFIED"), "must report the failure: {out}");
    assert!(out.contains("tampered") || out.contains("changed"), "must name the reason: {out}");
}

/// A session with no signature (no sidecar provenance) degrades to "unverified" and exit 0 — it must
/// never panic or block, mirroring the attribution fallback contract.
#[test]
fn an_unsigned_session_is_unverified_never_blocked() {
    let r = Repo::new();
    // A bare transcript on disk, with no sidecar at all.
    let loose = r.path().join("loose.jsonl");
    std::fs::write(&loose, "{\"type\":\"user\",\"message\":{\"content\":\"hi\"}}\n").unwrap();

    let (code, out, err) = r.agit(&["provenance", "verify", loose.to_str().unwrap()]);
    assert_eq!(code, 0, "an unsigned session must not block (exit 0): {err}{out}");
    assert!(out.contains("unverified"), "must report unverified: {out}");
    assert!(out.contains("no signature"), "must say why: {out}");
}

/// `agit provenance key` mints (once) and prints this machine's public key.
#[test]
fn provenance_key_prints_a_stable_public_key() {
    let r = Repo::new();
    let (code, out, err) = r.agit(&["provenance", "key"]);
    assert_eq!(code, 0, "key must succeed: {err}");
    assert!(out.contains("pubkey"), "must print the public key: {out}");
    // Minted once: a second call prints the same key.
    let (_, out2, _) = r.agit(&["provenance", "key"]);
    assert_eq!(out, out2, "the machine key must be stable across calls");
}

// ─────────────── unified session selector: default to the active agent, or name an agent ───────────────

/// The client-side session resolver, exercised through `agit convert`/`resume`: a file path and a
/// session id are UNCHANGED, an agent NAME picks that agent's latest, NO selector picks the active
/// agent's latest, and an unknown selector is a clear error. No UUID hunting required.
#[test]
fn convert_and_resume_default_to_active_latest_and_accept_agent_names() {
    let r = Repo::new(); // active agent = testmemory

    // testmemory gets a session of its own.
    let home1 = r.path().join("home1");
    seed_claude_session(&r, &home1, "mainsess", "testmemory work");
    assert_eq!(r.agit_env(&[("HOME", home1.to_str().unwrap())], &["a", "snap"]).0, 0, "snap testmemory");

    // frontend: a second agent this machine knows, with its own distinct session.
    assert_eq!(r.agit(&["a", "init", "frontend"]).0, 0, "init frontend");
    assert_eq!(r.agit(&["a", "switch", "frontend"]).0, 0, "switch to frontend");
    let home2 = r.path().join("home2");
    seed_claude_session(&r, &home2, "frontsess", "frontend work");
    assert_eq!(r.agit_env(&[("HOME", home2.to_str().unwrap())], &["a", "snap"]).0, 0, "snap frontend");
    assert_eq!(r.agit(&["a", "switch", "testmemory"]).0, 0, "back to testmemory as active");

    // (d) No selector -> the ACTIVE agent's latest (testmemory -> mainsess), not a usage error.
    let (code, out, err) = r.agit(&["convert", "--to", "codex"]);
    assert_eq!(code, 0, "bare convert resolves the active agent's latest: {err}{out}");
    assert!(out.contains("mainsess"), "converts testmemory's latest session: {out}");

    // (c) A known agent NAME -> that agent's latest (frontend -> frontsess).
    let (code, out, err) = r.agit(&["convert", "frontend", "--to", "codex"]);
    assert_eq!(code, 0, "agent-name convert resolves that agent's latest: {err}{out}");
    assert!(out.contains("frontsess"), "converts frontend's latest session: {out}");

    // (a) A session id in the active agent's store -> that session (unchanged).
    let (code, out, err) = r.agit(&["convert", "mainsess", "--to", "codex"]);
    assert_eq!(code, 0, "session-id convert unchanged: {err}{out}");
    assert!(out.contains("mainsess"), "converts the named session: {out}");

    // (b) A file path -> that transcript (unchanged).
    let loose = r.path().join("loose.jsonl");
    std::fs::write(
        &loose,
        "{\"type\":\"user\",\"sessionId\":\"loose\",\"cwd\":\"/tmp\",\"message\":{\"content\":\"hi\"}}\n",
    )
    .unwrap();
    let (code, out, err) = r.agit(&["convert", loose.to_str().unwrap(), "--to", "codex"]);
    assert_eq!(code, 0, "explicit-path convert unchanged: {err}{out}");
    assert!(out.contains("loose.jsonl"), "converts the explicit file: {out}");

    // (e) An unknown selector -> a clear error naming what was tried.
    let (code, _out, err) = r.agit(&["convert", "nonesuch", "--to", "codex"]);
    assert_ne!(code, 0, "an unknown selector is an error, not a silent default");
    assert!(err.contains("no session or agent `nonesuch`"), "names what was tried: {err}");

    // resume mirrors it: no positional resolves the active agent's latest (not a usage error) ...
    let (code, out, err) = r.agit(&["resume"]);
    assert_eq!(code, 0, "bare resume resolves the active agent's latest: {err}{out}");
    assert!(out.contains("Resume:"), "prints a resume command: {out}");
    // ... and an agent name resolves that agent's latest.
    let (code, out, err) = r.agit(&["resume", "frontend"]);
    assert_eq!(code, 0, "resume by agent name: {err}{out}");
    assert!(out.contains("Resume:"), "prints a resume command: {out}");
}

/// `agit provenance verify`: no argument verifies the ACTIVE agent's latest session; an agent NAME
/// verifies EVERY session that agent has (whole-agent mode) and returns non-zero if any is unverified.
#[test]
fn provenance_verify_defaults_to_active_and_verifies_a_whole_agent_by_name() {
    let r = Repo::new(); // active agent = testmemory
    let home1 = r.path().join("home1");
    seed_claude_session(&r, &home1, "mainsess", "testmemory work");
    assert_eq!(r.agit_env(&[("HOME", home1.to_str().unwrap())], &["a", "snap"]).0, 0, "snap testmemory");

    // No argument -> verify the active agent's latest (snap signed it, so it verifies, exit 0).
    let (code, out, err) = r.agit(&["provenance", "verify"]);
    assert_eq!(code, 0, "no-arg verify checks the active agent's latest and passes: {err}{out}");
    assert!(out.contains("mainsess"), "verifies the active agent's latest session: {out}");
    assert!(out.contains("verified"), "a snapped session verifies: {out}");

    // A second agent with two sessions of its own.
    assert_eq!(r.agit(&["a", "init", "frontend"]).0, 0, "init frontend");
    assert_eq!(r.agit(&["a", "switch", "frontend"]).0, 0, "switch to frontend");
    let home2 = r.path().join("home2");
    seed_claude_session(&r, &home2, "frontA", "frontend work A");
    seed_claude_session(&r, &home2, "frontB", "frontend work B");
    assert_eq!(r.agit_env(&[("HOME", home2.to_str().unwrap())], &["a", "snap"]).0, 0, "snap frontend");
    // Grab one of frontend's committed transcripts while frontend is the active store, to tamper later.
    let front_b = captured_transcript(&r, "claude-code", "frontB.jsonl");
    assert_eq!(r.agit(&["a", "switch", "testmemory"]).0, 0, "back to testmemory as active");

    // A known agent NAME -> verify ALL of that agent's sessions; all intact -> exit 0.
    let (code, out, err) = r.agit(&["provenance", "verify", "frontend"]);
    assert_eq!(code, 0, "whole-agent verify passes when every session verifies: {err}{out}");
    assert!(out.contains("agent `frontend`"), "names the agent: {out}");
    assert!(out.contains("frontA") && out.contains("frontB"), "lists every session: {out}");

    // Tamper one of frontend's sessions after it was signed: whole-agent verify must now fail loudly.
    let mut content = std::fs::read_to_string(&front_b).unwrap();
    content.push_str("{\"type\":\"user\",\"message\":{\"content\":\"forged line\"}}\n");
    std::fs::write(&front_b, content).unwrap();

    let (code, out, _err) = r.agit(&["provenance", "verify", "frontend"]);
    assert_ne!(code, 0, "one tampered session makes the whole-agent verify fail: {out}");
    assert!(out.contains("FAIL"), "flags the bad session in the per-session verdict: {out}");
}

// ─────────────────────── agit identity register: the OFFLINE paste flow (idpaste wave) ───────────────────────

/// The first `{`-prefixed line of `identity register`'s output, parsed as JSON. That line is the compact,
/// paste-able enroll block; the rest is the human instruction.
fn register_block(out: &str) -> serde_json::Value {
    let line = out.lines().find(|l| l.trim_start().starts_with('{')).unwrap_or_else(|| panic!("a JSON block line in: {out}"));
    serde_json::from_str(line).unwrap_or_else(|_| panic!("the register block is valid JSON: {line}"))
}

/// `agit identity register <you>` is OFFLINE and prints a block whose `enroll_sig` VERIFIES over
/// (username ‖ epoch ‖ ed25519_pub ‖ x25519_pub) against the printed `ed25519_pub` — the exact bytes the
/// hub re-derives — so the web paste carries a real possession proof. The signature is BOUND to the named
/// account: re-derived under a different username it must NOT verify.
#[test]
fn identity_register_prints_a_verifiable_enroll_block() {
    let r = Repo::new();
    let (code, out, err) = r.agit(&["identity", "register", "alice"]);
    assert_eq!(code, 0, "register must succeed with no network: {err}");
    let v = register_block(&out);
    let ed = v["ed25519_pub"].as_str().expect("ed25519_pub");
    let x = v["x25519_pub"].as_str().expect("x25519_pub");
    let epoch = v["epoch"].as_i64().expect("epoch is an integer");
    let sig = v["enroll_sig"].as_str().expect("enroll_sig");
    assert!(!v["label"].as_str().unwrap_or_default().is_empty(), "a device label defaults to the hostname: {out}");

    // Round-trip: re-derive the signed bytes and verify against the PRINTED public key.
    let msg = agit::agent::identity_enroll_message("alice", epoch, ed, x);
    assert!(
        agit::agent::verify_hex(ed, &msg, sig),
        "enroll_sig must verify over (username‖epoch‖ed‖x) against the printed ed25519_pub"
    );
    // The block is bound to the account it was signed for: another username breaks the signature.
    let wrong = agit::agent::identity_enroll_message("mallory", epoch, ed, x);
    assert!(!agit::agent::verify_hex(ed, &wrong, sig), "a block signed for `alice` must not verify under another account");

    // It points the user at the web paste, and makes NO network call (no hub is configured here, yet code == 0).
    assert!(out.contains("paste this into the hub"), "prints the paste instruction: {out}");
}

/// The printed block is safe to paste in the clear: it carries ONLY the public key halves, the epoch, the
/// signature and a label — never any private key material. The machine's on-disk private seed must appear
/// nowhere in the output.
#[test]
fn identity_register_block_has_no_private_key_material() {
    let r = Repo::new();
    let (code, out, err) = r.agit(&["identity", "register", "alice"]);
    assert_eq!(code, 0, "{err}");
    let v = register_block(&out);
    let obj = v.as_object().expect("the block is a JSON object");
    let mut fields: Vec<&str> = obj.keys().map(|s| s.as_str()).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        ["ed25519_pub", "enroll_sig", "epoch", "label", "x25519_pub"],
        "only public halves + epoch + sig + label — no private field"
    );

    // The private ed25519 seed on disk must never be printed, in any field or anywhere in stdout.
    let priv_hex = std::fs::read_to_string(r.path().join("agit-home/identity/ed25519")).expect("the machine key was minted");
    let priv_hex = priv_hex.trim();
    assert!(!priv_hex.is_empty(), "the machine private key exists on disk");
    assert!(!out.contains(priv_hex), "the machine's private key must never appear in the printed block");
    for val in obj.values() {
        if let Some(s) = val.as_str() {
            assert_ne!(s, priv_hex, "no printed field may equal the private seed");
        }
    }
}

/// Re-running `register` yields a strictly HIGHER epoch (client-side monotonic), so a re-paste always
/// out-ranks the previous block and the hub's per-key monotonic-epoch check accepts the update.
#[test]
fn identity_register_epoch_is_monotonic_across_runs() {
    let r = Repo::new();
    let epoch_of = |r: &Repo| -> i64 {
        let (code, out, err) = r.agit(&["identity", "register", "alice"]);
        assert_eq!(code, 0, "{err}");
        register_block(&out)["epoch"].as_i64().expect("epoch")
    };
    let first = epoch_of(&r);
    let second = epoch_of(&r);
    assert!(second > first, "a re-print must produce a strictly higher epoch ({second} > {first})");
}

/// The token-requiring `agit identity enroll` is GONE: the subcommand no longer dispatches. It is now an
/// unknown-subcommand usage error (non-zero), never a hidden POST — and the usage advertises `register`.
#[test]
fn identity_enroll_command_is_removed() {
    let r = Repo::new();
    let (code, _out, err) = r.agit(&["identity", "enroll"]);
    assert_ne!(code, 0, "enroll must no longer be a valid subcommand");
    assert!(
        err.contains("unknown subcommand") && err.contains("enroll"),
        "reports `enroll` as unknown: {err}"
    );
    assert!(err.contains("register"), "the usage line points at the new register command: {err}");
}

// ─────────────────── git-parity: native parsers REJECT unknown dash flags ───────────────────

/// SECURITY (FIX 1): `agit a scan --no-verify` / `--bogus` must be REJECTED, not silently turned into a
/// scan of a nonexistent "file" that reports "no secrets found" at exit 0 — the old behaviour HID real
/// secrets. The unknown option is named, the exit is non-zero, and a plain scan still finds the secret,
/// proving the detector was never disabled (only the parser changed).
#[test]
fn scan_rejects_unknown_flags_instead_of_hiding_secrets() {
    let r = Repo::new();
    r.write_agent("sessions/claude-code/s.jsonl", "{\"content\":\"AKIAIOSFODNN7EXAMPLE\"}\n");

    for bad in ["--no-verify", "--bogus"] {
        let (code, out, err) = r.agit(&["a", "scan", bad]);
        let all = format!("{out}{err}");
        assert_ne!(code, 0, "`agit a scan {bad}` must be rejected, not treated as a file target: {all}");
        assert!(err.contains(&format!("unknown option '{bad}'")), "must name the unknown option: {all}");
        assert!(!all.contains("no secrets found"), "a rejected scan must NOT claim the tree is clean: {all}");
    }

    // The seeded secret is still there and still caught: a plain scan reports it and exits non-zero.
    let (code, _o, err) = r.agit(&["a", "scan"]);
    assert_ne!(code, 0, "the seeded secret must still be reported by a plain scan: {err}");
    assert!(err.contains("aws-access-key-id"), "{err}");
    // And the report no longer walks the user toward a nonexistent `--no-verify` bypass.
    assert!(!err.contains("use --no-verify"), "scan must not recommend a --no-verify bypass it does not have: {err}");
    assert!(!err.contains("bypass this hook"), "scan is a report, not a hook: {err}");
    assert!(err.contains("AGIT_ALLOW_SECRETS"), "the real, disclosed override must be named: {err}");
}

/// FIX 2: `agit init --help` (and `-h`) must PRINT usage, exit 0, and touch NOTHING. It used to EXECUTE
/// init — appending `.agit/` to the repo .gitignore and printing "agit is ready" — because the parser
/// ignored every arg but `--agent`. Run in a fresh repo that init has never touched, so the side effects
/// (a created .gitignore, a bound .agit.toml, the success banner) are observable by their absence.
#[test]
fn init_help_prints_usage_without_side_effects() {
    for flag in ["--help", "-h"] {
        let dir = tempfile::tempdir().unwrap();
        let r = Repo { dir };
        r.sh("git init -q -b main .");
        r.sh("git config user.name dev && git config user.email d@x.com && git config commit.gpgsign false");
        r.write("app.ts", "x\n");
        r.sh("git add -A && git commit -qm seed");

        let (code, out, err) = r.agit(&["init", flag]);
        assert_eq!(code, 0, "`agit init {flag}` must exit 0: {err}");
        assert!(out.contains("usage") && out.contains("agit init"), "must print init usage: {out}");
        let all = format!("{out}{err}");
        assert!(!all.contains("agit is ready"), "`agit init {flag}` must NOT execute init: {all}");
        assert!(!all.contains("appended to the code repo .gitignore"), "must not edit .gitignore: {all}");
        assert!(!r.path().join(".gitignore").exists(), "`agit init {flag}` must not create the .gitignore init writes");
        assert!(!r.path().join(".agit.toml").exists(), "`agit init {flag}` must not bind an agent");
    }
}

/// FIX 2: `agit convert <session> --to <rt> --wriet` (a typo of `--write`) must be REJECTED. The old
/// parser swallowed the unknown flag, so the convert silently DRY-RAN and persisted nothing while exiting
/// 0 — the user thinks they wrote a resumable session and did not.
#[test]
fn convert_rejects_unknown_flag_not_silent_dry_run() {
    let r = Repo::new();
    let (code, out, err) = r.agit(&["convert", "somesession", "--to", "claude-code", "--wriet"]);
    let all = format!("{out}{err}");
    assert_ne!(code, 0, "a misspelled `--wriet` must be rejected, not silently dry-run at exit 0: {all}");
    assert!(err.contains("unknown option '--wriet'"), "must name the unknown option: {all}");
}

/// FIX 2: the other native parsers reject an unknown dash flag rather than swallowing it. `snap
/// --no-harnes` must not silently keep the harness on; `merge --dry-runn` must not run a REAL merge where
/// a preview was asked for.
#[test]
fn snap_and_merge_reject_unknown_dash_flags() {
    let r = Repo::new();

    let (sc, so, se) = r.agit(&["a", "snap", "--no-harnes"]);
    assert_ne!(sc, 0, "snap must reject --no-harnes, not swallow it: {so}{se}");
    assert!(se.contains("unknown option '--no-harnes'"), "{so}{se}");

    let (mc, mo, me) = r.agit(&["a", "merge", "peer", "--dry-runn"]);
    assert_ne!(mc, 0, "merge must reject --dry-runn (a swallowed typo runs a REAL merge): {mo}{me}");
    assert!(me.contains("unknown option '--dry-runn'"), "{mo}{me}");
}

/// A `--help` on a native command prints usage and exits 0 without side effects (here: without running
/// snap). The parser stops before any store write.
#[test]
fn native_command_help_exits_zero_without_running() {
    let r = Repo::new();
    let before = r.git_agent(&["rev-parse", "HEAD"]);
    let (code, out, _err) = r.agit(&["a", "snap", "--help"]);
    assert_eq!(code, 0, "snap --help must exit 0");
    assert!(out.contains("usage") && out.contains("agit a snap"), "must print snap usage: {out}");
    assert_eq!(r.git_agent(&["rev-parse", "HEAD"]), before, "snap --help must not commit anything");
}

/// Passthrough is untouched: only the NATIVE parsers reject unknown dash flags. A git verb agit forwards
/// (`agit a show <flag>`) must reach git verbatim — agit must NOT print its own `unknown option` there,
/// or every scripted `git … --flag` through the store would break.
#[test]
fn passthrough_git_verbs_still_forward_unknown_flags_to_git() {
    let r = Repo::new();
    let (_c, out, err) = r.agit(&["a", "show", "--unknownzzz"]);
    let all = format!("{out}{err}");
    assert!(
        !all.contains("unknown option '--unknownzzz'"),
        "agit must not intercept a passthrough git flag with its own native rejection: {all}"
    );
}

/// FIX 4: a snap blocked by the secret gate must EXIT NON-ZERO (so `snap && push` and `set -e` stop),
/// while a clean snap stays 0. The blocked snap still mirrors to disk and creates no commit.
#[test]
fn blocked_snap_exits_nonzero_clean_snap_stays_zero() {
    let r = Repo::new();

    // Clean snap → exit 0, HEAD advances.
    let clean = r.path().join("clean-home");
    seed_claude_session(&r, &clean, "ok1", "honest work");
    let base = r.git_agent(&["rev-parse", "HEAD"]);
    let (cc, co, ce) = r.agit_env(&[("HOME", clean.to_str().unwrap())], &["a", "snap"]);
    assert_eq!(cc, 0, "a clean snap must exit 0: {co}{ce}");
    let after_clean = r.git_agent(&["rev-parse", "HEAD"]);
    assert_ne!(base, after_clean, "a clean snap must commit: {co}");

    // Secret snap → non-zero, no new commit, but the dump is still mirrored.
    let leak = r.path().join("leak-home");
    seed_claude_session(&r, &leak, "leak2", "key AKIAIOSFODNN7EXAMPLE");
    let (lc, lo, le) = r.agit_env(&[("HOME", leak.to_str().unwrap())], &["a", "snap"]);
    assert_ne!(lc, 0, "a snap the gate blocks must exit non-zero: {lo}{le}");
    assert_eq!(r.git_agent(&["rev-parse", "HEAD"]), after_clean, "a blocked snap must not commit: {le}");
    assert!(r.captured("claude-code", "leak2.jsonl"), "the blocked dump is still mirrored to disk: {le}");
}

/// FIX 3: on a DIVERGED store, `agit a pull` must suggest a REF (e.g. `origin/main`) to reconcile, NEVER
/// the agent's own name — `agit a merge <self-name>` resolves back to this agent and dead-ends. The
/// suggested ref is the working path, proven by `agit a merge <ref> --dry-run` exiting 0.
#[test]
fn diverged_pull_suggests_the_upstream_ref_not_the_agent_name() {
    let r = Repo::new();
    let hub = r.path().join("hub.git");
    r.sh("git init -q --bare -b main hub.git");
    let hub_str = hub.to_str().unwrap();

    // A session in the store so the merge can resolve a runtime, then publish + track the hub.
    let home = r.path().join("cchome");
    seed_claude_session(&r, &home, "base", "shared work");
    assert_eq!(r.agit_env(&[("HOME", home.to_str().unwrap())], &["a", "snap"]).0, 0, "seed snap");
    assert_eq!(r.agit(&["a", "remote", "add", "origin", hub_str]).0, 0, "remote add");
    r.git_agent(&["push", "-u", "origin", "main"]); // origin/main = the snap commit; upstream set

    // Diverge: a local-only commit AND a remote-only commit the local does not have.
    r.git_agent(&["-c", "user.name=dev", "-c", "user.email=d@x.com", "commit", "--allow-empty", "-m", "local-only"]);
    let work = r.path().join("work");
    Command::new("git").args(["clone", "-q"]).arg(&hub).arg(&work).env("HOME", r.path()).output().unwrap();
    Command::new("git").arg("-C").arg(&work).env("HOME", r.path())
        .args(["-c", "user.name=other", "-c", "user.email=o@x.com", "commit", "--allow-empty", "-m", "remote-only"])
        .output().unwrap();
    Command::new("git").arg("-C").arg(&work).env("HOME", r.path()).args(["push", "-q", "origin", "main"]).output().unwrap();

    // Pull refuses the ff, detects divergence, and prints the suggestion.
    let (_c, _o, err) = r.agit(&["a", "pull"]);
    assert!(err.contains("diverged"), "pull must detect divergence: {err}");
    let sugg = err
        .lines()
        .find(|l| l.contains("agit a merge"))
        .map(|l| l.trim().to_string())
        .unwrap_or_else(|| panic!("pull must print a merge suggestion:\n{err}"));
    // The suggestion line carries an inline hint after the ref ("… reconcile by dialogue"), so take the
    // first token after the command as the ref.
    let target = sugg
        .trim_start_matches("agit a merge")
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string();

    // It is a REF (contains `/` or `@`), never the bare agent name.
    assert!(
        target.contains('/') || target.contains('@'),
        "the suggested target must be a ref, not the bare agent name: {target:?}\n{err}"
    );
    assert_ne!(target, "testmemory", "must never suggest the agent's own name: {err}");

    // And the suggested ref is the WORKING path: a dry-run merge against it resolves and exits 0.
    let (mc, mo, me) = r.agit(&["a", "merge", &target, "--dry-run"]);
    assert_eq!(mc, 0, "merging the suggested ref must work (--dry-run): {mo}{me}");
}

// ─────────────────── message hygiene: no em dash in a user-facing string ───────────────────

/// Guard for this wave's message rule: every user-facing CLI string is `<state>: <fact> (<why>)`,
/// punctuated with `:` / `(...)` / `;` and NEVER an em dash. This scans the touched source files for a
/// `—` sitting inside a string literal (normal or raw); comments and doc-comments keep their prose and
/// are exempt. Keeping the check in the test suite means a regression fails CI, not review.
#[test]
fn no_user_facing_string_carries_an_em_dash() {
    let root = env!("CARGO_MANIFEST_DIR");
    let files = ["init.rs", "session.rs", "sync.rs", "commands.rs", "main.rs", "view.rs", "agent.rs"];
    let mut offenders: Vec<String> = Vec::new();
    for f in files {
        let path = format!("{root}/src/{f}");
        let src = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
        for line in string_literal_em_dash_lines(&src) {
            offenders.push(format!("  {f}:{line}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "em dash inside a message string (use `:` / `(...)` / `;` instead):\n{}",
        offenders.join("\n")
    );
}

/// Return the 1-based line of every `—` that sits inside a Rust string literal. A minimal lexer that
/// tracks line/block comments, char literals and lifetimes, so only real string bytes are inspected.
fn string_literal_em_dash_lines(src: &str) -> Vec<usize> {
    let c: Vec<char> = src.chars().collect();
    let n = c.len();
    let (mut i, mut line) = (0usize, 1usize);
    let mut hits: Vec<usize> = Vec::new();
    while i < n {
        let ch = c[i];
        if ch == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        // line comment (covers `//`, `///` and `//!`)
        if ch == '/' && i + 1 < n && c[i + 1] == '/' {
            while i < n && c[i] != '\n' {
                i += 1;
            }
            continue;
        }
        // block comment (nesting-aware, like rustc)
        if ch == '/' && i + 1 < n && c[i + 1] == '*' {
            i += 2;
            let mut depth = 1;
            while i < n && depth > 0 {
                if c[i] == '\n' {
                    line += 1;
                    i += 1;
                } else if c[i] == '/' && i + 1 < n && c[i + 1] == '*' {
                    depth += 1;
                    i += 2;
                } else if c[i] == '*' && i + 1 < n && c[i + 1] == '/' {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        // raw string: r"…", r#"…"#, br"…" — no escapes inside; the terminator is `"` + N `#`
        if ch == 'r' || (ch == 'b' && i + 1 < n && c[i + 1] == 'r') {
            let mut k = i;
            if c[k] == 'b' {
                k += 1;
            }
            k += 1; // past the `r`
            let mut hashes = 0;
            while k < n && c[k] == '#' {
                hashes += 1;
                k += 1;
            }
            if k < n && c[k] == '"' {
                let mut j = k + 1;
                loop {
                    if j >= n {
                        break;
                    }
                    if c[j] == '"' && (1..=hashes).all(|h| j + h < n && c[j + h] == '#') {
                        j += 1 + hashes;
                        break;
                    }
                    if c[j] == '\n' {
                        line += 1;
                    }
                    if c[j] == '—' {
                        hits.push(line);
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
            // not a raw string after all — fall through and advance one char
        }
        // normal string
        if ch == '"' {
            let mut j = i + 1;
            while j < n {
                if c[j] == '\\' {
                    if j + 1 < n && c[j + 1] == '\n' {
                        line += 1;
                    }
                    j += 2;
                    continue;
                }
                if c[j] == '"' {
                    break;
                }
                if c[j] == '\n' {
                    line += 1;
                }
                if c[j] == '—' {
                    hits.push(line);
                }
                j += 1;
            }
            i = j + 1;
            continue;
        }
        // char literal vs lifetime
        if ch == '\'' {
            if i + 1 < n && c[i + 1] == '\\' {
                // escaped char literal: skip the escape indicator, then scan to the closing `'`
                let mut j = i + 3;
                while j < n && c[j] != '\'' {
                    j += 1;
                }
                i = j + 1;
                continue;
            } else if i + 2 < n && c[i + 2] == '\'' {
                i += 3; // simple char literal like 'x' or '"'
                continue;
            } else {
                i += 1; // a lifetime tick
                continue;
            }
        }
        i += 1;
    }
    hits
}
