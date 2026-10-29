//! A disposable process owns local privacy dependencies and acknowledges durable output.

use super::{
    dictionary::Dictionary,
    keys, management, policy,
    projector::{Mode, Outcome, Projector},
    storage, sync,
};
use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use zeroize::{Zeroize, Zeroizing};

pub(crate) const WORKER: &str = "__privacy-worker-v1";
pub(crate) const SYNC: &str = "__privacy-sync-v1";
pub(crate) const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    /// Echoed in the response, so a reply that arrives after its caller stopped waiting is
    /// recognized and discarded instead of answering the next request.
    #[serde(default)]
    pub id: u64,
    /// How long the caller waits; the worker's own watchdog allows this much and a margin.
    #[serde(default)]
    pub deadline_ms: u64,
    pub repo: Option<PathBuf>,
    pub mode: Mode,
    pub text: String,
    /// Text a stream already released before `text`: matched against, never returned.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context: String,
}

/// One worker reply.
#[derive(Serialize, Deserialize)]
pub(crate) struct Response {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

/// The least time a request may take before the worker's watchdog ends the process.
pub(crate) const WATCHDOG_FLOOR: Duration = Duration::from_secs(3);
/// What the watchdog allows beyond the caller's own deadline.
const WATCHDOG_MARGIN: Duration = Duration::from_secs(2);

static IN_WORKER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

thread_local! {
    static INLINE_STATE: std::cell::RefCell<Option<Loaded>> = const { std::cell::RefCell::new(None) };
}

/// Nested projection uses the disposable worker's own dependencies and watchdog.
pub(crate) fn inline(repo: Option<&Path>, text: &str, mode: Mode) -> Option<Outcome> {
    if !IN_WORKER.load(Ordering::Relaxed) {
        return None;
    }
    Some(INLINE_STATE.with(|state| {
        let Ok(mut state) = state.try_borrow_mut() else {
            return Outcome::skipped(text);
        };
        execute(
            &Request {
                id: 0,
                deadline_ms: 0,
                repo: repo.map(Path::to_owned),
                text: text.to_owned(),
                mode,
                context: String::new(),
            },
            &mut state,
        )
        .unwrap_or_else(|_| Outcome::skipped(text))
    }))
}

struct Loaded {
    repo: Option<PathBuf>,
    route: PathBuf,
    stamp: Vec<(u64, Option<std::time::SystemTime>)>,
    /// The other journals on the device, as of this load; their blocks are part of the policy.
    device: Option<DeviceStamp>,
    cursor: i64,
    policy_generation: i64,
    projector: Projector,
}

pub(crate) fn read_frame(reader: &mut impl Read) -> crate::Result<Zeroizing<Vec<u8>>> {
    let mut header = [0u8; 4];
    reader.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header) as usize;
    ensure!(length <= MAX_FRAME, "privacy frame exceeds its byte budget");
    let mut bytes = Zeroizing::new(vec![0u8; length]);
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

pub(crate) fn write_frame(writer: &mut impl Write, bytes: &[u8]) -> crate::Result<()> {
    ensure!(
        bytes.len() <= MAX_FRAME,
        "privacy frame exceeds its byte budget"
    );
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(bytes)?;
    writer.flush()?;
    Ok(())
}

fn route(
    root: &Path,
    hub: &str,
    credential: Option<&crate::infra::credentials::HubCredential>,
) -> crate::Result<(PathBuf, Option<super::crypto::Owner>)> {
    let owner = credential
        .and_then(|cred| cred.account_id.as_deref())
        .map(|id| keys::owner(hub, id))
        .transpose()?;
    let route = if let Some(owner) = &owner {
        keys::owner_directory(root, owner).join("dictionary")
    } else {
        keys::pending_directory(root, hub, credential.map(|cred| cred.username.as_str()))?
    };
    Ok((route, owner))
}

/// Rules this device registered while signed out, which live in the Hub's pending journals,
/// taken over by the account the device is signed in to.
struct Adoption<'a> {
    root: &'a Path,
    hub: &'a str,
    username: &'a str,
    owner: &'a super::crypto::Owner,
    /// The account key, when known: adopted records are then encrypted, and otherwise wait for
    /// synchronization to encrypt them.
    key: Option<&'a super::crypto::UserKey>,
}

