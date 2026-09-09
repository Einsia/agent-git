//! A choice applies only while its source, claim and destination still match the observed input.

use super::{Destination, read_link};
use crate::adapter::{self, native_snapshot};
use crate::domain::{import_lineage, link, repo::Repo, store::Store};
use crate::infra::config;
use anyhow::{Context, ensure};
use serde::Deserialize;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

fn native_cwd(source: &native_snapshot::Source, bytes: &[u8]) -> crate::Result<Option<String>> {
    struct KindMatch<const OPENCODE: bool>(bool);
    impl<'de, const OPENCODE: bool> Deserialize<'de> for KindMatch<OPENCODE> {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct KindVisitor<const OPENCODE: bool>;
            impl<'de, const OPENCODE: bool> serde::de::Visitor<'de> for KindVisitor<OPENCODE> {
                type Value = KindMatch<OPENCODE>;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("a native metadata record kind")
                }
                fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                    Ok(KindMatch(
                        value
                            == if OPENCODE {
                                "opencode.meta"
                            } else {
                                "session_meta"
                            },
                    ))
                }
                fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                    Ok(KindMatch(false))
                }
                fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                    Ok(KindMatch(false))
                }
                fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                    Ok(KindMatch(false))
                }
                fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                    Ok(KindMatch(false))
                }
                fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                    Ok(KindMatch(false))
                }
                fn visit_seq<S: serde::de::SeqAccess<'de>>(
                    self,
                    mut sequence: S,
                ) -> Result<Self::Value, S::Error> {
                    while sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                    Ok(KindMatch(false))
                }
                fn visit_map<M: serde::de::MapAccess<'de>>(
                    self,
                    mut map: M,
                ) -> Result<Self::Value, M::Error> {
                    while map
                        .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                        .is_some()
                    {}
                    Ok(KindMatch(false))
                }
            }
            deserializer.deserialize_any(KindVisitor::<OPENCODE>)
        }
    }
    struct Envelope<const OPENCODE: bool> {
        metadata: bool,
        duplicate: bool,
    }
    impl<'de, const OPENCODE: bool> Deserialize<'de> for Envelope<OPENCODE> {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct EnvelopeVisitor<const OPENCODE: bool>;
            impl<'de, const OPENCODE: bool> serde::de::Visitor<'de> for EnvelopeVisitor<OPENCODE> {
                type Value = Envelope<OPENCODE>;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("an object containing a native metadata envelope")
                }
                fn visit_map<M: serde::de::MapAccess<'de>>(
                    self,
                    mut map: M,
                ) -> Result<Self::Value, M::Error> {
                    let mut envelope = Envelope {
                        metadata: false,
                        duplicate: false,
                    };
                    let mut selector = false;
                    let mut payload = false;
                    while let Some(key) = map.next_key::<Cow<'de, str>>()? {
                        if key == if OPENCODE { "kind" } else { "type" } {
                            envelope.duplicate |= selector;
                            selector = true;
                            envelope.metadata |= map.next_value::<KindMatch<OPENCODE>>()?.0;
                        } else {
                            if !OPENCODE && key == "payload" {
                                envelope.duplicate |= payload;
                                payload = true;
                            }
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                    Ok(envelope)
                }
            }
            deserializer.deserialize_map(EnvelopeVisitor::<OPENCODE>)
        }
    }
    struct CwdProbe<const OPENCODE: bool> {
        present: bool,
        duplicate: bool,
    }
    impl<'de, const OPENCODE: bool> Deserialize<'de> for CwdProbe<OPENCODE> {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct ProbeVisitor<const OPENCODE: bool>;
            impl<'de, const OPENCODE: bool> serde::de::Visitor<'de> for ProbeVisitor<OPENCODE> {
                type Value = CwdProbe<OPENCODE>;
                fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    formatter.write_str("an object containing native directory metadata")
                }
                fn visit_map<M: serde::de::MapAccess<'de>>(
                    self,
                    mut map: M,
                ) -> Result<Self::Value, M::Error> {
                    let mut probe = CwdProbe {
                        present: false,
                        duplicate: false,
                    };
                    while let Some(key) = map.next_key::<Cow<'de, str>>()? {
                        if key == if OPENCODE { "directory" } else { "cwd" } {
                            probe.duplicate |= probe.present;
                            probe.present = true;
                        }
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                    Ok(probe)
                }
            }
            deserializer.deserialize_map(ProbeVisitor::<OPENCODE>)
        }
    }
    #[derive(Deserialize)]
    struct CodexProbe {
        payload: Option<CwdProbe<false>>,
    }
    #[derive(Deserialize)]
    struct Codex<'a> {
        #[serde(borrow)]
        payload: Option<Directory<'a>>,
    }
    #[derive(Deserialize)]
    struct Directory<'a> {
        #[serde(borrow)]
        id: Option<Cow<'a, str>>,
        #[serde(borrow)]
        cwd: Option<Cow<'a, str>>,
    }
    #[derive(Deserialize)]
    struct Claude<'a> {
        #[serde(borrow, rename = "sessionId")]
        id: Option<Cow<'a, str>>,
        #[serde(borrow)]
        cwd: Option<Cow<'a, str>>,
    }
    #[derive(Deserialize)]
    struct OpenCode<'a> {
        #[serde(borrow)]
        id: Option<Cow<'a, str>>,
        #[serde(borrow)]
        directory: Option<Cow<'a, str>>,
    }
    for line in bytes.split(|byte| *byte == b'\n') {
        let Ok(line) = std::str::from_utf8(line) else {
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let (id, cwd) = match source.runtime {
            "claude-code" => {
                let Ok(probe) = serde_json::from_str::<CwdProbe<false>>(line) else {
                    continue;
                };
                ensure!(
                    !probe.duplicate,
                    "the native working directory field is repeated"
                );
                if !probe.present {
                    continue;
                }
                let fields: Claude<'_> = serde_json::from_str(line)?;
                (fields.id, fields.cwd)
            }
            "codex" => {
                let Ok(envelope) = serde_json::from_str::<Envelope<false>>(line) else {
                    continue;
                };
                if !envelope.metadata {
                    continue;
                }
                ensure!(
                    !envelope.duplicate,
                    "the native working directory metadata envelope is repeated"
                );
                let Ok(probe) = serde_json::from_str::<CodexProbe>(line) else {
                    continue;
                };
                let Some(probe) = probe.payload else { continue };
                ensure!(
                    !probe.duplicate,
                    "the native working directory field is repeated"
                );
                if !probe.present {
                    continue;
                }
                let fields: Codex<'_> = serde_json::from_str(line)?;
                let Some(fields) = fields.payload else {
                    continue;
                };
                (fields.id, fields.cwd)
            }
            "opencode" => {
                let Ok(envelope) = serde_json::from_str::<Envelope<true>>(line) else {
                    continue;
                };
                if !envelope.metadata {
                    continue;
                }
                ensure!(
                    !envelope.duplicate,
                    "the native working directory metadata envelope is repeated"
                );
                let Ok(probe) = serde_json::from_str::<CwdProbe<true>>(line) else {
                    continue;
                };
                ensure!(
                    !probe.duplicate,
                    "the native working directory field is repeated"
                );
                if !probe.present {
                    continue;
                }
                let fields: OpenCode<'_> = serde_json::from_str(line)?;
                (fields.id, fields.directory)
            }
            _ => return Ok(None),
        };
        let Some(cwd) = cwd.filter(|cwd| !cwd.is_empty()) else {
            continue;
        };
        ensure!(
            id.as_deref() == Some(source.session_id.as_str()),
            "the native working directory has no matching selected session identity"
        );
        ensure!(
            !cwd.contains('\0') && Path::new(cwd.as_ref()).is_absolute(),
            "the native working directory is not an absolute filesystem path"
        );
        return Ok(Some(cwd.into_owned()));
    }
    Ok(None)
}

