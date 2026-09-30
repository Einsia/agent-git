//! Native process inspection retains only same-principal runtime coordinates.

use super::policy::CanonicalRoots;
use super::runtime_sources::Registry;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Report {
    pub processes: String,
    pub profiles: String,
    pub checked_at: i64,
}

struct Coordinates {
    home: PathBuf,
    executable: PathBuf,
}

pub(crate) fn discover(registry: &Registry, roots: &CanonicalRoots) -> crate::Result<Report> {
    let (processes, process_complete) = processes();
    let mut process_status = process_complete;
    let mut homes = std::collections::HashSet::new();
    for coordinates in processes {
        let home = match coordinates.home.canonicalize() {
            Ok(home) => home,
            // Exited processes and deleted temporary stores are not inspection failures.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                process_status = false;
                continue;
            }
        };
        if homes.insert(home.clone())
            && registry
                .enroll_observed(&home, Some(&coordinates.executable))
                .is_err()
        {
            process_status = false;
        }
    }
    let mut locations = roots.to_vec();
    if let Some(home) = crate::infra::config::user_home() {
        locations.push(home);
    }
    let mut profile_complete = true;
    let mut visited = 0;
    for location in locations {
        if visited >= 4096 {
            profile_complete = false;
            break;
        }
        let entries = match std::fs::read_dir(&location) {
            Ok(entries) => entries,
            Err(_) => {
                profile_complete = false;
                continue;
            }
        };
        let candidates = std::iter::once(Ok(location.clone()))
            .chain(entries.map(|entry| entry.map(|entry| entry.path())));
        for path in candidates {
            if visited >= 4096 {
                profile_complete = false;
                break;
            }
            visited += 1;
            let Ok(path) = path else {
                profile_complete = false;
                continue;
            };
            if !owned_directory(&path) || !recognized_store(&path) {
                continue;
            }
            let Ok(home) = path.canonicalize() else {
                profile_complete = false;
                continue;
            };
            if homes.insert(home.clone()) && registry.enroll_observed(&home, None).is_err() {
                profile_complete = false;
            }
        }
    }
    Ok(Report {
        processes: if cfg!(any(target_os = "linux", target_os = "macos")) {
            if process_status {
                "available"
            } else {
                "partial"
            }
        } else {
            "unsupported"
        }
        .into(),
        profiles: if profile_complete {
            "available"
        } else {
            "partial"
        }
        .into(),
        checked_at: chrono::Utc::now().timestamp_millis(),
    })
}

fn recognized_store(path: &Path) -> bool {
    path.join("sessions").is_dir()
        && (path.join("config.toml").is_file()
            || std::fs::read_dir(path).is_ok_and(|entries| {
                entries.take(256).flatten().any(|entry| {
                    entry
                        .file_name()
                        .to_str()
                        .and_then(|name| {
                            name.strip_prefix("state_")?
                                .strip_suffix(".sqlite")?
                                .parse::<u32>()
                                .ok()
                        })
                        .is_some()
                })
            }))
}

fn owned_directory(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        metadata.uid() == unsafe { libc::geteuid() }
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn native_executable(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "codex" | "codex.exe"))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn environment_home(bytes: &[u8]) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut configured = None;
    let mut home = None;
    for entry in bytes.split(|byte| *byte == 0) {
        for (key, target) in [
            (b"CODEX_HOME=".as_slice(), &mut configured),
            (b"HOME=".as_slice(), &mut home),
        ] {
            if let Some(value) = entry.strip_prefix(key) {
                if target.is_some() {
                    return None;
                }
                *target = Some(value);
            }
        }
    }
    let path = if let Some(value) = configured.filter(|value| !value.is_empty()) {
        PathBuf::from(std::ffi::OsStr::from_bytes(value))
    } else {
        PathBuf::from(std::ffi::OsStr::from_bytes(
            home.filter(|value| !value.is_empty())?,
        ))
        .join(".codex")
    };
    path.is_absolute().then_some(path)
}

#[cfg(target_os = "linux")]
fn processes() -> (Vec<Coordinates>, bool) {
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return (result, false);
    };
    let mut complete = true;
    for (count, entry) in entries.enumerate() {
        if count >= 65536 {
            complete = false;
            break;
        }
        let Ok(entry) = entry else {
            complete = false;
            continue;
        };
        if entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .is_none()
        {
            continue;
        }
        let path = entry.path();
        let Ok(before) = std::fs::metadata(&path) else {
            continue;
        };
        if before.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        let Ok(executable) = std::fs::read_link(path.join("exe")) else {
            continue;
        };
        if !native_executable(&executable) {
            continue;
        }
        let inspection = (|| -> Option<Coordinates> {
            let identity = std::fs::read(path.join("stat")).ok()?;
            let mut bytes = Vec::new();
            std::fs::File::open(path.join("environ"))
                .ok()?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() > 1024 * 1024 {
                return None;
            }
            let home = environment_home(&bytes);
            bytes.fill(0);
            let home = home?;
            let after = std::fs::metadata(&path).ok()?;
            let started = linux_start(&identity)?;
            if after.uid() != before.uid()
                || after.ino() != before.ino()
                || std::fs::read_link(path.join("exe")).ok()? != executable
                || Some(started) != linux_start(&std::fs::read(path.join("stat")).ok()?)
            {
                return None;
            }
            Some(Coordinates {
                home,
                executable: executable.clone(),
            })
        })();
        match inspection {
            Some(coordinates) => result.push(coordinates),
            None => complete = false,
        }
    }
    (result, complete)
}

#[cfg(target_os = "linux")]
fn linux_start(stat: &[u8]) -> Option<&[u8]> {
    let end = stat.iter().rposition(|byte| *byte == b')')?;
    stat[end + 1..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|word| !word.is_empty())
        .nth(19)
}

