//! Race resolved addresses before the transport sends authentication or data.

use anyhow::Context;
use futures_util::{StreamExt, stream::FuturesUnordered};
use http::Uri;
use std::{collections::VecDeque, future::Future, io, net::SocketAddr, time::Duration};
use tokio::{net::TcpStream, time::Instant};

const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

pub(super) async fn connect(uri: &Uri) -> crate::Result<TcpStream> {
    let host = uri.host().context("TCP destination is missing a host")?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let port = uri
        .port_u16()
        .unwrap_or(if uri.scheme_str() == Some("https") {
            443
        } else {
            80
        });
    let addresses = tokio::net::lookup_host((host, port)).await?;
    let stream = race(interleave(addresses), TcpStream::connect).await?;
    stream.set_nodelay(true)?;
    Ok(stream)
}

fn interleave(addresses: impl Iterator<Item = SocketAddr>) -> impl Iterator<Item = SocketAddr> {
    let mut addresses = addresses.peekable();
    let prefer_v6 = addresses.peek().is_some_and(SocketAddr::is_ipv6);
    let (mut preferred, mut alternate): (VecDeque<_>, VecDeque<_>) =
        addresses.partition(|address| address.is_ipv6() == prefer_v6);
    std::iter::from_fn(move || {
        let address = preferred.pop_front().or_else(|| alternate.pop_front());
        std::mem::swap(&mut preferred, &mut alternate);
        address
    })
}

async fn race<T, F, Fut>(
    addresses: impl Iterator<Item = SocketAddr>,
    mut connect: F,
) -> io::Result<T>
where
    F: FnMut(SocketAddr) -> Fut,
    Fut: Future<Output = io::Result<T>>,
{
    let mut addresses = addresses.peekable();
    let first = addresses
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "DNS returned no addresses"))?;
    let mut pending = FuturesUnordered::new();
    pending.push(connect(first));
    let delay = tokio::time::sleep(ATTEMPT_DELAY);
    tokio::pin!(delay);
    loop {
        tokio::select! {
            result = pending.next() => match result.expect("a connection attempt remains pending") {
                Ok(stream) => return Ok(stream),
                Err(error) => {
                    if let Some(address) = addresses.next() {
                        pending.push(connect(address));
                        delay.as_mut().reset(Instant::now() + ATTEMPT_DELAY);
                    } else if pending.is_empty() {
                        return Err(error);
                    }
                }
            },
            () = &mut delay, if addresses.peek().is_some() => {
                pending.push(connect(addresses.next().unwrap()));
                delay.as_mut().reset(Instant::now() + ATTEMPT_DELAY);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[tokio::test(start_paused = true)]
    async fn stalled_address_cannot_block_a_reachable_address_and_losers_are_dropped() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let addresses = (1..=3).map(|port| SocketAddr::from(([127, 0, 0, 1], port)));
        let started = Instant::now();
        let result = race(addresses, |address| {
            let dropped = dropped.clone();
            async move {
                match address.port() {
                    1 => {
                        let _guard = Dropped(dropped);
                        std::future::pending().await
                    }
                    2 => Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
                    _ => Ok(address),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(result.port(), 3);
        assert_eq!(started.elapsed(), ATTEMPT_DELAY);
        assert!(dropped.load(Ordering::SeqCst));

        let addresses = ["[::1]:1", "[::1]:2", "127.0.0.1:3", "127.0.0.1:4"]
            .map(|address| address.parse::<SocketAddr>().unwrap());
        assert_eq!(
            interleave(addresses.into_iter())
                .map(|address| address.port())
                .collect::<Vec<_>>(),
            [1, 3, 2, 4]
        );
    }
}
