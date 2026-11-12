//! Where a folder's conversations upload, as it was chosen for that folder on this computer, for
//! one account.
//!
//! The person chooses it in a terminal (`agit on --repo`, `agit project bind`, or a first
//! `agit push --to` for a folder that has none), or the device's own account binds the folder's
//! project on the web. Terminal and workspace conversations of the folder both follow it. A
//! workspace conversation uploads only to the destination chosen for its own project: one chosen
//! for another project of the same folder, or under another account, is never used, and such a
//! conversation stays on this computer until its own project's destination is confirmed here.
//!
//! The record also keeps the last binding the Hub showed for each of the folder's projects, and
//! a binding this computer can't take on its own, which the person is asked about once. One file
//! per folder, Hub and account, readable by its owner only.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which choice set a folder's destination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChosenFor {
    /// The person, for the folder, in a terminal.
    Folder,
    /// The device's own account, for this project (its id on this computer), on the web.
    Project(String),
}

/// A folder's chosen destination.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Chosen {
    pub repository: String,
    pub repository_id: String,
    pub chosen_for: ChosenFor,
    /// When it was chosen (RFC 3339).
    pub at: String,
}

/// A project's destination as the Hub binds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Binding {
    pub repository: String,
    pub repository_id: String,
}

/// A binding this computer can't take by itself, which the person is asked about once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Ask {
    binding: Binding,
    #[serde(default)]
    asked: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    version: u32,
    /// The canonical folder, Hub and account, so a record is never applied to another one.
    root: String,
    hub: String,
    account: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    destination: Option<Chosen>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    bindings: BTreeMap<String, Binding>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    asks: BTreeMap<String, Ask>,
    /// The repository each project's own binding was last applied with, so a binding is taken
    /// once: a later terminal choice for the folder isn't undone by the same binding again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    applied: BTreeMap<String, String>,
}

/// The folder, Hub and account a record belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Key {
    root: PathBuf,
    hub: String,
    account: String,
}

impl Key {
    /// `root`'s record on `hub` for `account`. The folder is taken as it is on disk.
    pub(crate) fn new(root: &Path, hub: &str, account: &str) -> Result<Self> {
        Ok(Self {
            root: root
                .canonicalize()
                .with_context(|| format!("cannot resolve {}", root.display()))?,
            hub: crate::hub::identity::normalize_hub(hub)?,
            account: account.to_owned(),
        })
    }

