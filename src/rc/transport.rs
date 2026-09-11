//! Proxy-aware TCP establishment followed by the hub's TLS and WebSocket handshake.

use anyhow::{Context, bail};
use http::Uri;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::proxy::matcher::Matcher;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tower_service::Service;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CONNECT_HEADERS: usize = 8192;
type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(super) async fn connect(request: Request) -> crate::Result<Socket> {
    let no_proxy = std::env::var("NO_PROXY")
        .or_else(|_| std::env::var("no_proxy"))
        .unwrap_or_default();
    let proxies = if no_proxy.split(',').any(|host| host.trim() == "*") {
        Matcher::builder().build()
    } else {
        Matcher::from_env()
    };
    connect_with(request, &proxies, CONNECT_TIMEOUT).await
}

fn proxy_destination(uri: &Uri) -> crate::Result<Uri> {
    let mut parts = uri.clone().into_parts();
    parts.scheme = Some(match uri.scheme_str() {
        Some("ws") => http::uri::Scheme::HTTP,
        Some("wss") => http::uri::Scheme::HTTPS,
        _ => bail!("RC requires a ws:// or wss:// hub URL"),
    });
    if uri.port().is_none() {
        let host = uri.host().context("RC hub URL is missing a host")?;
        let port = if uri.scheme_str() == Some("ws") {
            80
        } else {
            443
        };
        parts.authority = Some(format!("{host}:{port}").parse()?);
    }
    Ok(Uri::from_parts(parts)?)
}

async fn establish_tunnel(
    stream: &mut TcpStream,
    destination: &Uri,
    auth: Option<&http::HeaderValue>,
) -> crate::Result<()> {
    let authority = destination
        .authority()
        .context("RC hub URL is missing an authority")?;
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n").into_bytes();
    if let Some(auth) = auth {
        request.extend_from_slice(b"Proxy-Authorization: ");
        request.extend_from_slice(auth.as_bytes());
        request.extend_from_slice(b"\r\n");
    }
    request.extend_from_slice(b"\r\n");
    stream.write_all(&request).await?;

    // Stop exactly at the header boundary so tunneled bytes remain on the socket.
    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        if response.len() == MAX_CONNECT_HEADERS {
            bail!("proxy CONNECT response headers exceed the size limit");
        }
        response.push(stream.read_u8().await?);
    }
    let mut headers = [httparse::EMPTY_HEADER; 128];
    let mut parsed = httparse::Response::new(&mut headers);
    if !parsed.parse(&response)?.is_complete() {
        bail!("incomplete proxy CONNECT response");
    }
    match parsed.code {
        Some(200..=299) => Ok(()),
        Some(407) => bail!("proxy authorization required"),
        Some(code) => bail!("proxy CONNECT rejected with HTTP {code}"),
        None => bail!("proxy CONNECT response is missing a status"),
    }
}

