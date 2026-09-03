//! `agit upgrade` — bring the CLI itself up to the latest version the hub names.
//!
//! ## The whole trust chain
//!
//! 1. "What is the latest" is answered by the hub (`GET /api/cli/version`; the backend resolves
//!    it live from GitHub tags and caches it; a tag is a name, a commit is an identity — the
//!    backend's field design comments on that split). An older self-hosted hub has no such
//!    endpoint: on 404, `upgrade` says plainly "your hub does not know the latest version"
//!    instead of guessing.
//! 2. "Is it newer than mine" compares the semver inside the tag — a commit hash cannot say
//!    newer or older, only same or different.
//! 3. "Re-download" pulls this platform's asset `agit-<VERSION>-<triple>.tar.gz` from GitHub
//!    Releases (the naming contract of release.yml; the archive holds a bare `agit`) and checks
//!    it against the matching line in the same release's `SHA256SUMS` — the checksum string
//!    comes from the release too, so the trust anchor is the GitHub account's control over its
//!    releases, and the hub only points the way.
//! 4. Persisting is a temp file plus an atomic `rename`: lose power mid-upgrade and either the
//!    old binary is there unchanged or the new one is fully in place, never half an executable.
//!
//! ## The passing nudge (the passive path)
//!
//! User-facing command startup calls the nudge path: the local cache is consulted once a day,
//! only an expired one sends a short-timeout request, and a newer version
//! prints one unobtrusive line. A slow network, an old hub, or GitHub being down are all just "no
//! nudge today", not an error. JSON, quiet, CI, and internal hook/MCP paths never get extra
//! output or a network request. Startup notices go to stderr so a piped command's stdout remains
//! usable by a caller that is not expecting a notice.

use super::CmdResult;
use crate::hub::client::Client;
use crate::{ExitCode, ui};
use anyhow::{Context as _, Result, bail};
use clap::Args as ClapArgs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(ClapArgs)]
pub struct Args {
    /// Report only, don’t download.
    #[arg(long)]
    check: bool,
}

/// Nudge cache: one check per day. A failed write is not an error — at worst it asks again
/// tomorrow.
const NUDGE_INTERVAL_SECS: u64 = 24 * 3600;
/// Incidental checks must fail quickly. The explicit `agit upgrade` command keeps the normal
/// 30-second hub timeout; a startup hint is best-effort and must not make a command feel stuck.
const NUDGE_TIMEOUT_SECS: u64 = 2;

pub fn run(args: Args) -> CmdResult {
    if !crate::infra::config::is_production_release() {
        ui::error(&format!(
            "self-upgrade is disabled for the {} channel.",
            crate::infra::config::RELEASE_CHANNEL
        ));
        ui::hint("download a fresh artifact from the GitLab pipeline that built this CLI.");
        return Ok(ExitCode::Usage);
    }

    let cur = env!("CARGO_PKG_VERSION");
    let client = Client::from_env();

    let latest = match client.cli_version() {
        Ok(v) => v,
        Err(e) => {
            if let Some(api) = e.downcast_ref::<crate::hub::client::ApiError>()
                && api.status == 404
            {
                ui::error(&format!(
                    "this hub ({}) doesn't know the latest CLI version.",
                    client.base()
                ));
                ui::hint("it's running an older backend — upgrade the hub, or build from source.");
                return Ok(ExitCode::Network);
            }
            ui::error(&format!("couldn't ask the hub for the latest version: {e}"));
            return Ok(ExitCode::Network);
        }
    };

    match compare(cur, &latest.version) {
        Ordering::Less => {
            println!(
                "agit {} is available (you have {cur}).",
                ui::accent(&latest.version)
            );
            if latest.stale {
                println!(
                    "  {}",
                    ui::dim("served from cache — the hub couldn't reach GitHub just now")
                );
            }
            if args.check {
                println!("  run `agit upgrade` to install it: {}", latest.url);
                return Ok(ExitCode::Ok);
            }
        }
        Ordering::Same | Ordering::Newer => {
            println!(
                "{} agit {cur} is up to date.",
                ui::ok(ui::theme::symbols().check)
            );
            return Ok(ExitCode::Ok);
        }
    }

    if let Err(e) = download_and_replace(&latest) {
        ui::error(&format!("upgrade failed: {e:#}"));
        ui::hint(
            "fallback: `npx create-agit` again, or `cargo install --path .` from a fresh clone.",
        );
        return Ok(ExitCode::Network);
    }
    ui::success(&format!("upgraded to {}", latest.version));
    Ok(ExitCode::Ok)
}

