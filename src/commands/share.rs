//! `agit share` — mint a one-off read-only link.
//!
//! # End-to-end encryption
//!
//! Content is encrypted **locally** with AES-256-GCM and the key goes into the link fragment
//! (`#k=`). Browsers never send the fragment to the server, so the backend stores only ciphertext
//! and decryption happens in the browser.
//!
//! AES-GCM and not XChaCha20 because browsers support the former natively through WebCrypto — the
//! viewer decrypts the sanitized selected records with WebCrypto. Original records are accessed
//! through the source snapshot's account permissions.

use super::{CmdResult, require_login};
use crate::domain::link;
use crate::domain::meta;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::storage;
use crate::domain::store::Store;
use crate::domain::transcript;
use crate::hub::ShareRequest;
use crate::infra::config;
use crate::{ExitCode, ui};
use anyhow::Context;
use clap::{Args as ClapArgs, Subcommand};

#[derive(ClapArgs)]
pub struct Args {
    /// AgentGit ref or native session ID/prefix; omitted targets use AGIT_SESSION.
    #[arg(value_name = "owner/repo@ref | session")]
    pub target: Option<String>,

    /// Share the selected saved point's full LOG instead of its VIEW.
    #[arg(long)]
    pub full_log: bool,

    /// Unencrypted: mint a fetchable public link
    #[arg(long)]
    pub public: bool,

    /// Expiry: 24h / 7d / 30d / never
    #[arg(long, default_value = "7d", value_name = "duration")]
    pub expire: String,

    /// Max view count
    #[arg(long, value_name = "count")]
    pub views: Option<u32>,

    /// Add a passphrase (the server stores only its hash)
    #[arg(long)]
    pub password: bool,

    /// Print each public record the link would hold before the confirmation, not only the
    /// privacy summary.
    #[arg(long)]
    pub show_preview: bool,

    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// List the links you created
    List,
    /// Revoke a link
    Rm {
        #[arg(value_name = "slug")]
        slug: String,
    },
}

