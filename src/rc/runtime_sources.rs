//! Local runtime coordinates never come from remote control requests.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{File, Metadata};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const MAX_REGISTRY_BYTES: u64 = 1024 * 1024;
const REGISTRY_FILE: &str = "sources.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Enrollment {
    Automatic,
    Explicit,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileIdentity {
    volume: u64,
    file: u64,
    created_nanos: Option<u64>,
}

impl FileIdentity {
    fn matches(&self, current: &Self) -> bool {
        // Creation time availability depends on the filesystem and local build.
        self.volume == current.volume
            && self.file == current.file
            && match (self.created_nanos, current.created_nanos) {
                (Some(saved), Some(current)) => saved == current,
                _ => true,
            }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RuntimeSource {
    pub source_id: String,
    pub name: String,
    pub runtime: String,
    pub principal: String,
    pub home: PathBuf,
    pub identity: FileIdentity,
    pub executable: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    executable_origin: Option<String>,
    pub socket: Option<PathBuf>,
    pub generation: u64,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    enrollment: Option<Enrollment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    disabled_reason: Option<String>,
}

impl RuntimeSource {
    fn is_missing_temporary(&self, principal: &str, temporary_roots: &[PathBuf]) -> bool {
        if !self.enabled || self.runtime != "codex" || self.principal != principal {
            return false;
        }
        let legacy = self.enrollment.is_none()
            && self.socket.is_none()
            && self
                .home
                .file_name()
                .is_some_and(|name| name == self.name.as_str())
            && matches!(
                self.executable_origin.as_deref(),
                Some("path" | "observed" | "launch")
            );
        if self.enrollment != Some(Enrollment::Automatic) && !legacy {
            return false;
        }
        let temporary = temporary_roots.iter().any(|root| {
            let Ok(relative) = self.home.strip_prefix(root) else {
                return false;
            };
            let Some(first) = relative.components().next() else {
                return false;
            };
            // Unknown provenance only expires a recognizable temporary-directory allocation.
            !legacy || first.as_os_str().to_string_lossy().starts_with(".tmp")
        });
        temporary
            && std::fs::metadata(&self.home)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    }

    /// A copied store or replaced directory cannot inherit an existing control identity.
    pub fn validate(&self) -> crate::Result<()> {
        ensure!(self.enabled, "runtime source is disabled");
        ensure!(self.runtime == "codex", "unsupported runtime source");
        ensure!(
            self.principal == principal()?,
            "runtime source belongs to another OS account"
        );
        let home = self
            .home
            .canonicalize()
            .context("runtime home is unavailable")?;
        ensure!(
            home == self.home,
            "runtime home now resolves to a different directory; register it again"
        );
        ensure!(
            self.identity.matches(&file_identity(&home)?),
            "runtime home was replaced; register it again"
        );
        Ok(())
    }

    pub fn native(&self) -> crate::Result<super::native_codex::Source> {
        self.validate()?;
        let executable = self
            .executable
            .as_deref()
            .context("this source has no Codex executable; register it with --executable")?;
        super::native_codex::Source::new(&self.home, executable, self.socket.as_deref())
    }

    /// Only the opaque resource reference, source ID and native ID enter the cloud catalog.
    pub fn session_ref(&self, native: &str) -> String {
        crate::protocol::NativeSourceRef {
            source_id: self.source_id.clone(),
            generation: self.generation,
        }
        .session_ref(native)
    }
}

#[derive(Serialize, Deserialize)]
struct State {
    version: u32,
    sources: Vec<RuntimeSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    discovery: Option<super::runtime_discovery::Report>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            sources: Vec::new(),
            discovery: None,
        }
    }
}

#[derive(Clone)]
pub struct Registry {
    directory: PathBuf,
}

impl Registry {
    pub fn open() -> crate::Result<Self> {
        #[cfg(test)]
        let home = super::test_agit_home_override()
            .map(Ok)
            .unwrap_or_else(crate::infra::config::agit_home)?;
        #[cfg(not(test))]
        let home = crate::infra::config::agit_home()?;
        Self::at(home.join("runtime-sources"))
    }

    pub(crate) fn at(directory: PathBuf) -> crate::Result<Self> {
        crate::infra::config::create_state_dir(&directory)?;
        #[cfg(windows)]
        crate::infra::windows_security::private_directory(&directory)?;
        Ok(Self { directory })
    }