impl Adoption<'_> {
    /// The pending journals this account takes over: the signed-out one and the one kept for this
    /// username. A journal bound to another account is that account's, and is left alone.
    fn journals(&self) -> crate::Result<Vec<(PathBuf, Option<super::crypto::Owner>)>> {
        let mut journals = Vec::new();
        for name in [Some(self.username), None] {
            for path in keys::pending_generations(self.root, self.hub, name)? {
                let binding = storage::Store::binding(&path)?;
                if binding.as_ref().is_none_or(|bound| bound == self.owner) {
                    journals.push((path, binding));
                }
            }
        }
        Ok(journals)
    }

    /// Bind each pending journal to the account, copy the records `store` lacks into it, and
    /// adopt its decisions wherever the account has not decided the same rule. Signing in thus
    /// carries every rule over at once, without waiting for the network: until it has, the
    /// account journal is empty, and a check against it would pass what the user registered.
    /// `false` when `deadline` passed first.
    fn adopt(
        &self,
        store: &mut storage::Store,
        known: &mut Dictionary,
        deadline: Option<Instant>,
    ) -> crate::Result<bool> {
        for (path, _) in self.journals()? {
            let mut pending = storage::Store::open(&path, None)
                .or_else(|_| storage::Store::open(&path, Some(self.owner.clone())))?;
            pending.bind(self.owner)?;
            let mut records = Dictionary::default();
            pending.load_since(&mut records, 0, |version| {
                keys::cached(self.root, self.owner, Some(version))
            })?;
            let missing: Vec<_> = records
                .records()
                .filter(|record| known.get(&record.token).is_none())
                .cloned()
                .collect();
            for batch in missing.chunks(64) {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Ok(false);
                }
                store.append(batch, self.key)?;
                for record in batch {
                    known.accept(record.clone())?;
                }
            }
            for decision in pending.decisions()? {
                store.adopt_decision(&decision)?;
            }
            if let Some(key) = self.key {
                pending.encrypt_pending(key)?;
            }
        }
        Ok(true)
    }

    /// The records of the pending journals, read without changing them, so placeholders made
    /// while signed out are restored before adoption has run. `false` when one cannot be read.
    fn read(&self, dictionary: &mut Dictionary) -> bool {
        let read = (|| -> crate::Result<bool> {
            let mut complete = true;
            for (path, binding) in self.journals()? {
                let store = storage::Store::read_only(&path, binding)?;
                let mut records = Dictionary::default();
                complete &= store
                    .load_since(&mut records, 0, |version| {
                        keys::cached(self.root, self.owner, Some(version))
                    })?
                    .1;
                for record in records.records() {
                    if dictionary.get(&record.token).is_none() {
                        dictionary.accept(record.clone())?;
                    }
                }
            }
            Ok(complete)
        })();
        read.unwrap_or(false)
    }
}