#[cfg(target_os = "macos")]
fn processes() -> (Vec<Coordinates>, bool) {
    use std::os::unix::ffi::OsStrExt;
    let mut pids = vec![0i32; 65536];
    let count = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    };
    if count <= 0 {
        return (Vec::new(), false);
    }
    let mut complete = (count as usize) < pids.len();
    pids.truncate((count as usize).min(pids.len()));
    let mut result = Vec::new();
    for pid in pids.into_iter().filter(|pid| *pid > 0) {
        let Some(before) = mac_identity(pid) else {
            continue;
        };
        let mut path = [0u8; 4096];
        let path_length =
            unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), path.len() as u32) };
        if path_length <= 0 {
            continue;
        }
        let end = path
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(path.len());
        let executable = PathBuf::from(std::ffi::OsStr::from_bytes(&path[..end]));
        if !native_executable(&executable) {
            continue;
        }
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut length = 0;
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if ok != 0 || length == 0 || length > 1024 * 1024 {
            complete = false;
            continue;
        }
        let mut bytes = vec![0u8; length];
        let ok = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                bytes.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        let home = if ok == 0 && length <= bytes.len() {
            mac_environment(&bytes[..length]).and_then(environment_home)
        } else {
            None
        };
        bytes.fill(0);
        let mut current_path = [0u8; 4096];
        let current_length = unsafe {
            libc::proc_pidpath(
                pid,
                current_path.as_mut_ptr().cast(),
                current_path.len() as u32,
            )
        };
        if mac_identity(pid) != Some(before)
            || current_length != path_length
            || current_path != path
        {
            complete = false;
            continue;
        }
        match home {
            Some(home) => result.push(Coordinates { home, executable }),
            None => complete = false,
        }
    }
    (result, complete)
}

#[cfg(target_os = "macos")]
fn mac_identity(pid: i32) -> Option<(u64, u64)> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (read == size && info.pbi_uid == unsafe { libc::geteuid() })
        .then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}

#[cfg(any(target_os = "macos", all(test, target_os = "linux")))]
fn mac_environment(bytes: &[u8]) -> Option<&[u8]> {
    let argc = i32::from_ne_bytes(bytes.get(..4)?.try_into().ok()?);
    if !(1..=32768).contains(&argc) {
        return None;
    }
    let mut rest = &bytes[4..];
    rest = &rest[rest.iter().position(|byte| *byte == 0)? + 1..];
    while rest.first() == Some(&0) {
        rest = &rest[1..];
    }
    for _ in 0..argc {
        rest = &rest[rest.iter().position(|byte| *byte == 0)? + 1..];
    }
    Some(rest)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn processes() -> (Vec<Coordinates>, bool) {
    (Vec::new(), false)
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn environment_projection_ignores_arguments_and_unrelated_values() {
        let mut bytes = 3i32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/bin/codex\0\0codex\0CODEX_HOME=/forged-argument\0\0TOKEN=private-fixture\0CODEX_HOME=/actual-home\0HOME=/fallback\0");
        assert_eq!(
            environment_home(mac_environment(&bytes).unwrap()),
            Some(PathBuf::from("/actual-home"))
        );
        assert_eq!(
            environment_home(b"HOME=/profile\0"),
            Some(PathBuf::from("/profile/.codex"))
        );
        assert!(environment_home(b"TOKEN=private-fixture\0").is_none());
        assert!(environment_home(b"CODEX_HOME=relative\0HOME=/profile\0").is_none());
        assert!(environment_home(b"CODEX_HOME=/first\0CODEX_HOME=/second\0").is_none());
        assert!(mac_environment(&[0, 0, 0, 0]).is_none());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn live_process_coordinates_survive_exit_without_reviving_removed_sources() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("codex");
        std::fs::copy("/bin/sleep", &executable).unwrap();
        let home = temporary.path().join("arbitrary-runtime-name");
        std::fs::create_dir(&home).unwrap();
        let mut child = std::process::Command::new(&executable)
            .arg("60")
            .env_clear()
            .env("HOME", temporary.path())
            .env("CODEX_HOME", &home)
            .env("PRIVATE_SENTINEL", "not-a-runtime-coordinate")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let found = loop {
            if let Some(coordinates) = processes()
                .0
                .into_iter()
                .find(|coordinates| coordinates.home == home)
            {
                break Some(coordinates);
            }
            if std::time::Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        child.kill().unwrap();
        child.wait().unwrap();
        let found =
            found.expect("the current OS account's test process must expose its native home");
        let registry = Registry::at(temporary.path().join("registry")).unwrap();
        registry.register(&found.home, None, None, None).unwrap();
        let source = registry
            .enroll_observed(&found.home, Some(&found.executable))
            .unwrap();
        assert_eq!(source.executable, Some(executable.canonicalize().unwrap()));
        let explicit = registry
            .register(&found.home, Some(&found.executable), None, None)
            .unwrap();
        let unchanged = registry
            .enroll_observed(&found.home, Some(Path::new("/bin/sh")))
            .unwrap();
        assert_eq!(explicit.generation, unchanged.generation);
        assert_eq!(explicit.executable, unchanged.executable);
        assert_eq!(
            registry.resolve(&source.source_id).unwrap().home,
            home.canonicalize().unwrap()
        );
        assert!(
            !std::fs::read_to_string(temporary.path().join("registry/sources.json"))
                .unwrap()
                .contains("not-a-runtime-coordinate")
        );
        registry.remove(&source.source_id).unwrap();
        assert!(
            !registry
                .enroll_observed(&found.home, Some(&found.executable))
                .unwrap()
                .enabled
        );
    }
}