struct PathIdentity {
    path: PathBuf,
    canonical: PathBuf,
    handle: same_file::Handle,
    directory: bool,
}

impl PathIdentity {
    fn capture(path: &Path, directory: bool) -> crate::Result<Self> {
        let metadata = std::fs::symlink_metadata(path)?;
        ensure!(
            if directory {
                metadata.is_dir() && !metadata.is_symlink()
            } else {
                metadata.is_file() && !metadata.is_symlink()
            },
            "the selected source is not an ordinary filesystem object"
        );
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
            };
            options.custom_flags(
                FILE_FLAG_OPEN_REPARSE_POINT
                    | if directory {
                        FILE_FLAG_BACKUP_SEMANTICS
                    } else {
                        0
                    },
            );
        }
        let file = options.open(path)?;
        let opened = file.metadata()?;
        ensure!(
            if directory {
                opened.is_dir()
            } else {
                opened.is_file()
            },
            "the opened source is not the selected object kind"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                opened.file_attributes()
                    & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                    == 0,
                "the selected source is a reparse point"
            );
        }
        Ok(Self {
            path: path.to_owned(),
            canonical: path.canonicalize()?,
            handle: same_file::Handle::from_file(file)?,
            directory,
        })
    }

    fn verify(&self) -> crate::Result<()> {
        let current = Self::capture(&self.path, self.directory)?;
        ensure!(
            self.canonical == current.canonical && self.handle == current.handle,
            "the selected filesystem source was replaced"
        );
        Ok(())
    }
}

