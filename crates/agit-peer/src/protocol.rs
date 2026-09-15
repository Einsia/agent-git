//! Bounded endpoint frames travel inside the authenticated peer channel.

use anyhow::ensure;
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Canceling a write closes the endpoint; another frame cannot follow a partial write.
pub async fn write<W: AsyncWrite + Unpin, T: Serialize>(
    output: &mut W,
    value: &T,
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(value)?;
    ensure!(
        body.len() <= crate::MAX_FRAME_BYTES,
        "peer frame exceeds the size limit"
    );
    output.write_u32(body.len() as u32).await?;
    output.write_all(&body).await?;
    output.flush().await?;
    Ok(())
}

pub struct Reader<R> {
    input: R,
    header: [u8; 4],
    header_read: usize,
    body: Vec<u8>,
    body_read: usize,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(input: R) -> Self {
        Self {
            input,
            header: [0; 4],
            header_read: 0,
            body: Vec::new(),
            body_read: 0,
        }
    }

    /// Partial records belong to the reader so canceling a wait preserves framing.
    pub async fn read<T: DeserializeOwned>(&mut self) -> anyhow::Result<Option<T>> {
        while self.header_read < self.header.len() {
            let count = self
                .input
                .read(&mut self.header[self.header_read..])
                .await?;
            if count == 0 {
                ensure!(self.header_read == 0, "peer closed in a frame header");
                return Ok(None);
            }
            self.header_read += count;
        }
        let size = u32::from_be_bytes(self.header) as usize;
        ensure!(
            size > 0 && size <= crate::MAX_FRAME_BYTES,
            "invalid peer frame length"
        );
        self.body.resize(size, 0);
        while self.body_read < size {
            let count = self.input.read(&mut self.body[self.body_read..]).await?;
            ensure!(count > 0, "peer closed in a frame body");
            self.body_read += count;
        }
        let value = serde_json::from_slice(&self.body);
        self.header_read = 0;
        self.body_read = 0;
        self.body.clear();
        Ok(Some(value?))
    }
}
