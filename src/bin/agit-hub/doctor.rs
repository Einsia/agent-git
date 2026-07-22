//! `agit-hub doctor` — the operator/self-host diagnostic (Wave B of the debug-info design). Prints a
//! human-readable report: hub + schema version, DB backend (host/db only, never creds), per-table row
//! counts, blob backend (endpoint host + bucket only, never keys), data-root path + disk usage, health
//! checks that the DB and (if configured) the blob store are reachable, plus the registration flag and
//! listen config.
//!
//! ## Redaction discipline (mandatory, layered)
//!
//! 1. **Explicit field masks.** The DB URL is passed through [`agit::hubapi::redact_url`] (userinfo
//!    dropped, host/db/params kept); the S3 access/secret keys are shown as `set (masked)` and their
//!    values are NEVER read into the report.
//! 2. **A final scanner pass.** The whole assembled report goes through the shared secret scanner
//!    ([`agit::scan::scan_text`]) before printing; any line it flags is masked. So a credential that
//!    slipped past the field masks (an unexpected env value, a query param) is still caught.
//!
//! Reads `AGIT_HUB_DB` / `AGIT_HUB_S3_*` / `AGIT_HUB_REGISTRATION` from the environment exactly like the
//! server does, so the diagnostic reflects the same configuration a `serve` would boot with.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]

use std::path::Path;

use agit::hub::blob::Blobs;
use agit::hub::store::{self, Store};

use crate::cli::run_async;
use crate::{flag, has_flag};

/// The environment/args the report renders from. Resolved once from the process env + argv by
/// [`DoctorEnv::from_env`], then threaded into [`run_doctor`] as data — so tests drive it WITHOUT
/// mutating process-global env (which races parallel tests, exactly as the backup `Plan` avoids).
pub(crate) struct DoctorEnv {
    /// `AGIT_HUB_DB` — a `postgres://` URL selects Postgres; unset/other = the SQLite `hub.db`. May
    /// carry a password: it is only ever displayed through `redact_url`.
    pub(crate) db_url: Option<String>,
    /// `AGIT_HUB_S3_ENDPOINT` — set (non-empty) selects the S3 blob backend.
    pub(crate) s3_endpoint: Option<String>,
    /// `AGIT_HUB_S3_BUCKET`.
    pub(crate) s3_bucket: Option<String>,
    /// `AGIT_HUB_S3_ACCESS_KEY` — held only to report presence; the value is NEVER printed.
    pub(crate) s3_access_key: Option<String>,
    /// `AGIT_HUB_S3_SECRET_KEY` — held only to report presence; the value is NEVER printed.
    pub(crate) s3_secret_key: Option<String>,
    /// `AGIT_HUB_S3_REGION`.
    pub(crate) s3_region: Option<String>,
    /// `AGIT_HUB_REGISTRATION` (or `--open-registration`): self-service signup on/off.
    pub(crate) registration: bool,
    /// The listen host/port/tls the same argv would boot `serve` with (defaults mirror `serve_cmd`).
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) tls: bool,
}

impl DoctorEnv {
    fn from_env(args: &[String]) -> DoctorEnv {
        let get = |k: &str| std::env::var(k).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let registration = std::env::var("AGIT_HUB_REGISTRATION")
            .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "open" | "yes"))
            .unwrap_or(false)
            || has_flag(args, "--open-registration");
        DoctorEnv {
            db_url: get("AGIT_HUB_DB"),
            s3_endpoint: get("AGIT_HUB_S3_ENDPOINT"),
            s3_bucket: get("AGIT_HUB_S3_BUCKET"),
            s3_access_key: get("AGIT_HUB_S3_ACCESS_KEY"),
            s3_secret_key: get("AGIT_HUB_S3_SECRET_KEY"),
            s3_region: get("AGIT_HUB_S3_REGION"),
            registration,
            host: flag(args, "--host").unwrap_or_else(|| "127.0.0.1".to_string()),
            port: flag(args, "--port").and_then(|p| p.parse().ok()).unwrap_or(8177),
            tls: has_flag(args, "--tls"),
        }
    }
}