/// Every journal on this device other than `current`: each Hub's pending journals and every
/// account's dictionary.
fn device_journals(root: &Path, current: &Path) -> crate::Result<Vec<PathBuf>> {
    let entries = |directory: PathBuf| -> crate::Result<Vec<PathBuf>> {
        match std::fs::read_dir(&directory) {
            Ok(entries) => entries
                .map(|entry| Ok(entry?.path()))
                .collect::<crate::Result<_>>(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    };
    let mut journals = Vec::new();
    for mut directory in entries(root.join("pending"))? {
        for _ in 0..64 {
            if !directory.join("journal.sqlite").try_exists()? {
                break;
            }
            journals.push(directory.clone());
            directory = directory.join("next");
        }
    }
    for account in entries(root.join("accounts"))? {
        let directory = account.join("dictionary");
        if directory.join("journal.sqlite").try_exists()? {
            journals.push(directory);
        }
    }
    journals.retain(|directory| directory != current);
    Ok(journals)
}

/// Each of the device's other journals and the state of its database files. A rule added to any of
/// them changes this, so a running worker reloads before it checks again instead of passing the
/// value through under the policy it loaded earlier.
type DeviceStamp = Vec<(PathBuf, [(u64, Option<std::time::SystemTime>); 2])>;

fn device_stamp(journals: &[PathBuf]) -> DeviceStamp {
    let state = |path: PathBuf| {
        std::fs::metadata(path)
            .map(|meta| (meta.len(), meta.modified().ok()))
            .unwrap_or_default()
    };
    journals
        .iter()
        .map(|directory| {
            (
                directory.clone(),
                [
                    state(directory.join("journal.sqlite")),
                    state(directory.join("journal.sqlite-wal")),
                ],
            )
        })
        .collect()
}

/// What the device's other journals contribute to a policy.
#[derive(Default)]
struct DeviceRules {
    /// The values they block, with where each applies.
    blocks: Vec<(zeroize::Zeroizing<String>, policy::Source)>,
    /// Each journal that cannot be read, or that blocks a record it cannot read, with the account
    /// it is bound to when that much can be read.
    unreadable: Vec<(PathBuf, Option<super::crypto::Owner>)>,
}

/// The values `journals` block, in global scope or in `repository`'s (in any scope when
/// `repository` is `None`), with where each applies. A rule registered on this device protects
/// whatever the device publishes, signed in to any account or Hub or signed out: switching never
/// lets through a value the user blocked, while only the current journal's own decisions allow
/// one. A journal that cannot be read, or the record a block names, is listed as unreadable, so
/// the check is incomplete and callers withhold instead of publishing what it may block.
fn device_blocks(root: &Path, journals: &[PathBuf], repository: Option<&str>) -> DeviceRules {
    let mut rules = DeviceRules::default();
    for directory in journals {
        let owner = storage::Store::binding(directory).ok().flatten();
        let read = (|| -> crate::Result<bool> {
            let store = storage::Store::read_only(directory, owner.clone())?;
            let decisions: Vec<_> = store
                .decisions()?
                .into_iter()
                .filter(|decision| {
                    decision.block == Some(true)
                        && repository.is_none_or(|scope| {
                            decision.scope == "global" || decision.scope == scope
                        })
                })
                .collect();
            if decisions.is_empty() {
                return Ok(true);
            }
            let mut records = Dictionary::default();
            store.load_since(&mut records, 0, |version| {
                keys::cached(
                    root,
                    owner.as_ref().context("privacy owner is not available")?,
                    Some(version),
                )
            })?;
            let mut whole = true;
            for decision in decisions {
                match records.get(&decision.token) {
                    Some(record) => rules.blocks.push((
                        zeroize::Zeroizing::new(record.original.clone()),
                        if decision.scope == "global" {
                            policy::Source::GlobalUser
                        } else {
                            policy::Source::RepositoryUser
                        },
                    )),
                    None => whole = false,
                }
            }
            Ok(whole)
        })();
        if !read.unwrap_or(false) {
            rules.unreadable.push((directory.clone(), owner));
        }
    }
    rules
}

fn stamp(repo: Option<&Path>) -> Vec<(u64, Option<std::time::SystemTime>)> {
    let mut paths = vec![];
    if let Ok(path) = crate::infra::config::global_config_path() {
        paths.push(path);
    }
    if let Ok(path) = crate::infra::config::secret_filter_vault_path() {
        paths.push(path);
    }
    if let Ok(home) = crate::infra::config::agit_home() {
        paths.push(home.join(crate::domain::secrets::ALLOWLIST_FILE));
    }
    if let Some(repo) = repo {
        paths.push(
            crate::domain::repo::common_git_dir(repo).join("agit/secret-dictionary/vault.json"),
        );
    }
    paths
        .into_iter()
        .map(|path| {
            std::fs::metadata(path)
                .map(|meta| (meta.len(), meta.modified().ok()))
                .unwrap_or_default()
        })
        .collect()
}

fn hydration_projector(repo: Option<&Path>) -> crate::Result<Projector> {
    let root = keys::root()?;
    let hub = crate::infra::config::hub_url();
    let credential = crate::infra::credentials::load(&hub);
    let mut dictionary = Dictionary::default();
    let mut complete = true;
    let stored = (|| -> crate::Result<()> {
        let (directory, owner) = route(&root, &hub, credential.as_ref())?;
        if directory.join("journal.sqlite").try_exists()? {
            let store = storage::Store::read_only(&directory, owner.clone())?;
            complete &= store
                .load_since(&mut dictionary, 0, |version| {
                    keys::cached(
                        &root,
                        owner.as_ref().context("privacy owner is not available")?,
                        Some(version),
                    )
                })?
                .1;
        }
        if let (Some(owner), Some(credential)) = (&owner, &credential) {
            complete &= Adoption {
                root: &root,
                hub: &hub,
                username: &credential.username,
                owner,
                key: None,
            }
            .read(&mut dictionary);
        }
        Ok(())
    })();
    complete &= stored.is_ok();
    let mut missing = None;
    if let Some(repo) = repo {
        let legacy = crate::domain::secret_filter::RepositoryDictionary::open(repo)
            .and_then(|dictionary| dictionary.privacy_snapshot());
        let mut whole = legacy.is_ok();
        for record in legacy.map(|legacy| legacy.records).unwrap_or_default() {
            whole &= dictionary.accept(record).is_ok();
        }
        if !whole {
            complete = false;
            missing = Some(super::projector::Reason::LegacyRules);
        }
    }
    Ok(Projector {
        store: storage::Store::empty()?,
        dictionary,
        policy: policy::Snapshot::compile(&[], &[], false)?,
        key: None,
        complete,
        missing,
    })
}

fn hydrate(request: &Request) -> crate::Result<Outcome> {
    Ok(hydration_projector(request.repo.as_deref())?.transform(&request.text, request.mode))
}

struct FrozenHydration {
    repo: Option<PathBuf>,
    preserve_protocol_ids: bool,
    projector: Projector,
    used_values: std::collections::BTreeSet<String>,
}

impl Drop for FrozenHydration {
    fn drop(&mut self) {
        for mut value in std::mem::take(&mut self.used_values) {
            value.zeroize();
        }
    }
}

thread_local! {
    static FROZEN_HYDRATION: std::cell::RefCell<Option<FrozenHydration>> = const { std::cell::RefCell::new(None) };
}

fn frozen_hydration(request: &Request) -> crate::Result<Outcome> {
    FROZEN_HYDRATION.with(|slot| {
        let mut slot = slot.borrow_mut();
        if matches!(request.mode, Mode::FreezeHydration) {
            *slot = None;
            let preserve_protocol_ids: bool = serde_json::from_str(&request.text)?;
            let projector = hydration_projector(request.repo.as_deref())?;
            ensure!(projector.complete, "publication dictionary is incomplete");
            *slot = Some(FrozenHydration {
                repo: request.repo.clone(),
                preserve_protocol_ids,
                projector,
                used_values: Default::default(),
            });
        } else {
            let frozen = slot
                .as_mut()
                .context("publication dictionary is not frozen")?;
            ensure!(
                frozen.repo == request.repo,
                "publication dictionary belongs to another repository"
            );
            if matches!(request.mode, Mode::HydrateFrozenJsonl) {
                for (_, _, token) in super::projector::tokens(&request.text) {
                    if let Some(record) = frozen.projector.dictionary.get(token) {
                        frozen.used_values.insert(record.original.clone());
                    }
                }
                return Ok(frozen
                    .projector
                    .hydrate_frozen(&request.text, frozen.preserve_protocol_ids));
            }
        }
        Ok(Outcome {
            content: if matches!(request.mode, Mode::HydrationValues) {
                serde_json::to_string(
                    &slot
                        .as_ref()
                        .context("publication dictionary is not frozen")?
                        .used_values,
                )?
            } else {
                "true".into()
            },
            status: super::projector::Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
            unprocessed: 0,
            reason: None,
        })
    })
}

fn status() -> crate::Result<Outcome> {
    let root = keys::root()?;
    let hub = crate::infra::config::hub_url();
    let credential = crate::infra::credentials::load(&hub);
    let (directory, owner) = route(&root, &hub, credential.as_ref())?;
    let encrypted = owner
        .as_ref()
        .is_some_and(|owner| keys::cached(&root, owner, None).is_ok());
    let initialized = directory.join("journal.sqlite").try_exists()?;
    let mut dictionary = Dictionary::default();
    if initialized {
        let store = storage::Store::read_only(&directory, owner.clone())?;
        store.load_since(&mut dictionary, 0, |version| {
            keys::cached(
                &root,
                owner.as_ref().context("privacy owner is not available")?,
                Some(version),
            )
        })?;
    }
    // Rules registered under another account or Hub that this device cannot read, so the user can
    // be told which account to sign in to once.
    // A device whose journals cannot be listed is one unreadable entry, its privacy directory, so no
    // caller reads a failed listing as "every rule is readable".
    let unreadable: Vec<_> = match device_journals(&root, &directory) {
        Ok(journals) => device_blocks(&root, &journals, None).unreadable,
        Err(_) => vec![(root.clone(), None)],
    }
    .into_iter()
    .map(|(path, owner)| {
        serde_json::json!({
            "path": path,
            "hub": owner.as_ref().map(|owner| owner.hub.clone()),
            "account": owner.map(|owner| owner.account),
        })
    })
    .collect();
    Ok(Outcome {
        content: serde_json::json!({"initialized":initialized,"encrypted":encrypted,
        "records":dictionary.records().count(),"unreadable_rules":unreadable})
        .to_string(),
        status: super::projector::Status::Complete,
        replacements: 0,
        unresolved: 0,
        consumed: None,
        unprocessed: 0,
        reason: None,
    })
}

fn execute(request: &Request, loaded: &mut Option<Loaded>) -> crate::Result<Outcome> {
    ensure!(
        request.context.len() + request.text.len() <= super::projector::MAX_UNIT_BYTES,
        "privacy input exceeds its byte budget"
    );
    if matches!(
        request.mode,
        Mode::FreezeHydration | Mode::HydrateFrozenJsonl | Mode::HydrationValues
    ) {
        return frozen_hydration(request);
    }
    if matches!(request.mode, Mode::ProjectPublication) {
        return super::super::privacy_publication::project_worker(
            request.repo.as_deref(),
            &request.text,
        );
    }
    if matches!(
        request.mode,
        Mode::HydrateText | Mode::HydrateJsonl | Mode::HydrateEnvelopes
    ) {
        return hydrate(request);
    }
    if matches!(request.mode, Mode::Manage) {
        let command: management::Command = serde_json::from_str(&request.text)?;
        if command.action == "status" {
            return status();
        }
    }
    let root = keys::root()?;
    let hub = crate::infra::config::hub_url();
    let credential = crate::infra::credentials::load(&hub);
    let (route, owner) = route(&root, &hub, credential.as_ref())?;
    // Start-up only loads what exists. Opening the journal creates it, and a read-only command
    // that merely starts the worker (`agit doctor`, `agit status`) must leave the privacy
    // directory as it found it; the first request that needs the journal creates it.
    if matches!(request.mode, Mode::Prepare) && !route.join("journal.sqlite").try_exists()? {
        return Ok(Outcome {
            content: "{}".into(),
            status: super::projector::Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
            unprocessed: 0,
            reason: None,
        });
    }
    let stamp = stamp(request.repo.as_deref());
    let device = device_journals(&root, &route)
        .ok()
        .map(|journals| device_stamp(&journals));
    // How long each loading phase took, reported to the caller of a `Prepare` request.
    let mut phases = serde_json::Map::new();
    let mut phase = Instant::now();
    let mut lap = |name: &str, phases: &mut serde_json::Map<String, serde_json::Value>| {
        phases.insert(name.into(), (phase.elapsed().as_millis() as u64).into());
        phase = Instant::now();
    };
    if !loaded.as_ref().is_some_and(|loaded| {
        loaded.repo == request.repo
            && loaded.route == route
            && loaded.stamp == stamp
            && device.is_some()
            && loaded.device == device
            && loaded.projector.store.policy_generation().ok() == Some(loaded.policy_generation)
    }) {
        let key = owner
            .as_ref()
            .and_then(|owner| keys::cached(&root, owner, None).ok());
        let mut store = storage::Store::open(&route, owner.clone())?;
        let mut dictionary = Dictionary::default();
        let (cursor, mut complete) = store.load_since(&mut dictionary, 0, |version| {
            keys::cached(
                &root,
                owner.as_ref().context("privacy owner is not available")?,
                Some(version),
            )
        })?;
        if let Some(owner) = &owner {
            let adopted = credential
                .as_ref()
                .context("a privacy owner without a credential")
                .and_then(|credential| {
                    Adoption {
                        root: &root,
                        hub: &hub,
                        username: &credential.username,
                        owner,
                        key: key.as_ref(),
                    }
                    .adopt(&mut store, &mut dictionary, None)
                });
            complete &= adopted.unwrap_or(false);
        }
        lap("journal_ms", &mut phases);
        let global = (|| -> crate::Result<crate::domain::secret_filter::Matcher> {
            if !crate::infra::config::secret_filter_vault_path()?.try_exists()? {
                return Ok(Default::default());
            }
            crate::domain::secret_filter::VaultStore::open_default()?.matcher()
        })();
        complete &= global.is_ok();
        let global = global.unwrap_or_default();
        let legacy = request
            .repo
            .as_ref()
            .map(|repo| {
                crate::domain::secret_filter::RepositoryDictionary::open(repo)?.privacy_snapshot()
            })
            .transpose();
        let mut legacy_whole = legacy.is_ok();
        let legacy = legacy.ok().flatten().unwrap_or_default();
        let missing: Vec<_> = legacy
            .records
            .into_iter()
            .filter(|record| dictionary.get(&record.token).is_none())
            .collect();
        if !missing.is_empty() {
            if store.append(&missing, key.as_ref()).is_ok() {
                for record in missing {
                    legacy_whole &= dictionary.accept(record).is_ok();
                }
            } else {
                complete = false;
            }
        }
        for (id, value) in global.patterns() {
            let token = if let Some(record) = dictionary.for_value(value) {
                record.token.clone()
            } else {
                let record = super::dictionary::Record::new(
                    &store.dictionary_id,
                    value,
                    super::dictionary::Origin::GlobalUser,
                    super::dictionary::Label::secret("explicit-block"),
                )?;
                store.append(std::slice::from_ref(&record), key.as_ref())?;
                let token = record.token.clone();
                dictionary.accept(record)?;
                token
            };
            store.adopt_decision(&management::Decision {
                scope: "global".into(),
                token,
                name: id.to_owned(),
                block: Some(true),
                allow: None,
            })?;
        }
        let repository_scope = management::scope(request.repo.as_deref())?;
        for value in &legacy.blocks {
            if let Some(record) = dictionary.for_value(value) {
                store.adopt_decision(&management::Decision {
                    scope: repository_scope.clone(),
                    token: record.token.clone(),
                    name: "legacy repository rule".into(),
                    block: Some(true),
                    allow: None,
                })?;
            }
        }
        let mut blocks: Vec<_> = global
            .patterns()
            .map(|(_, value)| policy::Literal {
                value,
                source: policy::Source::GlobalUser,
            })
            .collect();
        blocks.extend(legacy.blocks.iter().map(|value| policy::Literal {
            value,
            source: policy::Source::RepositoryUser,
        }));
        let device_rules = device_journals(&root, &route)
            .map(|journals| device_blocks(&root, &journals, Some(&repository_scope)));
        let device_whole = device_rules
            .as_ref()
            .is_ok_and(|rules| rules.unreadable.is_empty());
        complete &= device_whole && legacy_whole;
        let device_rules = device_rules.unwrap_or_default();
        blocks.extend(
            device_rules
                .blocks
                .iter()
                .map(|(value, source)| policy::Literal {
                    value: value.as_str(),
                    source: *source,
                }),
        );
        lap("legacy_ms", &mut phases);
        let global_allows = crate::infra::config::agit_home()
            .map(|home| crate::domain::secrets::load_allowlist(&home))
            .unwrap_or_default();
        lap("allowlist_ms", &mut phases);
        let mut allows: Vec<_> = global_allows.iter().map(String::as_str).collect();
        allows.extend(legacy.allows.iter().map(|value| value.as_str()));
        let mut projector = Projector {
            store,
            dictionary,
            policy: policy::Snapshot::compile(&[], &[], true)?,
            key,
            complete,
            missing: (!device_whole)
                .then_some(super::projector::Reason::DeviceRules)
                .or((!legacy_whole).then_some(super::projector::Reason::LegacyRules)),
        };
        projector.policy =
            management::compile(&projector, request.repo.as_deref(), &blocks, &allows)?
                .with_entropy(if crate::infra::config::privacy_entropy_replace() {
                    super::detector::Entropy::Include
                } else {
                    super::detector::Entropy::Exclude
                });
        lap("compile_ms", &mut phases);
        let policy_generation = projector.store.policy_generation()?;
        *loaded = Some(Loaded {
            repo: request.repo.clone(),
            route,
            stamp,
            device,
            cursor,
            policy_generation,
            projector,
        });
    }
    if matches!(request.mode, Mode::Prepare) {
        return Ok(Outcome {
            content: serde_json::Value::Object(phases).to_string(),
            status: super::projector::Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
            unprocessed: 0,
            reason: None,
        });
    }
    let loaded = loaded
        .as_mut()
        .context("privacy worker is not initialized")?;
    let (cursor, complete) = loaded.projector.store.load_since(
        &mut loaded.projector.dictionary,
        loaded.cursor,
        |version| {
            keys::cached(
                &root,
                owner.as_ref().context("privacy owner is not available")?,
                Some(version),
            )
        },
    )?;
    loaded.cursor = cursor;
    loaded.projector.complete &= complete;
    loaded.projector.key = owner
        .as_ref()
        .and_then(|owner| keys::cached(&root, owner, None).ok());
    if matches!(request.mode, Mode::Manage) {
        let command = serde_json::from_str(&request.text)?;
        let result = management::execute(&mut loaded.projector, request.repo.as_deref(), command)?;
        return Ok(Outcome {
            content: serde_json::to_string(&result)?,
            status: super::projector::Status::Complete,
            replacements: 0,
            unresolved: 0,
            consumed: None,
            unprocessed: 0,
            reason: None,
        });
    }
    Ok(loaded
        .projector
        .transform_in(&request.context, &request.text, request.mode))
}

fn serve(clock: Instant, deadline: &AtomicU64) -> crate::Result<()> {
    let mut loaded = None;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    loop {
        let bytes = read_frame(&mut input)?;
        deadline.store(
            (clock.elapsed() + WATCHDOG_FLOOR).as_millis() as u64,
            Ordering::Release,
        );
        let request: Request = serde_json::from_slice(&bytes)?;
        let allowed = Duration::from_millis(request.deadline_ms) + WATCHDOG_MARGIN;
        deadline.store(
            (clock.elapsed() + allowed.max(WATCHDOG_FLOOR)).as_millis() as u64,
            Ordering::Release,
        );
        if matches!(
            request.mode,
            Mode::FreezeHydration | Mode::HydrateFrozenJsonl | Mode::HydrationValues
        ) {
            deadline.store(
                clock.elapsed().as_millis() as u64 + 30_000,
                Ordering::Release,
            );
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(&request, &mut loaded)
        }))
        .ok()
        .and_then(Result::ok)
        .unwrap_or_else(|| {
            loaded = None;
            let mut outcome = Outcome::skipped(&request.text);
            outcome.reason = Some(super::projector::Reason::StoreBusy);
            outcome
        });
        let encoded = Zeroizing::new(serde_json::to_vec(&Response {
            id: request.id,
            outcome,
        })?);
        write_frame(&mut output, &encoded)?;
        deadline.store(0, Ordering::Release);
    }
}

