//! `agit-hub backup` / `agit-hub restore`: operator-grade, one-command snapshot + restore of the
//! Hub's durable state, so nobody has to hand-run `pg_dump` + `tar` (the manual steps documented in
//! `docs/deploying-the-hub.md`).
//!
//! The Hub's durable state lives in three places:
//!   1. the **data ROOT** — every bare git repo, `audit.log`, and (on the fs blob backend)
//!      `<root>/blobs`;
//!   2. the **metadata DB** — SQLite `hub.db` under the root, OR Postgres when `AGIT_HUB_DB` is a
//!      `postgres://` URL;
//!   3. **S3/Garage blobs** when `AGIT_HUB_S3_ENDPOINT` is set — an EXTERNAL object store that is NOT
//!      on the data root, so a tarball of the root cannot contain them.
//!
//! ## Archive layout
//!
//! A single gzip tarball. The data-root contents sit at the top level (exactly the shape the old
//! manual `tar czf … -C /data .` produced, so an operator can still read repos straight out of it),
//! plus one reserved directory the restore consumes:
//!
//! ```text
//!   ./alice/frontend.git/…        the bare repos
//!   ./audit.log                   the audit trail
//!   ./blobs/…                     the fs blob store (absent on the S3 backend)
//!   .agit-backup/manifest.json    backend kind, schema version, timestamp, external-blobs flag
//!   .agit-backup/hub.db           SQLite: a consistent VACUUM INTO snapshot (never a raw WAL cp)
//!   .agit-backup/metadata.sql     Postgres: a pg_dump instead of hub.db
//! ```
//!
//! Transient files are excluded from the root copy: the live `hub.db` and its `-wal`/`-shm` sidecars
//! (folded into the consistent snapshot above) and any `*.lock` (git ref locks, `agit-store.lock`).
//!
//! The reserved name `.agit-backup` can never collide with real data: an owner namespace segment may
//! not start with a dot (`valid_username`), so no agent ever lands there.

use std::path::{Path, PathBuf};
use std::process::Command;

use agit::hub::store::{self, Store};

use crate::cli::run_async;
use crate::{flag, has_flag, positional};

/// The reserved directory the backup carries its manifest + DB snapshot in, kept apart from the
/// data-root contents so a restore can pick it out and nothing in the root can shadow it.
const RESERVE_DIR: &str = ".agit-backup";
/// Stamped into the manifest so a future reader can tell what it is looking at.
const FORMAT: &str = "agit-hub-backup-v1";

/// Which backends the environment selects — read once, then threaded through the pure functions so
/// the tests can drive either path WITHOUT mutating process-global env (which would race in parallel).
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    /// `Some(url)` when `AGIT_HUB_DB` is a `postgres://` URL (metadata lives in Postgres); `None` =
    /// the SQLite `hub.db` under the root.
    pub pg_url: Option<String>,
    /// `AGIT_HUB_S3_ENDPOINT` is set → blobs are external and NOT in the tarball.
    pub s3_configured: bool,
}

pub(crate) fn plan_from_env() -> Plan {
    let pg_url = std::env::var("AGIT_HUB_DB").ok().map(|s| s.trim().to_string()).filter(|s| is_pg_url(s));
    let s3_configured = std::env::var("AGIT_HUB_S3_ENDPOINT").ok().map(|s| !s.trim().is_empty()).unwrap_or(false);
    Plan { pg_url, s3_configured }
}

