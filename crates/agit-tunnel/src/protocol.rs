//! The worker protocol carries transport facts without session or caller authority.

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const VERSION: u32 = 1;
pub const MAX_PAYLOAD: usize = 8 * 1024 * 1024;
pub const MAX_RECORD: usize = MAX_PAYLOAD * 6 + 4096;
pub const QUEUE_CAP: usize = 512;
pub const QUEUE_BYTES: usize = MAX_PAYLOAD * 2;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case", deny_unknown_fields)]
pub enum Config {
    WebSocket {
        url: String,
        headers: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        direct: bool,
    },
    Ssh {
        host: String,
        command: Vec<String>,
    },
}

impl Config {
    pub fn validate(&self) -> crate::Result<()> {
        match self {
            Self::WebSocket { url, headers, .. } => {
                let url = url::Url::parse(url).context("invalid tunnel URL")?;
                ensure!(
                    matches!(url.scheme(), "ws" | "wss"),
                    "unsupported tunnel URL scheme"
                );
                ensure!(
                    url.username().is_empty() && url.password().is_none(),
                    "tunnel credentials belong in headers"
                );
                ensure!(headers.len() <= 32, "too many tunnel headers");
                ensure!(
                    headers
                        .iter()
                        .map(|(k, v)| k.len() + v.len())
                        .sum::<usize>()
                        <= 16384,
                    "tunnel headers exceed the size limit"
                );
            }
            Self::Ssh { host, command } => {
                ensure!(
                    !host.is_empty()
                        && !host.starts_with('-')
                        && host.len() <= 255
                        && !host.chars().any(|c| c.is_whitespace() || c.is_control()),
                    "invalid SSH host alias"
                );
                ensure!(
                    !command.is_empty()
                        && !command[0].is_empty()
                        && command.len() <= 64
                        && command.iter().map(String::len).sum::<usize>() <= 16384
                        && !command.iter().any(|arg| arg.chars().any(char::is_control)),
                    "invalid remote command"
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Packet {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<Close>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Close {
    pub code: u16,
    pub reason: String,
}

impl Packet {
    pub fn len(&self) -> usize {
        match self {
            Self::Text(s) => s.len(),
            Self::Binary(b) | Self::Ping(b) | Self::Pong(b) => b.len(),
            Self::Close(s) => s.as_ref().map_or(0, |s| s.reason.len()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn validate(&self) -> crate::Result<()> {
        let len = self.len();
        ensure!(len <= MAX_PAYLOAD, "tunnel payload exceeds the size limit");
        if matches!(self, Self::Ping(_) | Self::Pong(_)) {
            ensure!(len <= 125, "tunnel control packet exceeds the size limit");
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    Open { version: u32, config: Config },
    Send { serial: u64, packet: Packet },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    Connected { version: u32, worker_pid: u32 },
    Received { packet: Packet },
    Written { serial: u64 },
    Failed { message: String },
}

/// Partial records stay in the reader when another select branch wins.
pub struct Reader<R> {
    input: R,
    partial: Vec<u8>,
    cap: usize,
}

impl<R: AsyncBufRead + Unpin> Reader<R> {
    pub fn new(input: R, cap: usize) -> Self {
        Self {
            input,
            partial: Vec::new(),
            cap,
        }
    }

    pub async fn record(&mut self) -> crate::Result<Option<Vec<u8>>> {
        loop {
            let bytes = self.input.fill_buf().await?;
            if bytes.is_empty() {
                ensure!(
                    self.partial.is_empty(),
                    "tunnel closed with a truncated record"
                );
                return Ok(None);
            }
            let end = bytes.iter().position(|b| *b == b'\n');
            let take = end.map_or(bytes.len(), |n| n + 1);
            ensure!(
                self.partial.len() + take <= self.cap,
                "tunnel record exceeds the size limit"
            );
            self.partial.extend_from_slice(&bytes[..take]);
            self.input.consume(take);
            if end.is_some() {
                return Ok(Some(std::mem::take(&mut self.partial)));
            }
        }
    }

    pub async fn read<T: DeserializeOwned>(&mut self) -> crate::Result<Option<T>> {
        self.record()
            .await?
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("malformed tunnel record"))
            })
            .transpose()
    }
}

pub async fn write<W: AsyncWrite + Unpin>(
    output: &mut W,
    value: &impl Serialize,
) -> crate::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() < MAX_RECORD,
        "tunnel record exceeds the size limit"
    );
    bytes.push(b'\n');
    output.write_all(&bytes).await?;
    output.flush().await?;
    Ok(())
}