fn synchronize(hub: &str) -> crate::Result<()> {
    use super::sync::Transport;
    let started = Instant::now();
    let root = keys::root()?;
    storage::private_directory(&root)?;
    let lock_path = root.join("sync.lock");
    #[cfg(unix)]
    let lock = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(lock_path)?
    };
    #[cfg(windows)]
    let lock = crate::infra::windows_security::open_private_control(&lock_path)?;
    fs2::FileExt::try_lock_exclusive(&lock)?;
    let credential = crate::infra::credentials::load(hub)
        .context("privacy synchronization needs an authenticated account")?;
    let client = crate::hub::Client::for_privacy(hub);
    let document = client.ensure_key()?;
    let key = keys::decode(hub, &document)?;
    ensure!(
        credential
            .account_id
            .as_ref()
            .is_none_or(|account| account == &key.owner.account),
        "privacy account changed"
    );
    keys::cache(&root, &key)?;
    if credential.account_id.is_none() {
        // Account metadata enriches this exact token pair without replacing a concurrent login.
        let _ =
            crate::infra::credentials::save_verified_account(hub, &credential, &key.owner.account);
    }
    let directory = keys::owner_directory(&root, &key.owner).join("dictionary");
    let mut store = storage::Store::open(&directory, Some(key.owner.clone()))?;
    let mut known = Dictionary::default();
    store.load_since(&mut known, 0, |version| {
        keys::cached(&root, &key.owner, Some(version))
    })?;
    let adoption = Adoption {
        root: &root,
        hub,
        username: &credential.username,
        owner: &key.owner,
        key: Some(&key),
    };
    if !adoption.adopt(
        &mut store,
        &mut known,
        Some(started + Duration::from_secs(15)),
    )? {
        return Ok(());
    }
    let remaining = Duration::from_secs(20).saturating_sub(started.elapsed());
    let _ = sync::run(&mut store, &key, &root, &client, remaining)?;
    Ok(())
}