struct Repository {
    root: PathIdentity,
    common: PathIdentity,
    branch_tip: Option<String>,
}

fn branch_tip(repo: &Repo, branch: &str) -> crate::Result<Option<String>> {
    let (status, value, _) = repo.git_status_local(&[
        "rev-parse",
        "--verify",
        "--quiet",
        &format!("refs/heads/{branch}"),
    ])?;
    match status {
        Some(0) => {
            ensure!(
                matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit()),
                "the destination has an invalid frozen reference"
            );
            Ok(Some(value))
        }
        Some(1) => Ok(None),
        _ => anyhow::bail!("the destination branch could not be inspected"),
    }
}

pub(super) struct Snapshot {
    destination: Destination,
    store: Store,
    cwd: PathBuf,
    hub: String,
    source: native_snapshot::Source,
    native_identity: PathIdentity,
    native: Vec<u8>,
    native_cwd: Option<String>,
    link: Option<link::Link>,
    link_image: Option<Vec<u8>>,
    repository: Option<Repository>,
}

impl Snapshot {
    pub(super) fn capture(
        destination: Destination,
        store: Store,
        source: native_snapshot::Source,
        selected: Option<(Vec<u8>, link::Link)>,
        adapter: &dyn adapter::Adapter,
    ) -> crate::Result<Self> {
        let native_identity = PathIdentity::capture(&source.path, false)?;
        let native =
            adapter.snapshot_native_readonly(&source, native_snapshot::Limits::default())?;
        ensure!(
            native.source == source,
            "the native provider returned another source"
        );
        native_identity.verify()?;
        let native_cwd = if selected
            .as_ref()
            .is_some_and(|(_, selected)| selected.cwd.is_some())
        {
            None
        } else {
            native_cwd(&source, &native.bytes)?
        };
        let repository = if let Some(repo) = Repo::open(&destination.directory) {
            crate::commands::migration::check_readonly_repo_startup_local(&repo)?;
            let common = repo.common_dir_with_policy(crate::domain::repo::ReadPolicy::LocalOnly)?;
            Some(Repository {
                root: PathIdentity::capture(&destination.directory, true)?,
                common: PathIdentity::capture(&common, true)?,
                branch_tip: branch_tip(&repo, &destination.branch)?,
            })
        } else {
            ensure!(
                std::fs::symlink_metadata(&destination.directory)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "the destination is not an absent or readable repository"
            );
            None
        };
        let (link_image, link) = match selected {
            Some((bytes, selected)) => (Some(bytes), Some(selected)),
            None => (None, None),
        };
        Ok(Self {
            destination,
            store,
            cwd: std::env::current_dir()?,
            hub: config::hub_url(),
            source,
            native_identity,
            native: native.bytes,
            native_cwd,
            link,
            link_image,
            repository,
        })
    }

    pub(super) fn repo(&self) -> Option<Repo> {
        self.repository
            .as_ref()
            .map(|_| Repo::at(&self.destination.directory))
    }

    pub(super) fn selected_link(&self) -> link::Link {
        let mut selected = self
            .link
            .clone()
            .unwrap_or_else(|| link::Link::new(self.source.runtime, &self.source.session_id, None));
        if selected.cwd.is_none() {
            selected.cwd = self.native_cwd.clone();
        }
        selected
    }

    pub(super) fn native(&self) -> &[u8] {
        &self.native
    }

