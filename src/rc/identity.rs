//! Machine identity and the per-hub connection credential.
//!
//! Two files under `~/.agit/rc/`:
//!
//! * `identity.json` — `{ machine_fingerprint, display_name, created_at }`.
//!   Generated once. The fingerprint is what lets the hub upsert the same
//!   `rc_connections` row on every reconnect instead of minting a new one; lose
//!   it (reinstall) and the machine looks new — which is the correct outcome.
//! * `connections/<hub-host-key>.json` — `{ connection_id, token, hub }` (0600).
//!   The token is **not** the user's API token: it is a long-lived, RC-scoped
//!   credential that can be revoked on its own from either side. Prefix
//!   `agit_rc_` so it is recognisable in logs and caught by the secret scanner.

use crate::infra::config;
use crate::infra::hub_authority::HubAuthority;
use anyhow::{Context, anyhow, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub machine_fingerprint: String,
    pub display_name: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub connection_id: String,
    pub token: String,
    pub hub: String,
    pub created_at: String,
}

fn identity_path() -> crate::Result<PathBuf> {
    let path = super::rc_dir()?.join("identity.json");
    #[cfg(windows)]
    if path.try_exists()? {
        super::windows_security::validate_path(&path, false, true)?;
    }
    Ok(path)
}

fn connections_dir() -> crate::Result<PathBuf> {
    let d = super::rc_dir()?.join("connections");
    #[cfg(windows)]
    super::windows_security::private_directory(&d)?;
    #[cfg(not(windows))]
    std::fs::create_dir_all(&d)?;
    Ok(d)
}

fn connection_path(hub: &str) -> crate::Result<PathBuf> {
    let key = config::hub_host_key(hub)?;
    Ok(connections_dir()?.join(format!("{key}.json")))
}

fn read_connection(path: &Path) -> crate::Result<Connection> {
    let metadata =
        std::fs::symlink_metadata(path).context("cannot inspect the saved RC connection")?;
    ensure!(
        metadata.is_file(),
        "the saved RC connection is not a regular file"
    );
    #[cfg(windows)]
    super::windows_security::validate_path(path, false, true)
        .map_err(|_| anyhow!("the saved RC connection is not private"))?;
    let body = std::fs::read(path).context("cannot read the saved RC connection")?;
    serde_json::from_slice(&body).map_err(|_| anyhow!("the saved RC connection is invalid"))
}

fn recognized_legacy_connections(
    dir: &Path,
    authority: &HubAuthority,
) -> crate::Result<Vec<(PathBuf, Connection)>> {
    let mut connections = Vec::new();
    for entry in std::fs::read_dir(dir).context("cannot inspect saved RC connections")? {
        let entry = entry.context("cannot inspect a saved RC connection")?;
        let path = entry.path();
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_none_or(|extension| !extension.eq_ignore_ascii_case("json"))
        {
            continue;
        }
        let Ok(connection) = read_connection(&path) else {
            continue;
        };
        if authority.matches(&connection.hub)
            && config::legacy_hub_record_matches(&path, &connection.hub)
        {
            connections.push((path, connection));
        }
    }
    Ok(connections)
}