    pub fn list(&self) -> crate::Result<Vec<RuntimeSource>> {
        Ok(self.read()?.sources)
    }

    pub(crate) fn discovery(&self) -> crate::Result<Option<super::runtime_discovery::Report>> {
        Ok(self.read()?.discovery)
    }

    pub(crate) fn record_discovery(
        &self,
        report: super::runtime_discovery::Report,
    ) -> crate::Result<()> {
        let _lock = self.lock()?;
        let mut state = self.read()?;
        state.discovery = Some(report);
        self.save(&state)
    }

    /// Retirement revokes discovery coordinates without deleting transcripts or stopping tasks.
    pub(crate) fn retire_missing_temporary(&self) -> crate::Result<usize> {
        let principal = principal()?;
        let temporary_roots = vec![
            std::env::temp_dir(),
            #[cfg(unix)]
            PathBuf::from("/tmp"),
            #[cfg(unix)]
            PathBuf::from("/var/tmp"),
        ];
        let temporary_roots = temporary_roots
            .into_iter()
            .filter_map(|root| root.canonicalize().ok())
            .collect::<Vec<_>>();
        let candidates = self
            .read()?
            .sources
            .into_iter()
            .filter(|source| source.is_missing_temporary(&principal, &temporary_roots))
            .map(|source| source.source_id)
            .collect::<std::collections::HashSet<_>>();
        if candidates.is_empty() {
            return Ok(0);
        }
        let _lock = self.lock()?;
        let mut state = self.read()?;
        let mut retired = 0;
        for source in &mut state.sources {
            // Recheck under the registry lock so explicit re-enrollment wins a discovery race.
            if candidates.contains(&source.source_id)
                && source.is_missing_temporary(&principal, &temporary_roots)
            {
                source.generation = source
                    .generation
                    .checked_add(1)
                    .context("runtime source generation exhausted")?;
                source.enabled = false;
                source.disabled_reason = Some("missing_temporary_home".into());
                retired += 1;
            }
        }
        if retired > 0 {
            self.save(&state)?;
        }
        Ok(retired)
    }

    pub(crate) fn enroll_observed(
        &self,
        home: &Path,
        executable: Option<&Path>,
    ) -> crate::Result<RuntimeSource> {
        self.register_inner(home, executable, None, None, true)
    }

    pub(crate) fn pin_for_launch(
        &self,
        reference: &crate::protocol::NativeSourceRef,
    ) -> crate::Result<()> {
        let _lock = self.lock()?;
        let mut state = self.read()?;
        let source = state
            .sources
            .iter_mut()
            .find(|source| source.source_id == reference.source_id)
            .context("unknown runtime source")?;
        source.validate()?;
        ensure!(
            source.generation == reference.generation,
            "native source changed before attachment"
        );
        if source.executable_origin.as_deref() == Some("path") && source.executable.is_some() {
            // Discovery cannot replace a launcher's program while its child opens the source.
            source.executable_origin = Some("launch".into());
            self.save(&state)?;
        }
        Ok(())
    }

    pub fn resolve(&self, source_id: &str) -> crate::Result<RuntimeSource> {
        let source = self
            .read()?
            .sources
            .into_iter()
            .find(|source| source.source_id == source_id)
            .context("unknown runtime source; run agit rc sources list")?;
        source.validate()?;
        Ok(source)
    }

    pub fn register(
        &self,
        home: &Path,
        executable: Option<&Path>,
        socket: Option<&Path>,
        name: Option<&str>,
    ) -> crate::Result<RuntimeSource> {
        self.register_inner(home, executable, socket, name, false)
    }

    pub(crate) fn default_for_launch(
        &self,
    ) -> crate::Result<Option<crate::protocol::NativeSourceRef>> {
        let home = crate::adapter::codex::codex_home()?;
        let Ok(home) = home.canonicalize() else {
            return Ok(None);
        };
        let Some(source) = self
            .list()?
            .into_iter()
            .filter(|source| source.home == home)
            .max_by_key(|source| source.enabled)
        else {
            return Ok(None);
        };
        source.validate()?;
        Ok(Some(crate::protocol::NativeSourceRef {
            source_id: source.source_id,
            generation: source.generation,
        }))
    }