    fn verify(&self, candidate: Option<&import_lineage::Candidate>) -> crate::Result<()> {
        // Observation ignores inherited Git overrides; ordinary application must target the same repository.
        for key in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_NAMESPACE",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_REPLACE_REF_BASE",
            "GIT_SHALLOW_FILE",
            "GIT_GRAFT_FILE",
        ] {
            ensure!(
                std::env::var_os(key).is_none(),
                "unset {key} before applying a lineage choice"
            );
        }
        ensure!(
            std::env::current_dir()? == self.cwd && config::hub_url() == self.hub,
            "the import routing changed after the choice was prepared"
        );
        ensure!(
            self.cwd.join(config::store_root()?) == self.store.root(),
            "the selected store changed after the choice was prepared"
        );
        let (owner, name) = self
            .destination
            .slug
            .split_once('/')
            .context("the selected destination has no owner")?;
        ensure!(
            self.cwd.join(config::repo_dir(owner, name)?) == self.destination.directory,
            "the selected repository route changed after the choice was prepared"
        );
        let adapter = adapter::get(self.source.runtime)?;
        let current_source = adapter
            .lookup_native_readonly(&self.source.session_id, native_snapshot::Limits::default())?;
        ensure!(
            current_source == self.source,
            "the selected native provider identity changed"
        );
        self.native_identity.verify()?;
        let current = adapter
            .snapshot_native_readonly(&current_source, native_snapshot::Limits::default())?;
        ensure!(
            current.source == self.source && current.bytes == self.native,
            "the native transcript changed after the choice was prepared"
        );
        self.native_identity.verify()?;
        let current_link = read_link(&self.store, self.source.runtime, &self.source.session_id)?;
        ensure!(
            current_link.as_ref().map(|(bytes, _)| bytes) == self.link_image.as_ref(),
            "the session claim changed after the choice was prepared"
        );
        if let Some(repository) = &self.repository {
            repository.root.verify()?;
            repository.common.verify()?;
            let repo = Repo::open(&self.destination.directory)
                .context("the selected repository disappeared")?;
            let common = repo.common_dir_with_policy(crate::domain::repo::ReadPolicy::LocalOnly)?;
            ensure!(
                common.canonicalize()? == repository.common.canonical,
                "the selected repository changed its object store"
            );
            let (status, toplevel, _) = repo.git_status_local(&["rev-parse", "--show-toplevel"])?;
            ensure!(
                status == Some(0)
                    && Path::new(&toplevel).canonicalize()? == repository.root.canonical,
                "the selected repository writes another working directory"
            );
            let grafts = repository.common.path.join("info/grafts");
            match std::fs::metadata(grafts) {
                Ok(metadata) => ensure!(
                    metadata.len() == 0,
                    "remove Git graft overlays before applying a lineage choice"
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            crate::commands::migration::check_readonly_repo_startup_local(&repo)?;
            ensure!(
                branch_tip(&repo, &self.destination.branch)? == repository.branch_tip,
                "the destination branch changed after the choice was prepared"
            );
            if let Some(candidate) = candidate {
                ensure!(
                    import_lineage::candidate_still_matches(
                        &repo,
                        &self.destination.slug,
                        &self.selected_link(),
                        &self.native,
                        candidate
                    )?,
                    "the selected candidate no longer matches its observed evidence"
                );
            }
        } else {
            ensure!(
                candidate.is_none(),
                "a lineage candidate requires its original repository"
            );
            ensure!(
                std::fs::symlink_metadata(&self.destination.directory)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "the destination appeared after the choice was prepared"
            );
        }
        Ok(())
    }

    pub(super) fn accept(
        self,
        candidate: Option<import_lineage::Candidate>,
    ) -> crate::Result<Accepted> {
        self.verify(candidate.as_ref())?;
        Ok(Accepted {
            snapshot: self,
            candidate,
        })
    }
}

pub(in crate::commands::import) struct Accepted {
    snapshot: Snapshot,
    candidate: Option<import_lineage::Candidate>,
}

impl Accepted {
    pub(in crate::commands::import) fn store(&self) -> Store {
        self.snapshot.store.clone()
    }
    pub(in crate::commands::import) fn repo_dir(&self) -> &Path {
        &self.snapshot.destination.directory
    }
    pub(in crate::commands::import) fn initial_link(&self) -> Option<link::Link> {
        self.snapshot.link.clone()
    }
    pub(in crate::commands::import) fn previous_image(&self) -> Option<Vec<u8>> {
        self.snapshot.link_image.clone()
    }
    pub(in crate::commands::import) fn found(&self) -> crate::commands::import::Found {
        crate::commands::import::Found {
            runtime: self.snapshot.source.runtime,
            session_id: self.snapshot.source.session_id.clone(),
            cwd: self.snapshot.selected_link().cwd,
        }
    }
    pub(in crate::commands::import) fn link(&self) -> link::Link {
        let mut selected = self.snapshot.selected_link();
        selected.naming_ignored = false;
        selected
    }
    pub(in crate::commands::import) fn verify(
        &self,
        store: &Store,
        repo: &Repo,
        owner: &str,
        agent: &str,
        branch: &str,
        onto: Option<&str>,
    ) -> crate::Result<()> {
        ensure!(
            store.root() == self.snapshot.store.root() && repo.root() == self.repo_dir(),
            "the application uses another store or repository"
        );
        ensure!(
            format!("{owner}/{agent}") == self.snapshot.destination.slug
                && branch == self.snapshot.destination.branch,
            "the application uses another destination"
        );
        ensure!(
            onto == self
                .candidate
                .as_ref()
                .map(|candidate| candidate.commit.as_str()),
            "the application uses another lineage candidate"
        );
        self.snapshot.verify(self.candidate.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_working_directory_requires_its_explicit_runtime_identity() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path().to_str().unwrap();
        for runtime in ["codex", "claude-code", "opencode"] {
            let source = native_snapshot::Source {
                runtime,
                session_id: "selected".into(),
                path: directory.path().join("native"),
                database: runtime == "opencode",
            };
            let record = |id: serde_json::Value, cwd: serde_json::Value| match runtime {
                "codex" => serde_json::json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}}),
                "claude-code" => {
                    serde_json::json!({"type":"user","sessionId":id,"cwd":cwd,"message":{"content":"synthetic"}})
                }
                "opencode" => serde_json::json!({"kind":"opencode.meta","id":id,"directory":cwd}),
                _ => unreachable!(),
            };
            let read =
                |value: serde_json::Value| native_cwd(&source, format!("{value}\n").as_bytes());
            assert_eq!(
                read(record("selected".into(), cwd.into()))
                    .unwrap()
                    .as_deref(),
                Some(cwd)
            );
            for id in [serde_json::Value::Null, "other".into()] {
                assert!(read(record(id, cwd.into())).is_err());
            }
            for value in [
                serde_json::json!(42),
                serde_json::json!({"path":cwd}),
                "relative".into(),
                "invalid\0path".into(),
            ] {
                assert!(read(record("selected".into(), value)).is_err());
            }
            for value in [serde_json::Value::Null, "".into()] {
                assert!(read(record("selected".into(), value)).unwrap().is_none());
            }
            let mut missing_id = record("selected".into(), cwd.into());
            match runtime {
                "codex" => {
                    missing_id["payload"].as_object_mut().unwrap().remove("id");
                }
                "claude-code" => {
                    missing_id.as_object_mut().unwrap().remove("sessionId");
                }
                "opencode" => {
                    missing_id.as_object_mut().unwrap().remove("id");
                }
                _ => unreachable!(),
            }
            assert!(read(missing_id).is_err());
            let bytes = format!(
                "{{\"type\":\"unknown\",\"payload\":{{\"cwd\":\"ignored\"}}}}\n{}\n{{\"kind\":\"part\",\"data\":{{\"cwd\":\"ignored\"}}}}\n",
                record("selected".into(), cwd.into())
            );
            assert_eq!(
                native_cwd(&source, bytes.as_bytes()).unwrap().as_deref(),
                Some(cwd)
            );
            assert!(native_cwd(&source, b"not json\n").unwrap().is_none());
            assert!(native_cwd(&source, b"\xff\n").unwrap().is_none());
            let mut metadata = record("selected".into(), cwd.into());
            match runtime {
                "codex" => metadata["kind"] = serde_json::json!({"unrelated":true}),
                "opencode" => metadata["type"] = serde_json::json!(["unrelated"]),
                "claude-code" => metadata["payload"] = serde_json::json!({"cwd":false}),
                _ => unreachable!(),
            }
            assert_eq!(read(metadata).unwrap().as_deref(), Some(cwd));
            let encoded = serde_json::to_string(cwd).unwrap();
            let repeated = match runtime {
                "codex" => format!(
                    "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"selected\",\"cwd\":{encoded},\"cwd\":{encoded}}}}}\n"
                ),
                "claude-code" => format!(
                    "{{\"type\":\"user\",\"sessionId\":\"selected\",\"cwd\":{encoded},\"cwd\":{encoded}}}\n"
                ),
                "opencode" => format!(
                    "{{\"kind\":\"opencode.meta\",\"id\":\"selected\",\"directory\":{encoded},\"directory\":{encoded}}}\n"
                ),
                _ => unreachable!(),
            };
            assert!(native_cwd(&source, repeated.as_bytes()).is_err());
            let repeated_envelope = match runtime {
                "codex" => vec![
                    format!(
                        "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"selected\",\"cwd\":{encoded}}},\"payload\":{{\"id\":\"selected\",\"cwd\":{encoded}}}}}\n"
                    ),
                    format!(
                        "{{\"type\":\"session_meta\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"selected\",\"cwd\":{encoded}}}}}\n"
                    ),
                    format!(
                        "{{\"type\":{{\"unknown\":true}},\"type\":\"session_meta\",\"payload\":{{\"id\":\"selected\",\"cwd\":{encoded}}}}}\n"
                    ),
                ],
                "opencode" => vec![format!(
                    "{{\"kind\":\"opencode.meta\",\"kind\":\"opencode.meta\",\"id\":\"selected\",\"directory\":{encoded}}}\n"
                )],
                _ => vec![],
            };
            for repeated in repeated_envelope {
                assert!(native_cwd(&source, repeated.as_bytes()).is_err());
            }
            if runtime == "codex" {
                assert!(native_cwd(&source, b"{\"type\":\"response_item\",\"type\":\"response_item\",\"payload\":{\"cwd\":\"ignored\"}}\n").unwrap().is_none());
            }
        }
    }

    #[test]
    fn selected_claim_preserves_existing_directory_and_fills_only_its_absence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native");
        std::fs::write(&path, b"{}\n").unwrap();
        let source = native_snapshot::Source {
            runtime: "codex",
            session_id: "selected".into(),
            path: path.clone(),
            database: false,
        };
        let mut snapshot = Snapshot {
            destination: Destination {
                slug: "me/repo".into(),
                branch: "imported".into(),
                directory: directory.path().join("repo"),
            },
            store: Store::at(directory.path().join("store")),
            cwd: directory.path().into(),
            hub: "http://127.0.0.1:1".into(),
            source,
            native_identity: PathIdentity::capture(&path, false).unwrap(),
            native: b"{}\n".to_vec(),
            native_cwd: Some("native-directory".into()),
            link: None,
            link_image: None,
            repository: None,
        };
        assert_eq!(
            snapshot.selected_link().cwd.as_deref(),
            Some("native-directory")
        );
        snapshot.link = Some(link::Link::new("codex", "selected", None));
        assert_eq!(
            snapshot.selected_link().cwd.as_deref(),
            Some("native-directory")
        );
        snapshot.link.as_mut().unwrap().cwd = Some("existing-directory".into());
        assert_eq!(
            snapshot.selected_link().cwd.as_deref(),
            Some("existing-directory")
        );
        assert_eq!(
            snapshot.link.as_ref().unwrap().cwd.as_deref(),
            Some("existing-directory")
        );
    }

    /// Byte equality does not authorize a filesystem object substituted after preview.
    #[test]
    fn held_identity_rejects_equal_byte_replacement_and_symlink_sources() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("native");
        std::fs::write(&path, b"synthetic").unwrap();
        let observed = PathIdentity::capture(&path, false).unwrap();
        observed.verify().unwrap();
        std::fs::rename(&path, directory.path().join("prior")).unwrap();
        std::fs::write(&path, b"synthetic").unwrap();
        assert!(observed.verify().is_err());
        assert!(PathIdentity::capture(directory.path(), false).is_err());
        assert!(PathIdentity::capture(&path, true).is_err());
        #[cfg(unix)]
        {
            let alias = directory.path().join("alias");
            std::os::unix::fs::symlink(&path, &alias).unwrap();
            assert!(PathIdentity::capture(&alias, false).is_err());
        }
    }
}
