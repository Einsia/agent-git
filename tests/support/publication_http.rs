//! Loopback fixture shutdown must not wait indefinitely for a new connection.

use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const JOIN_LIMIT: Duration = Duration::from_secs(10);

/// The handle performing accept must be nonblocking so stop and deadline checks stay reachable.
pub fn accept(listener: &TcpListener) -> io::Result<(TcpStream, SocketAddr)> {
    listener.set_nonblocking(true)?;
    listener.accept()
}

/// An unfinished worker fails cleanup instead of trapping the test in an unconditional join.
pub fn join<T>(worker: JoinHandle<T>) -> Result<T, &'static str> {
    let deadline = Instant::now() + JOIN_LIMIT;
    while !worker.is_finished() {
        if Instant::now() >= deadline {
            return Err("loopback worker did not stop before its cleanup deadline");
        }
        thread::sleep(Duration::from_millis(5));
    }
    worker.join().map_err(|_| "loopback worker panicked")
}

/// A local publication fixture may discover that its named destination has no remote policy yet.
#[allow(dead_code)]
pub fn with_missing_agent_probe<T>(listener: &TcpListener, work: impl FnOnce() -> T) -> T {
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Probe {
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }
    impl Drop for Probe {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            let result = join(self.worker.take().unwrap());
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }

    let listener = listener.try_clone().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let _probe = Probe {
        stop,
        worker: Some(thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                let (mut stream, _) = match accept(&listener) {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("policy probe accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    header.push(byte[0]);
                    assert!(header.len() <= 65536);
                }
                let header = String::from_utf8(header).unwrap();
                let request = header.lines().next().unwrap();
                assert!(request.starts_with("GET /api/agents/"), "{request}");
                assert_eq!(request.split('/').count(), 6, "{request}");
                stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
            }
        })),
    };
    work()
}