/// True when `AGIT_HUB_DB` names a Postgres backend (same predicate the store/backup use).
fn is_pg_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("postgres://") || s.starts_with("postgresql://")
}

pub(crate) fn doctor_cmd(root: &Path, args: &[String]) -> i32 {
    let env = DoctorEnv::from_env(args);
    let root = root.to_path_buf();
    run_async(async move {
        // Open the same backends `serve` would, reading the very env just captured. A failure here is
        // itself the most important diagnostic, so it is reported (non-zero) rather than swallowed.
        let store = match Store::open(&root).await {
            Ok(s) => s,
            Err(e) => {
                // The error string can only carry the DB URL (host + creds); scrub it before printing.
                println!("{}", scrub_secrets(&format!("agit-hub doctor\n\nDATABASE: FAILED to open: {e}")));
                return 1;
            }
        };
        let blobs = Blobs::open(&root).await.ok();
        let report = run_doctor(&root, &store, blobs.as_ref(), &env).await;
        println!("{report}");
        0
    })
}

/// Assemble the (already redaction-scrubbed) diagnostic report. Pure w.r.t. the process environment:
/// everything it needs arrives via `store` / `blobs` / `env`, so a test drives it directly.
pub(crate) async fn run_doctor(root: &Path, store: &Store, blobs: Option<&Blobs>, env: &DoctorEnv) -> String {
    let mut out: Vec<String> = Vec::new();
    let line = |out: &mut Vec<String>, s: String| out.push(s);

    line(&mut out, "agit-hub doctor".to_string());
    line(&mut out, String::new());

    // ── version ──
    line(&mut out, "VERSION".to_string());
    line(&mut out, format!("  hub version:    {}", env!("CARGO_PKG_VERSION")));
    line(&mut out, format!("  build sha:      {}", option_env!("AGIT_BUILD_SHA").unwrap_or("(not recorded at build time)")));
    line(&mut out, format!("  schema version: {}", store::schema_version()));
    line(&mut out, String::new());

    // ── database ── backend + host/db only, never creds.
    line(&mut out, "DATABASE".to_string());
    match env.db_url.as_deref().filter(|u| is_pg_url(u)) {
        Some(url) => {
            // redact_url drops the `user:password@` userinfo, keeping scheme/host/port/db/query.
            line(&mut out, "  backend:        postgres".to_string());
            line(&mut out, format!("  target:         {} (credentials masked)", agit::hubapi::redact_url(url)));
        }
        None => {
            line(&mut out, "  backend:        sqlite".to_string());
            line(&mut out, format!("  target:         {} (0600)", root.join("hub.db").display()));
        }
    }
    // Row counts double as the DB reachability probe: a table that answers is a table we reached.
    let counts = store.table_counts().await;
    let db_reachable = counts.iter().any(|(_, n)| n.is_some());
    line(&mut out, "  row counts:".to_string());
    for (table, n) in &counts {
        let shown = n.map(|v| v.to_string()).unwrap_or_else(|| "n/a".to_string());
        line(&mut out, format!("    {table:<22} {shown}"));
    }
    line(&mut out, format!("  health:         {}", if db_reachable { "ok (reachable)" } else { "FAILED (no table answered)" }));
    line(&mut out, String::new());

    // ── blob storage ── endpoint host + bucket only, never keys.
    line(&mut out, "BLOB STORAGE".to_string());
    match blobs {
        Some(b) => {
            // describe() = `filesystem <dir>` or `s3 <endpoint>/<bucket>` — no keys, by construction.
            line(&mut out, format!("  backend:        {}", b.describe()));
            if env.s3_endpoint.is_some() {
                line(&mut out, format!("  s3 endpoint:    {}", env.s3_endpoint.as_deref().unwrap_or("")));
                line(&mut out, format!("  s3 bucket:      {}", env.s3_bucket.as_deref().unwrap_or("(unset)")));
                line(&mut out, format!("  s3 region:      {}", env.s3_region.as_deref().unwrap_or("(default)")));
                // Presence only — the values are never read into the report.
                line(&mut out, format!("  s3 access key:  {}", mask_present(&env.s3_access_key)));
                line(&mut out, format!("  s3 secret key:  {}", mask_present(&env.s3_secret_key)));
            }
            // Reachability probe: a HEAD for a well-formed, absent object. Ok(_) = reachable (fs always
            // is; S3 answers 404); Err = the store could not be reached.
            let health = match b.exists("doctor", "healthcheck", &"0".repeat(64)).await {
                Ok(_) => "ok (reachable)".to_string(),
                Err(e) => format!("FAILED ({e})"),
            };
            line(&mut out, format!("  health:         {health}"));
        }
        None => line(&mut out, "  backend:        FAILED to open (check AGIT_HUB_S3_* configuration)".to_string()),
    }
    line(&mut out, String::new());

    // ── data root + disk ──
    line(&mut out, "DATA ROOT".to_string());
    line(&mut out, format!("  path:           {}", root.display()));
    line(&mut out, format!("  size on disk:   {}", human_bytes(dir_size(root))));
    match disk_free_bytes(root) {
        Some(free) => line(&mut out, format!("  filesystem free:{} {}", " ", human_bytes(free))),
        None => line(&mut out, "  filesystem free: (unavailable)".to_string()),
    }
    line(&mut out, String::new());

    // ── config shape ──
    line(&mut out, "CONFIG".to_string());
    line(&mut out, format!("  registration:   {}", if env.registration { "open (self-service)" } else { "invite-only" }));
    line(&mut out, format!("  listen:         {}:{}{}", env.host, env.port, if env.tls { " (TLS terminated in front)" } else { "" }));

    // Final layered-redaction pass: mask any line the shared scanner flags as secret-bearing.
    scrub_secrets(&out.join("\n"))
}

