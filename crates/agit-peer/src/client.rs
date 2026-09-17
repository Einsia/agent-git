//! Cloud admission and rendezvous are independent of local session execution.

mod admission;
mod handshake;
pub use admission::verified_transport;
pub use handshake::join_controller;

use crate::cloud::*;
use agit_tunnel::{Config, Connection, Packet, PacketSink, PacketSource};
use anyhow::{Context, ensure};
use futures_util::{SinkExt, StreamExt};
use reqwest::Method;
use serde::de::DeserializeOwned;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RESPONSE: usize = 4 * 1024 * 1024;

#[derive(Debug)]
pub struct HttpFailure {
    pub status: u16,
    pub reason: Option<&'static str>,
}
impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cloud peer service returned HTTP {}", self.status)?;
        if let Some(reason) = self.reason {
            write!(f, ": {reason}")?;
        }
        Ok(())
    }
}
impl std::error::Error for HttpFailure {}

fn error_reason(status: u16, bytes: &[u8]) -> Option<&'static str> {
    let body = serde_json::from_slice::<serde_json::Value>(bytes).ok();
    match (
        status,
        body.as_ref().and_then(|value| value["error"].as_str()),
    ) {
        (503, Some("cloud peer relay is disabled")) => {
            Some("Cloud peer relay is not enabled on this Hub")
        }
        (503, Some("cloud peer relay is draining")) => {
            Some("Cloud peer relay is restarting; retry shortly")
        }
        (503, _) => Some("Cloud device connection is temporarily unavailable; retry shortly"),
        (401, _) => Some("sign in to this Hub again"),
        (403, _) => Some("this account is not allowed to access the device"),
        _ => None,
    }
}

/// Transport failures and temporary service refusals do not revoke an unexpired lease.
pub fn is_transient(error: &anyhow::Error) -> bool {
    if let Some(http) = error.downcast_ref::<HttpFailure>() {
        return matches!(http.status, 408 | 429 | 500..=599);
    }
    error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
        error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
    })
}

#[derive(Clone)]
pub struct Client {
    origin: String,
    transport_origin: String,
    direct: bool,
    http: reqwest::Client,
}

impl Client {
    pub fn new(origin: &str) -> anyhow::Result<Self> {
        let origin = Self::parse_origin(origin, false)?;
        Ok(Self {
            transport_origin: origin.clone(),
            direct: false,
            origin,
            http: Self::http_client(false)?,
        })
    }

    /// A trusted host may route admission and tunnel traffic internally without changing the issuer.
    /// The caller selects this endpoint from deployment configuration, never a peer-supplied URL.
    pub fn with_trusted_transport_origin(mut self, origin: &str) -> anyhow::Result<Self> {
        self.transport_origin = Self::parse_origin(origin, true)?;
        self.direct = true;
        self.http = Self::http_client(true)?;
        Ok(self)
    }

