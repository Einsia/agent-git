//! Portable policy scopes require an authenticated observation of the current remote selector.

use super::{crypto::Owner, decisions, keys, storage::Store};
use crate::{domain::repo::Repo, hub::identity, infra::hub_authority::HubAuthority};
use anyhow::{Context, ensure};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Selector {
    common: PathBuf,
    root: PathBuf,
    owner: Owner,
    hub: String,
    remote: String,
    slug: String,
    expected: Option<String>,
}

fn selector(path: &Path, owner: &Owner) -> crate::Result<Selector> {
    let repo = Repo::at(path);
    let hub = crate::infra::config::hub_url();
    ensure!(
        HubAuthority::parse(&hub)?.label() == owner.hub,
        "privacy Hub changed"
    );
    let credential =
        crate::infra::credentials::load(&hub).context("privacy account is unavailable")?;
    ensure!(
        credential.account_id.as_deref() == Some(owner.account.as_str()),
        "privacy account changed"
    );
    let remote = repo
        .remote_url()
        .context("privacy repository has no selected remote")?;
    let normalized = identity::normalize_hub(&remote)?;
    let (prefix, name) = normalized
        .rsplit_once('/')
        .context("invalid privacy remote")?;
    let (remote_hub, user) = prefix.rsplit_once('/').context("invalid privacy remote")?;
    ensure!(
        identity::normalize_hub(remote_hub)? == identity::normalize_hub(&hub)?,
        "privacy remote selects another Hub"
    );
    let name = name.strip_suffix(".git").unwrap_or(name);
    ensure!(
        [user, name].into_iter().all(|part| !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))),
        "invalid privacy repository selector"
    );
    let expected = identity::expected_for_transport(&repo, &hub)?.map(|id| id.agent_id);
    Ok(Selector {
        common: repo.common_dir()?.canonicalize()?,
        root: path.canonicalize()?,
        owner: owner.clone(),
        hub,
        remote,
        slug: format!("{user}/{name}"),
        expected,
    })
}

