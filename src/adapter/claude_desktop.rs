//! Claude Desktop adapter. **Export only** ([`Capability::ExportOnly`]).
//!
//! # What it is
//!
//! Claude Desktop (Claude.app)'s Code tab writes its transcript as standard Claude Code jsonl,
//! at `~/.claude/projects/<slug>/<uuid>.jsonl` — the same storage the CLI uses (evidence in
//! docs/mechanism-probing/desktop-apps.md §1.3, cited as §x.y from here on). So the disk format,
//! the rendering, the id minting and the writing are nothing new; this adapter reuses
//! [`super::claude_code`] for all of it (§4.6: "no second install path").
//!
//! The one thing it adds is the **hand-off**.
//!
//! # Why an install cannot resume directly
//!
//! The desktop application's session list is a derived index it maintains itself (the
//! `claude-code-sessions/<account>/<org>/local_*.json` sidecars — metadata only, no body). agit
//! does not write that index by hand (§3.2, the three reasons Road B is rejected): the path makes
//! agit guess an identity assertion about the user's account/org; the format has no public
//! documentation, the directory has been renamed once and is migrating into a VM disk image; and
//! writing it by hand bypasses the application's own initialization path. Forging the sidecar
//! means writing into a format nobody has promised.
//!
//! So the step after installing is a [`Next::HandOff`], whose two fields differ in nature and are
//! presented apart:
//!
//! - `trigger`: the official (but undocumented) ingestion entry point,
//!   `open 'claude://resume?session=<uuid>'` (§2.1). The application validates the UUID itself,
//!   resolves account/org itself and builds the sidecar itself — success or failure shows up only
//!   in its own UI and telemetry, where **agit cannot observe it**, and a 0 exit does not mean
//!   the ingestion succeeded. It needs the application to be signed in.
//! - `fallback`: `(cd <cwd> && claude --resume <uuid>)`. The transcript already sits in
//!   `~/.claude/projects/`, and to the CLI it is no different from any other session — this one
//!   is certain to work, exactly what §3.2 means by "the degradation goes in a direction known
//!   to work".
//!
//! [`Adapter::mint_id`] produces a UUIDv7 for the new session id, which satisfies the `C5` regex
//! and the `isUuid` check in the route implementation (§3.2) with no change at all.
//!
//! # The read side belongs to `claude-code`, not here
//!
//! Code tab sessions are listed, looked up and parsed by the Claude Code adapter already —
//! `agit import` adopts them today; it just draws no distinction (§3.2: "already happening
//! quietly"). This adapter's `resolve` / `sessions_for` / `all_sessions` uniformly return empty:
//! if the same files were reported once under each of two runtimes, `import`'s lookup
//! disambiguation and `status --check-missing`'s missed-capture list would report every Claude
//! Code session twice.
//!
//! `parse` is still implemented honestly (it delegates to Claude Code's parser, then labels
//! `runtime` truthfully): an unknown record type becomes [`EventKind::Other`] and enters the
//! dropped count, and Cowork's extra `attachment` / `last-prompt` / `queue-operation` records are
//! not counted as prompts (the table in §3.2).

use super::claude_code::ClaudeCode;
use super::{Adapter, Capability, Installed, Next, Session, SessionRef};
use crate::Result;
use std::path::Path;

pub struct ClaudeDesktop;