    pub(crate) fn enroll_default(&self) -> crate::Result<()> {
        let home = crate::adapter::codex::codex_home()?;
        if home.is_dir() {
            self.register_inner(&home, None, None, None, true)?;
        }
        Ok(())
    }

    fn register_inner(
        &self,
        home: &Path,
        executable: Option<&Path>,
        socket: Option<&Path>,
        name: Option<&str>,
        automatic: bool,
    ) -> crate::Result<RuntimeSource> {
        let home = home.canonicalize().context("cannot resolve runtime home")?;
        let identity = file_identity(&home)?;
        let principal = principal()?;
        let executable = executable
            .map(|path| -> crate::Result<PathBuf> {
                let path = path
                    .canonicalize()
                    .context("cannot resolve Codex executable")?;
                ensure!(path.is_file(), "Codex executable is not a file");
                Ok(path)
            })
            .transpose()?;
        if let Some(socket) = socket {
            ensure!(socket.is_absolute(), "Codex socket path must be absolute");
        }
        let rename_requested = name.is_some();
        let name = name.map(str::to_owned).unwrap_or_else(|| {
            home.file_name()
                .unwrap_or(home.as_os_str())
                .to_string_lossy()
                .into_owned()
        });
        ensure!(
            !name.trim().is_empty() && name.len() <= 128 && !name.chars().any(char::is_control),
            "source name must be nonempty text without control characters"
        );
        let _lock = self.lock()?;
        let mut state = self.read()?;
        // Automatic discovery must not revive an explicitly removed or replaced source.
        if automatic
            && let Some(source) = state.sources.iter_mut().find(|source| source.home == home)
        {
            let enrich = source.enabled
                && source.identity.matches(&identity)
                && source.principal == principal
                && (source.executable.is_none()
                    || source.executable_origin.as_deref() == Some("path"))
                && executable
                    .as_ref()
                    .is_some_and(|executable| Some(executable) != source.executable.as_ref());
            if enrich {
                source.executable = executable;
                source.executable_origin = Some("observed".into());
                source.generation = source
                    .generation
                    .checked_add(1)
                    .context("runtime source generation exhausted")?;
            }
            let source = source.clone();
            if enrich {
                self.save(&state)?;
            }
            return Ok(source);
        }
        if let Some(source) = state.sources.iter_mut().find(|source| {
            source.home == home
                && source.identity.matches(&identity)
                && source.principal == principal
        }) {
            let changed = !source.enabled
                || (rename_requested && source.name != name)
                || (executable.is_some()
                    && source.executable_origin.as_deref() != Some("explicit"))
                || executable
                    .as_ref()
                    .is_some_and(|exe| Some(exe) != source.executable.as_ref())
                || socket.is_some_and(|path| Some(path) != source.socket.as_deref());
            if changed {
                source.generation = source
                    .generation
                    .checked_add(1)
                    .context("runtime source generation exhausted")?;
                source.enabled = true;
                source.disabled_reason = None;
                if rename_requested {
                    source.name = name;
                }
                if executable.is_some() {
                    source.executable = executable;
                    source.executable_origin = Some("explicit".into());
                }
                if socket.is_some() {
                    source.socket = socket.map(Path::to_path_buf);
                }
            }
            let enrollment_changed = source.enrollment != Some(Enrollment::Explicit);
            source.enrollment = Some(Enrollment::Explicit);
            let source = source.clone();
            if changed || enrollment_changed {
                self.save(&state)?;
            }
            return Ok(source);
        }
        // Re-enrollment never leaves a stale identity active at the same coordinates.
        for source in &mut state.sources {
            if source.home == home && source.principal == principal && source.enabled {
                source.enabled = false;
                source.generation = source
                    .generation
                    .checked_add(1)
                    .context("runtime source generation exhausted")?;
            }
        }
        let source = RuntimeSource {
            source_id: format!("src-{}", uuid::Uuid::new_v4()),
            name,
            runtime: "codex".into(),
            principal,
            home,
            identity,
            executable_origin: Some(
                if executable.is_none() {
                    "path"
                } else if automatic {
                    "observed"
                } else {
                    "explicit"
                }
                .into(),
            ),
            executable: executable.or_else(|| {
                crate::adapter::which("codex").and_then(|path| path.canonicalize().ok())
            }),
            socket: socket.map(Path::to_path_buf),
            generation: 1,
            enabled: true,
            enrollment: Some(if automatic {
                Enrollment::Automatic
            } else {
                Enrollment::Explicit
            }),
            disabled_reason: None,
        };
        state.sources.push(source.clone());
        self.save(&state)?;
        Ok(source)
    }