pub fn run(mut args: Args) -> CmdResult {
    if wants_tui(&args) {
        match crate::tui::should_enter() {
            crate::tui::Verdict::Enter => {
                let Some(picked) = crate::tui::screens::sharing::pick(&std::env::current_dir()?)?
                else {
                    return Ok(ExitCode::Ok);
                };
                args = picked;
            }
            crate::tui::Verdict::Explain(note) => crate::tui::warn_skipped(&note),
            crate::tui::Verdict::NoTerminal => return Ok(ExitCode::Interactive),
            crate::tui::Verdict::Skip => {}
        }
    }
    let client = require_login()?;

    match &args.cmd {
        Some(Cmd::List) => return list(&client),
        Some(Cmd::Rm { slug }) => {
            if let Err(error) = client.revoke_share(slug) {
                return failed(error);
            }
            ui::success(&format!("revoked {slug}"));
            return Ok(ExitCode::Ok);
        }
        None => {}
    }

    let source = match selected_source(args.target.as_deref(), args.full_log) {
        Ok(source) => source,
        Err(error) => {
            ui::error(&format!("{error:#}"));
            if let Some(hinted) = error.downcast_ref::<super::Hinted>() {
                for line in hinted.hint.lines() {
                    ui::hint(line);
                }
            }
            return Ok(ExitCode::Precondition);
        }
    };
    super::echo::emit(
        "share",
        &[super::echo::Selection::new(
            source.label.clone(),
            source.selection_source,
        )],
    );

    let prepared = match protected_readable(&source, &[]) {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(hint) = repair_hint(&source, &error) {
                ui::error(&format!("{error:#}"));
                ui::hint(&hint);
                return Ok(ExitCode::Precondition);
            }
            return Err(error);
        }
    };
    if !prepared.holds_conversation {
        ui::error(&format!(
            "{} holds no conversation yet, so its link would open empty; no share was created",
            source.label
        ));
        match (&source.running, &source.repository, &source.branch) {
            (Some(running), ..) => {
                for line in including_running(&source, running) {
                    ui::hint(&line);
                }
            }
            (None, Some(repository), Some(branch)) => ui::hint(&format!(
                "`agit log {repository}@{branch}` lists the turns this branch holds"
            )),
            _ => {}
        }
        return Ok(ExitCode::Precondition);
    }
    let expire_secs = crate::input_argument(parse_expire(&args.expire))?;

    // Passphrase: hashed locally; the plaintext is never uploaded.
    let password_hash = if args.password {
        match ui::prompt::password("set a view passphrase")? {
            Some(p) if !p.is_empty() => Some(hash_password(&p)),
            Some(_) => {
                ui::error("the view passphrase must not be empty.");
                return Ok(ExitCode::Usage);
            }
            None => {
                ui::error("--password needs an interactive terminal to read the passphrase.");
                return Ok(ExitCode::Interactive);
            }
        }
    } else {
        None
    };

    let public = prepared
        .projection
        .share_public_value(&prepared.protected, &prepared.text);
    let serialized = serde_json::to_string(&public)?;
    let (payload, key) = if args.public {
        (serialized, None)
    } else {
        let (ct, k) = encrypt(serialized.as_bytes())?;
        (ct, Some(k))
    };
    anyhow::ensure!(
        payload.len() <= crate::domain::privacy_envelope::MAX_ENVELOPE_BYTES,
        "share exceeds the upload budget; no content was sent"
    );

    {
        use std::io::Write;
        use ui::privacy_preview;
        let mut output = std::io::stderr().lock();
        privacy_preview::write_report(&mut output, &prepared.report)?;
        writeln!(output, "Public metadata:")?;
        privacy_preview::write_value(&mut output, &public["metadata"], 2)?;
        if args.show_preview {
            writeln!(output, "Complete selected public records:")?;
            privacy_preview::write_session(&mut output, &prepared.protected)?;
        }
        output.flush()?;
    }
    if let Some(running) = &source.running {
        ui::warning(&match source.metadata.turn {
            Some(turn) => format!(
                "this link holds the conversation up to turn {turn}; the turn in progress is not in it"
            ),
            None => "this link holds the conversation up to its last saved turn; the turn in progress is not in it".to_owned(),
        });
        for line in including_running(&source, running) {
            ui::hint(&line);
        }
    }
    if !args.show_preview {
        let records = storage::parse_envelopes(&prepared.protected)?.len();
        ui::hint(&format!(
            "{records} public records selected; add --show-preview to print each one before you confirm"
        ));
    }

    let visibility = if args.public {
        "a public and unencrypted"
    } else {
        "an end-to-end encrypted"
    };
    let expiry = if expire_secs == 0 {
        "never".to_owned()
    } else {
        args.expire.clone()
    };
    let views = args
        .views
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unlimited".into());
    if std::env::var_os("AGIT_YES").is_none() {
        match ui::prompt::confirm(
            &format!(
                "create {visibility} share for {}? Anyone with the full link can read it until it expires or is withdrawn (expires {expiry}, {views} views)",
                source.label
            ),
            false,
        )? {
            Some(true) => {}
            Some(false) => {
                println!("share cancelled.");
                return Ok(ExitCode::Ok);
            }
            None => {
                ui::error(&format!(
                    "creating a share for {} requires confirmation.",
                    source.label
                ));
                ui::hint(
                    "rerun with -y (or --yes) to confirm the target and visibility, or confirm from a terminal",
                );
                return Ok(ExitCode::Interactive);
            }
        }
    }

    anyhow::ensure!(
        source.policy_digest()? == prepared.policy_digest,
        "privacy policy changed after the share preview; review a fresh preview before sharing"
    );
    let resp = match client.create_share(&ShareRequest {
        format_version: 2,
        payload,
        encrypted: !args.public,
        expire_seconds: expire_secs,
        max_views: args.views,
        password_hash,
    }) {
        Ok(response) => response,
        Err(error) => return failed(error),
    };
    anyhow::ensure!(
        resp.format_version == Some(2),
        "the Hub did not acknowledge the privacy share format; inspect or revoke share {} before retrying",
        resp.slug
    );

    ui::success("read-only copy created");
    crate::telemetry::activation::reached(
        crate::telemetry::activation::Milestone::FirstShare,
        client.base(),
    );

    // When encrypted, the key is appended to the fragment — it is never sent to the server.
    let link = match &key {
        Some(k) => format!("{}#k={}", resp.url, k),
        None => resp.url.clone(),
    };
    println!("\n  {}\n", ui::accent(&link));

    print!(
        "{}",
        ui::table::key_values(&[
            ("source", source.label.clone()),
            (
                "encrypted",
                if args.public {
                    ui::warn_text("no (public — content is fetchable and indexable)").to_string()
                } else {
                    ui::ok("yes (end-to-end; the server stores only ciphertext)").to_string()
                }
            ),
            (
                "expires",
                if expire_secs == 0 {
                    ui::warn_text("never").to_string()
                } else {
                    format!("in {}", args.expire)
                }
            ),
            (
                "view cap",
                args.views
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unlimited".into())
            ),
            (
                "passphrase",
                if args.password {
                    "yes".into()
                } else {
                    ui::dim("none").to_string()
                }
            ),
        ])
    );

    if key.is_some() {
        ui::hint(
            "the key lives after # in the link — never sent to the server; only the full link decrypts",
        );
    }
    ui::hint(&format!("revoke: agit share rm {}", resp.slug));
    Ok(ExitCode::Ok)
}

fn wants_tui(args: &Args) -> bool {
    args.target.is_none()
        && args.cmd.is_none()
        && !args.full_log
        && !args.public
        && args.expire == "7d"
        && args.views.is_none()
        && !args.password
        && !args.show_preview
}

struct ShareSource {
    raw: String,
    envelope: Option<String>,
    runtime: String,
    label: String,
    selection_source: super::echo::Source,
    protection_repo: Option<std::path::PathBuf>,
    projection_log: Option<String>,
    metadata: meta::Meta,
    branch: Option<String>,
    /// The repository a recorded point belongs to.
    repository: Option<String>,
    /// The conversation still writing to the branch whose tip is the selected point, when its
    /// transcript holds work that point does not have yet.
    running: Option<link::Link>,
}