    /// `root`'s record on `hub` for the account signed in there now; `None` when nobody is.
    pub(crate) fn signed_in(root: &Path, hub: &str) -> Result<Option<Self>> {
        match crate::infra::credentials::account_key(hub) {
            Some(account) => Self::new(root, hub, &account).map(Some),
            None => Ok(None),
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn account(&self) -> &str {
        &self.account
    }

    fn path(&self) -> Result<PathBuf> {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        for part in [
            self.root.to_string_lossy().as_bytes(),
            self.hub.as_bytes(),
            self.account.as_bytes(),
        ] {
            digest.update((part.len() as u64).to_le_bytes());
            digest.update(part);
        }
        Ok(directory()?.join(format!("{}.json", &hex::encode(digest.finalize())[..32])))
    }
}

fn directory() -> Result<PathBuf> {
    Ok(crate::infra::config::agit_home()?.join("folder-destinations"))
}

fn read(key: &Key) -> Result<Option<Record>> {
    let body = match std::fs::read(key.path()?) {
        Ok(body) => body,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("cannot read the folder's destination record"),
    };
    let record: Record =
        serde_json::from_slice(&body).context("the folder's destination record is unreadable")?;
    anyhow::ensure!(
        record.version == 1
            && record.root == key.root.to_string_lossy()
            && record.hub == key.hub
            && record.account == key.account,
        "the folder's destination record names another folder or account"
    );
    Ok(Some(record))
}

fn write(key: &Key, record: &Record) -> Result<()> {
    let directory = directory()?;
    crate::infra::config::create_state_dir(&directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    std::io::Write::write_all(temporary.as_file_mut(), &serde_json::to_vec_pretty(record)?)?;
    temporary.as_file().sync_all()?;
    temporary.persist(key.path()?)?;
    Ok(())
}

/// Holds `key`'s record for one read, change and write, across processes: the device's reconcile
/// and a terminal command would otherwise each write back what the other did not see.
fn change<T>(key: &Key, change: impl FnOnce(&mut Record) -> T) -> Result<T> {
    let directory = directory()?;
    crate::infra::config::create_state_dir(&directory)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(key.path()?.with_extension("lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut record = read(key)?.unwrap_or_else(|| Record {
        version: 1,
        root: key.root.to_string_lossy().into_owned(),
        hub: key.hub.clone(),
        account: key.account.clone(),
        destination: None,
        bindings: BTreeMap::new(),
        asks: BTreeMap::new(),
        applied: BTreeMap::new(),
    });
    let result = change(&mut record);
    write(key, &record)?;
    Ok(result)
}

/// The folder's chosen destination for this account, if one was chosen.
pub(crate) fn chosen(key: &Key) -> Result<Option<Chosen>> {
    Ok(read(key)?.and_then(|record| record.destination))
}

/// Records `repository` as where the folder uploads from now on. Returns whether that changed
/// it.
pub(crate) fn choose(
    key: &Key,
    repository: &str,
    repository_id: &str,
    chosen_for: ChosenFor,
) -> Result<bool> {
    change(key, |record| {
        let changed = record
            .destination
            .as_ref()
            .is_none_or(|chosen| chosen.repository_id != repository_id);
        if let ChosenFor::Project(project) = &chosen_for {
            record
                .applied
                .insert(project.clone(), repository_id.to_owned());
            record.bindings.insert(
                project.clone(),
                Binding {
                    repository: repository.to_owned(),
                    repository_id: repository_id.to_owned(),
                },
            );
        }
        record
            .asks
            .retain(|_, ask| ask.binding.repository_id != repository_id);
        record.destination = Some(Chosen {
            repository: repository.to_owned(),
            repository_id: repository_id.to_owned(),
            chosen_for,
            at: chrono::Utc::now().to_rfc3339(),
        });
        changed
    })
}

/// Takes the person's existing terminal choice for the folder as its record, when it has none:
/// nothing changes for them, so nothing is said.
pub(crate) fn adopt(key: &Key, repository: &str, repository_id: &str) -> Result<()> {
    change(key, |record| {
        if record.destination.is_none() {
            record.destination = Some(Chosen {
                repository: repository.to_owned(),
                repository_id: repository_id.to_owned(),
                chosen_for: ChosenFor::Folder,
                at: chrono::Utc::now().to_rfc3339(),
            });
        }
    })
}

/// Sets the folder's destination only when it has none yet, for a one-off push that names it.
/// Returns whether it did.
pub(crate) fn choose_first(key: &Key, repository: &str, repository_id: &str) -> Result<bool> {
    if chosen(key)?.is_some() {
        return Ok(false);
    }
    change(key, |record| {
        if record.destination.is_some() {
            return false;
        }
        record.destination = Some(Chosen {
            repository: repository.to_owned(),
            repository_id: repository_id.to_owned(),
            chosen_for: ChosenFor::Folder,
            at: chrono::Utc::now().to_rfc3339(),
        });
        true
    })
}

/// Whether `project`'s binding to `repository_id` was already applied as the folder's
/// destination once.
pub(crate) fn applied(key: &Key, project: &str, repository_id: &str) -> Result<bool> {
    Ok(read(key)?.is_some_and(|record| {
        record.applied.get(project).map(String::as_str) == Some(repository_id)
    }))
}

/// Remembers the Hub's binding for `project`, so its conversations upload only when the folder's
/// destination is that binding's repository.
pub(crate) fn note_binding(key: &Key, project: &str, binding: &Binding) -> Result<()> {
    if read(key)?.is_some_and(|record| record.bindings.get(project) == Some(binding)) {
        return Ok(());
    }
    change(key, |record| {
        record.bindings.insert(project.to_owned(), binding.clone());
    })
}

/// Keeps `binding` for `project` to ask the person about, once, at a terminal. A binding already
/// asked about is not asked about again.
pub(crate) fn ask_later(key: &Key, project: &str, binding: &Binding) -> Result<()> {
    if read(key)?.is_some_and(|record| {
        record
            .asks
            .get(project)
            .is_some_and(|ask| &ask.binding == binding)
    }) {
        return Ok(());
    }
    change(key, |record| {
        record.asks.insert(
            project.to_owned(),
            Ask {
                binding: binding.clone(),
                asked: false,
            },
        );
    })
}

/// A binding still to be asked about, and its project.
pub(crate) fn unasked(key: &Key) -> Result<Option<(String, Binding)>> {
    Ok(read(key)?.and_then(|record| {
        record
            .asks
            .into_iter()
            .find(|(_, ask)| !ask.asked)
            .map(|(project, ask)| (project, ask.binding))
    }))
}

/// A binding this computer holds for the person to decide: asked about already or not.
pub(crate) fn waiting(key: &Key) -> Result<Option<Binding>> {
    Ok(read(key)?.and_then(|record| record.asks.into_values().next().map(|ask| ask.binding)))
}

/// Records the person's answer about `project`'s binding: yes makes it the folder's destination;
/// no keeps the folder as it is, and the question is not asked again for that binding.
pub(crate) fn answer(key: &Key, project: &str, binding: &Binding, yes: bool) -> Result<bool> {
    if yes {
        return choose(
            key,
            &binding.repository,
            &binding.repository_id,
            ChosenFor::Project(project.to_owned()),
        );
    }
    change(key, |record| {
        if let Some(ask) = record.asks.get_mut(project)
            && &ask.binding == binding
        {
            ask.asked = true;
        }
        false
    })
}

/// Whether a conversation of `project` in the folder may upload to `repository_id`: the folder's
/// destination is that repository, and the Hub binds the project to the same repository, or, while
/// it has shown no binding for the project here, the destination was chosen for that project or
/// for the folder by the person.
/// A conversation whose project isn't known is judged by `candidates`, the folder's projects on
/// this computer: by the only one, else it uploads only when no known binding of any of them names
/// another repository.
pub(crate) fn allows(
    key: &Key,
    project: Option<&str>,
    candidates: &[String],
    repository_id: &str,
) -> Result<bool> {
    let Some(record) = read(key)? else {
        return Ok(false);
    };
    let Some(chosen) = &record.destination else {
        return Ok(false);
    };
    if chosen.repository_id != repository_id {
        return Ok(false);
    }
    let project = project.or(match candidates {
        [only] => Some(only.as_str()),
        _ => None,
    });
    let Some(project) = project else {
        return Ok(candidates.iter().all(|candidate| {
            record
                .bindings
                .get(candidate)
                .is_none_or(|binding| binding.repository_id == repository_id)
        }));
    };
    // The project's binding as the Hub last showed it decides first: one changed since the
    // folder's destination was chosen for this project names somewhere else now.
    Ok(match (record.bindings.get(project), &chosen.chosen_for) {
        (Some(binding), _) => binding.repository_id == repository_id,
        (None, ChosenFor::Project(chosen_for)) => chosen_for == project,
        (None, ChosenFor::Folder) => true,
    })
}

/// Said for a conversation held because its project's destination isn't confirmed here.
pub(crate) const HELD: &str = "Saved on this computer, not uploaded yet: this computer hasn't confirmed this project's repository. It uploads once it does.";

/// The question for a binding set on the web that this computer can't take by itself.
pub(crate) fn question(repository: &str) -> String {
    format!("Use {repository} for this folder? It was set on the web.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(repository: &str, id: &str) -> Binding {
        Binding {
            repository: repository.into(),
            repository_id: id.into(),
        }
    }

    /// A workspace conversation uploads only to its own project's destination. One the person
    /// chose for the folder serves a project the Hub binds to the same repository, or to none it
    /// has shown; one chosen for another project, or a project the Hub binds elsewhere, holds.
    /// Another account's record is another record.
    #[test]
    fn a_conversation_uploads_only_to_its_own_projects_destination() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            let folder = tempfile::tempdir().unwrap();
            let key = Key::new(folder.path(), "https://hub.example", "account-yuki").unwrap();
            let other = Key::new(folder.path(), "https://hub.example", "account-hao").unwrap();
            assert!(
                !allows(&key, Some("p"), &[], "repo-team").unwrap(),
                "nothing chosen"
            );
            assert!(choose(&key, "yuki/rc", "repo-own", ChosenFor::Folder).unwrap());
            assert!(allows(&key, Some("p"), &[], "repo-own").unwrap());
            assert!(!allows(&key, Some("p"), &[], "repo-team").unwrap());
            assert!(
                !allows(&other, Some("p"), &[], "repo-own").unwrap(),
                "another account"
            );
            note_binding(&key, "p", &binding("simco/agents", "repo-team")).unwrap();
            assert!(
                !allows(&key, Some("p"), &[], "repo-own").unwrap(),
                "the Hub binds the project elsewhere"
            );
            assert!(
                choose(
                    &key,
                    "simco/agents",
                    "repo-team",
                    ChosenFor::Project("p".into())
                )
                .unwrap()
            );
            assert!(allows(&key, Some("p"), &[], "repo-team").unwrap());
            assert!(
                !allows(&key, Some("q"), &[], "repo-team").unwrap(),
                "a destination chosen for another project"
            );
            note_binding(&key, "q", &binding("simco/agents", "repo-team")).unwrap();
            assert!(allows(&key, Some("q"), &[], "repo-team").unwrap());
            assert!(allows(&key, None, &["p".into(), "q".into()], "repo-team").unwrap());
            note_binding(&key, "q", &binding("lin/lumen", "repo-lin")).unwrap();
            assert!(!allows(&key, None, &["p".into(), "q".into()], "repo-team").unwrap());
        });
    }