/// Mirror of `store::is_pg_url` (private there): a `postgres://` / `postgresql://` URL selects Postgres.
fn is_pg_url(s: &str) -> bool {
    let s = s.trim();
    s.starts_with("postgres://") || s.starts_with("postgresql://")
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub(crate) struct Manifest {
    /// `agit-hub-backup-v1`.
    pub format: String,
    /// `sqlite` | `postgres` — the metadata backend the snapshot was taken from. A restore refuses to
    /// cross this (a SQLite dump into a Postgres target, or vice versa).
    pub backend: String,
    /// The schema version the DB snapshot carries.
    pub schema_version: i64,
    /// The backup timestamp. Injected by the caller: the CLI reads the real clock, tests pass a fixed
    /// value so the manifest is deterministic.
    pub created: String,
    /// True when `AGIT_HUB_S3_ENDPOINT` was set at backup time: the blobs are external and this tarball
    /// is deliberately incomplete without them. Recorded so a restore/operator is never surprised.
    pub external_blobs: bool,
    /// `filesystem` | `s3`.
    pub blob_backend: String,
    /// The live source row-counts (users, agents, tokens) at backup time, for the SQLite backend. The
    /// snapshot is verified against these at backup AND again at restore, so a short snapshot (dropped
    /// WAL frames on a live hub) is caught loudly instead of restoring a near-empty DB. `None` on the
    /// Postgres backend (its `pg_dump`/`psql` already fail loud) and on v1 backups predating this field.
    #[serde(default)]
    pub row_counts: Option<store::RowCounts>,
}

// ─────────────────────────── backup ───────────────────────────

pub(crate) fn backup_cmd(root: &Path, args: &[String]) -> i32 {
    let now = store::now_iso();
    // Default name: timestamped, in the current directory, so repeat runs never clobber each other.
    let default = format!("agit-hub-backup-{}.tgz", now.replace([':', '-'], ""));
    let out = flag(args, "--out").unwrap_or(default);
    let out = absolutize(Path::new(&out));
    let plan = plan_from_env();
    run_async(async move {
        match run_backup(root, &out, &now, &plan).await {
            Ok((manifest, warnings)) => {
                for w in &warnings {
                    eprintln!("⚠ {w}");
                }
                println!("wrote backup: {}", out.display());
                println!("  backend:    {} (schema v{})", manifest.backend, manifest.schema_version);
                println!("  blobs:      {}{}", manifest.blob_backend, if manifest.external_blobs { " (EXTERNAL, not in this tarball)" } else { "" });
                println!("  created:    {}", manifest.created);
                println!("  ⚠ This file holds the metadata DB (password + token digests). It is 0600; keep it secret and off-host.");
                agit::hub::audit::append(root, "cli", "backup", None, &format!("out={} backend={} external_blobs={}", out.display(), manifest.backend, manifest.external_blobs));
                0
            }
            Err(e) => {
                eprintln!("backup failed: {e}");
                1
            }
        }
    })
}

/// Produce the tarball. Pure w.r.t. the environment: the `Plan` is passed in, the timestamp is
/// injected. Returns the manifest it wrote plus any operator warnings (e.g. external blobs).
pub(crate) async fn run_backup(root: &Path, out: &Path, now: &str, plan: &Plan) -> Result<(Manifest, Vec<String>), String> {
    if !root.is_dir() {
        return Err(format!("data root does not exist or is not a directory: {}", root.display()));
    }
    let backend = if plan.pg_url.is_some() { "postgres" } else { "sqlite" };
    let mut warnings = Vec::new();

    let staging = tempfile::tempdir().map_err(|e| format!("failed to create a staging dir: {e}"))?;
    let reserve = staging.path().join(RESERVE_DIR);
    std::fs::create_dir_all(&reserve).map_err(|e| format!("failed to create the reserve dir: {e}"))?;

    // 1. The metadata snapshot.
    let mut row_counts = None;
    match &plan.pg_url {
        Some(url) => pg_dump(url, &reserve.join("metadata.sql"))?,
        None => {
            let s = Store::open_sqlite(root).await.map_err(|e| format!("failed to open the SQLite store at {}: {e}", root.display()))?;
            // The live source counts (through the store's own pool, which sees committed WAL frames):
            // the baseline the snapshot must reproduce.
            let source = s.row_counts().await.map_err(|e| format!("failed to read source row-counts: {e}"))?;
            let snap = reserve.join("hub.db");
            s.backup_sqlite_to(&snap).await.map_err(|e| format!("failed to snapshot hub.db: {e}"))?;
            // Verify the snapshot actually captured the live rows. On a live hub, VACUUM INTO can
            // intermittently snapshot only the pre-WAL baseline; the checkpoint in backup_to prevents
            // it, and this count check is the hard backstop — a short snapshot fails LOUD (no tarball
            // is written below), never a silent near-empty backup.
            let got = store::count_sqlite_rows(&snap).await.map_err(|e| format!("failed to read snapshot row-counts: {e}"))?;
            if got.users < source.users || got.agents < source.agents || got.tokens < source.tokens {
                return Err(format!(
                    "backup snapshot is SHORT of the live database (source users={} agents={} tokens={}, snapshot users={} agents={} tokens={}); \
                     committed data would be lost — refusing to write a partial backup",
                    source.users, source.agents, source.tokens, got.users, got.agents, got.tokens
                ));
            }
            row_counts = Some(source);
        }
    }

    // 2. The external-blobs warning: never silently produce an incomplete backup.
    if plan.s3_configured {
        warnings.push(
            "AGIT_HUB_S3_ENDPOINT is set: blob objects live in the external object store (e.g. Garage) and are NOT included in this tarball. Back the object store up separately."
                .to_string(),
        );
    }

    // 3. The manifest.
    let manifest = Manifest {
        format: FORMAT.to_string(),
        backend: backend.to_string(),
        schema_version: store::schema_version(),
        created: now.to_string(),
        external_blobs: plan.s3_configured,
        blob_backend: if plan.s3_configured { "s3".to_string() } else { "filesystem".to_string() },
        row_counts,
    };
    let mj = serde_json::to_vec_pretty(&manifest).map_err(|e| format!("failed to encode the manifest: {e}"))?;
    std::fs::write(reserve.join("manifest.json"), &mj).map_err(|e| format!("failed to write the manifest: {e}"))?;

    // 4. Build the tarball: data-root contents (minus transient files) + the reserve dir.
    create_tarball(root, staging.path(), out)?;
    Ok((manifest, warnings))
}

/// One gzip tarball: the data-root contents at the top level (excluding the live DB, its WAL sidecars,
/// and any `*.lock`), then the reserved `.agit-backup/` directory from the staging area. Shells out to
/// system `tar` (already the documented tool, and keeps the build free of a tar/flate2 dependency).
fn create_tarball(root: &Path, staging: &Path, out: &Path) -> Result<(), String> {
    // Pre-create the output at 0600 BEFORE tar writes into it, so the archive (which holds the DB and
    // password/token digests) is never world-readable, not even during the write. `tar -c -f` opens the
    // path with O_TRUNC and keeps an existing file's mode, so it stays 0600 the whole time.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(out)
            .map_err(|e| format!("failed to pre-create {} at 0600: {e}", out.display()))?;
    }
    let status = Command::new("tar")
        .arg("-c")
        .arg("-z")
        .arg("-f")
        .arg(out)
        // Fold the live WAL DB into the consistent snapshot instead of copying it raw.
        .arg("--exclude=./hub.db")
        .arg("--exclude=./hub.db-wal")
        .arg("--exclude=./hub.db-shm")
        // Transient advisory locks (git ref locks, agit-store.lock).
        .arg("--exclude=*.lock")
        .arg("-C")
        .arg(root)
        .arg(".")
        .arg("-C")
        .arg(staging)
        .arg(RESERVE_DIR)
        .status()
        .map_err(|e| tar_spawn_err("tar", &e))?;
    if !status.success() {
        return Err(format!("tar failed to create the archive (exit {:?})", status.code()));
    }
    // The tarball holds the DB + password/token digests: lock it to 0600 so it is never world-readable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o600)).map_err(|e| format!("failed to set 0600 on {}: {e}", out.display()))?;
    }
    Ok(())
}

