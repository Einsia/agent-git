//! Bounded synthetic HTTP exchanges shared by privacy protocol tests.

use crate::hub::{Client, identity::RemoteIdentity};
use serde_json::Value;
use std::io::{Read, Write};

const AGENT: &str = "00000000-0000-0000-0000-000000000001";

pub(super) fn server(
    responses: Vec<(u16, Value)>,
) -> (Client, RemoteIdentity, std::thread::JoinHandle<Vec<String>>) {
    server_with_headers(|_| responses, "")
}

pub(super) fn server_with_headers(
    responses: impl FnOnce(&str) -> Vec<(u16, Value)>,
    headers: &str,
) -> (Client, RemoteIdentity, std::thread::JoinHandle<Vec<String>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let hub = format!("http://{}", listener.local_addr().unwrap());
    let responses = responses(&hub);
    let headers = headers.to_owned();
    let worker = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (status, response) in responses {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "expected privacy request"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                assert!(count > 0);
                request.extend_from_slice(&buffer[..count]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let size = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .map(|v| v.trim().parse::<usize>().unwrap())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + size {
                        break;
                    }
                }
            }
            let body = response.to_string();
            write!(stream, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len()).unwrap();
            requests.push(String::from_utf8(request).unwrap());
        }
        requests
    });
    (
        Client::for_hub(&hub),
        RemoteIdentity::new(&hub, AGENT).unwrap(),
        worker,
    )
}
