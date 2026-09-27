//! SOCKS negotiation shares the request deadline with proxy resolution and TCP connection.
//! A host with several addresses is dialed by racing them, so one unreachable address costs
//! at most an attempt delay instead of its share of the connection deadline.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use ureq::unversioned::resolver::{DefaultResolver, Resolver};
use ureq::unversioned::transport::{
    ConnectProxyConnector, ConnectionDetails, Connector, Either, NextTimeout, RustlsConnector,
    TcpConnector, Transport, TransportAdapter, time,
};
use ureq::{Error, Proxy, ProxyProtocol};

pub(crate) fn agent(config: ureq::config::Config) -> ureq::Agent {
    let connector = SocksConnector
        .chain(ConnectProxyConnector::default())
        .chain(RacingTcpConnector)
        .chain(RustlsConnector::default());
    ureq::Agent::with_parts(config, connector, DefaultResolver::default())
}

/// How long a pending attempt runs alone before the next address joins the race.
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// Direct TCP connection that races the resolved addresses.
///
/// ureq's own connector tries addresses strictly in turn and gives the first one the largest
/// share of the connection deadline, so an address that silently drops packets delays every
/// connection by that whole share before a reachable address is tried. Here the next address
/// starts whenever the running attempts have neither connected nor failed within
/// [`ATTEMPT_DELAY`], a failure hands over at once, and the first connection wins.
#[derive(Debug)]
struct RacingTcpConnector;

impl<In: Transport> Connector<In> for RacingTcpConnector {
    type Out = Either<In, Box<dyn Transport>>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<In>,
    ) -> Result<Option<Self::Out>, Error> {
        // A proxy connector already produced the connection this one would open.
        if chained.is_some() {
            return Ok(chained.map(Either::A));
        }
        connect_tcp(details).map(|transport| Some(Either::B(transport)))
    }
}

fn tcp(details: &ConnectionDetails) -> Result<Box<dyn Transport>, Error> {
    TcpConnector::default()
        .connect(details, None::<()>)?
        .map(|transport| transport.boxed())
        .ok_or_else(|| Error::Io(invalid("TCP connection is missing")))
}

fn connect_tcp(details: &ConnectionDetails) -> Result<Box<dyn Transport>, Error> {
    if details.addrs.len() < 2 {
        return tcp(details);
    }
    // Each attempt owns what it borrows: a losing attempt keeps running until it connects or
    // the deadline passes, after this call has returned the winner.
    let uri = details.uri.clone();
    let config = details.config.clone();
    let request_level = details.request_level;
    let reason = details.timeout.reason;
    let current_time = details.current_time.clone();
    let run_connector = details.run_connector.clone();
    let attempt = move |addr: SocketAddr, left: Option<Duration>| {
        let resolver = DefaultResolver::default();
        let mut addrs = resolver.empty();
        addrs.push(addr);
        tcp(&ConnectionDetails {
            uri: &uri,
            addrs,
            config: &config,
            request_level,
            resolver: &resolver,
            now: current_time(),
            timeout: NextTimeout {
                after: left.map_or(time::Duration::NotHappening, Into::into),
                reason,
            },
            current_time: current_time.clone(),
            run_connector: run_connector.clone(),
        })
    };
    let budget = (!details.timeout.after.is_not_happening()).then(|| *details.timeout.after);
    race(
        interleave(&details.addrs),
        budget,
        ATTEMPT_DELAY,
        Arc::new(attempt),
        move || Error::Timeout(reason),
    )
}

/// Alternate address families, starting with the resolver's first choice, so a family that is
/// broken on this network cannot queue every address of the working one behind it.
fn interleave(addrs: &[SocketAddr]) -> Vec<SocketAddr> {
    let first_v6 = addrs.first().is_some_and(SocketAddr::is_ipv6);
    let (mut first, mut second): (VecDeque<_>, VecDeque<_>) = addrs
        .iter()
        .copied()
        .partition(|addr| addr.is_ipv6() == first_v6);
    let mut ordered = Vec::with_capacity(addrs.len());
    while let Some(addr) = first.pop_front() {
        ordered.push(addr);
        ordered.extend(second.pop_front());
    }
    ordered.extend(second);
    ordered
}

type Attempt<T> = dyn Fn(SocketAddr, Option<Duration>) -> Result<T, Error> + Send + Sync;