    pub fn remove(&self, source_id: &str) -> crate::Result<()> {
        let _lock = self.lock()?;
        let mut state = self.read()?;
        let source = state
            .sources
            .iter_mut()
            .find(|source| source.source_id == source_id)
            .context("unknown runtime source")?;
        if source.enabled {
            source.enabled = false;
            source.disabled_reason = None;
            source.generation = source
                .generation
                .checked_add(1)
                .context("runtime source generation exhausted")?;
            self.save(&state)?;
        }
        Ok(())
    }

    fn read(&self) -> crate::Result<State> {
        let path = self.directory.join(REGISTRY_FILE);
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(State::default());
            }
            Err(error) => return Err(error.into()),
            Ok(metadata) => validate_file(&metadata)?,
        }
        let file = private_options().read(true).open(path)?;
        validate_file(&file.metadata()?)?;
        let mut bytes = Vec::new();
        file.take(MAX_REGISTRY_BYTES + 1).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_REGISTRY_BYTES,
            "runtime source registry exceeds its size limit"
        );
        let state: State = serde_json::from_slice(&bytes)
            .context("runtime source registry is invalid; existing registrations were preserved")?;
        ensure!(
            state.version == 1,
            "unsupported runtime source registry version"
        );
        Ok(state)
    }

    fn save(&self, state: &State) -> crate::Result<()> {
        let bytes = serde_json::to_vec_pretty(state)?;
        ensure!(
            bytes.len() as u64 <= MAX_REGISTRY_BYTES,
            "runtime source registry exceeds its size limit"
        );
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join(REGISTRY_FILE))
            .map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }

    fn lock(&self) -> crate::Result<File> {
        let path = self.directory.join("sources.lock");
        #[cfg(windows)]
        let file = crate::infra::windows_security::open_private_control(&path)?;
        #[cfg(not(windows))]
        let file = private_options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        validate_file(&file.metadata()?)?;
        fs2::FileExt::lock_exclusive(&file)?;
        Ok(file)
    }
}

fn private_options() -> std::fs::OpenOptions {
    let options = crate::infra::config::state_file_options();
    #[cfg(unix)]
    let options = {
        use std::os::unix::fs::OpenOptionsExt;
        let mut options = options;
        options.custom_flags(libc::O_NOFOLLOW);
        options
    };
    options
}

fn validate_file(metadata: &Metadata) -> crate::Result<()> {
    ensure!(
        metadata.is_file(),
        "runtime source state is not a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
            "runtime source state is writable by another OS account"
        );
    }
    Ok(())
}

fn principal() -> crate::Result<String> {
    #[cfg(unix)]
    {
        Ok(format!("unix:{}", unsafe { libc::geteuid() }))
    }
    #[cfg(windows)]
    {
        Ok(format!(
            "windows:{}",
            crate::infra::windows_security::current_sid()?
        ))
    }
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!("runtime source registration is unsupported on this platform")
}

