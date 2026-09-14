//! Hub HTTP requests must use SOCKS proxies selected from the environment.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn hub_health_child() {
    if std::env::var_os("AGIT_TEST_SOCKS_CHILD").is_none() {
        return;
    }
    if std::env::var("AGIT_TEST_SOCKS_CHILD").as_deref() == Ok("timeout") {
        // The budget must allow the resolver worker to run before the handshake stalls.
        let client = agit::hub::Client::from_env_with_timeout(Duration::from_secs(2));
        let start = Instant::now();
        let error = client.health().expect_err("A stalled proxy must time out");
        eprintln!("Hub request failed after {:?}: {error:#}", start.elapsed());
        assert!(start.elapsed() < Duration::from_secs(5), "{error:#}");
        assert!(
            format!("{error:#}").to_lowercase().contains("timeout"),
            "{error:#}"
        );
    } else {
        agit::hub::Client::from_env_with_timeout(Duration::from_secs(5))
            .health()
            .expect("Hub health must reach the SOCKS tunnel");
    }
}

#[test]
fn socks5h_environment_proxy_carries_hub_health() {
    run_proxy("socks5h", "http://hub.invalid", false, |mut stream| {
        greeting(&mut stream);
        stream.write_all(&[5, 0]).unwrap();
        connect_request(&mut stream, b"\x05\x01\x00\x03\x0bhub.invalid\x00\x50");
        stream
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
            .unwrap();
        health_response(&mut stream);
    });
}

fn accept(listener: TcpListener) -> std::net::TcpStream {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(7)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(7)))
                    .unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "Proxy was never contacted");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("proxy accept failed: {error}"),
        }
    }
}