impl Adapter for ClaudeDesktop {
    fn id(&self) -> &'static str {
        "claude-desktop"
    }

    fn cli(&self) -> &'static str {
        // The desktop application has no executable on PATH — whether the CLI that can be found
        // (`claude`) exists says nothing about this target's availability. Report the
        // application's own bundle name, or a machine carrying the Claude Code CLI is wrongly
        // reported as "available" (§4.5's doctor row prints `/Applications/Claude.app` for the
        // same reason, and not the CLI).
        "Claude.app"
    }

    /// Not import-only: it installs a real deliverable, but the last step (ingestion) happens in
    /// an application process agit cannot observe, so success must not be claimed (§4.2).
    fn capability(&self) -> Capability {
        Capability::ExportOnly
    }

    fn format(&self) -> &'static str {
        // The Code tab writes Claude Code's own jsonl — one format family, so installing either
        // way goes through byte rewriting, and encrypted reasoning and compact boundaries pass
        // through unchanged (§4.3/§5.1).
        "claude-code"
    }

    /// Returns empty: the read side is carried by the `claude-code` adapter (the module doc's
    /// "read side" section).
    fn sessions_for(&self, _repo: &Path) -> Result<Vec<SessionRef>> {
        Ok(vec![])
    }

    /// Returns empty: same as above. Reporting twice is worse than missing a name — the lookup
    /// disambiguation goes ambiguous at once.
    fn all_sessions(&self) -> Result<Vec<SessionRef>> {
        Ok(vec![])
    }

    /// Returns None: same as above. A lookup by id hits `claude-code` first; a second hit here
    /// forks the answer out of nowhere.
    fn resolve(&self, _session_id: &str, _cwd: Option<&Path>) -> Option<std::path::PathBuf> {
        None
    }

    fn parse(&self, text: &str) -> Result<Session> {
        // A Code tab transcript is Claude Code jsonl (§1.3), so the whole parser is reused —
        // including the honesty of recording an unknown type as Other and of not counting
        // Cowork's extra record types as prompts. Only the label changes: which adapter read it
        // is written truthfully.
        let mut s = ClaudeCode.parse(text)?;
        s.runtime = "claude-desktop".into();
        Ok(s)
    }

    fn render(&self, session: &Session, new_id: &str, cwd: &Path) -> Result<String> {
        // The installed file must be Claude Code jsonl (one format family, §4.3), so the
        // IR → disk mapping is ClaudeCode's. The loss list for cross-vendor conversion is
        // identical to `--as claude-code`'s (§5.1); this adapter adds no loss of its own.
        ClaudeCode.render(session, new_id, cwd)
    }

    fn mint_id(&self) -> String {
        // The ingestion route takes UUIDs only (§2.1's `C5` regex + `isUuid`); a UUIDv7
        // satisfies it (§3.2).
        uuid::Uuid::now_v7().to_string()
    }

    fn install(&self, content: &str, new_id: &str, cwd: &Path) -> Result<Installed> {
        // The disk-writing logic is claude-code's (§4.6, Road A): the same location, the same
        // identity keys; only `Next` differs — the last step of resuming is left to the desktop
        // application itself.
        let written = ClaudeCode.install(content, new_id, cwd)?;
        Ok(Installed {
            path: written.path,
            next: handoff(new_id, cwd),
        })
    }

    /// The desktop application is not a CLI on PATH, so the default implementation
    /// (`which(cli())`) does not apply (§4.5).
    ///
    /// The convention: the app bundle path holds only on macOS, and every other platform returns
    /// false rather than guessing a path that does not exist — `doctor` reports "unavailable" and
    /// `resume`'s executable check offers `--no-launch` as the way out; both beat guessing wrong.
    fn available(&self) -> bool {
        cfg!(target_os = "macos") && Path::new("/Applications/Claude.app").is_dir()
    }
}

