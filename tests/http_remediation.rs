//! HTTP recovery belongs to the failure that ends the command, not to an incidental response.

#[cfg(unix)]
mod unix {
    use agit::infra::credentials::{HubCredential, save_at};
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, VecDeque};
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};

    const OLD_ACCESS: &str = "SYNTHETIC-http-old-access";
    const OLD_REFRESH: &str = "SYNTHETIC-http-old-refresh";
    const NEW_ACCESS: &str = "SYNTHETIC-http-new-access";
    const NEW_REFRESH: &str = "SYNTHETIC-http-new-refresh";
    const AGENT_ID: &str = "11111111-1111-4111-8111-111111111111";
    const SECRET_PATTERN: &str = "AKIA4X7QZ2M5RT6VW3JH";

    struct Request {
        method: String,
        target: String,
        authorization: Option<String>,
        body: Vec<u8>,
    }

    struct Step {
        method: &'static str,
        target: String,
        bearer: Option<&'static str>,
        status: u16,
        response: Value,
        after_response: Option<Box<dyn FnOnce() + Send>>,
    }

    impl Step {
        fn error(method: &'static str, target: &str, response: Value) -> Self {
            Self {
                method,
                target: target.into(),
                bearer: Some(OLD_ACCESS),
                status: 401,
                response,
                after_response: None,
            }
        }
    }

    struct Hub {
        base: String,
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<Vec<Request>>>,
    }

    impl Hub {
        fn new(build: impl FnOnce(&str) -> Vec<Step>) -> Self {
            Self::new_with_disconnect(build, false)
        }

        fn new_with_disconnect(build: impl FnOnce(&str) -> Vec<Step>, disconnect: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let mut steps = VecDeque::from(build(&base));
            let stop = Arc::new(AtomicBool::new(false));
            let stopped = Arc::clone(&stop);
            let worker = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                let mut requests = Vec::new();
                while !stopped.load(Ordering::Acquire) {
                    assert!(Instant::now() < deadline, "synthetic Hub deadline elapsed");
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(5));
                            continue;
                        }
                        Err(error) => panic!("synthetic Hub accept failed: {error}"),
                    };
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let request = read_request(&mut stream);
                    let mut step = steps.pop_front().unwrap_or_else(|| {
                        panic!(
                            "unexpected synthetic request: {} {}",
                            request.method, request.target
                        )
                    });
                    assert_eq!(request.method, step.method);
                    assert_eq!(request.target, step.target);
                    assert_eq!(
                        request.authorization,
                        step.bearer.map(|token| format!("Bearer {token}"))
                    );
                    if !disconnect {
                        let body = serde_json::to_vec(&step.response).unwrap();
                        write!(
                            stream,
                            "HTTP/1.1 {} Synthetic\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            step.status,
                            body.len()
                        )
                        .unwrap();
                        stream.write_all(&body).unwrap();
                        stream.flush().unwrap();
                    }
                    drop(stream);
                    if let Some(after_response) = step.after_response.take() {
                        after_response();
                    }
                    requests.push(request);
                }
                assert!(steps.is_empty(), "expected HTTP path was never exercised");
                requests
            });
            Self {
                base,
                stop,
                worker: Some(worker),
            }
        }

        fn finish(mut self) -> Vec<Request> {
            self.stop.store(true, Ordering::Release);
            self.worker.take().unwrap().join().unwrap()
        }
    }

    impl Drop for Hub {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    fn read_request(stream: &mut TcpStream) -> Request {
        let mut bytes = Vec::new();
        let mut buffer = [0; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "request ended before its headers");
            bytes.extend_from_slice(&buffer[..read]);
            assert!(bytes.len() <= 65536, "unexpected request size");
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let header = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let mut lines = header.lines();
        let mut start = lines.next().unwrap().split_whitespace();
        let method = start.next().unwrap().to_owned();
        let target = start.next().unwrap().to_owned();
        let mut body_length = 0;
        let mut authorization = None;
        for line in lines {
            if let Some((key, value)) = line.split_once(':') {
                if key.eq_ignore_ascii_case("content-length") {
                    body_length = value.trim().parse::<usize>().unwrap();
                }
                if key.eq_ignore_ascii_case("authorization") {
                    authorization = Some(value.trim().to_owned());
                }
            }
        }
        assert!(body_length <= 65536, "unexpected request body size");
        while bytes.len() < header_end + body_length {
            let read = stream.read(&mut buffer).unwrap();
            assert_ne!(read, 0, "request body was truncated");
            bytes.extend_from_slice(&buffer[..read]);
        }
        Request {
            method,
            target,
            authorization,
            body: bytes[header_end..header_end + body_length].to_vec(),
        }
    }

    struct Lab {
        _root: tempfile::TempDir,
        home: PathBuf,
        store: PathBuf,
        work: PathBuf,
    }

    impl Lab {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let root_path = root.path().canonicalize().unwrap();
            let home = root_path.join("home");
            let store = root_path.join("store");
            let work = root_path.join("work 'literal' <directory>");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&work).unwrap();
            Self {
                _root: root,
                home,
                store,
                work,
            }
        }

        fn command(&self, base: &str, args: &[&str]) -> Command {
            let mut command = Command::new(env!("CARGO_BIN_EXE_agit"));
            command
                .args(args)
                .stdin(Stdio::null())
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &self.home)
                .env("AGIT_HOME", &self.store)
                .env("AGIT_HUB_URL", base)
                .env("AGIT_SECRETS_KEYSTORE", "file")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("CI", "1")
                .env("NO_COLOR", "1")
                .current_dir(&self.work);
            command
        }

        fn credential_path(&self, base: &str) -> PathBuf {
            self.store.join("credentials").join(format!(
                "{}.json",
                agit::infra::config::hub_host_key(base).unwrap()
            ))
        }

        fn seed_credentials(&self, base: &str, refresh_valid: bool) {
            save_at(
                &self.credential_path(base),
                &credential(base, refresh_valid),
            )
            .unwrap();
        }

        fn state(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
            walkdir::WalkDir::new(self._root.path())
                .into_iter()
                .map(|entry| {
                    let entry = entry.unwrap();
                    assert!(!entry.file_type().is_symlink());
                    (
                        entry
                            .path()
                            .strip_prefix(self._root.path())
                            .unwrap()
                            .to_owned(),
                        entry
                            .file_type()
                            .is_file()
                            .then(|| fs::read(entry.path()).unwrap()),
                    )
                })
                .collect()
        }

        fn json(&self, base: &str, args: &[&str], code: i32) -> Value {
            let output = run_bounded(self.command(base, args));
            assert_eq!(output.status.code(), Some(code), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let value: Value = serde_json::from_slice(&output.stdout)
                .unwrap_or_else(|error| panic!("invalid JSON: {error}: {output:?}"));
            assert_eq!(value["exit_code"], code, "{value}");
            assert_eq!(value["ok"], code == 0, "{value}");
            for token in [OLD_ACCESS, OLD_REFRESH, NEW_ACCESS, NEW_REFRESH] {
                assert!(
                    !String::from_utf8_lossy(&output.stdout).contains(token),
                    "credential appeared in command output"
                );
            }
            value
        }

        fn initialize_repo(&self, base: &str) -> PathBuf {
            let output = run_bounded(self.command(base, &["init", "qa", "--no-bind"]));
            assert!(output.status.success(), "{output:?}");
            let path = self.store.join("repos/me/qa");
            assert!(path.join(".git").is_dir());
            let pin = json!({"hub":base,"agent_id":AGENT_ID}).to_string();
            self.git(&path, &["config", "--local", "agit.remoteIdentity", &pin]);
            self.git(
                &path,
                &["remote", "add", "origin", &format!("{base}/me/qa.git")],
            );
            path
        }

        fn git(&self, directory: &Path, args: &[&str]) -> Vec<u8> {
            let output = Command::new("git")
                .args(args)
                .stdin(Stdio::null())
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("HOME", &self.home)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .current_dir(directory)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
            output.stdout
        }

        fn assert_login(&self, value: &Value, base: &str) {
            assert_eq!(value["schema_version"], 2);
            assert_eq!(
                value["fix"],
                json!([{
                    "kind":"agit_command",
                    "argv":["agit","login","--hub",base],
                    "cwd":self.work,
                    "env":{"AGIT_HOME":self.store,"AGIT_HUB_URL":base},
                    "requires_interaction":true
                }]),
                "{value}"
            );
        }
    }

    fn run_bounded(command: Command) -> Output {
        run_with_input_bounded(command, None)
    }

    fn run_with_input_bounded(mut command: Command, input: Option<&[u8]>) -> Output {
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(input).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!("synthetic command deadline elapsed: {output:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn credential(base: &str, refresh_valid: bool) -> HubCredential {
        HubCredential {
            username: "me".into(),
            email: None,
            hub: Some(base.into()),
            access_token: OLD_ACCESS.into(),
            access_expires_at: "2099-01-01T00:00:00Z".into(),
            refresh_token: OLD_REFRESH.into(),
            refresh_expires_at: if refresh_valid {
                "2099-01-01T00:00:00Z"
            } else {
                "2000-01-01T00:00:00Z"
            }
            .into(),
        }
    }

    fn authentication_error(marker: &str) -> Value {
        json!({"error":marker,"kind":"unauthorized","fix":[{"kind":"authenticate"}]})
    }

    fn search_scope_step(target: &str, response: Value) -> Step {
        Step {
            method: "GET",
            target: target.into(),
            bearer: Some(OLD_ACCESS),
            status: 200,
            response,
            after_response: None,
        }
    }

    #[test]
    fn search_scope_reaches_the_hub_before_results_are_counted_or_paged() {
        for (scope, prefix, encoded_prefix) in [
            ("public", "is:public", "is%3Apublic"),
            ("mine", "owner:alice", "owner%3Aalice"),
            (
                "Alice/My-Repo",
                "agent:alice/my-repo",
                "agent%3Aalice%2Fmy-repo",
            ),
        ] {
            for kind in ["sessions", "agents"] {
                for version in ["1", "2"] {
                    for term in [None, Some("needle")] {
                        let query = term
                            .map_or_else(|| prefix.to_owned(), |term| format!("{prefix} {term}"));
                        let encoded = term.map_or_else(
                            || encoded_prefix.to_owned(),
                            |term| format!("{encoded_prefix}%20{term}"),
                        );
                        let lab = Lab::new();
                        let hub = Hub::new(|_| {
                            let mut steps = Vec::new();
                            if scope == "mine" {
                                steps.push(search_scope_step(
                                    "/api/auth/me",
                                    json!({"username":"alice"}),
                                ));
                            }
                            steps.push(search_scope_step(
                                &format!("/api/search/{kind}?q={encoded}&sort=recent&page=2&per=3"),
                                json!({"type":kind,"total":8,"page":2,"per":3,
                                    "items":[],"incomplete":true,"unknown":[]}),
                            ));
                            steps
                        });
                        lab.seed_credentials(&hub.base, false);
                        let mut argv = vec!["--json", "--json-version", version, "search"];
                        if let Some(term) = term {
                            argv.push(term);
                        }
                        argv.extend([
                            "--scope", scope, "--type", kind, "--mcp", "--sort", "recent",
                            "--page", "2", "-n", "3",
                        ]);
                        let value = lab.json(&hub.base, &argv, 0);
                        assert_eq!(value["schema_version"], version.parse::<u32>().unwrap());
                        assert_eq!(value["result"]["value"]["query"], query);
                        assert_eq!(value["result"]["value"]["total"], 8);
                        assert_eq!(value["result"]["value"]["incomplete"], true);
                        assert_eq!(value.get("fix").is_some(), version == "2");
                        assert_eq!(hub.finish().len(), if scope == "mine" { 2 } else { 1 });
                    }
                }
            }
        }
    }

    #[test]
    fn mine_uses_the_authenticated_identity_instead_of_the_saved_display_name() {
        let lab = Lab::new();
        let hub = Hub::new(|_| {
            vec![
                search_scope_step("/api/auth/me", json!({"username":"alice"})),
                search_scope_step(
                    "/api/search/sessions?q=owner%3Aalice%20needle&per=10",
                    json!({"type":"sessions","total":0,"items":[]}),
                ),
            ]
        });
        lab.seed_credentials(&hub.base, false);
        let value = lab.json(
            &hub.base,
            &["--json", "search", "needle", "--scope", "mine", "--mcp"],
            0,
        );
        assert_eq!(value["result"]["value"]["query"], "owner:alice needle");
        assert_eq!(hub.finish().len(), 2);
    }

    #[test]
    fn batch_scope_resolves_identity_once_before_every_search_and_rejects_all_on_conflict() {
        for version in ["1", "2"] {
            let lab = Lab::new();
            let hub = Hub::new(|_| {
                vec![
                    search_scope_step("/api/auth/me", json!({"username":"alice"})),
                    search_scope_step(
                        "/api/search/sessions?q=owner%3Aalice%20needle&per=10",
                        json!({"type":"sessions","total":0,"items":[]}),
                    ),
                    search_scope_step(
                        "/api/search/sessions?q=owner%3Aalice%20needle&per=10",
                        json!({"type":"sessions","total":0,"items":[]}),
                    ),
                ]
            });
            lab.seed_credentials(&hub.base, false);
            let value = lab.json(
                &hub.base,
                &[
                    "--json",
                    "--json-version",
                    version,
                    "search",
                    "needle",
                    "--query",
                    "needle",
                    "--scope",
                    "mine",
                ],
                0,
            );
            let result = &value["result"]["value"];
            assert_eq!(result["batch"], true);
            assert_eq!(result["results"].as_array().unwrap().len(), 2);
            for row in result["results"].as_array().unwrap() {
                assert_eq!(row["ok"], true);
                assert_eq!(row["query"], "owner:alice needle");
                assert_eq!(row["result"]["query"], "owner:alice needle");
            }
            assert_eq!(hub.finish().len(), 3);

            let hub = Hub::new(|_| Vec::new());
            lab.seed_credentials(&hub.base, false);
            for argv in [
                vec![
                    "--json",
                    "--json-version",
                    version,
                    "search",
                    "needle",
                    "--query",
                    "other is:private",
                    "--scope",
                    "public",
                ],
                vec![
                    "--json",
                    "--json-version",
                    version,
                    "search",
                    "needle",
                    "--repo",
                    "bob/demo",
                    "--scope",
                    "alice/demo",
                ],
            ] {
                let value = lab.json(&hub.base, &argv, 2);
                assert_eq!(value["result"]["format"], "empty");
            }
            assert!(hub.finish().is_empty());
        }
    }

    #[test]
    fn mcp_search_forwards_scope_to_the_authenticated_command() {
        for term in [None, Some("needle")] {
            let lab = Lab::new();
            let encoded = if term.is_some() {
                "agent%3Aalice%2Fmy-repo%20needle"
            } else {
                "agent%3Aalice%2Fmy-repo"
            };
            let hub = Hub::new(|_| {
                vec![search_scope_step(
                    &format!("/api/search/sessions?q={encoded}&per=10"),
                    json!({"type":"sessions","total":0,"items":[]}),
                )]
            });
            lab.seed_credentials(&hub.base, false);
            let mut arguments = json!({"scope":"alice/my-repo"});
            if let Some(term) = term {
                arguments["query"] = json!(term);
            }
            let input = format!(
                "{}\n{}\n",
                json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":"2025-03-26","capabilities":{},
                    "clientInfo":{"name":"synthetic-scope-client","version":"test"}}}),
                json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                    "name":"search","arguments":arguments}}),
            );
            let output =
                run_with_input_bounded(lab.command(&hub.base, &["mcp"]), Some(input.as_bytes()));
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            let replies: Vec<Value> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(replies.len(), 2);
            assert_eq!(replies[0]["result"]["protocolVersion"], "2025-03-26");
            let search: Value =
                serde_json::from_str(replies[1]["result"]["content"][0]["text"].as_str().unwrap())
                    .unwrap();
            assert_eq!(
                search["query"],
                if term.is_some() {
                    "agent:alice/my-repo needle"
                } else {
                    "agent:alice/my-repo"
                }
            );
            assert_eq!(search["total"], 0);
            assert_eq!(hub.finish().len(), 1);
        }
    }

    #[test]
    fn malformed_mcp_scope_never_falls_back_to_unscoped_search() {
        let lab = Lab::new();
        let hub = Hub::new(|_| Vec::new());
        lab.seed_credentials(&hub.base, false);
        for scope in [
            json!(null),
            json!(["public"]),
            json!({}),
            json!(7),
            json!(true),
        ] {
            let input = format!(
                "{}\n",
                json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                    "name":"search","arguments":{"query":"needle","scope":scope}}}),
            );
            let output =
                run_with_input_bounded(lab.command(&hub.base, &["mcp"]), Some(input.as_bytes()));
            assert!(output.status.success(), "{output:?}");
            let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
            let diagnostic = reply["result"]["content"][0]["text"].as_str().unwrap();
            assert!(diagnostic.starts_with("invalid search arguments: invalid search scope:"));
            assert_eq!(reply["result"]["isError"], true);
        }
        assert!(hub.finish().is_empty());
    }

    #[test]
    fn unsupported_or_conflicting_scopes_do_not_send_an_unscoped_search() {
        let lab = Lab::new();
        let hub = Hub::new(|_| Vec::new());
        lab.seed_credentials(&hub.base, false);
        for args in [
            vec![
                "--json", "search", "needle", "--scope", "public", "--counts",
            ],
            vec![
                "--json", "search", "needle", "--scope", "public", "--type", "prs",
            ],
            vec![
                "--json", "search", "needle", "--scope", "mine", "--type", "people",
            ],
            vec!["--json", "search", "--scope", "public", "-Q", ""],
            vec!["--json", "search", "--scope", "public", "-Q", " \t "],
            vec!["--json", "search", "needle", "--scope", "public", "-Q", ""],
            vec!["--json", "search", "needle is:private", "--scope", "public"],
            vec![
                "--json",
                "search",
                "needle agent:bob/repo",
                "--scope",
                "alice/repo",
            ],
        ] {
            let value = lab.json(&hub.base, &args, 2);
            assert_eq!(value["result"]["format"], "empty");
        }
        assert!(hub.finish().is_empty());
    }

    #[test]
    fn scope_does_not_allow_anonymous_search() {
        let lab = Lab::new();
        let hub = Hub::new(|_| Vec::new());
        for scope in ["mine", "public", "alice/repo"] {
            for term in [None, Some("needle")] {
                let mut argv = vec!["--json", "search", "--scope", scope];
                if let Some(term) = term {
                    argv.push(term);
                }
                let value = lab.json(&hub.base, &argv, 5);
                assert_eq!(value["ok"], false);
                lab.assert_login(&value, &hub.base);
            }
        }
        assert!(hub.finish().is_empty());
    }

    #[test]
    fn missing_credentials_keep_auth_category_and_selected_directory_in_all_formats() {
        for args in [
            vec!["search", "needle"],
            vec!["share", "list"],
            vec!["pr", "show", "1"],
            vec!["repo", "create", "qa"],
            vec!["repo", "list", "--remote"],
        ] {
            let lab = Lab::new();
            let hub = Hub::new(|_| vec![]);
            for version in [None, Some("1"), Some("2")] {
                let mut argv = vec!["-C", lab.work.to_str().unwrap(), "--json"];
                if let Some(version) = version {
                    argv.extend(["--json-version", version]);
                }
                argv.extend(args.iter().copied());
                let mut command = lab.command(&hub.base, &argv);
                command.current_dir(&lab.home);
                let output = run_bounded(command);
                assert_eq!(output.status.code(), Some(5), "{output:?}");
                assert!(output.stderr.is_empty(), "{output:?}");
                let value: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(value["exit_code"], 5);
                assert_eq!(value["ok"], false);
                if version == Some("1") {
                    assert_eq!(value["schema_version"], 1);
                    assert!(value.get("fix").is_none());
                } else {
                    lab.assert_login(&value, &hub.base);
                }
            }
            let mut command = lab.command(&hub.base, &["-C", lab.work.to_str().unwrap()]);
            command.args(&args).current_dir(&lab.home);
            let output = run_bounded(command);
            assert_eq!(output.status.code(), Some(5), "{output:?}");
            assert!(output.stdout.is_empty(), "{output:?}");
            assert!(String::from_utf8_lossy(&output.stderr).contains(&hub.base));
            assert!(!lab.credential_path(&hub.base).exists());
            assert!(hub.finish().is_empty());
        }
    }

    #[test]
    fn invalid_credential_configuration_is_not_missing_authentication() {
        for args in [vec!["search", "needle"], vec!["repo", "create", "qa"]] {
            for malformed in [false, true] {
                let lab = Lab::new();
                let hub = Hub::new(|_| vec![]);
                let path = lab.credential_path(&hub.base);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                let bytes = if malformed {
                    b"not credential JSON".to_vec()
                } else {
                    let mut value = credential(&hub.base, false);
                    value.hub = Some("https://other.invalid".into());
                    serde_json::to_vec(&value).unwrap()
                };
                fs::write(&path, &bytes).unwrap();
                let mut argv = vec!["--json"];
                argv.extend(args.iter().copied());
                let value = lab.json(&hub.base, &argv, 2);
                assert_eq!(value["fix"], json!([]));
                assert!(value.to_string().contains("credentials do not belong"));
                assert!(!value.to_string().contains("not logged in"));
                assert_eq!(fs::read(path).unwrap(), bytes);
                assert!(hub.finish().is_empty());
            }
        }
        let lab = Lab::new();
        let value = lab.json(
            "https://invalid.example/?route=other",
            &["--json", "search", "needle"],
            2,
        );
        assert_eq!(value["fix"], json!([]));
        assert!(!value.to_string().contains("not logged in"));
    }

    #[test]
    fn copied_login_hint_keeps_literal_hub_after_command_local_routing_expires() {
        for authenticated in [false, true] {
            let lab = Lab::new();
            let path = "/team&qa;printf/it's/$HOME";
            let hub = Hub::new(|_| {
                if authenticated {
                    vec![Step::error(
                        "GET",
                        &format!("{path}/api/shares"),
                        authentication_error("synthetic rejection"),
                    )]
                } else {
                    vec![]
                }
            });
            let selected = format!("{}{path}", hub.base);
            if authenticated {
                lab.seed_credentials(&selected, false);
            }
            let output = run_bounded(lab.command(&selected, &["share", "list"]));
            assert_eq!(output.status.code(), Some(5), "{output:?}");
            let stderr = String::from_utf8(output.stderr).unwrap();
            let line = stderr
                .lines()
                .find(|line| line.contains("with `agit login --hub "))
                .unwrap();
            let copied = line
                .split_once("with `")
                .unwrap()
                .1
                .strip_suffix('`')
                .unwrap();
            let script = format!("agit() {{ printf '%s\\n' \"$@\"; }}; {copied}");
            let replay = Command::new("sh")
                .args(["-c", &script])
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("AGIT_HUB_URL", "https://different.invalid")
                .env("HOME", &lab.home)
                .output()
                .unwrap();
            assert!(replay.status.success(), "{replay:?}");
            assert!(replay.stderr.is_empty(), "{replay:?}");
            assert_eq!(
                String::from_utf8(replay.stdout).unwrap(),
                format!("login\n--hub\n{selected}\n")
            );
            assert_eq!(hub.finish().len(), usize::from(authenticated));
        }
    }

    #[test]
    fn accepted_hub_spellings_keep_auth_errors_without_widening_the_fix_schema() {
        for scheme in ["HTTP", "hTtP"] {
            for authenticated in [false, true] {
                let lab = Lab::new();
                let hub = Hub::new(|_| {
                    if authenticated {
                        vec![Step::error(
                            "GET",
                            "/api/shares",
                            authentication_error("synthetic rejection"),
                        )]
                    } else {
                        vec![]
                    }
                });
                let selected = hub.base.replacen("http", scheme, 1);
                if authenticated {
                    lab.seed_credentials(&selected, false);
                }
                let value = lab.json(&selected, &["--json", "share", "list"], 5);
                assert_eq!(value["fix"], json!([]));
                assert!(value.to_string().contains(&selected));
                assert!(value.to_string().contains("agit login --hub"));
                assert_eq!(hub.finish().len(), usize::from(authenticated));
            }
        }
    }

    #[test]
    fn api_status_drives_auth_category_without_guessing_from_recipes_or_prose() {
        for (args, target, fallback) in [
            (
                vec!["search", "needle"],
                "/api/search/sessions?q=needle&per=10",
                6,
            ),
            (vec!["share", "list"], "/api/shares", 6),
            (vec!["rc", "list"], "/api/rc/connections", 6),
        ] {
            for status in [401, 403, 404, 500] {
                let lab = Lab::new();
                let hub = Hub::new(|_| {
                    let mut step =
                        Step::error("GET", target, authentication_error("HTTP 401: agit login"));
                    step.status = status;
                    vec![step]
                });
                lab.seed_credentials(&hub.base, false);
                let before = fs::read(lab.credential_path(&hub.base)).unwrap();
                let mut argv = vec!["--json"];
                argv.extend(args.iter().copied());
                let value = lab.json(&hub.base, &argv, if status == 401 { 5 } else { fallback });
                if status == 401 {
                    lab.assert_login(&value, &hub.base);
                } else {
                    assert_eq!(value["fix"], json!([]));
                    assert!(!value.to_string().contains("log in with `agit login --hub"));
                }
                assert!(value.to_string().contains("HTTP 401: agit login"));
                assert_eq!(fs::read(lab.credential_path(&hub.base)).unwrap(), before);
                assert_eq!(hub.finish().len(), 1);
            }
        }
    }

    #[test]
    fn hub_login_recipes_require_explicit_matching_context_in_every_json_version() {
        for (status, kind, recipe, login) in [
            (401, "unauthorized", Some(true), true),
            (401, "unauthorized", None, false),
            (401, "unauthorized", Some(false), false),
            (401, "share_password", Some(true), false),
            (401, "Unauthorized", Some(true), false),
            (401, " unauthorized ", Some(true), false),
            (401, "", Some(true), false),
            (404, "not_found", Some(true), false),
            (409, "conflict", Some(true), false),
            (503, "repository_unavailable", Some(true), false),
        ] {
            let lab = Lab::new();
            let hub = Hub::new(|_| {
                [None, Some("1"), Some("2"), None]
                    .into_iter()
                    .map(|_| {
                        let mut body = json!({"error":"synthetic credential refusal", "kind":kind});
                        if let Some(recipe) = recipe {
                            body["fix"] = if recipe {
                                json!([{"kind":"authenticate"}])
                            } else {
                                json!([])
                            };
                        }
                        let mut step =
                            Step::error("GET", "/api/search/sessions?q=needle&per=10", body);
                        step.status = status;
                        step
                    })
                    .collect()
            });
            lab.seed_credentials(&hub.base, false);
            let before = fs::read(lab.credential_path(&hub.base)).unwrap();
            for version in [None, Some("1"), Some("2")] {
                let mut args = vec!["--json"];
                if let Some(version) = version {
                    args.extend(["--json-version", version]);
                }
                args.extend(["search", "needle"]);
                let value = lab.json(&hub.base, &args, if status == 401 { 5 } else { 6 });
                assert!(value.to_string().contains("synthetic credential refusal"));
                assert!(value.to_string().contains(&hub.base));
                assert_eq!(
                    value.to_string().contains("log in with `agit login --hub"),
                    login
                );
                if version == Some("1") {
                    assert!(value.get("fix").is_none());
                } else if login {
                    lab.assert_login(&value, &hub.base);
                } else {
                    assert_eq!(value["fix"], json!([]));
                }
                assert_eq!(fs::read(lab.credential_path(&hub.base)).unwrap(), before);
            }
            let output = run_bounded(lab.command(&hub.base, &["search", "needle"]));
            assert_eq!(
                output.status.code(),
                Some(if status == 401 { 5 } else { 6 })
            );
            assert!(output.stdout.is_empty());
            let text = String::from_utf8(output.stderr).unwrap();
            assert!(text.contains("synthetic credential refusal"));
            assert!(text.contains(&hub.base));
            assert_eq!(text.contains("log in with `agit login --hub"), login);
            assert_eq!(fs::read(lab.credential_path(&hub.base)).unwrap(), before);
            assert_eq!(hub.finish().len(), 4);
        }
    }

    #[test]
    fn rejected_pat_keeps_auth_failure_instead_of_claiming_a_terminal_is_missing() {
        let lab = Lab::new();
        let hub = Hub::new(|_| {
            let mut step = Step::error(
                "POST",
                "/api/auth/login",
                authentication_error("synthetic rejected PAT"),
            );
            step.bearer = None;
            vec![step]
        });
        let mut command = lab.command(&hub.base, &["--json", "login", "--with-token"]);
        let input = lab.home.join("synthetic-pat.txt");
        fs::write(&input, OLD_ACCESS).unwrap();
        command.stdin(fs::File::open(input).unwrap());
        let output = run_bounded(command);
        assert_eq!(output.status.code(), Some(5), "{output:?}");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        lab.assert_login(&value, &hub.base);
        assert!(!value.to_string().contains("needs an interactive terminal"));
        assert!(!value.to_string().contains(OLD_ACCESS));
        assert!(!lab.credential_path(&hub.base).exists());
        let requests = hub.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[0].body).unwrap(),
            json!({"token":OLD_ACCESS})
        );
    }

    #[test]
    fn terminal_search_categories_and_counts_keep_server_recipes() {
        let lab = Lab::new();
        let kinds = ["sessions", "agents", "prs", "people"];
        let hub = Hub::new(|_| {
            kinds
                .iter()
                .map(|kind| {
                    Step::error(
                        "GET",
                        &format!("/api/search/{kind}?q=needle&per=10"),
                        authentication_error("synthetic terminal search"),
                    )
                })
                .chain([Step::error(
                    "GET",
                    "/api/search/counts?q=needle",
                    authentication_error("synthetic terminal counts"),
                )])
                .collect()
        });
        lab.seed_credentials(&hub.base, false);
        for kind in kinds {
            let value = lab.json(
                &hub.base,
                &["--json", "search", "needle", "--type", kind],
                5,
            );
            lab.assert_login(&value, &hub.base);
            assert!(value.to_string().contains("synthetic terminal search"));
        }
        let value = lab.json(&hub.base, &["--json", "search", "needle", "--counts"], 5);
        lab.assert_login(&value, &hub.base);
        assert!(value.to_string().contains("synthetic terminal counts"));
        assert_eq!(hub.finish().len(), kinds.len() + 1);
    }

    #[test]
    fn whoami_preserves_its_check_payload_and_uses_the_terminal_response() {
        let lab = Lab::new();
        let hub = Hub::new(|_| {
            let mut refresh = Step::error(
                "POST",
                "/api/auth/refresh",
                authentication_error("synthetic incidental refresh"),
            );
            refresh.bearer = None;
            vec![
                Step::error(
                    "GET",
                    "/api/auth/me",
                    authentication_error("synthetic terminal identity"),
                ),
                refresh,
            ]
        });
        lab.seed_credentials(&hub.base, true);
        let value = lab.json(&hub.base, &["--json", "whoami", "--check"], 5);
        lab.assert_login(&value, &hub.base);
        assert_eq!(value["result"]["value"]["account"], "me");
        assert_eq!(value["result"]["value"]["check"]["server_reachable"], true);
        assert_eq!(value["result"]["value"]["check"]["authenticated"], false);
        assert!(value.to_string().contains("synthetic terminal identity"));
        assert!(!value.to_string().contains("synthetic incidental refresh"));
        let requests = hub.finish();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[1].body).unwrap(),
            json!({"refresh_token":OLD_REFRESH})
        );
    }

    #[test]
    fn whoami_does_not_replace_other_http_refusals_with_sign_in_advice() {
        for (status, kind, recipe, login) in [
            (401, "unauthorized", true, true),
            (401, "unauthorized", false, false),
            (401, "share_password", true, false),
            (403, "forbidden", true, false),
            (503, "repository_unavailable", true, false),
        ] {
            let lab = Lab::new();
            let hub = Hub::new(|_| {
                let mut steps = Vec::new();
                for _ in [true, false] {
                    let mut body =
                        json!({"error":"synthetic identity refusal", "kind":kind, "fix":[]});
                    if recipe {
                        body["fix"] = json!([{"kind":"authenticate"}]);
                    }
                    let mut step = Step::error("GET", "/api/auth/me", body);
                    step.status = status;
                    steps.push(step);
                    if status == 401 {
                        let mut refresh = Step::error(
                            "POST",
                            "/api/auth/refresh",
                            authentication_error("synthetic discarded refresh"),
                        );
                        refresh.bearer = None;
                        steps.push(refresh);
                    }
                }
                steps
            });
            lab.seed_credentials(&hub.base, true);
            let before = fs::read(lab.credential_path(&hub.base)).unwrap();
            let code = if matches!(status, 401 | 403) { 5 } else { 6 };
            let value = lab.json(&hub.base, &["--json", "whoami", "--check"], code);
            assert!(value.to_string().contains("synthetic identity refusal"));
            assert!(!value.to_string().contains("synthetic discarded refresh"));
            assert_eq!(value["result"]["value"]["check"]["server_reachable"], true);
            assert_eq!(
                value.to_string().contains("log in with `agit login --hub"),
                login
            );
            if login {
                lab.assert_login(&value, &hub.base);
            } else {
                assert_eq!(value["fix"], json!([]));
                assert!(!value.to_string().contains("sign in again"));
            }
            let output = run_bounded(lab.command(&hub.base, &["whoami", "--check"]));
            assert_eq!(output.status.code(), Some(code));
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(stderr.contains("synthetic identity refusal"));
            assert!(!stderr.contains("synthetic discarded refresh"));
            assert_eq!(stderr.contains("log in with `agit login --hub"), login);
            if !login {
                assert!(!stderr.contains("sign in again"));
            }
            assert_eq!(fs::read(lab.credential_path(&hub.base)).unwrap(), before);
            assert_eq!(hub.finish().len(), if status == 401 { 4 } else { 2 });
        }
    }

    #[test]
    fn unavailable_pat_endpoint_does_not_invent_an_authentication_retry() {
        for recipe in [false, true] {
            let lab = Lab::new();
            let hub = Hub::new(|_| {
                let mut body = json!({"error":"synthetic repository unavailable", "kind":"repository_unavailable", "fix":[]});
                if recipe {
                    body["fix"] = json!([{"kind":"authenticate"}]);
                }
                let mut step = Step::error("POST", "/api/auth/login", body);
                step.status = 503;
                step.bearer = None;
                vec![step]
            });
            lab.seed_credentials(&hub.base, false);
            let before = fs::read(lab.credential_path(&hub.base)).unwrap();
            let input = lab.home.join("synthetic-pat.txt");
            fs::write(&input, OLD_ACCESS).unwrap();
            let mut command = lab.command(&hub.base, &["--json", "login", "--with-token"]);
            command.stdin(fs::File::open(input).unwrap());
            let output = run_bounded(command);
            assert_eq!(output.status.code(), Some(6), "{output:?}");
            assert!(output.stderr.is_empty());
            let value: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(value["fix"], json!([]));
            assert!(
                value
                    .to_string()
                    .contains("synthetic repository unavailable")
            );
            assert!(!value.to_string().contains("agit login"));
            assert!(!value.to_string().contains("mint a token"));
            assert!(!value.to_string().contains("needs an interactive terminal"));
            assert!(!value.to_string().contains(OLD_ACCESS));
            assert_eq!(fs::read(lab.credential_path(&hub.base)).unwrap(), before);
            assert_eq!(hub.finish().len(), 1);
        }
    }

    #[test]
    fn pairing_and_first_start_keep_remote_failure_categories_without_saving_a_connection() {
        for flags in [
            vec![],
            vec!["--quiet"],
            vec!["--json", "--json-version", "1"],
            vec!["--json", "--json-version", "2"],
        ] {
            for operation in [vec!["rc", "pair"], vec!["rc", "start", "--detach"]] {
                for (status, code) in [(401, 5), (503, 6), (500, 6), (200, 6)] {
                    let lab = Lab::new();
                    let hub = Hub::new(|_| {
                        let mut step = Step::error(
                            "POST",
                            "/api/rc/connections",
                            authentication_error("synthetic HTTP 401 wording"),
                        );
                        step.status = status;
                        vec![step]
                    });
                    assert!(
                        run_bounded(lab.command(&hub.base, &["config", "--list"]))
                            .status
                            .success()
                    );
                    lab.seed_credentials(&hub.base, false);
                    let identity = lab.store.join("rc/identity.json");
                    fs::create_dir_all(identity.parent().unwrap()).unwrap();
                    let identity_bytes = br#"{"machine_fingerprint":"synthetic-machine","display_name":"synthetic-machine","created_at":"2026-01-01T00:00:00Z"}"#;
                    fs::write(&identity, identity_bytes).unwrap();
                    let credentials = fs::read(lab.credential_path(&hub.base)).unwrap();
                    let mut args = flags.clone();
                    args.extend(operation.iter().copied());
                    let output = run_bounded(lab.command(&hub.base, &args));
                    assert_eq!(output.status.code(), Some(code), "{args:?}: {output:?}");
                    if flags.contains(&"--json") {
                        assert!(output.stderr.is_empty(), "{output:?}");
                        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
                        assert_eq!(value["command"], "rc");
                        assert_eq!(value["exit_code"], code);
                        assert_eq!(value["ok"], false);
                        assert_eq!(
                            value["schema_version"],
                            flags.last().unwrap().parse::<u32>().unwrap()
                        );
                        if flags.last() == Some(&"1") {
                            assert!(value.get("fix").is_none());
                        } else if code == 5 {
                            lab.assert_login(&value, &hub.base);
                        } else {
                            assert_eq!(value["fix"], json!([]));
                        }
                    } else {
                        assert!(!output.stderr.is_empty());
                        assert!(output.stdout.is_empty(), "{output:?}");
                    }
                    for token in [OLD_ACCESS, OLD_REFRESH] {
                        assert!(!String::from_utf8_lossy(&output.stdout).contains(token));
                        assert!(!String::from_utf8_lossy(&output.stderr).contains(token));
                    }
                    assert_eq!(fs::read(&identity).unwrap(), identity_bytes);
                    assert_eq!(
                        fs::read(lab.credential_path(&hub.base)).unwrap(),
                        credentials
                    );
                    assert!(
                        !lab.store
                            .join("rc/connections")
                            .join(format!(
                                "{}.json",
                                agit::infra::config::hub_host_key(&hub.base).unwrap()
                            ))
                            .exists()
                    );
                    assert!(!lab.store.join("rc/agitd.pid").exists());
                    let requests = hub.finish();
                    assert_eq!(requests.len(), 1);
                    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
                    assert_eq!(body["machine_fingerprint"], "synthetic-machine");
                    assert_eq!(body["display_name"], "synthetic-machine");
                }
            }
        }
    }

    #[test]
    fn rc_land_remote_refusals_preserve_lineage_and_native_evidence() {
        for flags in [
            vec![],
            vec!["--quiet"],
            vec!["--json", "--json-version", "1"],
            vec!["--json", "--json-version", "2"],
        ] {
            for case in [
                "unauthorized",
                "unavailable",
                "disconnect",
                "configuration",
                "lineage",
            ] {
                let existing_repos: &[bool] = if case == "unavailable" {
                    &[false, true]
                } else {
                    &[false]
                };
                for &existing_repo in existing_repos {
                    let lab = Lab::new();
                    let sends_request = !matches!(case, "configuration" | "lineage");
                    let code = match case {
                        "unauthorized" => 5,
                        "configuration" | "lineage" => 2,
                        _ => 6,
                    };
                    let hub = Hub::new_with_disconnect(
                        |_| {
                            if !sends_request {
                                return vec![];
                            }
                            let mut step = Step::error(
                                "GET",
                                "/api/agents/me/qa",
                                authentication_error("synthetic HTTP 401 wording"),
                            );
                            if case != "unauthorized" {
                                step.status = 503;
                            }
                            vec![step]
                        },
                        case == "disconnect",
                    );
                    assert!(
                        run_bounded(lab.command(&hub.base, &["config", "--list"]))
                            .status
                            .success()
                    );
                    lab.seed_credentials(&hub.base, false);
                    if existing_repo {
                        lab.initialize_repo(&hub.base);
                        assert!(
                            run_bounded(lab.command(&hub.base, &["config", "--list"]))
                                .status
                                .success()
                        );
                    }
                    if case == "configuration" {
                        fs::write(
                            lab.credential_path(&hub.base),
                            format!("{{\"token\":\"{OLD_ACCESS}\", invalid"),
                        )
                        .unwrap();
                    }
                    let native = lab.home.join(".codex/sessions/native-canary.jsonl");
                    fs::create_dir_all(native.parent().unwrap()).unwrap();
                    fs::write(&native, b"retained native transcript evidence\n").unwrap();
                    let before = lab.state();
                    let args = agit::commands::rc::land_argv(
                        if case == "lineage" { "../qa" } else { "me/qa" },
                        AGENT_ID,
                        "rc-http",
                        "codex",
                        "synthetic-rc-land",
                        lab.work.to_str().unwrap(),
                    );
                    let mut command = lab.command(&hub.base, &flags);
                    command.args(args);
                    let output = run_bounded(command);
                    assert_eq!(
                        output.status.code(),
                        Some(code),
                        "{case}/{flags:?}: {output:?}"
                    );
                    let text = if flags.contains(&"--json") {
                        assert!(output.stderr.is_empty(), "{output:?}");
                        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
                        assert_eq!(value["command"], "rc");
                        assert_eq!(value["exit_code"], code);
                        assert_eq!(value["ok"], false);
                        assert_eq!(
                            value["schema_version"],
                            flags.last().unwrap().parse::<u32>().unwrap()
                        );
                        if flags.last() == Some(&"1") {
                            assert!(value.get("fix").is_none());
                        } else if code == 5 {
                            lab.assert_login(&value, &hub.base);
                        } else {
                            assert_eq!(value["fix"], json!([]));
                        }
                        value.to_string()
                    } else {
                        assert!(output.stdout.is_empty(), "{output:?}");
                        assert!(!output.stderr.is_empty());
                        String::from_utf8_lossy(&output.stderr).into_owned()
                    };
                    if case == "configuration" {
                        assert!(text.contains("credentials do not belong"), "{text}");
                    } else if case == "lineage" {
                        assert!(text.contains("hub sent an unusable lineage"), "{text}");
                    }
                    for token in [OLD_ACCESS, OLD_REFRESH, NEW_ACCESS, NEW_REFRESH] {
                        assert!(!text.contains(token), "credential in output");
                    }
                    assert_eq!(lab.state(), before, "{case}/{flags:?}");
                    assert_eq!(lab.store.join("repos/me/qa").exists(), existing_repo);
                    assert!(
                        !lab.store
                            .join("store/codex/synthetic-rc-land.json")
                            .exists()
                    );
                    assert!(!lab.store.join("rc/agitd.pid").exists());
                    let requests = hub.finish();
                    assert_eq!(requests.len(), usize::from(sends_request));
                    if sends_request {
                        assert_eq!(requests[0].method, "GET");
                        assert_eq!(requests[0].target, "/api/agents/me/qa");
                        assert_eq!(
                            requests[0].authorization,
                            Some(format!("Bearer {OLD_ACCESS}"))
                        );
                        assert!(requests[0].body.is_empty());
                    }
                }
            }
        }
    }

    #[test]
    fn propagated_and_converted_terminal_errors_both_supply_actions() {
        for (args, method, target, code) in [
            (vec!["--json", "share", "list"], "GET", "/api/shares", 5),
            (
                vec!["--json", "clone", "me/qa", "--no-bind"],
                "GET",
                "/api/agents/me/qa",
                5,
            ),
            (
                vec!["--json", "repo", "create", "qa"],
                "POST",
                "/api/agents",
                5,
            ),
            (
                vec!["--json", "fetch", "me/qa"],
                "GET",
                "/api/agents/me/qa",
                5,
            ),
            (
                vec!["--json", "rc", "list"],
                "GET",
                "/api/rc/connections",
                5,
            ),
            (
                vec!["--json", "rc", "revoke", "synthetic"],
                "POST",
                "/api/rc/connections/synthetic/revoke",
                5,
            ),
            (
                vec!["--json", "rc", "pair"],
                "POST",
                "/api/rc/connections",
                5,
            ),
        ] {
            let lab = Lab::new();
            let hub = Hub::new(|_| {
                vec![Step::error(
                    method,
                    target,
                    authentication_error("synthetic terminal operation"),
                )]
            });
            lab.seed_credentials(&hub.base, false);
            let credential_before = fs::read(lab.credential_path(&hub.base)).unwrap();
            let value = lab.json(&hub.base, &args, code);
            lab.assert_login(&value, &hub.base);
            assert!(value.to_string().contains("synthetic terminal operation"));
            assert_eq!(
                fs::read(lab.credential_path(&hub.base)).unwrap(),
                credential_before
            );
            assert!(!lab.store.join("repos/me/qa").exists());
            assert_eq!(hub.finish().len(), 1);
        }
    }

    #[test]
    fn unsupported_recipes_cannot_author_commands_or_erase_diagnostics() {
        let wire_values = [
            None,
            Some(Value::Null),
            Some(json!([])),
            Some(json!({"kind":"authenticate"})),
            Some(json!("authenticate")),
            Some(json!([null, false, "authenticate", {}])),
            Some(json!([{"kind":"Authenticate"}])),
            Some(json!([{"kind":"unknown"}])),
            Some(json!([{"kind":"authenticate","argv":["unsafe-command"]}])),
            Some(json!([{"kind":"authenticate","hub":"https://foreign.invalid"}])),
        ];
        let lab = Lab::new();
        let hub = Hub::new(|_| {
            wire_values
                .iter()
                .map(|fix| {
                    let mut body =
                        json!({"error":"synthetic retained diagnosis","kind":"unauthorized"});
                    if let Some(fix) = fix {
                        body["fix"] = fix.clone();
                    } else {
                        body["fixes"] = json!([{"kind":"authenticate"}]);
                    }
                    Step::error("GET", "/api/search/sessions?q=needle&per=10", body)
                })
                .collect()
        });
        lab.seed_credentials(&hub.base, false);
        for _ in &wire_values {
            let value = lab.json(&hub.base, &["--json", "search", "needle"], 5);
            assert_eq!(value["fix"], json!([]), "{value}");
            assert!(value.to_string().contains("synthetic retained diagnosis"));
            assert!(!value.to_string().contains("log in with `agit login --hub"));
            assert!(!value.to_string().contains("unsafe-command"));
            assert!(!value.to_string().contains("foreign.invalid"));
        }
        assert_eq!(hub.finish().len(), wire_values.len());
    }

    #[test]
    fn supported_entries_deduplicate_and_explicit_v1_keeps_its_closed_envelope() {
        let lab = Lab::new();
        let hub = Hub::new(|_| {
            let mixed = json!({
                "error":"synthetic mixed recipes", "kind":"unauthorized",
                "fix":[null,{"kind":"unknown"},{"kind":"authenticate"},{"kind":"authenticate"}]
            });
            vec![
                Step::error("GET", "/api/search/sessions?q=needle&per=10", mixed.clone()),
                Step::error("GET", "/api/search/sessions?q=needle&per=10", mixed),
            ]
        });
        lab.seed_credentials(&hub.base, false);
        let value = lab.json(&hub.base, &["--json", "search", "needle"], 5);
        lab.assert_login(&value, &hub.base);
        let legacy = lab.json(
            &hub.base,
            &["--json", "--json-version", "1", "search", "needle"],
            5,
        );
        assert_eq!(legacy["schema_version"], 1);
        assert!(legacy.get("fix").is_none());
        let mut keys: Vec<_> = legacy
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "command",
                "diagnostics",
                "exit_code",
                "ok",
                "result",
                "schema",
                "schema_version"
            ]
        );
        assert_eq!(hub.finish().len(), 2);
    }

    #[test]
    fn swallowed_identity_failure_does_not_pollute_a_local_secret_policy_failure() {
        let lab = Lab::new();
        let hub = Hub::new(|_| {
            vec![
                Step::error(
                    "GET",
                    "/api/agents/me/qa",
                    authentication_error("synthetic scan probe"),
                ),
                Step::error(
                    "GET",
                    "/api/agents/me/qa",
                    authentication_error("synthetic fetch terminal"),
                ),
            ]
        });
        lab.seed_credentials(&hub.base, false);
        let repo = lab.initialize_repo(&hub.base);
        let secret_file = repo.join("memory/http-policy-fixture.txt");
        fs::write(
            &secret_file,
            format!("synthetic scanner fixture: {SECRET_PATTERN}\n"),
        )
        .unwrap();
        let refs_before = lab.git(&repo, &["show-ref"]);
        let config_before = fs::read(repo.join(".git/config")).unwrap();
        let status_before = lab.git(&repo, &["status", "--porcelain=v1"]);
        let value = lab.json(&hub.base, &["--json", "scan", "--secrets", "me/qa@main"], 7);
        assert_eq!(value["fix"], json!([]), "{value}");
        assert!(value.to_string().contains("aws-access-token"), "{value}");
        assert!(value.to_string().contains("secret-like patterns found"));
        assert!(!value.to_string().contains(SECRET_PATTERN));
        assert!(!value.to_string().contains("synthetic scan probe"));
        let terminal = lab.json(&hub.base, &["--json", "fetch", "me/qa"], 5);
        lab.assert_login(&terminal, &hub.base);
        assert_eq!(lab.git(&repo, &["show-ref"]), refs_before);
        assert_eq!(fs::read(repo.join(".git/config")).unwrap(), config_before);
        assert_eq!(lab.git(&repo, &["status", "--porcelain=v1"]), status_before);
        assert_eq!(hub.finish().len(), 2);
    }

    /// A remote left behind by interrupted publication remains usable without local pin state.
    #[test]
    fn ordinary_push_retries_existing_remotes_without_adopting_cached_identity() {
        for cached in [None, Some("stale"), Some("malformed")] {
            let lab = Lab::new();
            let bare = lab.home.join("remote.git");
            fs::create_dir_all(&bare).unwrap();
            lab.git(&bare, &["init", "--bare", "--quiet"]);
            let hub = Hub::new(|_| {
                (0..2)
                    .map(|_| Step {
                        method: "GET",
                        target: "/api/agents/me/qa".into(),
                        bearer: Some(OLD_ACCESS),
                        status: 200,
                        response: json!({"agent_id": AGENT_ID, "owner": "me", "name": "qa",
                    "clone_url": bare.to_str().unwrap(), "visibility": "private"}),
                        after_response: None,
                    })
                    .collect()
            });
            lab.seed_credentials(&hub.base, false);
            let repo = lab.initialize_repo(&hub.base);
            lab.git(
                &repo,
                &["config", "--local", "--unset", "agit.remoteIdentity"],
            );
            lab.git(&repo, &["remote", "remove", "origin"]);
            if let Some(cached) = cached {
                let pin = if cached == "malformed" {
                    "invalid-json".into()
                } else {
                    json!({"hub":hub.base,"agent_id":"22222222-2222-4222-8222-222222222222"})
                        .to_string()
                };
                lab.git(&repo, &["config", "--local", "agit.remoteIdentity", &pin]);
                lab.git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
            }
            let local_repo = agit::domain::repo::Repo::at(&repo);
            let pin_before =
                local_repo.git_opt(&["config", "--local", "--get", "agit.remoteIdentity"]);
            let head = lab.git(&repo, &["rev-parse", "main"]);
            for _ in 0..2 {
                lab.json(&hub.base, &["--json", "push", "me/qa", "-b", "main"], 0);
                assert_eq!(lab.git(&bare, &["rev-parse", "refs/heads/main"]), head);
                assert_eq!(
                    local_repo.git_opt(&["config", "--local", "--get", "agit.remoteIdentity"]),
                    pin_before
                );
            }
            assert_eq!(hub.finish().len(), 2);
        }
    }

    #[test]
    fn supervised_push_does_not_adopt_a_recreated_remote() {
        for pinned in [false, true] {
            let lab = Lab::new();
            let hub = Hub::new(|_| {
                if pinned {
                    vec![Step {
                        method: "GET",
                        target: "/api/agents/me/qa".into(),
                        bearer: Some(OLD_ACCESS),
                        status: 200,
                        response: json!({"agent_id":"22222222-2222-4222-8222-222222222222",
                    "owner":"me", "name":"qa", "clone_url":"unused", "visibility":"private"}),
                        after_response: None,
                    }]
                } else {
                    vec![]
                }
            });
            lab.seed_credentials(&hub.base, false);
            let repo = lab.initialize_repo(&hub.base);
            lab.git(&repo, &["remote", "remove", "origin"]);
            if !pinned {
                lab.git(
                    &repo,
                    &["config", "--local", "--unset", "agit.remoteIdentity"],
                );
            }
            let refs = lab.git(&repo, &["show-ref"]);
            let config = fs::read(repo.join(".git/config")).unwrap();
            let mut command = lab.command(&hub.base, &["--json", "push", "me/qa", "-b", "main"]);
            command.env("AGIT_EXPECTED_AGENT_ID", AGENT_ID);
            let output = run_bounded(command);
            assert!(!output.status.success(), "{output:?}");
            assert_eq!(lab.git(&repo, &["show-ref"]), refs);
            assert_eq!(fs::read(repo.join(".git/config")).unwrap(), config);
            assert_eq!(hub.finish().len(), usize::from(pinned));
        }
    }

    #[test]
    fn pinned_push_keeps_the_terminal_error_source_through_its_context() {
        let lab = Lab::new();
        let hub = Hub::new(|_| {
            vec![
                Step::error(
                    "GET",
                    "/api/agents/me/qa",
                    authentication_error("synthetic ignored push probe"),
                ),
                Step::error(
                    "GET",
                    "/api/agents/me/qa",
                    authentication_error("synthetic terminal pinned push"),
                ),
            ]
        });
        lab.seed_credentials(&hub.base, false);
        let repo = lab.initialize_repo(&hub.base);
        let refs_before = lab.git(&repo, &["show-ref"]);
        let config_before = fs::read(repo.join(".git/config")).unwrap();
        let value = lab.json(&hub.base, &["--json", "push", "me/qa", "-b", "main"], 5);
        lab.assert_login(&value, &hub.base);
        assert!(value.to_string().contains("synthetic terminal pinned push"));
        assert!(!value.to_string().contains("synthetic ignored push probe"));
        assert_eq!(lab.git(&repo, &["show-ref"]), refs_before);
        assert_eq!(fs::read(repo.join(".git/config")).unwrap(), config_before);
        assert_eq!(hub.finish().len(), 2);
    }

    #[test]
    fn rejected_refresh_recovered_by_a_sibling_does_not_pollute_a_later_policy_failure() {
        let lab = Lab::new();
        let prepare = "/api/agents/me/qa/visibility/public/prepare";
        let hub = Hub::new(|base| {
            let path = lab.credential_path(base);
            let mut fresh = credential(base, true);
            fresh.access_token = NEW_ACCESS.into();
            fresh.refresh_token = NEW_REFRESH.into();
            let mut refresh = Step::error(
                "POST",
                "/api/auth/refresh",
                authentication_error("synthetic rejected refresh"),
            );
            refresh.bearer = None;
            refresh.after_response = Some(Box::new(move || save_at(&path, &fresh).unwrap()));
            let recovered = Step {
                method: "POST",
                target: prepare.into(),
                bearer: Some(NEW_ACCESS),
                status: 200,
                response: json!({
                    "intent_id":"synthetic-intent", "expires_at":"2099-01-01T00:00:00Z",
                    "confirmation_phrase":"me/qa",
                    "snapshot":{"refs_digest":"synthetic-refs","ruleset_digest":"synthetic-rules"},
                    "findings":{"suspected_secrets":0,"truncated":false,"rules":[],"complete":false},
                    "warning":"synthetic scan warning"
                }),
                after_response: None,
            };
            vec![
                Step::error(
                    "POST",
                    prepare,
                    authentication_error("synthetic initial rejection"),
                ),
                refresh,
                recovered,
            ]
        });
        lab.seed_credentials(&hub.base, true);
        let repo = lab.initialize_repo(&hub.base);
        let refs_before = lab.git(&repo, &["show-ref"]);
        let value = lab.json(
            &hub.base,
            &["--json", "repo", "visibility", "me/qa", "public"],
            7,
        );
        assert_eq!(value["fix"], json!([]), "{value}");
        assert!(
            value
                .to_string()
                .contains("server did not complete the publication scan"),
            "{value}"
        );
        assert!(!value.to_string().contains("synthetic rejected refresh"));
        assert!(!value.to_string().contains("synthetic initial rejection"));
        let saved: HubCredential =
            serde_json::from_slice(&fs::read(lab.credential_path(&hub.base)).unwrap()).unwrap();
        assert_eq!(saved.access_token, NEW_ACCESS);
        assert_eq!(saved.refresh_token, NEW_REFRESH);
        assert_eq!(saved.username, "me");
        assert_eq!(saved.hub.as_deref(), Some(hub.base.as_str()));
        assert_eq!(lab.git(&repo, &["show-ref"]), refs_before);
        let requests = hub.finish();
        assert_eq!(requests.len(), 3);
        for index in [0, 2] {
            assert_eq!(
                serde_json::from_slice::<Value>(&requests[index].body).unwrap(),
                json!({"expected_agent_id":AGENT_ID})
            );
        }
        assert_eq!(
            serde_json::from_slice::<Value>(&requests[1].body).unwrap(),
            json!({"refresh_token":OLD_REFRESH})
        );
    }

    #[test]
    fn best_effort_remote_failures_preserve_their_local_results() {
        for args in [
            vec!["--json", "repo", "info", "me/qa"],
            vec!["--json", "fetch", "--all"],
        ] {
            let lab = Lab::new();
            let hub = Hub::new(|_| {
                vec![Step::error(
                    "GET",
                    "/api/agents/me/qa",
                    authentication_error("synthetic best effort"),
                )]
            });
            lab.seed_credentials(&hub.base, false);
            let repo = lab.initialize_repo(&hub.base);
            let before = lab.git(&repo, &["show-ref"]);
            let value = lab.json(&hub.base, &args, 0);
            assert_eq!(value["fix"], json!([]));
            assert!(value.to_string().contains("synthetic best effort"));
            assert_eq!(lab.git(&repo, &["show-ref"]), before);
            assert_eq!(hub.finish().len(), 1);
        }
    }

    #[test]
    fn failed_remote_revocation_does_not_suggest_logging_back_in() {
        for args in [vec!["--json", "logout"], vec!["--json", "logout", "--all"]] {
            let lab = Lab::new();
            let hub = Hub::new(|_| {
                vec![Step::error(
                    "POST",
                    "/api/auth/logout",
                    authentication_error("synthetic revoke refusal"),
                )]
            });
            lab.seed_credentials(&hub.base, false);
            let value = lab.json(&hub.base, &args, 0);
            assert_eq!(value["fix"], json!([]));
            assert!(!lab.credential_path(&hub.base).exists());
            assert_eq!(hub.finish().len(), 1);
        }
    }
}