/// A worker talks to its parent over stdin and stdout and needs no other descriptor it was born
/// with. A parent that spawns it while another thread creates a pipe for a different child can
/// hand it that pipe's write end before close-on-exec is set; kept open here, it keeps the other
/// child's reader from ever reaching end of file, and a parent waiting on that reader waits on
/// this worker, which waits on the parent. So every descriptor above stderr is closed before the
/// worker opens one of its own, and a worker that cannot tell which descriptors it holds does not
/// run: a sweep up to an estimated bound misses whatever was opened above it, and no limit this
/// process can read bounds the descriptors it inherited.
#[cfg(unix)]
#[must_use]
fn close_inherited_descriptors() -> bool {
    #[cfg(target_os = "linux")]
    if unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3 as libc::c_uint,
            libc::c_uint::MAX,
            0 as libc::c_uint,
        )
    } == 0
    {
        return true;
    }
    let Some(listed) = listed_descriptors() else {
        return false;
    };
    for descriptor in listed.into_iter().filter(|descriptor| *descriptor > 2) {
        unsafe {
            libc::close(descriptor);
        }
    }
    true
}

/// Every descriptor this process holds: from the kernel's process table where the platform
/// offers it, which needs no file system, otherwise from a descriptor directory. `None` when
/// neither can be read.
#[cfg(unix)]
fn listed_descriptors() -> Option<Vec<libc::c_int>> {
    #[cfg(target_os = "macos")]
    if let Some(listed) = process_table_descriptors() {
        return Some(listed);
    }
    for directory in ["/dev/fd", "/proc/self/fd"] {
        if let Ok(entries) = std::fs::read_dir(directory) {
            return Some(
                entries
                    .flatten()
                    .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                    .collect(),
            );
        }
    }
    None
}