impl ShareSource {
    fn policy_digest(&self) -> crate::Result<String> {
        match self.protection_repo.as_ref() {
            Some(root) => {
                let repo = Repo::open(root)
                    .ok_or_else(|| anyhow::anyhow!("the source repository is unavailable"))?;
                crate::domain::privacy::PrivacyPolicy::load(&repo)?.digest()
            }
            None => crate::domain::privacy::PrivacyPolicy::load_default()?.digest(),
        }
    }
}

struct PreparedShare {
    text: String,
    protected: String,
    projection: crate::domain::privacy_publication::SessionProjection,
    report: crate::domain::privacy_publication::ProjectionReport,
    policy_digest: String,
    /// Whether the copy holds anything a reader sees: records a runtime writes for itself are not
    /// shown, so a copy of only those opens empty.
    holds_conversation: bool,
}

/// Project native content before rendering so truncation cannot hide discovery context.
fn protected_readable(
    source: &ShareSource,
    additional: &[crate::domain::privacy::mandatory::MandatoryPolicy],
) -> crate::Result<PreparedShare> {
    let policy_digest = source.policy_digest()?;
    let repo = source
        .protection_repo
        .as_ref()
        .map(|root| {
            Repo::open(root).ok_or_else(|| {
                anyhow::anyhow!("the source repository is unavailable; no share was created")
            })
        })
        .transpose()?;
    let selected = match &source.envelope {
        Some(envelope) => envelope.clone(),
        None => crate::domain::privacy_publication::wrap_native(
            &source.raw,
            &source.runtime,
            &source.metadata.session,
        )?,
    };
    let log = source.projection_log.as_deref().unwrap_or(&selected);
    let projection = crate::domain::privacy_publication::project_frozen_with_sources(
        repo.as_ref(),
        source.branch.as_deref(),
        log,
        &selected,
        &source.metadata,
        additional,
    )?;
    let selected = projection.select(&selected)?;
    let protected = crate::domain::privacy::service::transform(
        source.protection_repo.as_deref(),
        &selected,
        crate::domain::privacy::projector::Mode::ProtectEnvelopes,
    );
    let (selected, secret_matches) = (protected.content, protected.replacements);
    let parsed = transcript::display::parse(&selected)?;
    anyhow::ensure!(
        source.policy_digest()? == policy_digest,
        "privacy policy changed while preparing the share"
    );
    Ok(PreparedShare {
        holds_conversation: parsed
            .events
            .iter()
            .any(|event| crate::adapter::is_conversation(event.kind)),
        text: ui::transcript::render_transcript(&parsed, usize::MAX),
        report: {
            let mut report = projection.report().clone();
            report.secret_matches += secret_matches;
            report
        },
        policy_digest,
        protected: selected,
        projection,
    })
}

struct SharePoint {
    repo: Repo,
    sha: String,
    branch: Option<String>,
    slug: String,
    selection_source: super::echo::Source,
}

fn selected_source(target: Option<&str>, full_log: bool) -> crate::Result<ShareSource> {
    let Some(target) = target else {
        if std::env::var_os("AGIT_SESSION").is_none_or(|value| value.is_empty()) {
            return this_conversation_source(full_log);
        }
        let point = resolve_point(refs::parse("@")?, false)?
            .ok_or_else(|| anyhow::anyhow!("the supplied session has no local repo"))?;
        return point_source(point, full_log);
    };
    let target = target.trim();
    if target.is_empty() {
        anyhow::bail!("a share target cannot be empty");
    }
    let explicit_ref = target.contains(['@', '~', '#', ':', '/']);
    let native = if explicit_ref {
        None
    } else {
        native_link(target)?
    };
    let spec = match parse_share_ref(target) {
        Ok(spec) => spec,
        Err(error) if native.is_none() => return Err(error),
        Err(_) => return live_source(native.unwrap(), full_log),
    };
    let point = resolve_point(spec, native.is_some());
    match (point, native) {
        (Ok(Some(_)), Some(_)) => anyhow::bail!(
            "`{target}` names both a saved ref and a native session; use owner/repo@ref to select the saved point"
        ),
        (Ok(Some(point)), None) => point_source(point, full_log),
        (Ok(None), Some(native)) => live_source(native, full_log),
        (Err(error), Some(native)) if refs::is_not_found(&error) => live_source(native, full_log),
        (Err(error), _) => Err(error),
        (Ok(None), None) => anyhow::bail!(
            "`{target}` is not a native session ID; name owner/repo@ref or set AGIT_SESSION to select a saved ref"
        ),
    }
}

