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
use crate::domain::secrets;
use crate::domain::session;
use crate::domain::storage;
use crate::domain::store::Store;
use crate::domain::transcript;
use crate::hub::ShareRequest;
use crate::infra::config;
use crate::{ExitCode, adapter, ui};
use clap::{Args as ClapArgs, Subcommand};

#[derive(ClapArgs)]
pub struct Args {
    /// Session id / prefix / transcript path (default: the current directory's repo)
    #[arg(value_name = "session")]
    pub target: Option<String>,

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

    let source = match &args.target {
        Some(target) => {
            let Some(store) = Store::open()? else {
                ui::error("no local store yet.");
                return Ok(ExitCode::Usage);
            };
            // An explicit session target still reads the runtime transcript through the store link.
            let lk = link::find(&store, target)?;
            ShareSource {
                raw: lk.read()?,
                runtime: lk.source,
                label: format!("session {}", link::short(&lk.session_id)),
            }
        }
        None => match current_repo_source(&std::env::current_dir()?) {
            Ok(source) => source,
            Err(error) => {
                ui::error(&format!("{error:#}"));
                ui::hint(
                    "name a session explicitly, or bind this directory with `agit init` / `agit clone`",
                );
                return Ok(ExitCode::Precondition);
            }
        },
    };

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
    let rt = adapter::infer_runtime(&raw).unwrap_or(source.runtime.as_str());
    let parsed = adapter::get(rt)?.parse(&raw)?;
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
                ui::error("creating a share requires an interactive confirmation.");
                ui::hint("run `agit share` from a terminal and confirm the target and visibility");
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
    runtime: String,
    label: String,
}

/// Read the settled session belonging to the AgentGit repo bound to the current directory.
///
/// A zero-argument share must never fall back to the machine-wide newest link: that can publish
/// another project's transcript. The repo's LOG is the canonical snapshot, so it is materialized
/// before the same secret scan and rendering path used by explicit targets.
fn current_repo_source(cwd: &std::path::Path) -> crate::Result<ShareSource> {
    let slug = super::context::qualify(&super::context::repo_for(cwd)?);
    let (owner, name) = super::parse_slug(&slug)?;
    let repo = super::clone::local_store(&owner, &name)?
        .ok_or_else(|| anyhow::anyhow!("{slug} is bound here but has no local AgentGit repo"))?;
    let stored = session::latest(&repo)
        .ok_or_else(|| anyhow::anyhow!("{slug} has no settled session to share"))?;
    repo_session_source(&repo, &stored, &slug)
}

/// Read a settled session from the ref that owns it.
///
/// Session branches can live in linked worktrees while the primary checkout stays on the file
/// line. Reading the primary checkout would pair one session's identity with another ref's LOG.
fn repo_session_source(
    repo: &crate::domain::repo::Repo,
    stored: &session::Stored,
    slug: &str,
) -> crate::Result<ShareSource> {
    let envelope = match &stored.branch {
        Some(branch) => {
            storage::materialize_at(repo.root(), &format!("refs/heads/{branch}"), meta::LOG_FILE)?
        }
        None => storage::materialize_worktree(repo.root(), meta::LOG_FILE)?,
    };
    let (raw, skipped) = transcript::unwrap_lossy(&envelope);
    if skipped > 0 {
        ui::warning(&format!(
            "skipped {skipped} malformed transcript line(s) while preparing the share"
        ));
    }
    Ok(ShareSource {
        raw,
        runtime: stored.runtime.clone(),
        label: format!("{slug} ({})", meta::short(&stored.id)),
    })
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
        let source = repo_session_source(&repo, &stored, "me/paper").unwrap();
        assert!(source.raw.contains("BRANCH-TRANSCRIPT"), "{}", source.raw);
        assert_eq!(source.runtime, "claude-code");
        assert!(
            source.label.starts_with("me/paper (agit-"),
            "{}",
            source.label
        );
    }
}