// ─────────────────────────── restore ───────────────────────────

pub(crate) fn restore_cmd(root: &Path, args: &[String]) -> i32 {
    let Some(archive) = positional(args, 1) else {
        eprintln!("usage: agit-hub restore <file.tgz> [--root <dir>] [--force]");
        return 2;
    };
    let archive = absolutize(Path::new(archive));
    let force = has_flag(args, "--force");
    let plan = plan_from_env();
    let target_pg = plan.pg_url.clone();
    run_async(async move {
        match run_restore(root, &archive, force, target_pg.as_deref()).await {
            Ok(manifest) => {
                println!("restored into: {}", root.display());
                println!("  backend:    {} (schema v{})", manifest.backend, manifest.schema_version);
                println!("  from backup created: {}", manifest.created);
                if manifest.external_blobs {
                    eprintln!("⚠ This backup's blobs were external (S3/Garage): restore the object store separately, this tarball did not carry them.");
                }
                agit::hub::audit::append(root, "cli", "restore", None, &format!("from={} backend={}", archive.display(), manifest.backend));
                0
            }
            Err(e) => {
                eprintln!("restore failed: {e}");
                1
            }
        }
    })
}

/// Inverse of [`run_backup`]. Refuses a non-empty root without `--force`, refuses a cross-backend
/// restore, and guards every archive member against path traversal before extracting anything.
pub(crate) async fn run_restore(root: &Path, archive: &Path, force: bool, target_pg_url: Option<&str>) -> Result<Manifest, String> {
    if !archive.is_file() {
        return Err(format!("no such backup file: {}", archive.display()));
    }

    // 1. Fail-closed on a non-empty root: never clobber live data silently.
    if root_nonempty(root) && !force {
        return Err(format!(
            "refusing to restore into a non-empty data root: {} (pass --force to overwrite it)",
            root.display()
        ));
    }

    // 2. Guard EVERY member against absolute paths / `..` escape BEFORE touching the filesystem — an
    //    attacker-crafted archive must not write outside the root.
    let members = tar_list(archive)?;
    for m in &members {
        guard_member(m)?;
    }
    assert_no_link_members(archive)?;
    if !members.iter().any(|m| m == &format!("{RESERVE_DIR}/manifest.json")) {
        return Err(format!("this file is not an agit-hub backup (no {RESERVE_DIR}/manifest.json inside)"));
    }

    // 3. Read the manifest out of the archive (into a scratch dir) so the cross-backend check runs
    //    BEFORE anything lands in the root.
    let scratch = tempfile::tempdir().map_err(|e| format!("failed to create a scratch dir: {e}"))?;
    tar_extract(archive, scratch.path(), &[&format!("{RESERVE_DIR}/manifest.json")])?;
    let mj = std::fs::read(scratch.path().join(RESERVE_DIR).join("manifest.json")).map_err(|e| format!("failed to read the manifest from the archive: {e}"))?;
    let manifest: Manifest = serde_json::from_slice(&mj).map_err(|e| format!("the archive's manifest.json is not valid: {e}"))?;

    // 4. Cross-backend guard: a SQLite dump cannot be restored into a Postgres target, or vice versa.
    let target_backend = if target_pg_url.is_some() { "postgres" } else { "sqlite" };
    if manifest.backend != target_backend {
        return Err(format!(
            "cross-backend restore refused: the backup is `{}` but the target is `{}` (set AGIT_HUB_DB to match, then retry)",
            manifest.backend, target_backend
        ));
    }
    if target_backend == "postgres" && !members.iter().any(|m| m == &format!("{RESERVE_DIR}/metadata.sql")) {
        return Err(format!("the backup declares the postgres backend but carries no {RESERVE_DIR}/metadata.sql"));
    }

    // 5. Extract the whole archive into the root (members already vetted in step 2).
    std::fs::create_dir_all(root).map_err(|e| format!("failed to create the data root {}: {e}", root.display()))?;
    tar_extract(archive, root, &[])?;

    // 6. Restore the metadata from the reserved dir, then remove it from the live root.
    let reserve = root.join(RESERVE_DIR);
    match target_pg_url {
        Some(url) => {
            let sql = reserve.join("metadata.sql");
            if !sql.is_file() {
                return Err(format!("the backup has no {RESERVE_DIR}/metadata.sql to restore into Postgres"));
            }
            psql_restore(url, &sql)?;
        }
        None => {
            let src = reserve.join("hub.db");
            if !src.is_file() {
                return Err(format!("the backup has no {RESERVE_DIR}/hub.db to place under the root"));
            }
            // Verify the snapshot carries at least the row-counts the manifest recorded at backup time.
            // A snapshot short of its own manifest means WAL frames were dropped when it was taken (or
            // the file was tampered/truncated): fail loud, non-zero, rather than silently restore a
            // near-empty DB that still looks valid. `None` (v1 backup / postgres) skips the check.
            if let Some(expected) = &manifest.row_counts {
                let got = store::count_sqlite_rows(&src)
                    .await
                    .map_err(|e| format!("failed to read the snapshot's row-counts for verification: {e}"))?;
                if got.users < expected.users || got.agents < expected.agents || got.tokens < expected.tokens {
                    return Err(format!(
                        "backup snapshot is SHORT of its manifest (manifest users={} agents={} tokens={}, snapshot users={} agents={} tokens={}); \
                         this backup lost committed data and is being refused — restore an intact backup",
                        expected.users, expected.agents, expected.tokens, got.users, got.agents, got.tokens
                    ));
                }
            }
            let dest = root.join("hub.db");
            // A stale live DB (under --force) must not linger next to the restored snapshot.
            let _ = std::fs::remove_file(root.join("hub.db-wal"));
            let _ = std::fs::remove_file(root.join("hub.db-shm"));
            place_file(&src, &dest)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&reserve);
    Ok(manifest)
}

// ─────────────────────────── helpers ───────────────────────────

/// True when `root` exists and holds at least one entry.
fn root_nonempty(root: &Path) -> bool {
    std::fs::read_dir(root).map(|mut rd| rd.next().is_some()).unwrap_or(false)
}

/// Reject an archive member that could escape the extraction root: an absolute path, or any `..`
/// component. `.`-leading names (like `.agit-backup`) are fine — only literal `..` is a traversal.
fn guard_member(name: &str) -> Result<(), String> {
    let trimmed = name.trim_end_matches('/');
    if trimmed.starts_with('/') || trimmed.starts_with("~") {
        return Err(format!("refusing an archive with an absolute member path: {name:?}"));
    }
    // Windows-style drive/backslash, defensively.
    if trimmed.contains('\\') || trimmed.contains(':') {
        return Err(format!("refusing an archive with a suspicious member path: {name:?}"));
    }
    for comp in trimmed.split('/') {
        if comp == ".." {
            return Err(format!("refusing an archive with a `..` traversal member: {name:?}"));
        }
    }
    Ok(())
}

/// Reject an archive that carries a symlink or hardlink member. `..`/absolute paths are already refused
/// by [`guard_member`], but a symlink member (`evil -> /etc`) followed by a write through it can escape
/// the root on a tar that does not mitigate it. A real backup holds only plain files and dirs, so a link
/// member is always malicious or unexpected: refuse the whole archive. `tar -tv` marks the member type in
/// the first column (`l` symlink, `h` hardlink).
fn assert_no_link_members(archive: &Path) -> Result<(), String> {
    let out = Command::new("tar")
        .arg("-tvzf")
        .arg(archive)
        .output()
        .map_err(|e| tar_spawn_err("tar", &e))?;
    if !out.status.success() {
        return Err(format!("tar could not list the archive {} (exit {:?})", archive.display(), out.status.code()));
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        match line.chars().next() {
            Some('l') => return Err(format!("refusing an archive with a symlink member: {line:?}")),
            Some('h') => return Err(format!("refusing an archive with a hardlink member: {line:?}")),
            _ => {}
        }
    }
    Ok(())
}

/// List the archive's member names (`tar -tzf`).
fn tar_list(archive: &Path) -> Result<Vec<String>, String> {
    let out = Command::new("tar")
        .arg("-tzf")
        .arg(archive)
        .output()
        .map_err(|e| tar_spawn_err("tar", &e))?;
    if !out.status.success() {
        return Err(format!("tar could not read the archive {} (exit {:?})", archive.display(), out.status.code()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().map(|l| l.to_string()).filter(|l| !l.is_empty()).collect())
}

/// Extract (all members, or the named ones) into `dest`. No `-P`, so `tar` also strips leading slashes
/// as a second line of defence behind [`guard_member`].
fn tar_extract(archive: &Path, dest: &Path, members: &[&str]) -> Result<(), String> {
    let mut cmd = Command::new("tar");
    cmd.arg("-x").arg("-z").arg("-f").arg(archive).arg("-C").arg(dest);
    for m in members {
        cmd.arg(m);
    }
    let status = cmd.status().map_err(|e| tar_spawn_err("tar", &e))?;
    if !status.success() {
        return Err(format!("tar failed to extract {} (exit {:?})", archive.display(), status.code()));
    }
    Ok(())
}

/// Move `src` to `dest`, falling back to copy+remove across filesystems (the staging dir and the root
/// can be on different mounts).
fn place_file(src: &Path, dest: &Path) -> Result<(), String> {
    if std::fs::rename(src, dest).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dest).map_err(|e| format!("failed to place {} at {}: {e}", src.display(), dest.display()))?;
    let _ = std::fs::remove_file(src);
    Ok(())
}

/// Split a Postgres connection URL into a credential-free form (safe to hand to a child process in
/// argv, where any local user can read it via `ps auxww` or `/proc/<pid>/cmdline`) and the password,
/// which the caller supplies through `PGPASSWORD` instead. `/proc/<pid>/environ` is readable only by
/// the process owner, so env keeps the secret off a shared host's process table. Only the password is
/// stripped; scheme, user, host, port, database, and query params stay in the returned URL. A URL with
/// no userinfo or no password is returned unchanged with `None`.
fn split_pg_password(url: &str) -> (String, Option<String>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_string(), None);
    };
    let (authority, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let Some((userinfo, host)) = authority.rsplit_once('@') else {
        return (url.to_string(), None);
    };
    match userinfo.split_once(':') {
        Some((user, pass)) => (format!("{scheme}://{user}@{host}{tail}"), Some(pass.to_string())),
        None => (url.to_string(), None),
    }
}

/// `pg_dump` the Postgres metadata into a plain-SQL file. `--clean --if-exists` so a restore drops and
/// recreates cleanly; `--no-owner` so it does not depend on the dumping role existing on the target.
/// The password is passed via `PGPASSWORD`, never in argv; the URL we do put in argv is credential-free.
fn pg_dump(url: &str, out: &Path) -> Result<(), String> {
    let (safe_url, password) = split_pg_password(url);
    let mut cmd = Command::new("pg_dump");
    cmd.arg("--clean").arg("--if-exists").arg("--no-owner").arg("-d").arg(&safe_url).arg("-f").arg(out);
    if let Some(p) = password {
        cmd.env("PGPASSWORD", p);
    }
    let status = cmd.status().map_err(|e| tar_spawn_err("pg_dump", &e))?;
    if !status.success() {
        return Err(format!("pg_dump failed (exit {:?}); the metadata dump is incomplete, refusing to write a half backup", status.code()));
    }
    Ok(())
}

/// Restore a `pg_dump` plain-SQL file with `psql -v ON_ERROR_STOP=1`, so any statement failure aborts
/// loudly instead of leaving a silently partial database. The password goes via `PGPASSWORD`, never argv.
fn psql_restore(url: &str, sql: &Path) -> Result<(), String> {
    let (safe_url, password) = split_pg_password(url);
    let mut cmd = Command::new("psql");
    cmd.arg("-v").arg("ON_ERROR_STOP=1").arg("-d").arg(&safe_url).arg("-f").arg(sql);
    if let Some(p) = password {
        cmd.env("PGPASSWORD", p);
    }
    let status = cmd.status().map_err(|e| tar_spawn_err("psql", &e))?;
    if !status.success() {
        return Err(format!("psql failed (exit {:?}); the database may be partially restored", status.code()));
    }
    Ok(())
}

/// A spawn error, with a clear "is it on PATH?" for the common missing-binary case (never a silent
/// empty dump).
fn tar_spawn_err(bin: &str, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        format!("`{bin}` was not found on PATH; install it (or add it to PATH) and retry")
    } else {
        format!("failed to run `{bin}`: {e}")
    }
}