// ── Version comparison ────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum Ordering {
    Less,
    Same,
    Newer,
}

/// semver tuple comparison; a prerelease suffix (`-rc.1`) sorts before the same release number.
/// Deliberately not the full semver spec — the only question here is whether to offer a download.
fn parse(v: &str) -> Option<(u64, u64, u64, bool)> {
    // A release tag looks like `agit-v0.1.0` (the README release flow); the version the server
    // returns is the stripped "0.1.0". Same convention on both sides: strip `agit-`, then `v`.
    let v = v.strip_prefix("agit-").unwrap_or(v);
    let v = v.strip_prefix('v').unwrap_or(v);
    let (core, stable) = match v.split_once('-') {
        Some((c, _)) => (c, false), // a suffix = prerelease
        None => (v, true),
    };
    let mut it = core.split('.');
    let mut parts = [0u64; 3];
    for (i, slot) in parts.iter_mut().enumerate() {
        match it.next() {
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
                *slot = p.parse().ok()?;
            }
            None if i == 2 => {} // v1.2 counts as v1.2.0
            _ => return None,
        }
    }
    if it.next().is_some() {
        return None;
    }
    Some((parts[0], parts[1], parts[2], stable))
}

fn compare(current: &str, latest: &str) -> Ordering {
    let (Some(c), Some(l)) = (parse(current), parse(latest)) else {
        // An unparsable version cannot be ordered — silently answer "same" and never nudge.
        return Ordering::Same;
    };
    if l.0 != c.0 || l.1 != c.1 || l.2 != c.2 {
        return if (l.0, l.1, l.2) > (c.0, c.1, c.2) {
            Ordering::Less
        } else {
            Ordering::Newer
        };
    }
    match (c.3, l.3) {
        (a, b) if a == b => Ordering::Same,
        (false, true) => Ordering::Less, // mine is a prerelease, theirs is a release → upgrade
        _ => Ordering::Newer,
    }
}

// ── Download and atomic replace ───────────────────────────────────────

/// The targets release.yml builds (Windows is not in the matrix — install from source there).
fn triple() -> Result<&'static str> {
    let t = std::env::consts::ARCH;
    let os = std::env::consts::OS;
    match (os, t) {
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        _ => bail!(
            "no prebuilt binary for {os}/{t} — build from source: git clone https://github.com/Einsia/agent-git && ./setup.sh"
        ),
    }
}

fn asset_name(version: &str) -> Result<String> {
    Ok(format!("agit-{version}-{}.tar.gz", triple()?))
}