    /// A one-off push sets the folder's destination only when it has none.
    #[test]
    fn a_first_push_sets_a_destination_and_a_later_one_does_not() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            let folder = tempfile::tempdir().unwrap();
            let key = Key::new(folder.path(), "https://hub.example", "account").unwrap();
            assert!(choose_first(&key, "me/first", "repo-first").unwrap());
            assert!(!choose_first(&key, "me/second", "repo-second").unwrap());
            assert_eq!(chosen(&key).unwrap().unwrap().repository, "me/first");
        });
    }

    /// A binding is asked about once; no keeps the folder as it is, yes makes it the folder's
    /// destination.
    #[test]
    fn a_binding_is_asked_about_once() {
        let home = tempfile::tempdir().unwrap();
        crate::rc::with_agit_home(home.path(), || {
            let folder = tempfile::tempdir().unwrap();
            let key = Key::new(folder.path(), "https://hub.example", "account").unwrap();
            let team = binding("simco/agents", "repo-team");
            ask_later(&key, "p", &team).unwrap();
            assert_eq!(unasked(&key).unwrap(), Some(("p".into(), team.clone())));
            assert!(!answer(&key, "p", &team, false).unwrap());
            assert_eq!(unasked(&key).unwrap(), None);
            ask_later(&key, "p", &team).unwrap();
            assert_eq!(unasked(&key).unwrap(), None, "asked once");
            assert_eq!(waiting(&key).unwrap(), Some(team.clone()));
            assert!(answer(&key, "p", &team, true).unwrap());
            assert_eq!(waiting(&key).unwrap(), None);
            assert_eq!(chosen(&key).unwrap().unwrap().repository, "simco/agents");
        });
    }
}