/// Start `addrs` in order, each after `delay` or after the previous failure, and return the
/// first success. `attempt` receives what is left of `budget`; `None` means no deadline.
/// When every attempt fails, the last failure is returned.
fn race<T: Send + 'static>(
    addrs: Vec<SocketAddr>,
    budget: Option<Duration>,
    delay: Duration,
    attempt: Arc<Attempt<T>>,
    timeout: impl FnOnce() -> Error,
) -> Result<T, Error> {
    let deadline = budget.map(|budget| Instant::now() + budget);
    let (sender, results) = mpsc::channel();
    let mut queue = addrs.into_iter().peekable();
    let mut next_start = Instant::now();
    let mut running = 0usize;
    let mut failure = None;
    loop {
        let now = Instant::now();
        if deadline.is_some_and(|deadline| now >= deadline) {
            return Err(timeout());
        }
        if now >= next_start
            && let Some(addr) = queue.next()
        {
            let sender = sender.clone();
            let attempt = Arc::clone(&attempt);
            let left = deadline.map(|deadline| deadline.saturating_duration_since(now));
            std::thread::spawn(move || {
                // Nobody receives a result once another attempt has won; dropping it closes
                // the late connection.
                let _ = sender.send(attempt(addr, left));
            });
            running += 1;
            next_start = now + delay;
        }
        if running == 0 {
            return Err(failure.unwrap_or_else(timeout));
        }
        let wake = [queue.peek().map(|_| next_start), deadline]
            .into_iter()
            .flatten()
            .min();
        let received = match wake {
            Some(wake) => results.recv_timeout(wake.saturating_duration_since(now)),
            None => results
                .recv()
                .map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(Ok(connected)) => return Ok(connected),
            Ok(Err(error)) => {
                running -= 1;
                failure = Some(error);
                next_start = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // `sender` lives in this frame, so the channel cannot close while it waits.
            Err(mpsc::RecvTimeoutError::Disconnected) => unreachable!(),
        }
    }
}

#[derive(Debug)]
struct SocksConnector;

impl Connector for SocksConnector {
    type Out = Box<dyn Transport>;

    fn connect(
        &self,
        details: &ConnectionDetails,
        _: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        let Some(proxy) = details.config.proxy().filter(|proxy| {
            matches!(
                proxy.protocol(),
                ProxyProtocol::Socks4
                    | ProxyProtocol::Socks4A
                    | ProxyProtocol::Socks5
                    | ProxyProtocol::Socks5h
            ) && !proxy.is_no_proxy(details.uri)
        }) else {
            return Ok(None);
        };
        let deadline = Deadline {
            start: Instant::now(),
            timeout: details.timeout,
        };
        let addrs = details
            .resolver
            .resolve(proxy.uri(), details.config, deadline.remaining()?)?;
        let proxy_details = ConnectionDetails {
            uri: proxy.uri(),
            addrs,
            config: details.config,
            request_level: details.request_level,
            resolver: details.resolver,
            now: (details.current_time)(),
            timeout: deadline.remaining()?,
            current_time: details.current_time.clone(),
            run_connector: details.run_connector.clone(),
        };
        let transport = connect_tcp(&proxy_details)?;
        let mut stream = Handshake {
            io: TransportAdapter::new(transport),
            deadline,
        };
        let host = details
            .uri
            .host()
            .ok_or_else(|| invalid("SOCKS target host is missing"))?
            .trim_matches(['[', ']']);
        let port = details
            .uri
            .port_u16()
            .unwrap_or(if details.uri.scheme_str() == Some("https") {
                443
            } else {
                80
            });
        let target = if proxy.resolve_target() {
            let ip = details
                .addrs
                .iter()
                .find(|addr| proxy.protocol() != ProxyProtocol::Socks4 || addr.is_ipv4())
                .ok_or_else(|| invalid("SOCKS target has no compatible address"))?
                .ip();
            Target::Ip(ip)
        } else {
            host.parse().map(Target::Ip).unwrap_or(Target::Host(host))
        };
        match proxy.protocol() {
            ProxyProtocol::Socks4 | ProxyProtocol::Socks4A => {
                socks4(&mut stream, target, port)?;
            }
            ProxyProtocol::Socks5 | ProxyProtocol::Socks5h => {
                socks5(&mut stream, proxy, target, port)?;
            }
            _ => unreachable!(),
        }
        stream.deadline.remaining()?;
        Ok(Some(stream.io.into_inner()))
    }
}

struct Deadline {
    start: Instant,
    timeout: NextTimeout,
}

impl Deadline {
    fn remaining(&self) -> Result<NextTimeout, Error> {
        if self.timeout.after.is_not_happening() {
            return Ok(self.timeout);
        }
        let remaining = self
            .timeout
            .after
            .checked_sub(self.start.elapsed())
            .filter(|duration| !duration.is_zero())
            .ok_or(Error::Timeout(self.timeout.reason))?;
        Ok(NextTimeout {
            after: remaining.into(),
            reason: self.timeout.reason,
        })
    }
}

struct Handshake {
    io: TransportAdapter,
    deadline: Deadline,
}

impl Read for Handshake {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io
            .set_timeout(self.deadline.remaining().map_err(Error::into_io)?);
        self.io.read(buf)
    }
}