fn run_proxy(
    scheme: &str,
    target: &str,
    timeout: bool,
    server: impl FnOnce(std::net::TcpStream) + Send + 'static,
) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || server(accept(listener)));
    let prefix = if scheme.contains("://") {
        scheme.to_string()
    } else {
        format!("{scheme}://")
    };
    let output = run_request(&format!("{prefix}{address}"), target, "", timeout);
    let server_result = server.join();
    assert!(
        output.status.success() && server_result.is_ok(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_request(proxy: &str, target: &str, no_proxy: &str, timeout: bool) -> std::process::Output {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    for key in [
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        command.env_remove(key);
    }
    let mut child = command
        .args(["--exact", "hub_health_child", "--nocapture"])
        .env(
            "AGIT_TEST_SOCKS_CHILD",
            if timeout { "timeout" } else { "success" },
        )
        .env("AGIT_HOME", home.path())
        .env("AGIT_HUB_URL", target)
        .env("ALL_PROXY", proxy)
        .env("NO_PROXY", no_proxy)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

fn greeting(stream: &mut std::net::TcpStream) {
    let mut header = [0; 2];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(header[0], 5);
    let mut methods = vec![0; usize::from(header[1])];
    stream.read_exact(&mut methods).unwrap();
    assert!(methods.contains(&0));
}

fn connect_request(stream: &mut std::net::TcpStream, expected: &[u8]) {
    let mut request = vec![0; expected.len()];
    stream.read_exact(&mut request).unwrap();
    assert_eq!(request, expected);
}

fn health_response(stream: &mut std::net::TcpStream) {
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        assert!(request.len() < 8192);
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        request.push(byte[0]);
    }
    assert!(request.starts_with(b"GET /api/health HTTP/1.1\r\n"));
    let body = r#"{"status":"ok","version":"test"}"#;
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

#[test]
fn stalled_socks_greeting_obeys_request_deadline_and_closes_socket() {
    run_proxy("socks5h", "http://hub.invalid", true, |mut stream| {
        greeting(&mut stream);
        let mut byte = [0];
        assert_eq!(
            stream.read(&mut byte).unwrap(),
            0,
            "Timed-out handshake must close its socket"
        );
    });
}

#[test]
fn stalled_socks_connect_obeys_request_deadline() {
    run_proxy("socks5h", "http://hub.invalid", true, |mut stream| {
        greeting(&mut stream);
        stream.write_all(&[5, 0]).unwrap();
        connect_request(&mut stream, b"\x05\x01\x00\x03\x0bhub.invalid\x00\x50");
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
    });
}

#[test]
fn partial_socks_responses_do_not_restart_the_deadline() {
    run_proxy("socks5h", "http://hub.invalid", true, |mut stream| {
        greeting(&mut stream);
        stream.write_all(&[5, 0]).unwrap();
        connect_request(&mut stream, b"\x05\x01\x00\x03\x0bhub.invalid\x00\x50");
        // Each gap fits inside the request budget; the complete reply exceeds it.
        for byte in [5, 0, 0, 1, 127, 0, 0, 1, 0, 80] {
            std::thread::sleep(Duration::from_millis(1200));
            if stream.write_all(&[byte]).is_err() {
                break;
            }
        }
    });
}

#[test]
fn socks5_authentication_carries_hub_health() {
    run_proxy(
        "socks5h://user:pass@",
        "http://hub.invalid",
        false,
        |mut stream| {
            greeting(&mut stream);
            stream.write_all(&[5, 2]).unwrap();
            connect_request(&mut stream, b"\x01\x04user\x04pass");
            stream.write_all(&[1, 0]).unwrap();
            connect_request(&mut stream, b"\x05\x01\x00\x03\x0bhub.invalid\x00\x50");
            stream
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
                .unwrap();
            health_response(&mut stream);
        },
    );
}

#[test]
fn socks5_local_ipv4_resolution_carries_hub_health() {
    run_proxy("socks5", "http://127.0.0.1", false, |mut stream| {
        greeting(&mut stream);
        stream.write_all(&[5, 0]).unwrap();
        connect_request(&mut stream, &[5, 1, 0, 1, 127, 0, 0, 1, 0, 80]);
        stream
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80])
            .unwrap();
        health_response(&mut stream);
    });
}

#[test]
fn socks4a_remote_resolution_carries_hub_health() {
    run_proxy("socks4a", "http://hub.invalid", false, |mut stream| {
        connect_request(
            &mut stream,
            b"\x04\x01\x00\x50\x00\x00\x00\x01\x00hub.invalid\x00",
        );
        stream.write_all(&[0, 90, 0, 80, 127, 0, 0, 1]).unwrap();
        health_response(&mut stream);
    });
}

#[test]
fn socks4_local_resolution_carries_hub_health() {
    run_proxy("socks4", "http://127.0.0.1", false, |mut stream| {
        connect_request(&mut stream, &[4, 1, 0, 80, 127, 0, 0, 1, 0]);
        stream.write_all(&[0, 90, 0, 80, 127, 0, 0, 1]).unwrap();
        health_response(&mut stream);
    });
}

#[test]
fn stalled_socks_authentication_obeys_request_deadline() {
    run_proxy(
        "socks5h://user:pass@",
        "http://hub.invalid",
        true,
        |mut stream| {
            greeting(&mut stream);
            stream.write_all(&[5, 2]).unwrap();
            connect_request(&mut stream, b"\x01\x04user\x04pass");
            let mut byte = [0];
            assert_eq!(stream.read(&mut byte).unwrap(), 0);
        },
    );
}

#[test]
fn stalled_socks4_reply_obeys_request_deadline() {
    run_proxy("socks4a", "http://hub.invalid", true, |mut stream| {
        connect_request(
            &mut stream,
            b"\x04\x01\x00\x50\x00\x00\x00\x01\x00hub.invalid\x00",
        );
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
    });
}

#[test]
fn socks5_ipv6_target_and_reply_carry_hub_health() {
    run_proxy("socks5h", "http://[::1]", false, |mut stream| {
        greeting(&mut stream);
        stream.write_all(&[5, 0]).unwrap();
        let mut request = vec![5, 1, 0, 4];
        request.extend(std::net::Ipv6Addr::LOCALHOST.octets());
        request.extend([0, 80]);
        connect_request(&mut stream, &request);
        request[1] = 0;
        stream.write_all(&request).unwrap();
        health_response(&mut stream);
    });
}

#[test]
fn socks5_domain_reply_is_consumed_before_http() {
    run_proxy("socks5h", "http://hub.invalid", false, |mut stream| {
        greeting(&mut stream);
        stream.write_all(&[5, 0]).unwrap();
        connect_request(&mut stream, b"\x05\x01\x00\x03\x0bhub.invalid\x00\x50");
        stream
            .write_all(b"\x05\x00\x00\x03\x0bproxy.local\x00\x50")
            .unwrap();
        health_response(&mut stream);
    });
}

#[test]
fn no_proxy_bypasses_socks_without_resolving_the_proxy() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let target = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || health_response(&mut accept(listener)));
    let output = run_request("socks5h://proxy.invalid:1080", &target, "127.0.0.1", false);
    let server_result = server.join();
    assert!(
        output.status.success() && server_result.is_ok(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn http_connect_proxy_still_carries_hub_health() {
    run_proxy("http", "http://hub.invalid", false, |mut stream| {
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            assert!(request.len() < 8192);
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        assert!(request.starts_with(b"CONNECT hub.invalid:80 HTTP/1.1\r\n"));
        stream
            .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
            .unwrap();
        health_response(&mut stream);
    });
}