/// `set (masked)` when a credential env var is present, `not set` otherwise — the presence is useful
/// operator signal; the value is a secret and is never emitted.
fn mask_present(v: &Option<String>) -> &'static str {
    if v.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
        "set (masked)"
    } else {
        "not set"
    }
}

/// The final redaction backstop: run the shared secret scanner over the whole assembled report and mask
/// any line it flags, so a credential that slipped past the explicit field masks never reaches stdout.
/// The scanner reports a 1-based line index; the whole flagged line is replaced with a marker (never the
/// secret, not even redacted, since the surrounding label is not worth risking a partial leak).
fn scrub_secrets(report: &str) -> String {
    let findings = agit::scan::scan_text(report);
    if findings.is_empty() {
        return report.to_string();
    }
    use std::collections::HashSet;
    let flagged: HashSet<usize> = findings.iter().map(|f| f.line).collect();
    report
        .lines()
        .enumerate()
        .map(|(i, l)| {
            if flagged.contains(&(i + 1)) {
                "  [redacted: a secret-like value was masked by the scanner backstop]".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Total size of every regular file under `root` (best-effort; unreadable entries are skipped).
fn dir_size(root: &Path) -> u64 {
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Free bytes on the filesystem holding `path`, via `statvfs`. `None` off unix (or on error).
#[cfg(unix)]
fn disk_free_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path; `stat` is zeroed and only read on a 0 return.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut stat) != 0 {
            return None;
        }
        // Available blocks to an unprivileged process * fragment size.
        Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
    }
}

#[cfg(not(unix))]
fn disk_free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Human-friendly byte size (binary units). Small and dependency-free.
fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agit::hub::acl::{Scope, Visibility};

    fn base_env() -> DoctorEnv {
        DoctorEnv {
            db_url: None,
            s3_endpoint: None,
            s3_bucket: None,
            s3_access_key: None,
            s3_secret_key: None,
            s3_region: None,
            registration: false,
            host: "127.0.0.1".to_string(),
            port: 8177,
            tls: false,
        }
    }

    /// Seed a real SQLite hub: one user, one agent, one token, so the row counts are non-trivial.
    async fn seed(root: &Path) -> Store {
        let store = Store::open_sqlite(root).await.unwrap();
        let salt = agit::hub::kdf::gen_salt().unwrap();
        let kdf_id = agit::hub::kdf::current_kdf_id();
        store
            .add_user(store::User {
                username: "alice".into(),
                pw_hash: agit::hub::kdf::hash_password("password-123", &salt, &kdf_id).unwrap(),
                salt,
                kdf: kdf_id,
                is_admin: true,
                created: "2020-01-01T00:00:00Z".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        crate::cli::create_agent(&store, "frontend", "alice", Visibility::Private).await.unwrap();
        crate::cli::issue_token(&store, "ci", "alice", Some("alice/frontend"), Scope::Read, None).await.unwrap();
        store
    }

    /// The core redaction contract: a DB URL carrying a password and the S3 secret key both present, and
    /// NEITHER value appears in the doctor output — while host/db/bucket and the real row counts do.
    /// This test FAILS if a credential leaks.
    #[tokio::test]
    async fn doctor_redacts_db_password_and_s3_secret_and_counts_rows() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path()).await;
        let blobs = Blobs::open(dir.path()).await.unwrap();

        let env = DoctorEnv {
            db_url: Some("postgres://dbuser:leak-db-pass-123@db.internal:5432/agithub?sslmode=require".to_string()),
            s3_endpoint: Some("https://s3.internal:3900".to_string()),
            s3_bucket: Some("agit-blobs".to_string()),
            s3_access_key: Some("AKIA-not-a-secret".to_string()),
            s3_secret_key: Some("leak-me-s3-secret-value".to_string()),
            s3_region: Some("garage".to_string()),
            ..base_env()
        };
        let report = run_doctor(dir.path(), &store, Some(&blobs), &env).await;

        // The credentials must be ABSENT.
        assert!(!report.contains("leak-db-pass-123"), "the DB password leaked into the report:\n{report}");
        assert!(!report.contains("leak-me-s3-secret-value"), "the S3 secret key leaked into the report:\n{report}");

        // The safe, useful bits ARE shown: host, db name, bucket, backend.
        assert!(report.contains("db.internal"), "the DB host is shown: {report}");
        assert!(report.contains("agithub"), "the DB name is shown: {report}");
        assert!(report.contains("agit-blobs"), "the S3 bucket is shown: {report}");
        assert!(report.contains("postgres"), "the DB backend is named: {report}");
        assert!(report.contains("set (masked)"), "the S3 secret is reported present-but-masked: {report}");

        // Row counts reflect the seeded rows.
        assert!(report.contains("users"), "row-count table is present: {report}");
        assert!(report.contains("users                  1"), "one seeded user is counted: {report}");
        assert!(report.contains("agents                 1"), "one seeded agent is counted: {report}");
        assert!(report.contains("tokens                 1"), "one seeded token is counted: {report}");
    }

    /// The scanner backstop masks a credential that slipped past the explicit field masks entirely — an
    /// AWS key embedded in an otherwise-innocent line is replaced, not printed.
    #[test]
    fn scrub_masks_a_line_the_scanner_flags() {
        let raw = "  some field:   AKIAIOSFODNN7EXAMPLE\n  another:      fine";
        let scrubbed = scrub_secrets(raw);
        assert!(!scrubbed.contains("AKIAIOSFODNN7EXAMPLE"), "the scanner backstop must mask a leaked key: {scrubbed}");
        assert!(scrubbed.contains("[redacted:"), "the masked line carries the marker: {scrubbed}");
        assert!(scrubbed.contains("another:"), "innocent lines are left untouched: {scrubbed}");
    }

    /// The SQLite default path: no AGIT_HUB_DB, so the backend reads sqlite and the target is the hub.db.
    #[tokio::test]
    async fn doctor_shows_sqlite_backend_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = seed(dir.path()).await;
        let blobs = Blobs::open(dir.path()).await.unwrap();
        let report = run_doctor(dir.path(), &store, Some(&blobs), &base_env()).await;
        assert!(report.contains("backend:        sqlite"), "{report}");
        assert!(report.contains("hub.db"), "the sqlite target names the db file: {report}");
        assert!(report.contains("filesystem"), "the fs blob backend is shown: {report}");
        assert!(report.contains("health:         ok"), "the DB health probe passes: {report}");
    }
}