/// A bare share without `AGIT_SESSION` names the agent conversation the runtime's own variables
/// name, read as `agit status` and `agit commit` read them. Several are never narrowed to one, and
/// a conversation with no branch yet holds nothing a link could show.
fn this_conversation_source(full_log: bool) -> crate::Result<ShareSource> {
    let sessions = super::context::runtime_sessions();
    let refusal = |message: &str, hint: String| -> anyhow::Error {
        super::Hinted {
            message: message.to_owned(),
            hint,
        }
        .into()
    };
    match sessions.as_slice() {
        [] => Err(refusal(
            "nothing to share was named",
            "name it: `agit share <owner>/<repo>@<branch>` or `agit share <session-id>`".into(),
        )),
        [session] => {
            let Some(target) = &session.target else {
                let mut hint = live_copy(&session.session_id);
                match &session.project {
                    Some(save) if save.upload_off => hint.push_str(
                        "\nor, once this turn has ended and saved on this computer: `agit share`",
                    ),
                    Some(save) if save.sign_in == super::project::SignIn::Current => hint.push_str(
                        "\nor, once this turn has ended and uploaded by itself: `agit share`",
                    ),
                    _ => {}
                }
                return Err(refusal(
                    "this conversation has no saved turns yet, so its link would open empty; no share was created",
                    hint,
                ));
            };
            let mut point = resolve_point(parse_share_ref(target)?, false)?
                .ok_or_else(|| anyhow::anyhow!("{target} has no local AgentGit repo"))?;
            point.selection_source = super::echo::Source::ThisConversation;
            point_source(point, full_log)
        }
        several => Err(refusal(
            "this command runs inside more than one agent conversation; name the one to share",
            several
                .iter()
                .map(|session| format!("`agit share {}`", session.session_id))
                .collect::<Vec<_>>()
                .join("\n"),
        )),
    }
}

/// The step that shares conversation `session` as it is now, the turn in progress included.
fn live_copy(session: &str) -> String {
    format!(
        "`agit share {session}` makes a read-only copy of the conversation as it is now, the turn in progress included; it does not upload the conversation to the repository"
    )
}

fn parse_share_ref(target: &str) -> crate::Result<refs::RefSpec> {
    if let Some((name, _)) = target.split_once('@')
        && !name.is_empty()
        && !name.contains('/')
    {
        let mut spec = refs::parse(&format!("local/{target}"))?;
        let refs::RepoSel::Slug(_, name) = spec.repo else {
            anyhow::bail!("invalid local repository qualifier");
        };
        spec.repo = refs::RepoSel::Local(name);
        return Ok(spec);
    }
    refs::parse(target)
}

fn native_link(target: &str) -> crate::Result<Option<link::Link>> {
    // Before agit's first record on this computer there is no store, and none is made to look.
    let Some(store) = Store::open()? else {
        return discovered_native(target.trim());
    };
    let matches: Vec<_> = link::list(&store)
        .into_iter()
        .filter(|link| link.session_id.starts_with(target.trim()))
        .take(2)
        .collect();
    match matches.len() {
        0 => discovered_native(target.trim()),
        1 => Ok(matches.into_iter().next()),
        _ => anyhow::bail!("`{target}` matches several native sessions; give a longer session ID"),
    }
}

/// The folder a conversation ran in, as its own read-only snapshot records it, whatever carrier
/// the runtime keeps it in; the copy's workspace alias rests on it.
fn recorded_folder(adapter: &dyn crate::adapter::Adapter, raw: &str) -> Option<String> {
    adapter
        .parse(raw)
        .ok()
        .and_then(|session| session.cwd)
        .filter(|cwd| std::path::Path::new(cwd).is_absolute())
        .or_else(|| {
            raw.lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find_map(|record| crate::adapter::transcript_cwd::record_cwd(&record))
        })
}

/// A conversation agit has no record of yet, found read-only by its full native ID in the
/// runtimes' own stores on this computer, as the runtime variables find the current one. A prefix
/// finds nothing here, and an ID two runtimes both hold names neither.
fn discovered_native(id: &str) -> crate::Result<Option<link::Link>> {
    let limits = crate::adapter::native_snapshot::Limits::default();
    let mut found: Vec<(&str, std::path::PathBuf)> = Vec::new();
    for &runtime in crate::adapter::RUNTIMES {
        let Ok(adapter) = crate::adapter::get(runtime) else {
            continue;
        };
        let Ok(source) = adapter.lookup_native_readonly(id, limits) else {
            continue;
        };
        if !found.iter().any(|(_, path)| *path == source.path) {
            found.push((runtime, source.path));
        }
    }
    match found.as_slice() {
        [] => Ok(None),
        [(runtime, _)] => Ok(Some(link::Link::new(runtime, id, None))),
        _ => anyhow::bail!(
            "`{id}` names conversations of several runtimes on this computer; no share was created"
        ),
    }
}

fn live_source(native: link::Link, full_log: bool) -> crate::Result<ShareSource> {
    if full_log {
        anyhow::bail!(
            "--full-log needs an AgentGit ref such as owner/repo@branch; native session IDs select the live transcript"
        );
    }
    let protection_repo = match (&native.owner, &native.agent) {
        (Some(owner), Some(agent)) => {
            let (owner, agent) = super::parse_slug(&format!("{owner}/{agent}"))?;
            Some(config::repo_dir(&owner, &agent)?)
        }
        _ => None,
    };
    let runtime = if native.source == "claude-desktop" {
        "claude-code"
    } else {
        &native.source
    };
    let adapter = crate::adapter::get(runtime)?;
    let limits = crate::adapter::native_snapshot::Limits {
        bytes: crate::domain::privacy_publication::MAX_INPUT_BYTES,
        working_bytes: crate::domain::privacy_publication::MAX_INPUT_BYTES * 2,
        ..Default::default()
    };
    let source = adapter.lookup_native_readonly(&native.session_id, limits)?;
    let snapshot = adapter
        .snapshot_native_readonly(&source, limits)
        .context("cannot read native session within the privacy preparation input budget")?;
    let raw = String::from_utf8(snapshot.bytes).context("native session is not UTF-8")?;
    let cwd = native
        .cwd
        .clone()
        .or_else(|| recorded_folder(adapter.as_ref(), &raw))
        .unwrap_or_default();
    let metadata = meta::Meta::new(
        meta::session_hash(&cwd, raw.as_bytes()),
        native.source.clone(),
        cwd,
    );
    Ok(ShareSource {
        raw,
        envelope: None,
        runtime: native.source.clone(),
        label: format!(
            "live runtime transcript {}:{}",
            native.source,
            link::short(&native.session_id)
        ),
        selection_source: super::echo::Source::Explicit,
        protection_repo,
        projection_log: None,
        metadata,
        branch: native.branch.clone(),
        repository: None,
        running: None,
    })
}

