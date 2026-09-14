//! SOCKS negotiation shares the request deadline with proxy resolution and TCP connection.

use std::io::{self, Read, Write};
use std::net::IpAddr;
use std::time::Instant;

use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    ConnectProxyConnector, ConnectionDetails, Connector, NextTimeout, RustlsConnector,
    TcpConnector, Transport, TransportAdapter,
};
use ureq::{Error, Proxy, ProxyProtocol};

pub(crate) fn agent(config: ureq::config::Config) -> ureq::Agent {
    let connector = SocksConnector
        .chain(ConnectProxyConnector::default())
        .chain(TcpConnector::default())
        .chain(RustlsConnector::default());
    ureq::Agent::with_parts(config, connector, DefaultResolver::default())
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
        let transport = TcpConnector::default()
            .connect(&proxy_details, None::<()>)?
            .ok_or_else(|| invalid("SOCKS proxy connection is missing"))?;
        let mut stream = Handshake {
            io: TransportAdapter::new(transport.boxed()),
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
