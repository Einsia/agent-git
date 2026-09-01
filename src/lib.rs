//! AgentGit CLI.
//!
//! # The product premise (it decides every design below)
//!
//! The target user **already uses Claude Code or Codex every day**. So every command has to
//! answer why it is worth learning one more action. Three hard constraints follow:
//!
//! 1. **Adoption is explicit, and it arrives in one step**. `agit import <session-id> -n <agent>`
//!    picks one session, names it and records the version that opens its history — one command.
//!    The intermediate "has a link, has no version" state means nothing to the user; nobody wants
//!    to stop there.
//!
//!    The "explicit" half decides that of the 18858 sessions on this machine (11.2 GB) only the
//!    few the user picks enter version control — an agent's "memory" is the work you selected,
//!    not whatever happens to be on disk.
//!
//!    The rule for the cost half: **an operation that scales with the number of sessions must not
//!    open a transcript**. `agit log` (the list), `agit status` and the candidate search inside
//!    `import` read store links and the runtime index only (Codex queries the `threads` table in
//!    0.40 ms, Claude Code globs one level in 2.2 ms). Parsing one 3 MB session costs 6.8 ms and
//!    a list of 18810 sessions costs 390 ms — that is what does not hold.
//!
//!    Written as "import does not parse transcripts", the reason is too wide: `import` parses
//!    **the one session the user picked**, once, and it does not scale with how many sessions sit
//!    on disk — the same cost `commit` owes anyway, only earlier. What must not break is the
//!    sentence above about enumeration.
//!
//!    Offline still works: `agit import --link-only` is the link-only route.
//!
//! 2. **A version comes from one act of the author**. `agit commit` is that act: it produces one
//!    piece of session metadata (see [`domain::meta`]) covering everything in the transcript at
//!    that moment. `import` records the opening version through the same code. A version lands in
//!    `~/.agit/repos/<owner>/<name>/`; that path needs the account name and the signature is
//!    bound to it too, so both commands require being signed in.
//!
//! 3. **"Picking up" is the core of the product**. `clone` / `resume` are about the person on the
//!    other end — you can take my agent and use it directly, git history and my signature
//!    included. Doing only "my side" (sign in, commit, push) misses the whole argument.
//!
//! # Layers
//!
//! ```text
//!   commands/   One file per subcommand. Only "parse arguments → call domain → hand to ui".
//!       ↓
//!   domain/     Business logic. No println! at all — testable, reusable by non-CLI callers.
//!       ↓
//!   adapter/    Runtime adaptation. Translates each runtime's on-disk format into one IR.
//!       ↓
//!   ui/         Terminal rendering. tty-aware: off a tty it degrades to pipeable plain text.
//! ```
//!
//! `hub/` cuts across: the backend HTTP client, plus the git subprocesses that need
//! authentication (`clone` / `fetch` / `push`) — authentication is the hub's business, and where
//! a token comes from and how an expired one is exchanged are both answered there.
//!
//! **"Which agent this repo should pick up" is answered by the server**, and no binding file is
//! written into the code repo.
//!
//! # The hub backend uses this crate as a library too
//!
//! A session transcript on the web must be the same thing as `agit show`, so the backend reuses
//! the IR of [`adapter`] and the turn splitting of [`domain::turn`] instead of writing a second
//! parser — two IR definitions drift silently. Of the layers above only `adapter` and
//! `domain` are shared: `commands` / `ui` / `hub` all sit behind the `cli` feature (see
//! Cargo.toml), so the backend's `default-features = false` does not link in a TUI for the sake
//! of one JSON endpoint.

pub mod adapter;
#[cfg(feature = "cli")]
pub mod commands;
#[cfg(feature = "cli")]
pub mod hub;
/// RC wire protocol shared with the hub and viewers. Not feature-gated: the
/// backend compiles against it with `default-features = false`.
pub mod protocol;
/// `agit rc` — the resident daemon (`agitd`). Behind the `rc` feature so the
/// backend's `default-features = false` import stays free of tokio and TLS.
#[cfg(feature = "rc")]
pub mod rc;
/// The human-facing side: the terminal interface a bare command enters when "someone is sitting
/// at the terminal". The split with `ui`: `ui` is line rendering and inline questions, `tui` is
/// full-screen browsing and selection.
#[cfg(feature = "cli")]
pub mod tui;
#[cfg(feature = "cli")]
pub mod ui;