/// Register interest without contacting the Hub. Failure clears any previously observed scope.
pub fn remember(store: &mut Store, path: &Path) -> crate::Result<()> {
    let common = crate::domain::repo::common_git_dir(path).canonicalize()?;
    let root = path.canonicalize()?;
    let selected = store.owner().and_then(|owner| selector(path, owner).ok());
    let encoded = selected.as_ref().map(serde_json::to_string).transpose()?;
    let tx = store
        .db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let old: Option<Option<String>> = tx
        .query_row(
            "SELECT selector FROM policy_repositories WHERE common=?1 AND root=?2",
            params![common.to_string_lossy(), root.to_string_lossy()],
            |row| row.get(0),
        )
        .optional()?;
    if old.is_none() {
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM policy_repositories", [], |row| {
            row.get(0)
        })?;
        ensure!(
            count < 1024,
            "privacy repository associations exceed their budget"
        );
    }
    if old.as_ref() != Some(&encoded) {
        tx.execute(
            "INSERT INTO policy_repositories(common,root,selector,scope) VALUES (?1,?2,?3,NULL)
            ON CONFLICT(common,root) DO UPDATE SET selector=excluded.selector,scope=NULL",
            params![common.to_string_lossy(), root.to_string_lossy(), encoded],
        )?;
        tx.execute(
            "UPDATE metadata SET value=CAST(value AS INTEGER)+1 WHERE key='policy_generation'",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Workers accept an association only while the complete local selector still matches it.
pub fn scope(store: &Store, path: &Path) -> Option<String> {
    let selected = selector(path, store.owner()?).ok()?;
    let encoded = serde_json::to_string(&selected).ok()?;
    store
        .db
        .query_row(
            "SELECT scope FROM policy_repositories WHERE common=?1 AND root=?2 AND selector=?3",
            params![
                selected.common.to_string_lossy(),
                selected.root.to_string_lossy(),
                encoded
            ],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .ok()
        .flatten()
        .flatten()
}

fn save(store: &mut Store, selected: &Selector, scope: Option<String>) -> crate::Result<()> {
    // Authentication or remote selection can change during the request.
    ensure!(
        selector(&selected.root, &selected.owner)? == *selected,
        "privacy repository changed during synchronization"
    );
    let encoded = serde_json::to_string(selected)?;
    let tx = store
        .db
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute("UPDATE policy_repositories SET scope=?4 WHERE common=?1 AND root=?2 AND selector=?3 AND scope IS NOT ?4",
        params![selected.common.to_string_lossy(),selected.root.to_string_lossy(),encoded,scope])?;
    if changed != 0 {
        tx.execute(
            "UPDATE metadata SET value=CAST(value AS INTEGER)+1 WHERE key='policy_generation'",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// An unavailable lookup is not evidence against an association whose local selector still
/// matches. Only a successful response can select a new scope or invalidate the observed identity.
fn resolve(client: &crate::hub::Client, selected: &Selector) -> crate::Result<Option<String>> {
    let (user, name) = selected
        .slug
        .split_once('/')
        .context("invalid privacy selector")?;
    let remote = client.get_agent(user, name)?;
    ensure!(
        remote.owner == user && remote.name.eq_ignore_ascii_case(name),
        "privacy repository response differs from its selector"
    );
    let observed = identity::RemoteIdentity::new(client.base(), &remote.agent_id)?;
    observed_scope(selected, &observed)
}

fn observed_scope(
    selected: &Selector,
    observed: &identity::RemoteIdentity,
) -> crate::Result<Option<String>> {
    ensure!(
        identity::normalize_hub(&selected.hub)? == observed.hub,
        "privacy repository observation belongs to another Hub"
    );
    let expected = selected
        .expected
        .as_deref()
        .map(|id| identity::RemoteIdentity::new(&selected.hub, id))
        .transpose()?;
    if expected
        .as_ref()
        .is_some_and(|expected| expected != observed)
    {
        return Ok(None);
    }
    Ok(Some(decisions::repository_scope(
        &observed.hub,
        &observed.agent_id,
    )?))
}

pub fn refresh(
    store: &mut Store,
    client: &crate::hub::Client,
    deadline: Instant,
) -> crate::Result<()> {
    let roots: Vec<String> = store
        .db
        .prepare("SELECT root FROM policy_repositories ORDER BY checked,common,root LIMIT 64")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for root in roots {
        if Instant::now() >= deadline {
            break;
        }
        let path = Path::new(&root);
        store.db.execute(
            "UPDATE policy_repositories SET checked=?1 WHERE root=?2",
            params![chrono::Utc::now().timestamp_millis(), root],
        )?;
        if remember(store, path).is_err() {
            continue;
        }
        let Some(selected) = store.owner().and_then(|owner| selector(path, owner).ok()) else {
            continue;
        };
        if let Ok(resolved) = resolve(client, &selected) {
            let _ = save(store, &selected, resolved);
        }
    }
    Ok(())
}

/// Fresh authenticated observations update every matching registration before callers reject an
/// old identity. Cached responses cannot overwrite a newer observation from another client.
pub(crate) fn observe(client: &crate::hub::Client, user: &str, name: &str, id: &str) {
    let _ = (|| -> crate::Result<()> {
        let observed = identity::RemoteIdentity::new(client.base(), id)?;
        let account = client
            .credential_snapshot()
            .and_then(|credential| credential.account_id)
            .context("privacy account is unavailable")?;
        let owner = Owner {
            hub: HubAuthority::parse(client.base())?.label(),
            account,
        };
        let directory = keys::owner_directory(&keys::root()?, &owner).join("dictionary");
        if !directory.join("journal.sqlite").is_file() {
            return Ok(());
        }
        let store = Store::read_only(&directory, Some(owner.clone()))?;
        let registrations: Vec<(String, Option<String>)> = store
            .db
            .prepare(
                "SELECT selector,scope FROM policy_repositories WHERE selector IS NOT NULL LIMIT 1024",
            )?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        let mut changes = Vec::new();
        for (encoded, previous_scope) in registrations {
            let selected: Selector = serde_json::from_str(&encoded)?;
            let Some((selected_user, selected_name)) = selected.slug.split_once('/') else {
                continue;
            };
            if selected.owner != owner
                || identity::normalize_hub(&selected.hub)?
                    != identity::normalize_hub(client.base())?
                || selected_user != user
                || !selected_name.eq_ignore_ascii_case(name)
            {
                continue;
            }
            let scope = observed_scope(&selected, &observed)?;
            if scope != previous_scope
                && selector(&selected.root, &owner).ok().as_ref() == Some(&selected)
            {
                changes.push((selected, scope));
            }
        }
        drop(store);
        if changes.is_empty() {
            return Ok(());
        }
        let mut store = Store::open(&directory, Some(owner))?;
        for (selected, scope) in changes {
            // Each save checks the live account and selector again; one stale path cannot
            // prevent another checkout of the same repository from observing its new identity.
            let _ = save(&mut store, &selected, scope);
        }
        Ok(())
    })();
}

/// Explicit registration may resolve a remote; capture workers only read the resulting association.
pub fn register(path: &Path) {
    let _ = (|| -> crate::Result<()> {
        let hub = crate::infra::config::hub_url();
        let account = crate::infra::credentials::load(&hub)
            .and_then(|c| c.account_id)
            .context("privacy account is unavailable")?;
        let owner = Owner {
            hub: HubAuthority::parse(&hub)?.label(),
            account,
        };
        let directory = keys::owner_directory(&keys::root()?, &owner).join("dictionary");
        let mut store = Store::open(&directory, Some(owner.clone()))?;
        remember(&mut store, path)?;
        let selected = selector(path, &owner)?;
        let client = crate::hub::Client::for_privacy(&hub);
        let resolved = resolve(&client, &selected)?;
        save(&mut store, &selected, resolved)
    })();
}
