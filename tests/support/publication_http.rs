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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};

    #[test]
    fn an_idle_cloned_listener_returns_without_needing_a_client() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let cloned = listener.try_clone().unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            let outcome = accept(&cloned).map(|_| ());
            sender.send(outcome).unwrap();
            while !stopping.load(Ordering::Acquire) {
                assert!(
                    matches!(accept(&cloned), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
                );
                thread::sleep(Duration::from_millis(5));
            }
        });
        let outcome = receiver.recv_timeout(Duration::from_secs(3));
        stop.store(true, Ordering::Release);
        if matches!(outcome, Err(mpsc::RecvTimeoutError::Timeout)) {
            // A blocked accept is released before a failed assertion leaves the fixture.
            let _wake = TcpStream::connect_timeout(&address, Duration::from_secs(3));
        }
        join(worker).expect("the listener probe worker must stop");
        assert!(
            matches!(outcome, Ok(Err(error)) if error.kind() == io::ErrorKind::WouldBlock),
            "an idle fixture listener must return WouldBlock"
        );
    }
}
