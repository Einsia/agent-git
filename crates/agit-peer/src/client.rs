//! Cloud admission and rendezvous are independent of local session execution.

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
}
impl std::fmt::Display for HttpFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cloud peer service returned HTTP {}", self.status)
    }
}
impl std::error::Error for HttpFailure {}

#[derive(Clone)]
pub struct Client {
    origin: String,
    http: reqwest::Client,
}

impl Client {
    pub fn new(origin: &str) -> anyhow::Result<Self> {
        let url = url::Url::parse(origin).context("invalid cloud peer origin")?;
        let loopback = url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        ensure!(
            url.scheme() == "https" || (url.scheme() == "http" && loopback),
            "cloud peer origin requires HTTPS"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "cloud peer origin cannot contain credentials, a path, or a query"
        );
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            origin: url.as_str().trim_end_matches('/').into(),
            http,
        })
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
            .request(method, format!("{}{path}", self.origin))
            .bearer_auth(token.expose());
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await.context("cloud peer request failed")?;
        if !response.status().is_success() {
            return Err(HttpFailure {
                status: response.status().as_u16(),
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
                && same_device(&dialed.connection.grant.target, target),
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
        self.validate_grant(&grant)?;
        ensure!(
            same_device(&grant.target, &executor.device),
            "cloud grant addresses another executor"
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
        let mut url = url::Url::parse(&self.origin)?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("invalid cloud socket scheme"))?;
        url.set_path(path);
        Ok(Config::WebSocket {
            url: url.into(),
            headers: vec![(
                "Authorization".into(),
                format!("Bearer {}", device.token.expose()),
            )],
        })
    }

    pub fn presence_config(&self, device: &DeviceCredential) -> anyhow::Result<Config> {
        self.socket_config(device, "/api/peer/presence")
    }

    pub fn data_config(&self, device: &DeviceCredential) -> anyhow::Result<Config> {
        self.socket_config(device, "/api/peer/data")
    }
}

fn same_device(left: &Device, right: &Device) -> bool {
    left.id == right.id
        && left.owner == right.owner
        && left.machine_id == right.machine_id
        && left.credential_epoch == right.credential_epoch
        && left.certificate == right.certificate
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