/// Functional domains. Each domain carries its own helpers.
///
/// The test: a function only one domain uses stays inside that domain. The rule keeps another
/// catch-all `core/` from growing.
pub mod domain {
    /// Install a session into a runtime (the native format is rewritten byte by byte;
    /// cross-runtime goes through the IR).
    pub mod install;
    /// Links: what stands for a session in the store (not a copy).
    pub mod link;
    /// The merge transaction lock (the target branch is locked between opening the merge and
    /// recording it).
    pub mod mergetx;
    /// Session metadata `session/meta.json`: the product of `agit commit`, and the version ID
    /// itself.
    pub mod meta;
    /// Search query syntax (`in:` / `owner:` / `-excluded` / `"phrase"`). Shared by the backend
    /// and the CLI.
    pub mod query;
    /// Content redaction: erasing the coordinates of a published copy (secret masking +
    /// persona / path / IP disguise).
    pub mod redact;
    /// Reference syntax: parsing and resolving `owner/repo@ref`, `@`, `ref~n`, `ref#n`,
    /// `ref#n.k`, `ref:path`.
    pub mod refs;
    /// The git wrapper of a repo (a version is a tag).
    pub mod repo;
    /// Low-entropy secrets the user registers explicitly: the local encrypted vault and the
    /// literal matcher.
    #[cfg(feature = "secret-vault")]
    pub mod secret_filter;
    /// Secret scanning.
    pub mod secrets;
    /// Enumerating and selecting materialized content (the transcripts inside a repo).
    pub mod session;
    /// Compatibility layer for reading legacy snapshot.json; the commit path uses
    /// `meta`/`storage`.
    pub mod snapshot;
    /// v0/v1 session storage: content-addressed events, sequencing and materialization.
    pub mod storage;
    /// The local store: a plain directory holding links.
    pub mod store;
    /// Text splitting and similarity. The only implementation of every split on the search path
    /// (the backend uses it too).
    pub mod text;
    /// Envelopes: the shape, hash and VIEW slicing of v0 JSONL / v1 event objects.
    pub mod transcript;
    /// Turn splitting and the hash chain. For looking and comparing only; not a version ID.
    pub mod turn;
    /// Workspaces: the binding between a local directory and a repo, plus the pinned branch.
    pub mod workspace;
}

/// Cross-domain infrastructure.
///
/// **Not** another junk drawer: only what every domain needs and what belongs to no single domain
/// goes here (where $AGIT_HOME is, where the token is kept). What only one domain uses stays out.
pub mod infra {
    /// Path and environment-variable resolution. The single answer to every "where is it".
    pub mod config;
    /// Credential storage: where the token is kept, and how expiry is decided.
    pub mod credentials;
    /// Where each runtime's project memory directory is.
    pub mod runtime_memory;
    /// Discovery of the runtime session the current process belongs to (no CLI output here).
    pub mod runtime_session;
}

pub type Result<T> = anyhow::Result<T>;

/// The return shape of a command. `commands::CmdResult` is the same thing, but it sits behind
/// the `cli` feature and `tui` needs it too, so the alias lives at the crate root.
pub type CmdResultAlias = Result<ExitCode>;

/// One note to the user, on stderr.
///
/// At the crate root rather than in [`ui`] because the domain layer needs it (the file-permission
/// degradation warning) while `ui` exists only under the `cli` feature. Linked as a library it
/// degrades to uncolored output — there is no terminal at all then.
pub fn warn(msg: &str) {
    #[cfg(feature = "cli")]
    ui::warning(msg);
    #[cfg(not(feature = "cli"))]
    eprintln!("note {msg}");
}

/// Exit codes. Uniform across the CLI (the PRD's "contract for scripts and agents"):
///
/// | Code | Meaning |
/// |------|---------|
/// | 0    | success (including nothing to do) |
/// | 2    | usage error |
/// | 3    | a reference does not resolve, or is ambiguous |
/// | 4    | a precondition is not met |
/// | 5    | not signed in, or the credentials are no longer valid |
/// | 6    | network / hub error |
/// | 7    | policy refusal (secret scan, gate) |
/// | 8    | interaction is required but this run is non-interactive |
///
/// `Failure(1)` is kept only while the legacy call sites migrate; new code must use one of the
/// kinds above. A named type rather than a bare i32 makes every command's signature
/// self-explanatory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    Ok = 0,
    /// Migration leftover; new code uses a more precise kind.
    Failure = 1,
    Usage = 2,
    /// A reference does not resolve, or is ambiguous.
    Ref = 3,
    /// A precondition is not met.
    Precondition = 4,
    /// Not signed in, or the credentials are no longer valid.
    Auth = 5,
    /// Network / hub error.
    Network = 6,
    /// Policy refusal (secret scan, gate).
    Policy = 7,
    /// Interaction is required but this run is non-interactive.
    Interactive = 8,
}

impl ExitCode {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}