fn resolve_point(
    mut spec: refs::RefSpec,
    native_available: bool,
) -> crate::Result<Option<SharePoint>> {
    let selection_source = super::echo::Source::for_spec(&spec);
    if !matches!(
        spec.tail,
        refs::Tail::None | refs::Tail::Tilde(_) | refs::Tail::Turn(_)
    ) {
        anyhow::bail!(
            "share accepts a complete saved point; event, range and file selectors are not supported"
        );
    }
    let context = if matches!(spec.base, refs::Base::At) {
        let context = super::context::at_context()?;
        spec.base = refs::Base::SessionBranch(context.branch.clone());
        Some(context)
    } else {
        None
    };
    if matches!(spec.base, refs::Base::Default) {
        anyhow::bail!("sharing a repository needs an explicit point: owner/repo@branch");
    }
    let slug = match &spec.repo {
        refs::RepoSel::Slug(owner, name) => format!("{owner}/{name}"),
        refs::RepoSel::Local(name) => {
            let me = crate::infra::credentials::current_user().unwrap_or_else(|| "local".into());
            let matches = super::clone::checkouts_named(&me, name)?;
            match matches.as_slice() {
                [only] => only.slug(),
                _ => anyhow::bail!(
                    "`{name}` does not identify a unique local repo; use owner/repo@ref"
                ),
            }
        }
        refs::RepoSel::Context => {
            if std::env::var_os("AGIT_SESSION").is_none() {
                return Ok(None);
            }
            match context.map(Ok).unwrap_or_else(super::context::at_context) {
                Ok(context) => super::context::qualify(&context.repo),
                Err(_) if native_available => return Ok(None),
                Err(error) => return Err(error),
            }
        }
    };
    let (owner, name) = super::parse_slug(&slug)?;
    let repo = match Repo::open(config::repo_dir(&owner, &name)?) {
        Some(repo) => repo,
        None if native_available && matches!(spec.repo, refs::RepoSel::Context) => return Ok(None),
        None => anyhow::bail!("{slug} has no local AgentGit repo; clone it before sharing"),
    };
    let resolved = refs::resolve(&repo, &spec)?;
    Ok(Some(SharePoint {
        repo,
        sha: resolved.sha,
        branch: resolved.branch,
        slug,
        selection_source,
    }))
}

/// Metadata, content and confirmation describe the same immutable saved point.
fn point_source(point: SharePoint, full_log: bool) -> crate::Result<ShareSource> {
    let snapshot = storage::metadata_local(point.repo.root(), &point.sha)?;
    if !snapshot.is_session_line() || snapshot.session.is_empty() {
        anyhow::bail!(
            "this point is not a settled session; select a session branch or recorded version"
        );
    }
    let sequence = if full_log {
        meta::LOG_FILE
    } else {
        meta::VIEW_FILE
    };
    let limit = crate::domain::privacy_publication::MAX_INPUT_BYTES;
    let (log, envelope) = if full_log {
        let log = storage::identity_log_at(point.repo.root(), &point.sha, snapshot.layout, limit)?;
        (log.clone(), log)
    } else {
        storage::materialize_pair_local(point.repo.root(), &point.sha, limit, limit)?
    };
    let running = running_conversation(&point, &snapshot);
    if !full_log && snapshot.layout == meta::LayoutVersion::V0 {
        let reachable: std::collections::HashSet<_> = log
            .split_inclusive('\n')
            .map(storage::event_id)
            .collect::<crate::Result<_>>()?;
        for line in envelope.split_inclusive('\n') {
            if !reachable.contains(&storage::event_id(line)?)
                && !legacy_synthetic(&storage::parse_envelope_line(line)?.content)
            {
                anyhow::bail!(
                    "this point's VIEW contains an event outside its LOG; no share was created"
                );
            }
        }
    }
    if !full_log && storage::unbalanced_view_markers(&envelope)? != 0 {
        anyhow::bail!(
            "this point's VIEW contains misplaced or mismatched markers; no share was created"
        );
    }
    let projection_log = if full_log {
        log
    } else {
        if snapshot.layout == meta::LayoutVersion::V0 {
            storage::make_view_reachable(&log, &envelope)?
        } else {
            log
        }
    };
    crate::domain::privacy_publication::check_input(&projection_log, &envelope)?;
    let (raw, skipped) = transcript::unwrap_lossy(&envelope);
    if skipped > 0 {
        anyhow::bail!(
            "this point's {sequence} contains unreadable transcript entries; no share was created"
        );
    }
    Ok(ShareSource {
        raw,
        envelope: Some(envelope),
        runtime: snapshot.runtime.clone(),
        label: format!(
            "{sequence} of {}@{}",
            point.slug,
            &point.sha[..12.min(point.sha.len())]
        ),
        selection_source: point.selection_source,
        protection_repo: Some(point.repo.root().to_path_buf()),
        projection_log: Some(projection_log),
        metadata: snapshot,
        branch: point.branch,
        repository: Some(point.slug),
        running,
    })
}