fn download(url: &str) -> Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .user_agent(concat!("agit/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();
    let mut resp = agent
        .get(url)
        .call()
        .with_context(|| format!("GET {url}"))?;
    let mut buf = Vec::new();
    resp.body_mut().as_reader().read_to_end(&mut buf)?;
    Ok(buf)
}

fn download_and_replace(latest: &crate::hub::CliVersion) -> Result<()> {
    if !latest.npm_package.is_empty() {
        return download_from_npm(latest);
    }
    let repo = if latest.repo.is_empty() {
        "Einsia/agent-git"
    } else {
        &latest.repo
    };
    let asset = asset_name(&latest.version)?;
    let base = format!(
        "https://github.com/{repo}/releases/download/{tag}",
        tag = latest.tag
    );

    println!("{}", ui::dim(&format!("downloading {asset} from {base}")));
    let tarball = download(&format!("{base}/{asset}"))?;
    // SHA256SUMS is one file with a line per asset (release.yml): find this line by bare
    // filename; a missing line = an incomplete release — better not to upgrade than install blind.
    let sums = download(&format!("{base}/SHA256SUMS"))
        .context("SHA256SUMS missing — the release is incomplete, not upgrading blind")?;
    let sums = String::from_utf8(sums)?;
    let expect = sums
        .lines()
        .find_map(|l| {
            l.split_whitespace()
                .collect::<Vec<_>>()
                .split_first()
                .filter(|(_, rest)| rest.first() == Some(&asset.as_str()))
                .map(|(h, _)| h.to_string())
        })
        .with_context(|| format!("SHA256SUMS has no line for {asset}"))?;

    use sha2::Digest as _;
    let got = hex::encode(sha2::Sha256::digest(&tarball));
    if got != expect {
        bail!("checksum mismatch (got {got}, want {expect}) — not installing");
    }

    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("current_exe has no parent")?;
    let tmp_tar: PathBuf = dir.join(format!(".agit-upgrade-{}.tar.gz", std::process::id()));
    std::fs::write(&tmp_tar, &tarball)?;
    // Unpack into a separate temp directory, not straight into the exe directory: the bare `agit`
    // in the archive must not touch the running binary — only the final atomic rename replaces
    // it. GNU tar's --transform is out too (macOS bsdtar does not know that flag).
    let unpack: PathBuf = dir.join(format!(".agit-unpack-{}", std::process::id()));
    std::fs::create_dir(&unpack)?;
    let out = std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(&tmp_tar)
        .arg("-C")
        .arg(&unpack)
        .output()
        .context("failed to run tar (it is needed to unpack the release asset)");
    let _ = std::fs::remove_file(&tmp_tar);
    let out = out?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&unpack);
        bail!("tar failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let bytes = std::fs::read(unpack.join("agit"))
        .context("the archive did not contain a bare `agit` binary")?;
    let _ = std::fs::remove_dir_all(&unpack);
    atomic_replace(&exe, &bytes)
}

fn atomic_replace(exe: &Path, bytes: &[u8]) -> Result<()> {
    let dir = exe.parent().context("current_exe has no parent")?;
    let tmp: PathBuf = dir.join(format!(".agit-upgrade-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    // rename is atomic: processes reading the exe (this one included) keep the old inode; the
    // next `agit` invocation gets the new one. A rename inside one directory stays on one
    // filesystem.
    std::fs::rename(&tmp, exe).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

// ── npm registry path ─────────────────────────────────────────────────

/// The platform key in node's spelling: `@einsia/agent-git-<key>`. The same key set that
/// npm/lib/platform.js and publish.mjs use.
fn npm_platform_key() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Ok("linux-x64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        ("macos", "x86_64") => Ok("darwin-x64"),
        ("macos", "aarch64") => Ok("darwin-arm64"),
        (os, t) => bail!(
            "no prebuilt binary for {os}/{t} — build from source: git clone https://github.com/Einsia/agent-git && ./setup.sh"
        ),
    }
}

/// Take this platform's subpackage tgz from the registry, check it against the packument's SRI
/// (sha512 base64), unpack package/bin/agit, replace atomically. The registry address is
/// overridable (a self-hosted mirror).
fn download_from_npm(latest: &crate::hub::CliVersion) -> Result<()> {
    use sha2::Digest as _;
    let registry = std::env::var("AGIT_NPM_REGISTRY")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://registry.npmjs.org".into());
    let pkg = format!("{}-{}", latest.npm_package, npm_platform_key()?);
    let meta_url = format!("{registry}/{}/{}", pkg.replace('/', "%2F"), latest.version);

    println!(
        "{}",
        ui::dim(&format!("fetching {pkg}@{} metadata", latest.version))
    );
    let meta = download(&meta_url).with_context(|| format!("GET {meta_url}"))?;
    let meta: serde_json::Value =
        serde_json::from_slice(&meta).context("registry metadata is not JSON")?;
    let tarball_url = meta
        .pointer("/dist/tarball")
        .and_then(|v| v.as_str())
        .with_context(|| format!("{pkg}@{} has no dist.tarball", latest.version))?
        .to_string();
    let integrity = meta
        .pointer("/dist/integrity")
        .and_then(|v| v.as_str())
        .with_context(|| format!("{pkg}@{} has no dist.integrity", latest.version))?
        .to_string();
    let expect = integrity
        .strip_prefix("sha512-")
        .context("only sha512 SRI is supported")?
        .to_string();

    println!("{}", ui::dim(&format!("downloading {tarball_url}")));
    let tgz = download(&tarball_url)?;
    let got = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(&tgz))
    };
    if got != expect {
        bail!("integrity mismatch (got sha512-{got}, want {integrity}) — not installing");
    }

    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("current_exe has no parent")?;
    // Same rule as the GitHub path: unpack into a separate temp directory; only the final atomic
    // rename replaces the binary.
    let unpack: PathBuf = dir.join(format!(".agit-unpack-{}", std::process::id()));
    std::fs::create_dir(&unpack)?;
    let tmp_tgz = unpack.join("pkg.tgz");
    std::fs::write(&tmp_tgz, &tgz)?;
    let out = std::process::Command::new("tar")
        .args(["-xzf"])
        .arg(&tmp_tgz)
        .arg("-C")
        .arg(&unpack)
        .output()
        .context("failed to run tar (needed to unpack the npm tarball)")?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(&unpack);
        bail!("tar failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let bytes = std::fs::read(unpack.join("package/bin/agit"))
        .context("the platform package did not contain package/bin/agit")?;
    let _ = std::fs::remove_dir_all(&unpack);
    atomic_replace(&exe, &bytes)
}

// ── The passing nudge ─────────────────────────────────────────────────

struct NudgeCache {
    checked_at: u64,
    latest: String,
}

fn nudge_path() -> Option<PathBuf> {
    Some(
        crate::infra::config::agit_home()
            .ok()?
            .join("cli-update.json"),
    )
}

fn load_cache() -> Option<NudgeCache> {
    let text = std::fs::read_to_string(nudge_path()?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(NudgeCache {
        checked_at: v.get("checked_at")?.as_u64()?,
        latest: v.get("latest")?.as_str()?.to_string(),
    })
}

fn save_cache(latest: &str) {
    let Some(p) = nudge_path() else { return };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = serde_json::json!({"checked_at": now, "latest": latest});
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(p, serde_json::to_string(&body).unwrap());
}

/// Check for an update at a user-facing CLI startup.
///
/// Machine-readable output, quiet mode, CI, and the internal hook/MCP commands must skip both
/// the network request and the notice. Eligible user-facing invocations share the daily cache.
pub fn maybe_startup_nudge(command: &str, json: bool) {
    let quiet = std::env::var_os("AGIT_QUIET").is_some();
    let ci = std::env::var_os("CI").is_some();
    if !startup_nudge_allowed(command, json, quiet, ci) {
        return;
    }
    maybe_nudge_with_timeout(std::time::Duration::from_secs(NUDGE_TIMEOUT_SECS));
}

fn startup_nudge_allowed(command: &str, json: bool, quiet: bool, ci: bool) -> bool {
    !json && !quiet && !ci && !matches!(command, "upgrade" | "hooks" | "mcp")
}

fn maybe_nudge_with_timeout(timeout: std::time::Duration) {
    if !crate::infra::config::is_production_release() {
        return;
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cached = load_cache();
    if let Some(c) = &cached
        && now.saturating_sub(c.checked_at) < NUDGE_INTERVAL_SECS
    {
        return nudge_with(&c.latest);
    }
    // A failed ask (old hub, upstream down) still stamps the time: otherwise every eligible
    // startup sends another request, and "one failure costs a day" must not become "one failure
    // costs every invocation".
    let Ok(latest) = Client::from_env_with_timeout(timeout).cli_version() else {
        save_cache("");
        return;
    };
    save_cache(&latest.version);
    nudge_with(&latest.version);
}

fn nudge_with(latest: &str) {
    if !latest.is_empty() && compare(env!("CARGO_PKG_VERSION"), latest) == Ordering::Less {
        let message = ui::dim(&format!("agit {latest} is available — use `agit upgrade`"));
        eprintln!("{message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_compare_matrix() {
        assert_eq!(compare("0.1.0", "0.1.1"), Ordering::Less);
        assert_eq!(compare("0.1.0", "0.2.0"), Ordering::Less);
        assert_eq!(compare("0.1.0", "1.0.0"), Ordering::Less);
        assert_eq!(compare("0.1.0", "0.1.0"), Ordering::Same);
        assert_eq!(compare("1.0.0", "0.9.9"), Ordering::Newer);
        assert_eq!(compare("0.2.0-rc.1", "0.2.0"), Ordering::Less);
        assert_eq!(compare("0.2.0", "0.2.0-rc.1"), Ordering::Newer);
        // an unparsable version nudges nobody
        assert_eq!(compare("dev-build", "0.2.0"), Ordering::Same);
        assert_eq!(compare("0.1.0", "garbage"), Ordering::Same);
        assert_eq!(compare("0.1.0", "v0.2"), Ordering::Less);
    }

    #[test]
    fn asset_names_cover_shipped_platforms() {
        let n = asset_name("0.1.0");
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(n.unwrap(), "agit-0.1.0-x86_64-unknown-linux-musl.tar.gz");
        let _ = n;
    }

    #[test]
    fn startup_nudge_is_only_for_interactive_user_commands() {
        assert!(startup_nudge_allowed("run", false, false, false));
        assert!(startup_nudge_allowed("resume", false, false, false));
        assert!(startup_nudge_allowed("push", false, false, false));
        for command in ["upgrade", "hooks", "mcp"] {
            assert!(!startup_nudge_allowed(command, false, false, false));
        }
        assert!(!startup_nudge_allowed("run", true, false, false));
        assert!(!startup_nudge_allowed("run", false, true, false));
        assert!(!startup_nudge_allowed("run", false, false, true));
    }
}