async fn connect_with(
    request: Request,
    proxies: &Matcher,
    timeout: Duration,
) -> crate::Result<Socket> {
    let destination = proxy_destination(request.uri())?;
    let proxy = proxies.intercept(&destination);
    let route = match &proxy {
        Some(proxy) => {
            if proxy.uri().scheme_str() != Some("http") {
                bail!(
                    "RC supports http:// CONNECT proxies; configure HTTP_PROXY/HTTPS_PROXY with an HTTP proxy or use NO_PROXY for this hub"
                );
            }
            format!("HTTP CONNECT proxy {}", proxy.uri())
        }
        None => "direct connection (no matching proxy)".to_owned(),
    };

    // One deadline covers DNS, TCP, CONNECT, TLS and WebSocket negotiation.
    tokio::time::timeout(timeout, async {
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        connector.set_connect_timeout(Some(timeout));
        let stream = match proxy {
            Some(proxy) => {
                // Only proxy credentials belong on CONNECT. For WSS, the RC
                // bearer token stays inside the hub's TLS tunnel.
                let mut stream = connector
                    .call(proxy.uri().clone())
                    .await
                    .context("proxy TCP connection failed")?
                    .into_inner();
                establish_tunnel(&mut stream, &destination, proxy.basic_auth())
                    .await
                    .context("proxy CONNECT failed")?;
                stream
            }
            None => connector
                .call(destination)
                .await
                .context("TCP connection failed")?
                .into_inner(),
        };
        // Keep the original hub URI for SNI, certificate validation and Host.
        let (socket, _) =
            tokio_tungstenite::client_async_tls_with_config(request, stream, None, None)
                .await
                .context("hub TLS/WebSocket handshake failed")?;
        Ok::<_, anyhow::Error>(socket)
    })
    .await
    .with_context(|| format!("RC connection timed out via {route}"))?
    .with_context(|| format!("RC connection failed via {route}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn request(url: &str) -> Request {
        let mut request = url.into_client_request().unwrap();
        request
            .headers_mut()
            .insert("authorization", "Bearer rc-test-secret".parse().unwrap());
        request
    }

    async fn read_headers(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            assert!(bytes.len() < 8192, "unexpectedly large test request");
            bytes.push(stream.read_u8().await.unwrap());
        }
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn websocket_schemes_select_the_corresponding_http_proxy() {
        let matcher = Matcher::builder()
            .http("http://plain-proxy.test:8080")
            .https("http://tls-proxy.test:8888")
            .build();
        for (url, expected) in [
            ("ws://hub.test/rc/ws", "plain-proxy.test"),
            ("wss://hub.test/rc/ws", "tls-proxy.test"),
        ] {
            let destination = proxy_destination(&url.parse().unwrap()).unwrap();
            assert_eq!(
                matcher.intercept(&destination).unwrap().uri().host(),
                Some(expected)
            );
        }
    }

    #[test]
    fn connect_destinations_have_explicit_default_ports() {
        for (url, authority) in [
            ("ws://hub.test/rc/ws", "hub.test:80"),
            ("wss://hub.test/rc/ws", "hub.test:443"),
            ("ws://[::1]/rc/ws", "[::1]:80"),
            ("wss://[::1]/rc/ws", "[::1]:443"),
            ("ws://hub.test:8765/rc/ws", "hub.test:8765"),
        ] {
            let destination = proxy_destination(&url.parse().unwrap()).unwrap();
            assert_eq!(destination.authority().unwrap().as_str(), authority);
        }
    }

    #[tokio::test]
    async fn fragmented_connect_response_reaches_the_original_websocket_host() {
        for (url, authority) in [
            ("ws://unresolvable.invalid/rc/ws", "unresolvable.invalid:80"),
            ("ws://[2001:db8::1]/rc/ws", "[2001:db8::1]:80"),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let headers = read_headers(&mut stream).await;
                assert!(headers.starts_with(&format!("CONNECT {authority} HTTP/1.1\r\n")));
                for fragment in [
                    "HTTP/1.1 ",
                    "200 Connection established\r\n",
                    "Proxy-Agent: test\r\n\r",
                    "\n",
                ] {
                    stream.write_all(fragment.as_bytes()).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                tokio_tungstenite::accept_async(stream).await.unwrap()
            });
            let matcher = Matcher::builder().http(format!("http://{address}")).build();
            connect_with(request(url), &matcher, TEST_TIMEOUT)
                .await
                .unwrap();
            tokio::time::timeout(TEST_TIMEOUT, server)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn connect_response_validation_is_bounded_and_does_not_echo_proxy_bytes() {
        for response in [
            b"HTTP/1.1 403 private-proxy-value\r\n\r\n".to_vec(),
            b"HTTP/1.1 200x private-proxy-value\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\ninvalid header\r\n\r\n".to_vec(),
            b"HTTP/1.1 200 OK\r\n".to_vec(),
            vec![b'x'; MAX_CONNECT_HEADERS + 1],
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_headers(&mut stream).await;
                stream.write_all(&response).await.unwrap();
            });
            let matcher = Matcher::builder().http(format!("http://{address}")).build();
            let error = connect_with(
                request("ws://unresolvable.invalid/rc/ws"),
                &matcher,
                TEST_TIMEOUT,
            )
            .await
            .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("proxy CONNECT failed"), "{message}");
            assert!(!message.contains("private-proxy-value"));
            server.await.unwrap();
        }
    }

    #[test]
    fn no_proxy_bypasses_domains_addresses_and_subnets() {
        let matcher = Matcher::builder()
            .all("http://proxy.test:8888")
            .no(".internal.test,127.0.0.1,::1,10.0.0.0/8,2001:db8::/32")
            .build();
        for host in [
            "internal.test",
            "hub.internal.test",
            "127.0.0.1",
            "[::1]",
            "10.2.3.4",
            "[2001:db8::1]",
        ] {
            let destination =
                proxy_destination(&format!("wss://{host}/rc/ws").parse().unwrap()).unwrap();
            assert!(
                matcher.intercept(&destination).is_none(),
                "must bypass {host}"
            );
        }
        let destination =
            proxy_destination(&"wss://notinternal.test/rc/ws".parse().unwrap()).unwrap();
        assert!(matcher.intercept(&destination).is_some());
        assert!(
            Matcher::builder()
                .all("http://proxy.test")
                .no("*")
                .build()
                .intercept(&destination)
                .is_none()
        );
    }

    #[tokio::test]
    #[allow(
        clippy::result_large_err,
        reason = "tungstenite fixes the handshake callback error type"
    )]
    async fn connect_preserves_hub_headers_and_isolates_proxy_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let headers = read_headers(&mut stream).await.to_ascii_lowercase();
            assert!(headers.starts_with("connect unresolvable.invalid:8765 http/1.1\r\n"));
            assert!(headers.contains("proxy-authorization: basic "));
            assert!(!headers.contains("rc-test-secret"));
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            let mut websocket =
                tokio_tungstenite::accept_hdr_async(stream, |request: &Request, response| {
                    assert_eq!(request.uri().path(), "/rc/ws");
                    assert_eq!(request.headers()["host"], "unresolvable.invalid:8765");
                    assert_eq!(request.headers()["authorization"], "Bearer rc-test-secret");
                    assert!(!request.headers().contains_key("proxy-authorization"));
                    Ok(response)
                })
                .await
                .unwrap();
            let message = websocket.next().await.unwrap().unwrap();
            websocket.send(message).await.unwrap();
        });
        let matcher = Matcher::builder()
            .http(format!("http://proxy-user:proxy-password@{address}"))
            .build();
        let mut socket = connect_with(
            request("ws://unresolvable.invalid:8765/rc/ws"),
            &matcher,
            TEST_TIMEOUT,
        )
        .await
        .unwrap();
        socket
            .send(Message::Text("rc payload".into()))
            .await
            .unwrap();
        let reply = tokio::time::timeout(TEST_TIMEOUT, socket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(reply.into_text().unwrap(), "rc payload");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn no_proxy_reaches_the_hub_without_contacting_the_proxy() {
        let hub = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = hub.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = hub.accept().await.unwrap();
            tokio_tungstenite::accept_async(stream).await.unwrap()
        });
        let matcher = Matcher::builder()
            .all(format!("http://{}", proxy.local_addr().unwrap()))
            .no("127.0.0.1")
            .build();
        connect_with(
            request(&format!("ws://{address}/rc/ws")),
            &matcher,
            TEST_TIMEOUT,
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), proxy.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn proxy_rejection_does_not_fall_back_or_disclose_credentials() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_headers(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let matcher = Matcher::builder()
            .https(format!("http://proxy-user:proxy-password@{address}"))
            .build();
        let error = connect_with(
            request("wss://unresolvable.invalid/rc/ws"),
            &matcher,
            TEST_TIMEOUT,
        )
        .await
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("HTTP CONNECT proxy"), "{message}");
        assert!(
            message.contains("proxy authorization required"),
            "{message}"
        );
        for secret in ["proxy-user", "proxy-password", "rc-test-secret"] {
            assert!(!message.contains(secret), "error leaked credentials");
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stalled_proxy_and_websocket_handshakes_expire() {
        for via_proxy in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_headers(&mut stream).await;
                let mut remaining = Vec::new();
                stream.read_to_end(&mut remaining).await.unwrap();
            });
            let (url, matcher) = if via_proxy {
                (
                    "wss://unresolvable.invalid/rc/ws".to_owned(),
                    Matcher::builder().all(format!("http://{address}")).build(),
                )
            } else {
                (format!("ws://{address}/rc/ws"), Matcher::builder().build())
            };
            let error = connect_with(request(&url), &matcher, Duration::from_millis(200))
                .await
                .unwrap_err();
            assert!(error.to_string().contains("timed out"), "{error:#}");
            tokio::time::timeout(TEST_TIMEOUT, server)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn unsupported_proxy_schemes_fail_explicitly() {
        for proxy in ["https://proxy.test:8443", "socks5://proxy.test:1080"] {
            let matcher = Matcher::builder().all(proxy).build();
            let error = connect_with(request("wss://hub.test/rc/ws"), &matcher, TEST_TIMEOUT)
                .await
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("supports http:// CONNECT proxies")
            );
        }
    }
}