/// The one conversation writing to the branch whose tip `point` is, when it has written what
/// that tip does not hold yet. An older point of the branch, or a branch several conversations
/// write to, names none.
fn running_conversation(point: &SharePoint, snapshot: &meta::Meta) -> Option<link::Link> {
    let branch = point.branch.as_deref()?;
    let tip = point
        .repo
        .git(&[
            "rev-parse",
            "--verify",
            &format!("refs/heads/{branch}^{{commit}}"),
        ])
        .ok()?;
    if tip.trim() != point.sha {
        return None;
    }
    let (owner, name) = point.slug.split_once('/')?;
    let [conversation] = <[link::Link; 1]>::try_from(link::active_for_branch(
        &Store::open().ok()??,
        owner,
        name,
        branch,
    ))
    .ok()?;
    super::resume::holds_unsettled_work(&point.repo, &point.sha, snapshot, &conversation)
        .unwrap_or(false)
        .then_some(conversation)
}

/// The commands that give a link holding the turn `running` has not settled yet. Waiting for the
/// turn's end is offered only where its folder saves that turn into the branch by itself and
/// counts this conversation among the ones it saves; otherwise the wait would end with the branch
/// where it is.
fn including_running(source: &ShareSource, running: &link::Link) -> Vec<String> {
    let mut lines = vec![live_copy(&running.session_id)];
    let (Some(repository), Some(branch), Some(cwd)) =
        (&source.repository, &source.branch, running.cwd.as_deref())
    else {
        return lines;
    };
    let share = format!("agit share {repository}@{branch}");
    let folder = std::path::Path::new(cwd);
    if !super::project::captured_by_project(&running.source, &running.session_id, folder) {
        return lines;
    }
    match super::project::saving(folder) {
        super::project::Saving::Uploads => lines.push(format!(
            "or, once that turn has ended and uploaded by itself: `{share}`"
        )),
        super::project::Saving::Held { held, .. } if held.code == "push_auto_off" => lines.push(
            format!("or, once that turn has ended and saved on this computer: `{share}`"),
        ),
        _ => {}
    }
    lines
}

/// Only writer-shaped synthetic content may be absent from a legacy VIEW's LOG.
fn legacy_synthetic(content: &serde_json::Value) -> bool {
    let Some(object) = content.as_object() else {
        return false;
    };
    if object.len() != 3 {
        return false;
    }
    if content["type"] == "system"
        && content["source"].is_string()
        && matches!(
            content["subtype"].as_str(),
            Some(
                "agit:__merge_start__"
                    | "agit:__merge_end__"
                    | "agit:__cherry_pick_start__"
                    | "agit:__cherry_pick_end__"
                    | "agit:__revert__"
            )
        )
    {
        return true;
    }
    content["type"] == "user"
        && content["agit"] == "merge_summary"
        && content["message"]
            .as_object()
            .is_some_and(|message| message.len() == 2)
        && content["message"]["role"] == "user"
        && content["message"]["content"].is_string()
}

/// For a session whose merges recorded some of its history again, the step that shows what a
/// repaired copy would leave out. Only the listing: nothing it suggests uploads.
fn repair_hint(source: &ShareSource, error: &anyhow::Error) -> Option<String> {
    let merged = source
        .projection_log
        .as_deref()
        .is_some_and(|log| log.contains("agit:__merge_start__"));
    let repeated =
        format!("{error:#}").contains(crate::domain::privacy_publication::REPEATED_EVENT);
    let (Some(repository), Some(branch)) = (&source.repository, &source.branch) else {
        return None;
    };
    (merged && repeated).then(|| {
        format!(
            "a merge recorded some of this session's history again; `agit repair merges {repository}@{branch} --dry-run` lists what a repaired copy would leave out"
        )
    })
}

fn failed(error: anyhow::Error) -> CmdResult {
    super::fix::register_terminal_api_error(&error);
    ui::error(&format!("{error:#}"));
    Ok(super::terminal_error_code(&error, ExitCode::Network))
}

fn list(client: &crate::hub::Client) -> CmdResult {
    let shares = match client.list_shares() {
        Ok(shares) => shares,
        Err(error) => return failed(error),
    };
    if shares.is_empty() {
        println!("no active shares.");
        return Ok(ExitCode::Ok);
    }
    let rows: Vec<Vec<String>> = shares
        .iter()
        .map(|s| {
            vec![
                s.slug.clone(),
                s.url.clone(),
                s.expires_at.clone().unwrap_or_else(|| "never".into()),
            ]
        })
        .collect();
    println!("{}", ui::table::render(&["slug", "link", "expires"], &rows));
    Ok(ExitCode::Ok)
}