fn file_identity(path: &Path) -> crate::Result<FileIdentity> {
    let metadata = std::fs::metadata(path)?;
    ensure!(metadata.is_dir(), "runtime home is not a directory");
    let created_nanos = metadata
        .created()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_nanos()).ok());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(FileIdentity {
            volume: metadata.dev(),
            file: metadata.ino(),
            created_nanos,
        })
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES,
            GetFileInformationByHandle,
        };
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
            .open(path)?;
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut info) } != 0,
            "cannot read runtime directory identity"
        );
        Ok(FileIdentity {
            volume: info.dwVolumeSerialNumber as u64,
            file: ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            created_nanos,
        })
    }
    #[cfg(not(any(unix, windows)))]
    anyhow::bail!("runtime directory identity is unsupported on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temporary_retirement_preserves_explicit_sources_and_rejects_reused_coordinates() {
        let root = tempfile::tempdir().unwrap();
        let registry = Registry::at(root.path().join("registry")).unwrap();
        let allocated = tempfile::tempdir().unwrap();
        let home = allocated.path().join("home/.codex");
        std::fs::create_dir_all(&home).unwrap();
        let automatic = registry.enroll_observed(&home, None).unwrap();
        let explicit_home = root.path().join("registered-home");
        std::fs::create_dir(&explicit_home).unwrap();
        let explicit = registry.register(&explicit_home, None, None, None).unwrap();
        let legacy_home = allocated.path().join("legacy/.codex");
        std::fs::create_dir_all(&legacy_home).unwrap();
        let legacy = registry.enroll_observed(&legacy_home, None).unwrap();
        let mut state = registry.read().unwrap();
        state
            .sources
            .iter_mut()
            .find(|row| row.source_id == legacy.source_id)
            .unwrap()
            .enrollment = None;
        registry.save(&state).unwrap();
        std::fs::rename(&explicit_home, root.path().join("offline-home")).unwrap();
        drop(allocated);
        assert_eq!(registry.retire_missing_temporary().unwrap(), 2);
        assert_eq!(registry.retire_missing_temporary().unwrap(), 0);
        let retired = registry
            .list()
            .unwrap()
            .into_iter()
            .find(|row| row.source_id == automatic.source_id)
            .unwrap();
        assert!(!retired.enabled);
        assert_eq!(retired.generation, automatic.generation + 1);
        assert_eq!(
            retired.disabled_reason.as_deref(),
            Some("missing_temporary_home")
        );
        let retained = registry
            .list()
            .unwrap()
            .into_iter()
            .find(|row| row.source_id == explicit.source_id)
            .unwrap();
        assert!(retained.enabled);
        assert_eq!(retained.generation, explicit.generation);
        std::fs::rename(root.path().join("offline-home"), &explicit_home).unwrap();
        registry.resolve(&explicit.source_id).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        assert!(!registry.enroll_observed(&home, None).unwrap().enabled);
        assert!(registry.resolve(&automatic.source_id).is_err());
        std::fs::remove_dir_all(&home).unwrap();
        std::fs::remove_dir_all(home.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn creation_time_availability_preserves_registered_source_identity() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("native");
        std::fs::create_dir(&home).unwrap();
        let registry = Registry::at(root.path().join("registry")).unwrap();
        let source = registry.register(&home, None, None, None).unwrap();
        let mut state = registry.read().unwrap();
        state.sources[0].identity.created_nanos = None;
        registry.save(&state).unwrap();
        registry.resolve(&source.source_id).unwrap();
        let enrolled = registry.register(&home, None, None, None).unwrap();
        assert_eq!(enrolled.source_id, source.source_id);
        assert_eq!(enrolled.generation, source.generation);
        let mut known = source.identity.clone();
        known.created_nanos = Some(1);
        let mut unavailable = known.clone();
        unavailable.created_nanos = None;
        assert!(known.matches(&unavailable));
        assert!(unavailable.matches(&known));
        let mut replaced = known.clone();
        replaced.created_nanos = Some(2);
        assert!(!known.matches(&replaced));
        replaced.created_nanos = None;
        replaced.file += 1;
        assert!(!unavailable.matches(&replaced));
        replaced = unavailable.clone();
        replaced.volume += 1;
        assert!(!unavailable.matches(&replaced));
    }

    #[test]
    fn independent_stores_and_replacements_never_reuse_conversation_identity() {
        let root = tempfile::tempdir().unwrap();
        let registry = Registry::at(root.path().join("registry")).unwrap();
        let home = root.path().join("custom-profile");
        let copy = root.path().join("copied-profile");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&copy).unwrap();
        let first = registry.register(&home, None, None, None).unwrap();
        assert_eq!(
            registry
                .register(&home, None, None, None)
                .unwrap()
                .source_id,
            first.source_id
        );
        let other = registry.register(&copy, None, None, None).unwrap();
        assert_ne!(
            first.session_ref("same-thread"),
            other.session_ref("same-thread")
        );
        std::fs::rename(&home, root.path().join("old-home")).unwrap();
        std::fs::create_dir(&home).unwrap();
        assert!(registry.resolve(&first.source_id).is_err());
        let replaced = registry.register(&home, None, None, None).unwrap();
        assert_ne!(first.source_id, replaced.source_id);
        assert!(
            !registry
                .list()
                .unwrap()
                .iter()
                .find(|source| source.source_id == first.source_id)
                .unwrap()
                .enabled
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_aliases_converge_and_removal_does_not_touch_native_data() {
        let root = tempfile::tempdir().unwrap();
        let registry = Registry::at(root.path().join("registry")).unwrap();
        let home = root.path().join("actual-home");
        std::fs::create_dir(&home).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        let executable = std::env::current_exe().unwrap();
        let original = registry
            .register(&home, Some(&executable), None, Some("Named source"))
            .unwrap();
        let again = registry.register(&alias, None, None, None).unwrap();
        assert_eq!(original.source_id, again.source_id);
        assert_eq!(original.name, again.name);
        assert_eq!(original.executable, again.executable);
        assert_eq!(original.generation, again.generation);
        registry.remove(&original.source_id).unwrap();
        assert!(registry.resolve(&original.source_id).is_err());
        assert!(home.exists());
        let enrolled = registry.register(&home, None, None, None).unwrap();
        assert_eq!(enrolled.source_id, original.source_id);
        assert!(enrolled.generation > original.generation);
    }

    #[test]
    fn malformed_registry_is_never_replaced_by_an_empty_registration() {
        let root = tempfile::tempdir().unwrap();
        let registry = Registry::at(root.path().join("registry")).unwrap();
        let path = registry.directory.join(REGISTRY_FILE);
        std::fs::write(&path, "not-json").unwrap();
        assert!(registry.register(root.path(), None, None, None).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "not-json");
    }

    #[test]
    fn concurrent_enrollment_preserves_every_source() {
        let root = tempfile::tempdir().unwrap();
        let registry = Registry::at(root.path().join("registry")).unwrap();
        let mut workers = Vec::new();
        for index in 0..4 {
            let directory = registry.directory.clone();
            let home = root.path().join(format!("home-{index}"));
            std::fs::create_dir(&home).unwrap();
            workers.push(std::thread::spawn(move || {
                Registry::at(directory)
                    .unwrap()
                    .register(&home, None, None, None)
                    .unwrap()
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(registry.list().unwrap().len(), 4);
    }
    #[test]
    fn a_launch_binding_survives_observing_its_child_but_not_explicit_replacement() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("native");
        std::fs::create_dir(&home).unwrap();
        let launcher = root.path().join("launcher");
        let native = root.path().join("native-binary");
        std::fs::write(&launcher, "launcher").unwrap();
        std::fs::write(&native, "native").unwrap();
        let registry = Registry::at(root.path().join("registry")).unwrap();
        let source = registry
            .register(&home, Some(&launcher), None, None)
            .unwrap();
        let mut state = registry.read().unwrap();
        state.sources[0].executable_origin = Some("path".into());
        registry.save(&state).unwrap();
        let reference = crate::protocol::NativeSourceRef {
            source_id: source.source_id.clone(),
            generation: source.generation,
        };
        registry.pin_for_launch(&reference).unwrap();
        let observed = registry.enroll_observed(&home, Some(&native)).unwrap();
        assert_eq!(observed.generation, source.generation);
        assert_eq!(observed.executable, source.executable);
        registry.pin_for_launch(&reference).unwrap();
        let replaced = registry.register(&home, Some(&native), None, None).unwrap();
        assert!(replaced.generation > source.generation);
        assert!(registry.pin_for_launch(&reference).is_err());
        assert_eq!(replaced.executable, Some(native.canonicalize().unwrap()));
    }

    #[test]
    fn automatic_enrollment_never_revives_removed_or_replaced_homes() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("native");
        std::fs::create_dir(&home).unwrap();
        let registry = Registry::at(root.path().join("registry")).unwrap();
        let source = registry
            .register_inner(&home, None, None, None, true)
            .unwrap();
        registry.remove(&source.source_id).unwrap();
        let disabled = registry
            .register_inner(&home, None, None, None, true)
            .unwrap();
        assert!(!disabled.enabled);
        assert_eq!(disabled.generation, source.generation + 1);
        std::fs::rename(&home, root.path().join("moved")).unwrap();
        std::fs::create_dir(&home).unwrap();
        let replaced = registry
            .register_inner(&home, None, None, None, true)
            .unwrap();
        assert_eq!(replaced.source_id, source.source_id);
        assert!(replaced.validate().is_err());
    }
}