/// The hand-off instruction that follows an install ([`Next::HandOff`]).
///
/// The two fields are presented apart because they differ in nature (§4.2): `trigger` can fail
/// where agit cannot observe it (success or failure lives only in the application's own
/// UI/telemetry), while `fallback` is a path certain to work. Both must be **steps someone can
/// follow** — a "you may need to import it by hand" that leaves no next step does not go here.
fn handoff(new_id: &str, cwd: &Path) -> Next {
    Next::HandOff {
        // The official ingestion entry point (§2.1): the application validates the UUID itself,
        // resolves account/org itself and builds the sidecar itself; agit writes not one byte
        // into its private storage.
        trigger: format!("open 'claude://resume?session={new_id}'"),
        // The path around the desktop application: the transcript already sits in
        // ~/.claude/projects/ in Claude Code format, and the CLI resumes it directly (§3.2).
        fallback: format!("(cd {} && claude --resume {new_id})", cwd.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Event, EventKind};
    use super::*;

    /// Tests that change process-level environment variables overwrite each other under parallel
    /// execution (the rule laid down by commit.rs's `Sites` comment), so this is the **only** test
    /// that changes HOME, and this lock holds off any later one of the same kind. The window
    /// narrows to "set → install → restore on Drop".
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Points HOME somewhere else temporarily and restores the old value on Drop.
    struct HomeGuard(Option<std::ffi::OsString>);

    impl HomeGuard {
        fn point_at(dir: &Path) -> HomeGuard {
            let old = std::env::var_os("HOME");
            // SAFETY: the caller must hold ENV_LOCK first, so env changes inside this process
            // are serial.
            unsafe { std::env::set_var("HOME", dir) };
            HomeGuard(old)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: the lock's lifetime covers the guard's lifetime (declaration order in the
            // test).
            match &self.0 {
                Some(v) => unsafe { std::env::set_var("HOME", v) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    /// §4.3's declaration: export only, and the disk format is in claude-code's family.
    #[test]
    fn declares_export_only_with_the_claude_code_format_family() {
        let a = ClaudeDesktop;
        assert_eq!(a.id(), "claude-desktop");
        assert_eq!(a.capability(), Capability::ExportOnly);
        assert_eq!(a.format(), "claude-code");
        assert_eq!(
            a.format(),
            ClaudeCode.format(),
            "byte rewriting requires one format family (§5.1)"
        );
        // ExportOnly can be installed into (it is not ImportOnly) but does not enter the default
        // resume list — the latter is guaranteed by default_targets()'s Resumable filter, pinned
        // in mod.rs.
        assert!(a.installable());
    }

    /// This pins the hand-off wording. clone/resume print these two strings unchanged, and the
    /// wording comes from §4.4's sample output: `trigger` is the deep link, `fallback` is the CLI
    /// path certain to work.
    #[test]
    fn handoff_carries_the_deep_link_and_the_cli_fallback() {
        let id = "019f9a81-aaaa-7bbb-8ccc-0123456789ab";
        let Next::HandOff { trigger, fallback } =
            handoff(id, Path::new("/Users/nana/Projects/payments"))
        else {
            panic!("a hand-off target must yield HandOff");
        };
        assert_eq!(trigger, format!("open 'claude://resume?session={id}'"));
        assert_eq!(
            fallback,
            format!("(cd /Users/nana/Projects/payments && claude --resume {id})")
        );
    }

    /// This pins that install lands where the doc says (§3.2, Road A:
    /// `~/.claude/projects/<slug>/<uuid>.jsonl`), that the content is byte-for-byte unchanged,
    /// and that what comes back is a HandOff rather than a runnable command.
    #[test]
    fn install_writes_the_transcript_into_claude_projects() {
        let _serial = ENV_LOCK.lock().unwrap();
        let d = tempfile::tempdir().unwrap();
        let _home = HomeGuard::point_at(d.path());

        let cwd = Path::new("/Users/nana/Projects/payments");
        let content = "{\"type\":\"user\",\"sessionId\":\"x\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";
        let new_id = "019f9a81-aaaa-7bbb-8ccc-0123456789ab";
        let ins = ClaudeDesktop.install(content, new_id, cwd).unwrap();

        let want = d
            .path()
            .join(".claude")
            .join("projects")
            .join(crate::adapter::claude_code::slug_for(cwd))
            .join(format!("{new_id}.jsonl"));
        assert_eq!(ins.path, want, "the path must be where §3.2 says");
        assert_eq!(
            std::fs::read_to_string(&ins.path).unwrap(),
            content,
            "the content stays byte-for-byte unchanged; the app does not own the transcript"
        );

        let Next::HandOff { trigger, fallback } = &ins.next else {
            panic!("a hand-off target must not yield a runnable command");
        };
        assert!(
            trigger.contains(&format!("claude://resume?session={new_id}")),
            "trigger must carry the deep link: {trigger}"
        );
        assert!(
            fallback.contains(&format!("claude --resume {new_id}")),
            "fallback must be the CLI path certain to work: {fallback}"
        );
    }

    /// A minted id must be a UUID — the `claude://resume` route runs a UUID check first (§2.1's
    /// `C5` regex + `isUuid`), so an invalid id reaching the application is a wasted trip.
    #[test]
    fn minted_ids_pass_the_resume_route_uuid_check() {
        let id = ClaudeDesktop.mint_id();
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "{id} must be a UUID");
    }

    /// parse is honest: a Code tab transcript is Claude Code jsonl, so ordinary content yields
    /// ordinary events, while unknown types and Cowork's extra records become Other and enter the
    /// dropped count instead of being counted as prompts (§3.2).
    #[test]
    fn parse_is_honest_and_labels_the_desktop_runtime() {
        let prompt = "a real question";
        let text = format!(
            concat!(
                r#"{{"type":"user","sessionId":"s1","cwd":"/repo","message":{{"role":"user","content":"{prompt}"}}}}"#,
                "\n",
                r#"{{"type":"last-prompt","sessionId":"s1","lastPrompt":"{prompt}","leafUuid":"u1"}}"#,
                "\n",
                r#"{{"type":"queue-operation","sessionId":"s1","operation":"enqueue","content":"{prompt}"}}"#,
                "\n",
                r#"{{"type":"brand-new-future-type","sessionId":"s1"}}"#,
                "\n",
            ),
            prompt = prompt
        );
        let s = ClaudeDesktop.parse(&text).unwrap();
        assert_eq!(s.runtime, "claude-desktop", "runtime names the reader");
        assert_eq!(s.id, "s1");
        assert_eq!(s.cwd.as_deref(), Some("/repo"));
        let c = s.counts();
        assert_eq!(c.prompts, 1, "prompt copies do not count: {}", c.prompts);
        assert_eq!(c.dropped, 3, "last-prompt + enqueue + unknown all drop");
    }

    /// render produces Claude Code jsonl — the desktop application's Code tab reads no other
    /// storage.
    #[test]
    fn render_produces_claude_code_jsonl() {
        let s = Session {
            id: "src".into(),
            runtime: "codex".into(),
            cwd: None,
            events: vec![
                Event::text(EventKind::UserPrompt, "question", None),
                Event::text(EventKind::AssistantReply, "answer", None),
            ],
        };
        let out = ClaudeDesktop.render(&s, "nid", Path::new("/repo")).unwrap();
        for line in out.lines() {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["sessionId"], "nid");
            assert!(
                v.get("message").is_some(),
                "each line must be Claude Code shape: {line}"
            );
        }
        // The render output reads back through this adapter's own parse (a round trip inside one
        // format family).
        let back = ClaudeDesktop.parse(&out).unwrap();
        assert_eq!(back.id, "nid");
        assert_eq!(back.counts().prompts, 1);
    }

    /// This pins where the read side belongs: the same Code tab sessions are already listed and
    /// looked up by `claude-code`, and reporting them again here double-counts in `import`'s
    /// disambiguation and in `status`'s missed-capture list.
    #[test]
    fn the_read_side_is_owned_by_the_claude_code_adapter() {
        let a = ClaudeDesktop;
        assert!(a.sessions_for(Path::new("/whatever")).unwrap().is_empty());
        assert!(a.all_sessions().unwrap().is_empty());
        assert!(a.resolve("anything", None).is_none());
    }

    /// A non-macOS platform reports unavailable rather than guessing an app bundle path (§4.5).
    #[test]
    fn availability_is_platform_honest() {
        if cfg!(target_os = "macos") {
            // On macOS the answer depends on whether it is really installed; both are legal.
            let _ = ClaudeDesktop.available();
        } else {
            assert!(!ClaudeDesktop.available(), "unsupported OS is unavailable");
        }
    }
}
