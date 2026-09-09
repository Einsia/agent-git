//! `agit share` — mint a one-off read-only link.
//!
//! # End-to-end encryption
//!
//! Content is encrypted **locally** with AES-256-GCM and the key goes into the link fragment
//! (`#k=`). Browsers never send the fragment to the server, so the backend stores only ciphertext
//! and decryption happens in the browser.
//!
//! AES-GCM and not XChaCha20 because browsers support the former natively through WebCrypto — the
//! viewer decrypts with zero dependencies and no wasm to bundle.

use super::{CmdResult, require_login};
use crate::domain::link;
use crate::domain::meta;
use crate::domain::refs;
use crate::domain::repo::Repo;
use crate::domain::secrets;
use crate::domain::storage;
use crate::domain::store::Store;
use crate::domain::transcript;
use crate::hub::ShareRequest;
use crate::infra::config;
use crate::{ExitCode, adapter, ui};
use clap::{Args as ClapArgs, Subcommand};

#[derive(ClapArgs)]
pub struct Args {
    /// AgentGit ref or native session ID/prefix; omitted targets use AGIT_SESSION.
    #[arg(value_name = "session")]
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

pub fn run(args: Args) -> CmdResult {
    let client = require_login()?;

    match &args.cmd {
        Some(Cmd::List) => return list(&client),
        Some(Cmd::Rm { slug }) => {
            client.revoke_share(slug)?;
            ui::success(&format!("revoked {slug}"));
            return Ok(ExitCode::Ok);
        }
        None => {}
    }

    let source = match selected_source(args.target.as_deref(), args.full_log) {
        Ok(source) => source,
        Err(error) => {
            ui::error(&format!("{error:#}"));
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

    let raw = source.raw;

    // ── Scan before sharing ──
    //
    // This is less reversible than push: once a link has been visited, the content may already be
    // cached or indexed.
    // The allowlist has to really be loaded. The hint below tells the reader to add a false
    // positive to `.agit-allow-secrets`, and passing an empty set would leave that way out
    // closed — together with `AGIT_ALLOW_SECRETS` being deliberately off here, one false
    // positive could block sharing permanently with no way around it.
    let hits = secrets::scan_text(&raw, &secrets::load_allowlist(&config::agit_home()?));
    let registered = crate::domain::secret_filter::VaultStore::open_default()?.matcher()?;
    let (registered_hits, registered_truncated) =
        secrets::registered_hits_semantic_capped(&raw, 5, &registered);
    if !hits.is_empty() || !registered_hits.is_empty() {
        let reported = hits.len() + registered_hits.len();
        let qualifier = if registered_truncated {
            "at least "
        } else {
            ""
        };
        ui::error(&format!(
            "this session has {qualifier}{reported} suspected secrets — refusing to share."
        ));
        for h in hits.iter().take(5) {
            println!("  {} line {}  {}", h.rule, h.line, ui::dim(&h.redacted));
        }
        if !registered_hits.is_empty() {
            // Registered rules show only the kind and the line; they print no name/id and
            // never the matched text.
            for found in &registered_hits {
                println!(
                    "  registered-secret line {}  {}",
                    found.line,
                    ui::dim("[redacted:registered-secret]")
                );
            }
        }
        // There is deliberately **no** AGIT_ALLOW_SECRETS escape hatch here: what push sends
        // still lands in an agent that has an owner and can be made private, while a sharing
        // link is readable by anyone.
        if !hits.is_empty() {
            ui::hint(
                "if they’re false positives, add them to the store’s .agit-allow-secrets allowlist",
            );
        }
        if !registered_hits.is_empty() {
            ui::hint(
                "registered-secret rules ignore allowlists; inspect labels with `agit secrets list` and unregister one only if the value is no longer secret",
            );
        }
        return Ok(ExitCode::Failure);
    }

    // Render a readable transcript before sharing — a share exists to be read by people, not
    // parsed by machines.
    let parsed = match source.envelope.as_deref() {
        Some(envelope) => transcript::display::parse(envelope)?,
        None => {
            let rt = adapter::infer_runtime(&raw).unwrap_or(source.runtime.as_str());
            adapter::get(rt)?.parse(&raw)?
        }
    };
    let readable = ui::transcript::render_transcript(&parsed, 20000);

    let expire_secs = parse_expire(&args.expire)?;

    // Passphrase: hashed locally; the plaintext is never uploaded.
    let password_hash = if args.password {
        match ui::prompt::password("set a view passphrase")? {
            Some(p) if !p.is_empty() => Some(hash_password(&p)),
            _ => {
                ui::error("--password needs an interactive terminal to read the passphrase.");
                return Ok(ExitCode::Usage);
            }
        }
    } else {
        None
    };

    let (payload, key) = if args.public {
        (readable, None)
    } else {
        let (ct, k) = encrypt(readable.as_bytes())?;
        (ct, Some(k))
    };

    let visibility = if args.public {
        "public and unencrypted"
    } else {
        "end-to-end encrypted"
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
                "create a {visibility} share for {} (expires {expiry}, {views} views)?",
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
                ui::error("creating a share requires confirmation.");
                ui::hint(
                    "rerun with -y (or --yes) to confirm the target and visibility, or confirm from a terminal",
                );
                return Ok(ExitCode::Interactive);
            }
        }
    }

    let resp = client.create_share(&ShareRequest {
        payload,
        encrypted: !args.public,
        expire_seconds: expire_secs,
        max_views: args.views,
        password_hash,
    })?;

    let s = ui::theme::symbols();
    println!("{} share created", ui::ok(s.check));

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

struct ShareSource {
    raw: String,
    envelope: Option<String>,
    runtime: String,
    label: String,
    selection_source: super::echo::Source,
}

struct SharePoint {
    repo: Repo,
    sha: String,
    slug: String,
    selection_source: super::echo::Source,
}

fn selected_source(target: Option<&str>, full_log: bool) -> crate::Result<ShareSource> {
    let Some(target) = target else {
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
    let Some(store) = Store::open()? else {
        return Ok(None);
    };
    let matches: Vec<_> = link::list(&store)
        .into_iter()
        .filter(|link| link.session_id.starts_with(target.trim()))
        .take(2)
        .collect();
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        _ => anyhow::bail!("`{target}` matches several native sessions; give a longer session ID"),
    }
}

fn live_source(native: link::Link, full_log: bool) -> crate::Result<ShareSource> {
    if full_log {
        anyhow::bail!(
            "--full-log needs an AgentGit ref such as owner/repo@branch; native session IDs select the live transcript"
        );
    }
    Ok(ShareSource {
        raw: native.read()?,
        envelope: None,
        runtime: native.source.clone(),
        label: format!(
            "live runtime transcript {}:{}",
            native.source,
            link::short(&native.session_id)
        ),
        selection_source: super::echo::Source::Explicit,
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
    let sha = refs::resolve(&repo, &spec)?.sha;
    Ok(Some(SharePoint {
        repo,
        sha,
        slug,
        selection_source,
    }))
}

/// Metadata, content and confirmation describe the same immutable saved point.
fn point_source(point: SharePoint, full_log: bool) -> crate::Result<ShareSource> {
    let snapshot = meta::read_at_ref_result(&point.repo, &point.sha)?
        .ok_or_else(|| anyhow::anyhow!("this point has no session metadata"))?;
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
    let envelope = point
        .repo
        .show_result(&point.sha, sequence)?
        .ok_or_else(|| anyhow::anyhow!("this point has no {sequence}; no share was created"))?;
    if !full_log && snapshot.layout == meta::LayoutVersion::V0 {
        let log = point
            .repo
            .show_result(&point.sha, meta::LOG_FILE)?
            .ok_or_else(|| anyhow::anyhow!("this point has no LOG to validate its VIEW"))?;
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
    let (raw, skipped) = transcript::unwrap_lossy(&envelope);
    if skipped > 0 {
        anyhow::bail!(
            "this point's {sequence} contains unreadable transcript entries; no share was created"
        );
    }
    Ok(ShareSource {
        raw,
        envelope: Some(envelope),
        runtime: snapshot.runtime,
        label: format!(
            "{sequence} of {}@{}",
            point.slug,
            &point.sha[..12.min(point.sha.len())]
        ),
        selection_source: point.selection_source,
    })
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

fn list(client: &crate::hub::Client) -> CmdResult {
    let shares = client.list_shares()?;
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