/// Load or create the machine identity.
pub fn identity() -> crate::Result<Identity> {
    let p = identity_path()?;
    if let Ok(s) = std::fs::read_to_string(&p)
        && let Ok(id) = serde_json::from_str::<Identity>(&s)
    {
        return Ok(id);
    }
    let id = Identity {
        machine_fingerprint: uuid::Uuid::new_v4().to_string(),
        display_name: super::hostname(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    write_private(&p, &serde_json::to_string_pretty(&id)?)?;
    Ok(id)
}

pub fn set_display_name(name: &str) -> crate::Result<Identity> {
    let mut id = identity()?;
    id.display_name = name.to_string();
    write_private(&identity_path()?, &serde_json::to_string_pretty(&id)?)?;
    Ok(id)
}

pub fn connection(hub: &str) -> crate::Result<Option<Connection>> {
    let authority = HubAuthority::parse(hub)?;
    let path = connection_path(hub)?;
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let connection = read_connection(&path)?;
            ensure!(
                authority.matches(&connection.hub),
                "the saved RC connection belongs to a different Hub"
            );
            return Ok(Some(connection));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(anyhow!("cannot inspect the saved RC connection")),
    }
    let mut selected: Option<Connection> = None;
    for (_, connection) in recognized_legacy_connections(&connections_dir()?, &authority)? {
        if let Some(previous) = &selected {
            ensure!(
                previous.connection_id == connection.connection_id
                    && previous.token == connection.token
                    && previous.created_at == connection.created_at,
                "saved RC connections for this Hub conflict; pair this machine again"
            );
        } else {
            selected = Some(connection);
        }
    }
    Ok(selected)
}

pub fn save_connection(c: &Connection) -> crate::Result<()> {
    write_private(&connection_path(&c.hub)?, &serde_json::to_string_pretty(c)?)
}

pub fn remove_connection(hub: &str) -> crate::Result<bool> {
    let authority = HubAuthority::parse(hub)?;
    let path = connection_path(hub)?;
    let mut paths = Vec::new();
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {
            let connection = read_connection(&path)?;
            ensure!(
                authority.matches(&connection.hub),
                "the saved RC connection belongs to a different Hub"
            );
            paths.push(path);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(anyhow!("cannot inspect the saved RC connection")),
    }
    paths.extend(
        recognized_legacy_connections(&connections_dir()?, &authority)?
            .into_iter()
            .map(|(path, _)| path),
    );
    let removed = !paths.is_empty();
    for path in paths {
        std::fs::remove_file(path).context("cannot remove the saved RC connection")?;
    }
    Ok(removed)
}

/// Write then chmod 0600. Same discipline as `infra::credentials`.
fn write_private(p: &std::path::Path, body: &str) -> crate::Result<()> {
    #[cfg(windows)]
    super::windows_security::write_private_file(p, body.as_bytes())?;
    #[cfg(not(windows))]
    std::fs::write(p, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved_connection(hub: &str, token: &str) -> Connection {
        Connection {
            connection_id: "synthetic-connection".into(),
            token: token.into(),
            hub: hub.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn write_legacy(connection: &Connection) -> PathBuf {
        let path = connections_dir().unwrap().join(format!(
            "{}.json",
            config::legacy_hub_host_key(&connection.hub)
        ));
        write_private(&path, &serde_json::to_string(connection).unwrap()).unwrap();
        path
    }

    #[test]
    fn legacy_lookup_proves_hub_binding_without_changing_the_record() {
        let tmp = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(tmp.path(), || {
            let record = saved_connection("HTTP://127.0.0.1:8177", "synthetic-rc-token");
            let path = write_legacy(&record);
            let before = std::fs::read(&path).unwrap();
            assert!(connection("HTTP://127.0.0.1:8178").unwrap().is_none());
            let selected = connection("https://127.0.0.1:8177/other").unwrap().unwrap();
            assert_eq!(selected.token, record.token);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(
                std::fs::read_dir(connections_dir().unwrap())
                    .unwrap()
                    .count(),
                1
            );
        });
    }

    #[test]
    fn canonical_records_override_legacy_only_when_their_binding_is_valid() {
        let tmp = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(tmp.path(), || {
            let hub = "https://node.test:8177";
            let legacy = write_legacy(&saved_connection(hub, "synthetic-old-token"));
            let path = connection_path(hub).unwrap();
            for body in [
                "invalid json".to_string(),
                r#"{"connection_id":"id","token":"synthetic-token","created_at":"time"}"#.into(),
                serde_json::to_string(&saved_connection(
                    "https://foreign.test",
                    "synthetic-foreign-token",
                ))
                .unwrap(),
                serde_json::to_string(&saved_connection(
                    "http://user:secret@node.test:8177",
                    "synthetic-invalid-token",
                ))
                .unwrap(),
            ] {
                write_private(&path, &body).unwrap();
                assert!(connection(hub).is_err());
                assert!(remove_connection(hub).is_err());
                assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
                assert!(legacy.is_file());
            }
            save_connection(&saved_connection(hub, "synthetic-new-token")).unwrap();
            assert_eq!(
                connection(hub).unwrap().unwrap().token,
                "synthetic-new-token"
            );
            assert!(legacy.is_file());
        });
    }

    #[test]
    fn legacy_alias_conflicts_do_not_select_a_token_by_directory_order() {
        let tmp = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(tmp.path(), || {
            let hub = "http://node.test:8177";
            let first = saved_connection(hub, "synthetic-token");
            write_legacy(&first);
            let mut alias = saved_connection("HTTP://NODE.TEST:8177/api", "synthetic-token");
            let alias_path = write_legacy(&alias);
            assert_eq!(connection(hub).unwrap().unwrap().token, first.token);
            alias.token = "synthetic-conflicting-token".into();
            write_private(&alias_path, &serde_json::to_string(&alias).unwrap()).unwrap();
            assert!(connection(hub).is_err());
            alias.token = first.token.clone();
            alias.connection_id = "different-connection".into();
            write_private(&alias_path, &serde_json::to_string(&alias).unwrap()).unwrap();
            assert!(connection(hub).is_err());
        });
    }

    #[test]
    fn arbitrary_filenames_and_nonregular_canonical_slots_are_not_connections() {
        let tmp = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(tmp.path(), || {
            let hub = "http://node.test:8177";
            let arbitrary = connections_dir().unwrap().join("unrecognized.json");
            write_private(
                &arbitrary,
                &serde_json::to_string(&saved_connection(hub, "synthetic-token")).unwrap(),
            )
            .unwrap();
            assert!(connection(hub).unwrap().is_none());
            let path = connection_path(hub).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert!(connection(hub).is_err());
            assert!(remove_connection(hub).is_err());
            assert!(arbitrary.is_file());
        });
    }

    #[test]
    fn removing_a_hub_removes_bound_aliases_and_preserves_foreign_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(tmp.path(), || {
            let hub = "http://node.test:8177";
            let canonical = saved_connection(hub, "synthetic-current-token");
            save_connection(&canonical).unwrap();
            let legacy = write_legacy(&saved_connection(hub, "synthetic-old-token"));
            let alias = write_legacy(&saved_connection(
                "http://node.test:08177",
                "synthetic-alias-token",
            ));
            let foreign = write_legacy(&saved_connection(
                "HTTP://foreign.test:8177",
                "synthetic-foreign-token",
            ));
            let before = std::fs::read(&foreign).unwrap();
            assert!(remove_connection("HTTP://node.test:8177").unwrap());
            assert!(!legacy.exists());
            assert!(!alias.exists());
            assert!(!connection_path(hub).unwrap().exists());
            assert_eq!(std::fs::read(&foreign).unwrap(), before);
            assert!(connection(hub).unwrap().is_none());
            assert!(!remove_connection(hub).unwrap());
            assert_eq!(
                connection("https://foreign.test:8177")
                    .unwrap()
                    .unwrap()
                    .token,
                "synthetic-foreign-token"
            );
        });
    }

    #[test]
    fn invalid_hubs_never_create_connection_files() {
        let tmp = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(tmp.path(), || {
            for hub in [
                "",
                "http://user:secret@node.test",
                "http://node.test:invalid",
                "https://node.test/?private",
            ] {
                assert!(save_connection(&saved_connection(hub, "synthetic-token")).is_err());
                assert!(connection(hub).is_err());
                assert!(remove_connection(hub).is_err());
            }
            assert!(!tmp.path().join("rc").exists());
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_canonical_slots_fail_closed_without_loading_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(tmp.path(), || {
            let hub = "http://node.test:8177";
            let legacy = write_legacy(&saved_connection(hub, "synthetic-token"));
            let path = connection_path(hub).unwrap();
            std::os::unix::fs::symlink(&legacy, &path).unwrap();
            assert!(connection(hub).is_err());
            assert!(remove_connection(hub).is_err());
            assert!(legacy.is_file());
            std::fs::remove_file(&path).unwrap();
            std::os::unix::fs::symlink(tmp.path().join("missing"), &path).unwrap();
            assert!(connection(hub).is_err());
            assert!(remove_connection(hub).is_err());
        });
    }

    #[test]
    fn identity_is_stable_across_reads() {
        let tmp = tempfile::tempdir().unwrap();
        // `AGIT_HOME` is **process-wide** and the tests run multi-threaded: without the lock
        // this test and the ledger probe in `rc::mod` trample each other (one has just pointed
        // it here, the other deletes it), and the symptom is intermittent red that looks
        // unrelated to the change. See `rc::with_agit_home`.
        crate::rc::with_agit_home(tmp.path(), || {
            let a = identity().unwrap();
            let b = identity().unwrap();
            assert_eq!(a.machine_fingerprint, b.machine_fingerprint);
            assert!(!a.display_name.is_empty());
        });
    }
}