/// The first call sizes the table, which only this process could grow, and it opens nothing in
/// between. A reply that fills the buffer, spare entry included, may have been cut short, so it
/// does not count as a listing.
#[cfg(target_os = "macos")]
fn process_table_descriptors() -> Option<Vec<libc::c_int>> {
    let pid = unsafe { libc::getpid() };
    let entry = std::mem::size_of::<libc::proc_fdinfo>();
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return None;
    }
    let room = needed as usize / entry + 1;
    let mut table = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0,
        };
        room
    ];
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            table.as_mut_ptr().cast(),
            libc::c_int::try_from(room * entry).ok()?,
        )
    };
    if written <= 0 || written as usize >= room * entry {
        return None;
    }
    table.truncate(written as usize / entry);
    Some(table.iter().map(|info| info.proc_fd).collect())
}

/// Check before normal CLI startup so worker failure cannot run business commands.
pub fn entry(args: &[OsString]) -> Option<i32> {
    let mode = args.get(1)?.to_str()?;
    if mode != WORKER && mode != SYNC {
        return None;
    }
    #[cfg(unix)]
    if !close_inherited_descriptors() {
        return Some(4);
    }
    IN_WORKER.store(true, Ordering::Relaxed);
    std::panic::set_hook(Box::new(|_| {}));
    let clock = Instant::now();
    let deadline = std::sync::Arc::new(AtomicU64::new(25_000));
    let monitor = deadline.clone();
    let watchdog = std::thread::Builder::new()
        .name("privacy-deadline".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let stop = monitor.load(Ordering::Acquire);
                if stop > 0 && clock.elapsed().as_millis() as u64 >= stop {
                    std::process::exit(4);
                }
            }
        });
    if watchdog.is_err() {
        return Some(4);
    }
    unsafe {
        rusqlite::ffi::sqlite3_hard_heap_limit64(64 * 1024 * 1024);
    }
    #[cfg(unix)]
    unsafe {
        let memory = libc::rlimit {
            rlim_cur: 1024 * 1024 * 1024,
            rlim_max: 1024 * 1024 * 1024,
        };
        libc::setrlimit(libc::RLIMIT_DATA, &memory);
        let core = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        libc::setrlimit(libc::RLIMIT_CORE, &core);
    }
    let success = if mode == WORKER && args.len() == 2 {
        serve(clock, &deadline).is_ok()
    } else if mode == SYNC && args.len() == 3 {
        args[2].to_str().is_some_and(|hub| synchronize(hub).is_ok())
    } else {
        false
    };
    Some(if success { 0 } else { 4 })
}