/// Parse `7d` / `24h` / `30m` / `never`. 0 means it never expires.
fn parse_expire(s: &str) -> crate::Result<i64> {
    let s = s.trim().to_ascii_lowercase();
    if s == "never" || s == "0" {
        return Ok(0);
    }
    let split = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| anyhow::anyhow!("durations need a unit, e.g. 24h / 7d (got: {s})"))?;
    let (num, unit) = s.split_at(split);
    let n: i64 = num
        .parse()
        .map_err(|_| anyhow::anyhow!("the number part of the duration didn’t parse: {s}"))?;
    let mult = match unit {
        "m" | "min" => 60,
        "h" | "hr" => 3600,
        "d" | "day" => 86400,
        "w" => 604800,
        other => anyhow::bail!("unknown unit `{other}` (supported: m/h/d/w)"),
    };
    Ok(n * mult)
}

/// Hash a passphrase.
///
/// sha256 with a salt prefix. This is not a password-storage-grade KDF — a share passphrase has a
/// different threat model: it is "a casually forwarded link is not readable right away", not "a
/// high-value credential is protected". The real confidentiality comes from the encryption key in
/// the fragment.
fn hash_password(p: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"agit-share-v1\0");
    h.update(p.as_bytes());
    hex::encode(h.finalize())
}

