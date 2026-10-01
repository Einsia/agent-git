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
use zeroize::Zeroizing;

pub(crate) const WORKER: &str = "__privacy-worker-v1";
pub(crate) const SYNC: &str = "__privacy-sync-v1";
pub(crate) const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub repo: Option<PathBuf>,
    pub mode: Mode,
    pub text: String,
}

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
                repo: repo.map(Path::to_owned),
                text: text.to_owned(),
                mode,
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

fn stamp(repo: Option<&Path>) -> Vec<(u64, Option<std::time::SystemTime>)> {
    let mut paths = vec![];
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

fn hydrate(request: &Request) -> crate::Result<Outcome> {
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
        Ok(())
    })();
    complete &= stored.is_ok();
    if let Some(repo) = &request.repo {
        let legacy = crate::domain::secret_filter::RepositoryDictionary::open(repo)
            .and_then(|dictionary| dictionary.privacy_snapshot());
        match legacy {
            Ok(legacy) => {
                for record in legacy.records {
                    complete &= dictionary.accept(record).is_ok();
                }
            }
            Err(_) => complete = false,
        }
    }
    let mut projector = Projector {
        store: storage::Store::empty()?,
        dictionary,
        policy: policy::Snapshot::compile(&[], &[], false)?,
        key: None,
        complete,
    };
    Ok(projector.transform(&request.text, request.mode))
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
    Ok(Outcome {
        content: serde_json::json!({"initialized":initialized,"encrypted":encrypted,
        "records":dictionary.records().count()})
        .to_string(),
        status: super::projector::Status::Complete,
        replacements: 0,
        unresolved: 0,
        consumed: None,
    })
}

fn execute(request: &Request, loaded: &mut Option<Loaded>) -> crate::Result<Outcome> {
    ensure!(
        request.text.len() <= super::projector::MAX_UNIT_BYTES,
        "privacy input exceeds its byte budget"
    );
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
    let stamp = stamp(request.repo.as_deref());
    if !loaded.as_ref().is_some_and(|loaded| {
        loaded.repo == request.repo
            && loaded.route == route
            && loaded.stamp == stamp
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
        complete &= legacy.is_ok();
        let legacy = legacy.ok().flatten().unwrap_or_default();
        let missing: Vec<_> = legacy
            .records
            .into_iter()
            .filter(|record| dictionary.get(&record.token).is_none())
            .collect();
        if !missing.is_empty() {
            if store.append(&missing, key.as_ref()).is_ok() {
                for record in missing {
                    complete &= dictionary.accept(record).is_ok();
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
        let global_allows = crate::infra::config::agit_home()
            .map(|home| crate::domain::secrets::load_allowlist(&home))
            .unwrap_or_default();
        let mut allows: Vec<_> = global_allows.iter().map(String::as_str).collect();
        allows.extend(legacy.allows.iter().map(|value| value.as_str()));
        let mut projector = Projector {
            store,
            dictionary,
            policy: policy::Snapshot::compile(&[], &[], true)?,
            key,
            complete,
        };
        projector.policy =
            management::compile(&projector, request.repo.as_deref(), &blocks, &allows)?;
        let policy_generation = projector.store.policy_generation()?;
        *loaded = Some(Loaded {
            repo: request.repo.clone(),
            route,
            stamp,
            cursor,
            policy_generation,
            projector,
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
        });
    }
    Ok(loaded.projector.transform(&request.text, request.mode))
}

fn serve(clock: Instant, deadline: &AtomicU64) -> crate::Result<()> {
    let mut loaded = None;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    loop {
        let bytes = read_frame(&mut input)?;
        deadline.store(clock.elapsed().as_millis() as u64 + 3000, Ordering::Release);
        let request: Request = serde_json::from_slice(&bytes)?;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(&request, &mut loaded)
        }))
        .ok()
        .and_then(Result::ok)
        .unwrap_or_else(|| {
            loaded = None;
            Outcome::skipped(&request.text)
        });
        let encoded = Zeroizing::new(serde_json::to_vec(&outcome)?);
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
    for name in [Some(credential.username.as_str()), None] {
        for pending_path in keys::pending_generations(&root, hub, name)? {
            let mut pending = match storage::Store::open(&pending_path, None)
                .or_else(|_| storage::Store::open(&pending_path, Some(key.owner.clone())))
            {
                Ok(store) => store,
                Err(_) => continue,
            };
            pending.bind(&key.owner)?;
            let mut records = Dictionary::default();
            pending.load_since(&mut records, 0, |version| {
                keys::cached(&root, &key.owner, Some(version))
            })?;
            let missing: Vec<_> = records
                .records()
                .filter(|record| known.get(&record.token).is_none())
                .cloned()
                .collect();
            for batch in missing.chunks(64) {
                if started.elapsed() >= Duration::from_secs(15) {
                    return Ok(());
                }
                store.append(batch, Some(&key))?;
                for record in batch {
                    known.accept(record.clone())?;
                }
            }
            for decision in pending.decisions()? {
                store.adopt_decision(&decision)?;
            }
            pending.encrypt_pending(&key)?;
        }
    }
    let remaining = Duration::from_secs(20).saturating_sub(started.elapsed());
    let _ = sync::run(&mut store, &key, &root, &client, remaining)?;
    Ok(())
}

/// Check before normal CLI startup so worker failure cannot run business commands.
pub fn entry(args: &[OsString]) -> Option<i32> {
    let mode = args.get(1)?.to_str()?;
    if mode != WORKER && mode != SYNC {
        return None;
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