impl Write for Handshake {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io
            .set_timeout(self.deadline.remaining().map_err(Error::into_io)?);
        self.io.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

enum Target<'a> {
    Ip(IpAddr),
    Host(&'a str),
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn socks4(stream: &mut Handshake, target: Target<'_>, port: u16) -> io::Result<()> {
    let mut request = vec![4, 1];
    request.extend(port.to_be_bytes());
    match target {
        Target::Ip(IpAddr::V4(ip)) => {
            request.extend(ip.octets());
            request.push(0);
        }
        Target::Host(host) => {
            request.extend([0, 0, 0, 1, 0]);
            request.extend(host.as_bytes());
            request.push(0);
        }
        Target::Ip(IpAddr::V6(_)) => return Err(invalid("SOCKS4 requires an IPv4 target")),
    }
    stream.write_all(&request)?;
    let mut response = [0; 8];
    stream.read_exact(&mut response)?;
    if response[..2] != [0, 90] {
        return Err(invalid("SOCKS4 proxy refused the connection"));
    }
    Ok(())
}

fn socks5(stream: &mut Handshake, proxy: &Proxy, target: Target<'_>, port: u16) -> io::Result<()> {
    stream.write_all(if proxy.username().is_some() {
        &[5, 2, 2, 0]
    } else {
        &[5, 1, 0]
    })?;
    let mut response = [0; 2];
    stream.read_exact(&mut response)?;
    match response {
        [5, 0] => {}
        [5, 2] if proxy.username().is_some() => {
            let username = proxy.username().unwrap();
            let password = proxy.password().unwrap_or("");
            let mut auth = vec![1];
            push_string(&mut auth, username)?;
            push_string(&mut auth, password)?;
            stream.write_all(&auth)?;
            stream.read_exact(&mut response)?;
            if response != [1, 0] {
                return Err(invalid("SOCKS5 proxy authentication failed"));
            }
        }
        _ => return Err(invalid("SOCKS5 proxy rejected the authentication methods")),
    }
    let mut request = vec![5, 1, 0];
    match target {
        Target::Ip(IpAddr::V4(ip)) => {
            request.push(1);
            request.extend(ip.octets());
        }
        Target::Ip(IpAddr::V6(ip)) => {
            request.push(4);
            request.extend(ip.octets());
        }
        Target::Host(host) => {
            request.push(3);
            push_string(&mut request, host)?;
        }
    }
    request.extend(port.to_be_bytes());
    stream.write_all(&request)?;
    let mut response = [0; 4];
    stream.read_exact(&mut response)?;
    if response[..3] != [5, 0, 0] {
        return Err(invalid("SOCKS5 proxy refused the connection"));
    }
    let address_length = match response[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut length = [0];
            stream.read_exact(&mut length)?;
            usize::from(length[0])
        }
        _ => return Err(invalid("SOCKS5 proxy returned an invalid address type")),
    };
    let mut address = vec![0; address_length + 2];
    stream.read_exact(&mut address)?;
    Ok(())
}

fn push_string(packet: &mut Vec<u8>, value: &str) -> io::Result<()> {
    let length = u8::try_from(value.len()).map_err(|_| invalid("SOCKS value is too long"))?;
    packet.push(length);
    packet.extend(value.as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(last: u8) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, last], 443))
    }

    /// An address that never answers must not hold a reachable one for its share of the
    /// deadline: dialing in turn spends most of the budget on the first address before the
    /// second is tried, which is far outside the bound asserted here.
    #[test]
    fn a_silent_address_does_not_delay_a_reachable_one() {
        let (silent, reachable) = (addr(1), addr(2));
        let started = Instant::now();
        let winner = race(
            vec![silent, reachable],
            Some(Duration::from_secs(30)),
            ATTEMPT_DELAY,
            Arc::new(move |target: SocketAddr, left: Option<Duration>| {
                if target == silent {
                    std::thread::sleep(left.unwrap());
                    return Err(Error::Timeout(ureq::Timeout::Connect));
                }
                Ok(target)
            }),
            || Error::Timeout(ureq::Timeout::Connect),
        );
        assert_eq!(winner.unwrap(), reachable);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "reachable address waited {:?} behind a silent one",
            started.elapsed()
        );
    }

    /// Any failure of one address hands over to the next; an implementation that retries only
    /// refused connections gives up on a host whose first address is unroutable.
    #[test]
    fn an_unroutable_address_hands_over_to_the_next() {
        let (unroutable, reachable) = (addr(1), addr(2));
        let winner = race(
            vec![unroutable, reachable],
            Some(Duration::from_secs(30)),
            ATTEMPT_DELAY,
            Arc::new(move |target: SocketAddr, _: Option<Duration>| {
                if target == unroutable {
                    return Err(Error::Io(io::Error::from(
                        io::ErrorKind::NetworkUnreachable,
                    )));
                }
                Ok(target)
            }),
            || Error::Timeout(ureq::Timeout::Connect),
        );
        assert_eq!(winner.unwrap(), reachable);
    }
}
