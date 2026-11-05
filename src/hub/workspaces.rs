//! A device's workspace and its folder projects, as `agit rc start --here` sets them up with the
//! person's own sign-in.

use anyhow::Result;
use serde::Deserialize;

/// The person's workspace on one of their devices.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DeviceWorkspace {
    pub id: String,
}

/// What binding a folder as a project answered.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FolderBind {
    /// The folder is a project of the workspace now (or already was).
    Bound(serde_json::Value),
    /// The Hub asks which repository the folder's sessions go to; `suggested` is the new one it
    /// would create, if any. Nothing was created.
    Choice { suggested: Option<String> },
}

impl super::Client {
    /// The person's workspace on their own device `device_id`: the one they have there, or a new
    /// one named `name` when they have none.
    pub(crate) fn device_workspace(&self, device_id: &str, name: &str) -> Result<DeviceWorkspace> {
        self.put(
            &format!("api/peer/devices/{device_id}/workspace"),
            &serde_json::json!({ "name": name }),
        )
    }

    /// Binds the folder `local_path` of the device as a project of `workspace_id`. Without a
    /// `repository` the Hub answers with its choice and creates nothing.
    pub(crate) fn bind_folder(
        &self,
        workspace_id: &str,
        local_path: &str,
        repository: Option<&str>,
    ) -> Result<FolderBind> {
        let mut body = serde_json::json!({ "local_path": local_path });
        if let Some(repository) = repository {
            body["repository"] = serde_json::json!(repository);
        }
        match self
            .post::<_, serde_json::Value>(&format!("api/workspaces/{workspace_id}/projects"), &body)
        {
            Ok(bound) => Ok(FolderBind::Bound(bound)),
            Err(error) => match error.downcast_ref::<super::client::ApiError>() {
                Some(api) if api.status == 409 && api.kind == "repository_choice" => {
                    Ok(FolderBind::Choice {
                        suggested: api.suggested_repository().map(str::to_owned),
                    })
                }
                _ => Err(error),
            },
        }
    }
}

/// A scripted Hub for tests of the flows that use these calls.
#[cfg(test)]
pub(crate) mod test_hub {
    use std::io::{Read, Write};

    /// Answers one request per `(status, body)` in `script`, in order; returns the Hub's base URL
    /// and a handle that yields each request's line and body.
    pub(crate) fn serve(
        script: Vec<(u16, &str)>,
    ) -> (String, std::thread::JoinHandle<Vec<(String, String)>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let script: Vec<(u16, String)> = script
            .into_iter()
            .map(|(status, body)| (status, body.to_owned()))
            .collect();
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (status, body) in script {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 8192];
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if rest.len() >= length {
                            break;
                        }
                    }
                    if read == 0 {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                let line = text.lines().next().unwrap_or_default().to_owned();
                let sent = text
                    .split_once("\r\n\r\n")
                    .map(|(_, body)| body.to_owned())
                    .unwrap_or_default();
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .as_bytes(),
                    )
                    .unwrap();
                seen.push((line, sent));
            }
            seen
        });
        (base, handle)
    }

    /// `AGIT_HOME` in a temporary folder for one test, so no real sign-in is read.
    pub(crate) struct TestHome {
        _lock: std::sync::MutexGuard<'static, ()>,
        _home: tempfile::TempDir,
        previous: Option<std::ffi::OsString>,
    }

    impl TestHome {
        pub(crate) fn new() -> Self {
            let lock = crate::infra::config::env_lock();
            let home = tempfile::tempdir().unwrap();
            let previous = std::env::var_os("AGIT_HOME");
            unsafe { std::env::set_var("AGIT_HOME", home.path()) };
            Self {
                _lock: lock,
                _home: home,
                previous,
            }
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(previous) => unsafe { std::env::set_var("AGIT_HOME", previous) },
                None => unsafe { std::env::remove_var("AGIT_HOME") },
            }
        }
    }

    pub(crate) fn client(base: &str) -> crate::hub::Client {
        crate::hub::Client::for_stored_hub_with_timeout(base, std::time::Duration::from_secs(5))
    }
}

#[cfg(test)]
mod tests {
    use super::test_hub::{TestHome, client, serve};

    /// The device's workspace is asked for by device and named on creation only.
    #[test]
    fn the_device_workspace_is_asked_for_by_device() {
        let _home = TestHome::new();
        let (base, hub) = serve(vec![(
            201,
            r#"{"id":"ws-1","name":"my laptop","owner_id":"a"}"#,
        )]);
        let workspace = client(&base)
            .device_workspace("dev-9", "my laptop")
            .unwrap();
        assert_eq!(workspace.id, "ws-1");
        let (line, sent) = hub.join().unwrap().remove(0);
        assert!(
            line.starts_with("PUT /api/peer/devices/dev-9/workspace "),
            "{line}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&sent).unwrap(),
            serde_json::json!({"name":"my laptop"})
        );
    }

    /// A folder bind without a repository reads the Hub's choice, and its suggested new name.
    #[test]
    fn a_folder_bind_reads_the_hubs_choice() {
        let _home = TestHome::new();
        let (base, hub) = serve(vec![
            (
                409,
                r#"{"error":"choose","kind":"repository_choice","fix":[],"candidates":["acme/work"],"suggested":"alice/work-sessions","unavailable":[],"audiences":{}}"#,
            ),
            (409, r#"{"error":"No device","kind":"conflict"}"#),
        ]);
        assert_eq!(
            client(&base).bind_folder("ws-1", "/work", None).unwrap(),
            super::FolderBind::Choice {
                suggested: Some("alice/work-sessions".into())
            }
        );
        assert!(
            client(&base).bind_folder("ws-1", "/work", None).is_err(),
            "another 409 is an error"
        );
        let seen = hub.join().unwrap();
        let (line, sent) = &seen[0];
        assert!(
            line.starts_with("POST /api/workspaces/ws-1/projects "),
            "{line}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(sent).unwrap(),
            serde_json::json!({"local_path":"/work"})
        );
    }
}