    fn parse_origin(origin: &str, allow_internal: bool) -> anyhow::Result<String> {
        let url = url::Url::parse(origin).context("invalid cloud peer origin")?;
        let local = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
                || (allow_internal && host.ends_with(".svc.cluster.local"))
        });
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && local),
            "cloud peer origin requires HTTPS"
        );
        ensure!(
            url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "cloud peer origin cannot contain credentials, a path, or a query"
        );
        Ok(url.as_str().trim_end_matches('/').into())
    }

    fn http_client(direct: bool) -> anyhow::Result<reqwest::Client> {
        let client = reqwest::Client::builder()
            .pool_idle_timeout(Duration::from_secs(30))
            // Dialing an unreachable address must leave time to join the relay offer.
            .connect_timeout(Duration::from_secs(3))
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none());
        Ok(if direct { client.no_proxy() } else { client }.build()?)
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        token: &Secret,
        body: Option<serde_json::Value>,
    ) -> anyhow::Result<T> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.transport_origin))
            .bearer_auth(token.expose());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await.context("cloud peer request failed")?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let mut bytes = Vec::new();
            while let Ok(Some(chunk)) = response.chunk().await {
                if bytes.len() + chunk.len() > 16 * 1024 {
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            return Err(HttpFailure {
                status,
                reason: error_reason(status, &bytes),
            }
            .into());
        }
        ensure!(
            response
                .content_length()
                .is_none_or(|length| length <= MAX_RESPONSE as u64),
            "cloud peer response exceeds its size limit"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(
                bytes.len() + chunk.len() <= MAX_RESPONSE,
                "cloud peer response exceeds its size limit"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid cloud peer response"))
    }

    pub async fn enroll(
        &self,
        account: &Secret,
        enrollment: &Enrollment,
    ) -> anyhow::Result<DeviceCredential> {
        self.register(account, enrollment, "/api/peer/devices")
            .await
    }

    pub async fn register_controller(
        &self,
        account: &Secret,
        enrollment: &Enrollment,
    ) -> anyhow::Result<DeviceCredential> {
        self.register(account, enrollment, "/api/peer/controllers")
            .await
    }

    async fn register(
        &self,
        account: &Secret,
        enrollment: &Enrollment,
        path: &str,
    ) -> anyhow::Result<DeviceCredential> {
        let credential: DeviceCredential = self
            .request(
                Method::POST,
                path,
                account,
                Some(serde_json::to_value(enrollment)?),
            )
            .await?;
        ensure!(
            credential.device.owner.issuer == self.origin
                && credential.device.machine_id == enrollment.machine_id
                && credential.device.certificate == enrollment.certificate,
            "cloud enrollment identity mismatch"
        );
        Ok(credential)
    }

    pub async fn renew_controller(&self, device: &DeviceCredential) -> anyhow::Result<()> {
        let _: serde_json::Value = self
            .request(Method::PUT, "/api/peer/controllers/me", &device.token, None)
            .await?;
        Ok(())
    }

    pub async fn revoke(&self, account: &Secret, device: &Device) -> anyhow::Result<()> {
        let _: serde_json::Value = self
            .request(
                Method::DELETE,
                &format!("/api/peer/devices/{}", device.id),
                account,
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn devices(
        &self,
        account: &Secret,
        after: Option<&str>,
    ) -> anyhow::Result<DevicePage> {
        let mut url = url::Url::parse(&format!("{}/api/peer/devices", self.origin))?;
        if let Some(after) = after {
            url.query_pairs_mut().append_pair("after", after);
        }
        let path = &url.as_str()[self.origin.len()..];
        let page: DevicePage = self.request(Method::GET, path, account, None).await?;
        ensure!(
            page.devices
                .iter()
                .all(|entry| entry.device.owner.issuer == self.origin),
            "cloud device issuer mismatch"
        );
        Ok(page)
    }

    pub async fn connect(
        &self,
        account: &Secret,
        source: &Device,
        target: &Device,
    ) -> anyhow::Result<DialedConnection> {
        let dialed: DialedConnection = self
            .request(
                Method::POST,
                "/api/peer/connections",
                account,
                Some(
                    serde_json::json!({"source_device_id":source.id, "target_device_id":target.id}),
                ),
            )
            .await?;
        self.validate_grant(&dialed.connection.grant)?;
        ensure!(
            same_device(&dialed.connection.grant.source, source)
                && same_endpoint(&dialed.connection.grant.target, target),
            "cloud connection identity changed; refresh device enrollment"
        );
        Ok(dialed)
    }

    pub async fn verify(
        &self,
        executor: &DeviceCredential,
        token: &Secret,
    ) -> anyhow::Result<ConnectionGrant> {
        let grant: ConnectionGrant = self
            .request(
                Method::POST,
                "/api/peer/grants/verify",
                &executor.token,
                Some(serde_json::json!({"token":token})),
            )
            .await?;
        self.validate_executor_grant(executor, &grant)?;
        Ok(grant)
    }

    /// Presence grants come from the authenticated Hub channel, before relay admission and peer TLS.
    pub async fn offered_grant(
        &self,
        executor: &DeviceCredential,
        token: &Secret,
        grant: Option<ConnectionGrant>,
    ) -> anyhow::Result<ConnectionGrant> {
        match grant {
            Some(grant) => {
                self.validate_executor_grant(executor, &grant)?;
                Ok(grant)
            }
            None => self.verify(executor, token).await,
        }
    }

    fn validate_executor_grant(
        &self,
        executor: &DeviceCredential,
        grant: &ConnectionGrant,
    ) -> anyhow::Result<()> {
        self.validate_grant(grant)?;
        ensure!(
            same_device(&grant.target, &executor.device),
            "cloud grant addresses another executor"
        );
        Ok(())
    }

    pub async fn renew(
        &self,
        executor: &DeviceCredential,
        token: &Secret,
        previous: &ConnectionGrant,
    ) -> anyhow::Result<ConnectionGrant> {
        let grant: ConnectionGrant = self
            .request(
                Method::POST,
                "/api/peer/grants/renew",
                &executor.token,
                Some(serde_json::json!({"token":token})),
            )
            .await?;
        self.validate_grant(&grant)?;
        ensure!(
            grant.id == previous.id
                && grant.caller == previous.caller
                && same_device(&grant.source, &previous.source)
                && same_device(&grant.target, &previous.target)
                && same_device(&grant.target, &executor.device)
                && grant.expires_at_ms > previous.expires_at_ms,
            "cloud renewal must preserve connection identity and advance its lease"
        );
        Ok(grant)
    }

    fn validate_grant(&self, grant: &ConnectionGrant) -> anyhow::Result<()> {
        ensure!(
            grant.caller.issuer == self.origin
                && grant.source.owner.issuer == self.origin
                && grant.target.owner.issuer == self.origin
                && grant.source.owner.account_id == grant.caller.account_id,
            "cloud grant caller or issuer mismatch"
        );
        let now = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())?;
        ensure!(
            grant.expires_at_ms > now,
            "cloud grant expired before endpoint admission"
        );
        Ok(())
    }

    fn socket_config(&self, device: &DeviceCredential, path: &str) -> anyhow::Result<Config> {
        ensure!(
            device.device.owner.issuer == self.origin,
            "cloud device issuer mismatch"
        );
        let mut url = url::Url::parse(&self.transport_origin)?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("invalid cloud socket scheme"))?;
        url.set_path(path);
        Ok(Config::WebSocket {
            url: url.into(),
            direct: self.direct,
            headers: vec![(
                "Authorization".into(),
                format!("Bearer {}", device.token.expose()),
            )],
        })
    }

    pub fn presence_config(&self, device: &DeviceCredential) -> anyhow::Result<Config> {
        let mut config = self.socket_config(device, "/api/peer/presence")?;
        if let Config::WebSocket { headers, .. } = &mut config {
            headers.push(("X-Agit-Peer-Offer".into(), "grant-v1".into()));
        }
        Ok(config)
    }

    pub fn data_config(&self, device: &DeviceCredential) -> anyhow::Result<Config> {
        self.socket_config(device, "/api/peer/data")
    }
}

fn same_endpoint(left: &Device, right: &Device) -> bool {
    left.id == right.id
        && left.owner == right.owner
        && left.machine_id == right.machine_id
        && left.certificate == right.certificate
}

fn same_device(left: &Device, right: &Device) -> bool {
    same_endpoint(left, right) && left.credential_epoch == right.credential_epoch
}

async fn text(sink: &mut PacketSink, source: &mut PacketSource) -> anyhow::Result<String> {
    loop {
        match source.next().await.context("cloud tunnel closed")?? {
            Packet::Text(text) => return Ok(text),
            Packet::Ping(bytes) => sink.send(Packet::Pong(bytes)).await?,
            Packet::Pong(_) => {}
            _ => anyhow::bail!("cloud tunnel closed or sent an invalid handshake"),
        }
    }
}

pub async fn join_data(
    connection: Connection,
    link_id: &str,
    ticket: Secret,
) -> anyhow::Result<Connection> {
    let pid = connection.worker_pid;
    let (mut sink, mut source) = connection.split();
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        sink.send(Packet::Text(serde_json::to_string(&DataJoin { ticket })?))
            .await?;
        let ready: DataReady = serde_json::from_str(&text(&mut sink, &mut source).await?)
            .map_err(|_| anyhow::anyhow!("invalid cloud relay ready frame"))?;
        ensure!(
            ready.link_id == link_id,
            "cloud relay paired another connection"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("cloud relay pairing timed out")??;
    Ok(Connection::from_parts(pid, sink, source))
}

pub struct Presence {
    pub epoch: String,
    pub worker_pid: u32,
    sink: PacketSink,
    source: PacketSource,
}

impl Presence {
    pub async fn open(connection: Connection) -> anyhow::Result<Self> {
        let worker_pid = connection.worker_pid;
        let (mut sink, mut source) = connection.split();
        let ready = tokio::time::timeout(REQUEST_TIMEOUT, text(&mut sink, &mut source)).await??;
        let event: PresenceEvent = serde_json::from_str(&ready)
            .map_err(|_| anyhow::anyhow!("invalid cloud presence ready frame"))?;
        let PresenceEvent::Ready { epoch } = event else {
            anyhow::bail!("cloud presence was not acknowledged")
        };
        Ok(Self {
            epoch,
            worker_pid,
            sink,
            source,
        })
    }

    pub async fn next(&mut self) -> anyhow::Result<PresenceEvent> {
        loop {
            let packet = tokio::time::timeout(Duration::from_secs(45), self.source.next())
                .await?
                .context("cloud presence closed")??;
            match packet {
                Packet::Ping(bytes) => {
                    tokio::time::timeout(REQUEST_TIMEOUT, self.sink.send(Packet::Pong(bytes)))
                        .await??;
                }
                Packet::Pong(_) => {}
                Packet::Text(text) => {
                    let event: PresenceEvent = serde_json::from_str(&text)
                        .map_err(|_| anyhow::anyhow!("invalid cloud presence offer"))?;
                    ensure!(
                        matches!(event, PresenceEvent::Offer { .. }),
                        "cloud presence registered twice"
                    );
                    return Ok(event);
                }
                _ => anyhow::bail!("cloud presence closed or sent invalid data"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn presence_grant_retains_executor_binding_without_an_http_round_trip() {
        let issuer = "http://127.0.0.1:0";
        let client = Client::new(issuer).unwrap();
        let device = Device {
            id: "executor".into(),
            owner: crate::access::Principal {
                issuer: issuer.into(),
                account_id: "owner".into(),
            },
            machine_id: "machine".into(),
            display_name: "Executor".into(),
            certificate: crate::Identity::generate().unwrap().certificate().clone(),
            credential_epoch: 1,
        };
        let executor = DeviceCredential {
            device: device.clone(),
            token: Secret::new("executor-token".into()),
        };
        let token = Secret::new("grant-token".into());
        let grant = ConnectionGrant {
            id: "offered-grant".into(),
            caller: device.owner.clone(),
            source: Device {
                id: "controller".into(),
                ..device.clone()
            },
            target: device,
            expires_at_ms: i64::MAX,
        };
        assert_eq!(
            client
                .offered_grant(&executor, &token, Some(grant.clone()))
                .await
                .unwrap()
                .id,
            grant.id
        );
        let mut stale = grant;
        stale.target.credential_epoch += 1;
        assert!(
            client
                .offered_grant(&executor, &token, Some(stale))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn trusted_transport_routes_requests_without_changing_device_authority() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let issuer = "https://public-hub.example";
        let device = Device {
            id: "executor".into(),
            owner: crate::access::Principal {
                issuer: issuer.into(),
                account_id: "owner".into(),
            },
            machine_id: "machine".into(),
            display_name: "Executor".into(),
            certificate: crate::Identity::generate().unwrap().certificate().clone(),
            credential_epoch: 1,
        };
        let server_device = device.clone();
        let server = tokio::spawn(async move {
            for incorrect_issuer in [false, true] {
                let (socket, _) = listener.accept().await.unwrap();
                let mut socket = BufReader::new(socket);
                let mut line = String::new();
                socket.read_line(&mut line).await.unwrap();
                assert_eq!(line, "GET /api/peer/devices HTTP/1.1\r\n");
                loop {
                    line.clear();
                    socket.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut device = server_device.clone();
                if incorrect_issuer {
                    device.owner.issuer = format!("http://{address}");
                }
                let body = serde_json::to_string(&DevicePage {
                    devices: vec![DevicePresence {
                        device,
                        online: true,
                    }],
                    next_cursor: None,
                })
                .unwrap();
                socket.get_mut().write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
            }
        });
        let client = Client::new(issuer)
            .unwrap()
            .with_trusted_transport_origin(&format!("http://{address}"))
            .unwrap();
        assert_eq!(client.origin(), issuer);
        let token = Secret::new("fixture-token".into());
        assert_eq!(
            client.devices(&token, None).await.unwrap().devices[0]
                .device
                .owner
                .issuer,
            issuer
        );
        assert!(
            client
                .devices(&token, None)
                .await
                .unwrap_err()
                .to_string()
                .contains("issuer mismatch")
        );
        let Config::WebSocket { url, direct, .. } = client
            .data_config(&DeviceCredential { device, token })
            .unwrap()
        else {
            panic!("cloud transport must remain a WebSocket");
        };
        assert_eq!(url, format!("ws://{address}/api/peer/data"));
        assert!(direct);
        server.await.unwrap();
        assert!(
            Client::new(issuer)
                .unwrap()
                .with_trusted_transport_origin("http://public.example")
                .is_err()
        );
        assert!(Client::new("http://relay.default.svc.cluster.local").is_err());
        assert!(
            Client::new(issuer)
                .unwrap()
                .with_trusted_transport_origin("http://relay.default.svc.cluster.local")
                .is_ok()
        );
    }

    #[test]
    fn credentials_cannot_be_sent_to_insecure_remote_origins_or_url_paths() {
        for origin in [
            "http://remote.example",
            "https://user:secret@example.test",
            "https://example.test/path",
            "https://example.test/?token=value",
        ] {
            assert!(Client::new(origin).is_err());
        }
        assert_eq!(
            Client::new("https://cloud.example/").unwrap().origin(),
            "https://cloud.example"
        );
        assert!(Client::new("http://127.0.0.1:8177").is_ok());
    }
}

#[cfg(test)]
mod error_tests {
    #[test]
    fn service_errors_expose_only_known_safe_diagnostics() {
        assert_eq!(
            super::error_reason(503, br#"{"error":"cloud peer relay is disabled"}"#),
            Some("Cloud peer relay is not enabled on this Hub")
        );
        let unknown =
            super::error_reason(503, br#"{"error":"database password=private"}"#).unwrap();
        assert!(!unknown.contains("private"));
        assert!(!unknown.contains("database"));
    }
}