/// AES-256-GCM encryption, returning (base64url ciphertext, base64url key).
///
/// The nonce is random and prefixed to the ciphertext — the receiver takes it from the head, so
/// it needs no channel of its own.
fn encrypt(plaintext: &[u8]) -> crate::Result<(String, String)> {
    use aes_gcm::aead::{Aead, KeyInit, OsRng, rand_core::RngCore};
    use aes_gcm::{Aes256Gcm, Key, Nonce};
    use base64::Engine;

    let mut key_bytes = [0u8; 32];
    OsRng.fill_bytes(&mut key_bytes);
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key_bytes));

    let mut nonce_bytes = [0u8; 12];
    OsRng.fill_bytes(&mut nonce_bytes);

    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce_bytes), plaintext)
        .map_err(|e| anyhow::anyhow!("encryption failed: {e}"))?;

    let mut blob = nonce_bytes.to_vec();
    blob.extend_from_slice(&ct);

    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    Ok((engine.encode(&blob), engine.encode(key_bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{session, storage};

    fn claim() -> String {
        format!("{}{}", meta::ID_PREFIX, "b".repeat(meta::ID_HEX_LEN))
    }

    #[test]
    fn expire_parsing() {
        assert_eq!(parse_expire("24h").unwrap(), 86400);
        assert_eq!(parse_expire("7d").unwrap(), 604800);
        assert_eq!(parse_expire("30m").unwrap(), 1800);
        assert_eq!(parse_expire("never").unwrap(), 0);
        assert!(
            parse_expire("7").is_err(),
            "a duration without a unit is rejected"
        );
        assert!(
            parse_expire("7y").is_err(),
            "an unsupported unit is rejected"
        );
    }

    #[test]
    fn encryption_roundtrips_and_is_randomized() {
        use aes_gcm::aead::{Aead, KeyInit};
        use aes_gcm::{Aes256Gcm, Key, Nonce};
        use base64::Engine;

        let msg = b"session transcript";
        let (ct1, k1) = encrypt(msg).unwrap();
        let (ct2, _) = encrypt(msg).unwrap();
        assert_ne!(ct1, ct2, "ciphertext must not be deterministic");

        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let blob = engine.decode(&ct1).unwrap();
        let key = engine.decode(&k1).unwrap();
        let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key));
        let (nonce, body) = blob.split_at(12);
        assert_eq!(
            cipher.decrypt(Nonce::from_slice(nonce), body).unwrap(),
            msg,
            "the derived key must decrypt back to the original"
        );
    }

    #[test]
    fn encryption_and_expiry_are_the_defaults() {
        // A share command that defaults to public and never expiring is too easy to misuse.
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            a: super::Args,
        }
        let w = W::parse_from(["x"]);
        assert!(!w.a.public, "the default must be encrypted");
        assert_eq!(w.a.expire, "7d", "the default must have an expiry");
    }

    #[test]
    fn password_hash_is_salted_and_stable() {
        let a = hash_password("hunter2");
        assert_eq!(a, hash_password("hunter2"), "the hash must be stable");
        assert_ne!(a, hash_password("hunter3"));
        // not bare sha256 (a domain-separation prefix is mixed in)
        use sha2::{Digest, Sha256};
        let bare = hex::encode(Sha256::digest(b"hunter2"));
        assert_ne!(a, bare, "the hash must use a domain-separation prefix");
    }

    /// The selected session's ref owns the shared transcript even when another ref is checked out.
    /// The step "share the branch once the turn has ended" is offered only for a conversation the
    /// folder saves by itself. In a folder that uploads by itself, a Claude Code conversation gets
    /// it; an OpenCode one that agit's plugin never ran for does not, since its turn never reaches
    /// the branch. Judging by the folder's state alone gives both the step.
    #[test]
    fn waiting_for_the_turn_is_offered_only_for_a_conversation_the_folder_saves() {
        if crate::rc::in_isolated_test(
            "commands::share::tests::waiting_for_the_turn_is_offered_only_for_a_conversation_the_folder_saves",
        ) {
            return;
        }
        const HUB: &str = "https://example.test";
        let home = tempfile::tempdir().unwrap();
        unsafe {
            std::env::set_var("AGIT_HOME", home.path());
            std::env::set_var("AGIT_HUB_URL", HUB);
        }
        crate::infra::credentials::save_at(
            &home
                .path()
                .join("credentials")
                .join(format!("{}.json", config::hub_host_key(HUB).unwrap())),
            &crate::infra::credentials::HubCredential {
                account_id: Some("alice".into()),
                username: "alice".into(),
                email: None,
                hub: Some(HUB.into()),
                access_token: "fixture-access".into(),
                refresh_token: "fixture-refresh".into(),
                access_expires_at: "2099-01-01T00:00:00Z".into(),
                refresh_expires_at: "2099-01-01T00:00:00Z".into(),
            },
        )
        .unwrap();
        let folder = home.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        let folder = folder.canonicalize().unwrap();
        crate::commands::project::save_policy_for_test(
            &folder,
            "alice/app",
            &crate::hub::identity::RemoteIdentity::new(HUB, "bbbbbbbb-0000-4000-8000-000000000002")
                .unwrap(),
            "alice",
        );
        assert_eq!(
            crate::commands::project::saving(&folder),
            crate::commands::project::Saving::Uploads
        );
        let source = ShareSource {
            raw: String::new(),
            envelope: None,
            runtime: "claude-code".into(),
            label: "VIEW of alice/app@talk".into(),
            selection_source: crate::commands::echo::Source::Explicit,
            protection_repo: None,
            projection_log: None,
            metadata: meta::Meta::new_file_line(),
            branch: Some("talk".into()),
            repository: Some("alice/app".into()),
            running: None,
        };
        let lines = |runtime: &str| {
            let conversation = link::Link::new(
                runtime,
                "46246246-0000-4000-8000-000000000001",
                Some(&folder),
            );
            including_running(&source, &conversation)
        };
        let wait =
            "or, once that turn has ended and uploaded by itself: `agit share alice/app@talk`";
        assert!(lines("claude-code").iter().any(|line| line == wait));
        let opencode = lines("opencode");
        assert!(
            !opencode
                .iter()
                .any(|line| line.contains("once that turn has ended")),
            "{opencode:?}"
        );
        assert!(opencode[0].starts_with("`agit share 46246246-0000-4000-8000-000000000001`"));
    }

    #[test]
    fn a_session_in_a_linked_worktree_is_read_from_its_branch() {
        let d = tempfile::tempdir().unwrap();
        let repo = crate::domain::repo::Repo::init(&d.path().join("repo")).unwrap();
        repo.git(&["config", "commit.gpgsign", "false"]).unwrap();
        meta::write(repo.root(), &meta::Meta::new_file_line()).unwrap();
        std::fs::write(repo.root().join("AGENTS.md"), "# shared\n").unwrap();
        repo.add_all().unwrap();
        repo.commit("file line").unwrap();
        repo.git(&["branch", "session-a", "main"]).unwrap();

        let worktree = crate::commands::worktree::checkout(&repo, "session-a").unwrap();
        let raw = "{\"type\":\"user\",\"sessionId\":\"s1\",\"message\":{\"role\":\"user\",\"content\":\"BRANCH-TRANSCRIPT\"}}\n";
        let envelope = transcript::wrap_lines(raw, "claude-code", &claim());
        storage::write_snapshot(worktree.root(), &envelope, &envelope).unwrap();
        meta::write(
            worktree.root(),
            &meta::Meta::new(claim(), "claude-code".into(), "/work".into()),
        )
        .unwrap();
        worktree.add_all().unwrap();
        worktree.commit("settled session").unwrap();

        assert_eq!(repo.current_branch().as_deref(), Some("main"));
        assert!(worktree.is_linked_worktree());
        assert!(!repo.root().join(meta::LOG_FILE).exists());

        let stored = session::latest(&repo).unwrap();
        assert_eq!(stored.branch.as_deref(), Some("session-a"));
        let point = SharePoint {
            repo: Repo::open(repo.root()).unwrap(),
            sha: repo.git(&["rev-parse", "refs/heads/session-a"]).unwrap(),
            branch: Some("session-a".into()),
            slug: "me/paper".into(),
            selection_source: crate::commands::echo::Source::Explicit,
        };
        let next = transcript::wrap_lines(
            &raw.replace("BRANCH-TRANSCRIPT", "LATER-CONTENT"),
            "claude-code",
            &claim(),
        );
        storage::write_snapshot(worktree.root(), &(envelope + &next), &next).unwrap();
        worktree.add_all().unwrap();
        worktree.commit("advance selected branch").unwrap();
        let source = point_source(point, false).unwrap();
        assert!(!source.raw.contains("LATER-CONTENT"));
        assert!(source.raw.contains("BRANCH-TRANSCRIPT"), "{}", source.raw);
        assert_eq!(source.runtime, "claude-code");
        assert!(
            source.label.starts_with("VIEW of me/paper@"),
            "{}",
            source.label
        );
    }
}