/// Resolve a path against the current directory if it is relative, so a later `tar -C` cannot reinterpret
/// it. Best-effort: if the cwd is unreadable, leave it as given.
fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        return p.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(p),
        Err(_) => p.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    /// Seed a SQLite hub root: two bare repos, an audit.log, a fs blob, a stray `*.lock`, and a hub.db
    /// holding a user + agent + token. Returns the fixed timestamp used everywhere so tests stay
    /// deterministic (no `now_iso` in the assertions).
    async fn seed_root(root: &Path) {
        // Two bare repos under the two-level owner/name layout.
        write(&root.join("alice").join("frontend.git").join("HEAD"), "ref: refs/heads/main\n");
        write(&root.join("alice").join("frontend.git").join("config"), "[core]\n\tbare = true\n");
        write(&root.join("bob").join("backend.git").join("HEAD"), "ref: refs/heads/main\n");
        // A transient ref lock that MUST be excluded from the backup.
        write(&root.join("alice").join("frontend.git").join("refs").join("heads").join("main.lock"), "deadbeef\n");
        // The audit trail.
        write(&root.join("audit.log"), "{\"actor\":\"cli\",\"action\":\"seed\"}\n");
        // A filesystem blob.
        write(&root.join("blobs").join("ab").join("cdef1234"), "blob-body-42");

        // The metadata DB with one user / agent / token row each.
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
        crate::cli::create_agent(&store, "frontend", "alice", agit::hub::acl::Visibility::Private).await.unwrap();
        crate::cli::issue_token(&store, "ci", "alice", Some("alice/frontend"), agit::hub::acl::Scope::Read, None).await.unwrap();
    }

    fn sqlite_plan() -> Plan {
        Plan { pg_url: None, s3_configured: false }
    }

    #[tokio::test]
    async fn backup_then_restore_round_trips_root_and_db() {
        let src = tempfile::tempdir().unwrap();
        seed_root(src.path()).await;
        let users_before = Store::open_sqlite(src.path()).await.unwrap().users().await.len();

        let outdir = tempfile::tempdir().unwrap();
        let out = outdir.path().join("backup.tgz");
        let (manifest, warnings) =
            run_backup(src.path(), &out, "2020-02-02T02:02:02Z", &sqlite_plan()).await.expect("backup succeeds");
        assert_eq!(manifest.backend, "sqlite");
        assert!(!manifest.external_blobs);
        assert_eq!(manifest.created, "2020-02-02T02:02:02Z", "the injected timestamp is what lands in the manifest");
        assert!(warnings.is_empty(), "no external-blobs warning on the fs backend");
        assert!(out.is_file());

        // Restore into a FRESH, empty root.
        let dst = tempfile::tempdir().unwrap();
        // tempdir() is created non-empty-safe: it exists but is empty, so no --force needed.
        std::fs::remove_dir(dst.path()).unwrap(); // prove restore recreates it
        run_restore(dst.path(), &out, false, None).await.expect("restore succeeds");

        // The root's durable files survive verbatim.
        assert_eq!(std::fs::read_to_string(dst.path().join("alice").join("frontend.git").join("HEAD")).unwrap(), "ref: refs/heads/main\n");
        assert_eq!(std::fs::read_to_string(dst.path().join("bob").join("backend.git").join("HEAD")).unwrap(), "ref: refs/heads/main\n");
        assert_eq!(std::fs::read_to_string(dst.path().join("audit.log")).unwrap(), "{\"actor\":\"cli\",\"action\":\"seed\"}\n");
        assert_eq!(std::fs::read_to_string(dst.path().join("blobs").join("ab").join("cdef1234")).unwrap(), "blob-body-42");
        // The transient lock was excluded.
        assert!(!dst.path().join("alice").join("frontend.git").join("refs").join("heads").join("main.lock").exists(), "*.lock is excluded from the backup");
        // The reserve dir is consumed, not left in the live root.
        assert!(!dst.path().join(RESERVE_DIR).exists(), "the .agit-backup reserve is removed after restore");
        assert!(dst.path().join("hub.db").is_file(), "hub.db is placed under the root");

        // The DB rows come back: same users/agents/tokens.
        let restored = Store::open_sqlite(dst.path()).await.unwrap();
        assert_eq!(restored.users().await.len(), users_before);
        assert_eq!(restored.user("alice").await.map(|u| u.is_admin), Some(true));
        assert_eq!(restored.agents().await.len(), 1);
        assert_eq!(restored.agent_scoped("alice", "frontend").await.map(|a| a.name), Some("frontend".to_string()));
        assert_eq!(restored.tokens().await.len(), 1);
        assert_eq!(restored.tokens().await[0].owner.as_deref(), Some("alice"));
    }

    #[tokio::test]
    async fn restore_into_nonempty_root_needs_force() {
        let src = tempfile::tempdir().unwrap();
        seed_root(src.path()).await;
        let outdir = tempfile::tempdir().unwrap();
        let out = outdir.path().join("backup.tgz");
        run_backup(src.path(), &out, "2020-02-02T02:02:02Z", &sqlite_plan()).await.unwrap();

        // A root that already holds data.
        let dst = tempfile::tempdir().unwrap();
        write(&dst.path().join("preexisting.txt"), "live data");

        // Without --force: refused, and nothing is clobbered.
        let err = run_restore(dst.path(), &out, false, None).await.unwrap_err();
        assert!(err.contains("non-empty") && err.contains("--force"), "{err}");
        assert!(dst.path().join("preexisting.txt").exists(), "the refused restore left live data intact");

        // With --force: proceeds.
        run_restore(dst.path(), &out, true, None).await.expect("--force overrides the non-empty guard");
        assert!(dst.path().join("hub.db").is_file());
        assert!(dst.path().join("alice").join("frontend.git").join("HEAD").is_file());
    }

    /// Build a minimal archive whose manifest declares an arbitrary backend, without a live DB of that
    /// kind — enough to exercise the cross-backend refusal (which fires before any DB work).
    fn fake_backup(backend: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let fake_root = dir.path().join("root");
        std::fs::create_dir_all(&fake_root).unwrap();
        write(&fake_root.join("audit.log"), "{}\n");
        let staging = dir.path().join("staging");
        let reserve = staging.join(RESERVE_DIR);
        std::fs::create_dir_all(&reserve).unwrap();
        let manifest = Manifest {
            format: FORMAT.into(),
            backend: backend.into(),
            schema_version: 2,
            created: "2020-02-02T02:02:02Z".into(),
            external_blobs: false,
            blob_backend: "filesystem".into(),
            row_counts: None,
        };
        std::fs::write(reserve.join("manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        // A stand-in for whichever payload that backend would carry.
        if backend == "postgres" {
            write(&reserve.join("metadata.sql"), "-- dummy dump\n");
        } else {
            write(&reserve.join("hub.db"), "not-a-real-db");
        }
        let out = dir.path().join("fake.tgz");
        create_tarball(&fake_root, &staging, &out).unwrap();
        (dir, out)
    }

    #[tokio::test]
    async fn cross_backend_restore_is_refused_both_ways() {
        // A SQLite backup cannot be restored into a Postgres target (target_pg_url = Some).
        let (_d1, sqlite_backup) = fake_backup("sqlite");
        let dst1 = tempfile::tempdir().unwrap();
        std::fs::remove_dir(dst1.path()).unwrap();
        let err = run_restore(dst1.path(), &sqlite_backup, false, Some("postgres://u:p@localhost/db")).await.unwrap_err();
        assert!(err.contains("cross-backend") && err.contains("sqlite") && err.contains("postgres"), "{err}");

        // A Postgres backup cannot be restored into a SQLite target (target_pg_url = None).
        let (_d2, pg_backup) = fake_backup("postgres");
        let dst2 = tempfile::tempdir().unwrap();
        std::fs::remove_dir(dst2.path()).unwrap();
        let err = run_restore(dst2.path(), &pg_backup, false, None).await.unwrap_err();
        assert!(err.contains("cross-backend"), "{err}");
        // The refusal fired before extraction: nothing landed in the target.
        assert!(!dst2.path().exists() || std::fs::read_dir(dst2.path()).unwrap().next().is_none(), "a refused cross-backend restore writes nothing");
    }

    #[tokio::test]
    async fn s3_backup_warns_and_records_external_blobs() {
        let src = tempfile::tempdir().unwrap();
        seed_root(src.path()).await;
        let outdir = tempfile::tempdir().unwrap();
        let out = outdir.path().join("backup.tgz");
        let plan = Plan { pg_url: None, s3_configured: true };
        let (manifest, warnings) = run_backup(src.path(), &out, "2020-02-02T02:02:02Z", &plan).await.unwrap();
        assert!(manifest.external_blobs, "external_blobs is recorded in the manifest");
        assert_eq!(manifest.blob_backend, "s3");
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("NOT included"), "the warning is loud about the missing blobs: {}", warnings[0]);
    }

    #[test]
    fn guard_member_rejects_traversal_and_absolute_paths() {
        assert!(guard_member("alice/frontend.git/HEAD").is_ok());
        assert!(guard_member(".agit-backup/manifest.json").is_ok());
        assert!(guard_member("./audit.log").is_ok());
        assert!(guard_member("/etc/passwd").is_err());
        assert!(guard_member("../../etc/passwd").is_err());
        assert!(guard_member("alice/../../etc/passwd").is_err());
        assert!(guard_member("a/../../b").is_err());
        assert!(guard_member("~/x").is_err());
    }

    #[test]
    fn split_pg_password_strips_only_the_password() {
        // The password comes out; scheme, user, host, port, db, and query params stay in the URL.
        let (url, pass) = split_pg_password("postgres://alice:s3cr3t@db.host:5434/agithub?sslmode=require");
        assert_eq!(url, "postgres://alice@db.host:5434/agithub?sslmode=require");
        assert_eq!(pass.as_deref(), Some("s3cr3t"));
        // A URL with a user but no password is unchanged.
        let (url, pass) = split_pg_password("postgres://alice@db.host/agithub");
        assert_eq!(url, "postgres://alice@db.host/agithub");
        assert_eq!(pass, None);
        // No userinfo at all is unchanged.
        let (url, pass) = split_pg_password("postgres://db.host/agithub");
        assert_eq!(url, "postgres://db.host/agithub");
        assert_eq!(pass, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_backup_tarball_is_created_0600() {
        use std::os::unix::fs::PermissionsExt;
        let src = tempfile::tempdir().unwrap();
        seed_root(src.path()).await;
        let outdir = tempfile::tempdir().unwrap();
        let out = outdir.path().join("backup.tgz");
        run_backup(src.path(), &out, "2020-02-02T02:02:02Z", &sqlite_plan()).await.unwrap();
        let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the tarball holds the DB + password/token digests, so it must be 0600");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_refuses_a_symlink_member() {
        // An archive whose members are otherwise clean (valid manifest, no `..`) but which carries a
        // symlink member must be refused before anything is extracted.
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("stage");
        std::fs::create_dir_all(stage.join(RESERVE_DIR)).unwrap();
        let manifest = Manifest {
            format: FORMAT.into(),
            backend: "sqlite".into(),
            schema_version: 2,
            created: "2020-02-02T02:02:02Z".into(),
            external_blobs: false,
            blob_backend: "filesystem".into(),
            row_counts: None,
        };
        std::fs::write(stage.join(RESERVE_DIR).join("manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        write(&stage.join("hub.db"), "x");
        std::os::unix::fs::symlink("/etc/passwd", stage.join("evil")).unwrap();
        // Pack WITHOUT -h so the symlink is stored as a symlink member, not dereferenced.
        let archive = dir.path().join("evil.tgz");
        assert!(Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(&stage).arg(".").status().unwrap().success());

        let dst = tempfile::tempdir().unwrap();
        std::fs::remove_dir(dst.path()).unwrap();
        let err = run_restore(dst.path(), &archive, false, None).await.unwrap_err();
        assert!(err.contains("symlink member"), "{err}");
        assert!(!dst.path().join("hub.db").exists(), "a refused restore extracts nothing");
    }

    /// Add `n` extra users to a live store WITHOUT closing it, so the freshly-committed rows sit in the
    /// `-wal` (no checkpoint) — the exact live-hub shape where `VACUUM INTO` used to snapshot only the
    /// pre-WAL baseline and silently drop them.
    async fn add_users(store: &Store, n: usize) {
        for i in 0..n {
            let salt = agit::hub::kdf::gen_salt().unwrap();
            let kdf_id = agit::hub::kdf::current_kdf_id();
            store
                .add_user(store::User {
                    username: format!("user{i:03}"),
                    pw_hash: agit::hub::kdf::hash_password("password-123", &salt, &kdf_id).unwrap(),
                    salt,
                    kdf: kdf_id,
                    is_admin: false,
                    created: "2020-01-01T00:00:00Z".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn backup_captures_rows_that_live_in_the_wal() {
        // Regression: a backup of a LIVE hub whose committed data sits in the -wal must capture ALL of
        // it. Keep a live store (and its pool) OPEN so the rows are never checkpointed by a drop, then
        // back up through a second store — the checkpoint in backup_to must fold the WAL frames in.
        let src = tempfile::tempdir().unwrap();
        seed_root(src.path()).await; // 1 user (alice) + 1 agent + 1 token
        let live = Store::open_sqlite(src.path()).await.unwrap();
        add_users(&live, 27).await; // 28 users total, all committed but sitting in the WAL
        let expected_users = live.users().await.len();
        assert_eq!(expected_users, 28, "sanity: 28 users are committed on the live store");
        assert!(src.path().join("hub.db-wal").exists(), "sanity: the writes are in the -wal, not checkpointed");

        let outdir = tempfile::tempdir().unwrap();
        let out = outdir.path().join("backup.tgz");
        let (manifest, _w) =
            run_backup(src.path(), &out, "2020-02-02T02:02:02Z", &sqlite_plan()).await.expect("backup succeeds");
        // The manifest records the live source counts, and the backup passed its own snapshot verify.
        let counts = manifest.row_counts.expect("sqlite backup records row-counts");
        assert_eq!(counts.users, 28, "the manifest records the full live user count");
        drop(live); // the live handle is no longer needed

        // Restore and prove NONE of the WAL-resident rows were dropped.
        let dst = tempfile::tempdir().unwrap();
        std::fs::remove_dir(dst.path()).unwrap();
        run_restore(dst.path(), &out, false, None).await.expect("restore succeeds");
        let restored = Store::open_sqlite(dst.path()).await.unwrap();
        assert_eq!(restored.users().await.len(), 28, "every WAL-resident user is captured, none dropped");
        assert_eq!(restored.tokens().await.len(), 1);
        assert_eq!(restored.agents().await.len(), 1);
    }

    /// Build a backup tarball around a real (small) SQLite snapshot but with a manifest whose row-counts
    /// are INFLATED past what the snapshot holds — i.e. a short/dropped-WAL backup masquerading as full.
    async fn short_backup(claimed: store::RowCounts) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        // A real snapshot holding exactly one user (alice), via the same VACUUM-INTO path.
        let seed = dir.path().join("seed");
        seed_root(&seed).await;
        let staging = dir.path().join("staging");
        let reserve = staging.join(RESERVE_DIR);
        std::fs::create_dir_all(&reserve).unwrap();
        Store::open_sqlite(&seed).await.unwrap().backup_sqlite_to(&reserve.join("hub.db")).await.unwrap();
        let manifest = Manifest {
            format: FORMAT.into(),
            backend: "sqlite".into(),
            schema_version: store::schema_version(),
            created: "2020-02-02T02:02:02Z".into(),
            external_blobs: false,
            blob_backend: "filesystem".into(),
            row_counts: Some(claimed),
        };
        std::fs::write(reserve.join("manifest.json"), serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
        let fake_root = dir.path().join("root");
        std::fs::create_dir_all(&fake_root).unwrap();
        write(&fake_root.join("audit.log"), "{}\n");
        let out = dir.path().join("short.tgz");
        create_tarball(&fake_root, &staging, &out).unwrap();
        (dir, out)
    }

    #[tokio::test]
    async fn restore_rejects_a_snapshot_short_of_its_manifest() {
        // A snapshot with FEWER rows than its own manifest claims (a dropped-WAL backup) must be refused
        // LOUD at restore — never a silent success that leaves a near-empty DB.
        let (_d, archive) = short_backup(store::RowCounts { users: 28, agents: 1, tokens: 1 }).await;
        let dst = tempfile::tempdir().unwrap();
        std::fs::remove_dir(dst.path()).unwrap();
        let err = run_restore(dst.path(), &archive, false, None).await.unwrap_err();
        assert!(err.contains("SHORT of its manifest"), "restore must reject a short snapshot loudly: {err}");
        assert!(!dst.path().join("hub.db").exists(), "a rejected short restore places no hub.db");

        // A manifest that matches the snapshot restores fine (the guard is not over-eager).
        let (_d2, ok_archive) = short_backup(store::RowCounts { users: 1, agents: 1, tokens: 1 }).await;
        let dst2 = tempfile::tempdir().unwrap();
        std::fs::remove_dir(dst2.path()).unwrap();
        run_restore(dst2.path(), &ok_archive, false, None).await.expect("an intact backup restores");
        assert!(dst2.path().join("hub.db").is_file());
    }
}
