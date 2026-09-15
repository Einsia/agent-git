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
