//! Private worker transfers keep native event limits independent of IPC frame limits.

use super::projector::{MAX_UNIT_BYTES, Mode};
use serde::{Deserialize, Serialize};

pub(super) const CHUNK_BYTES: usize = MAX_UNIT_BYTES / 2;

#[derive(Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Command {
    Begin {
        operation: String,
        mode: Mode,
        bytes: usize,
        output_limit: usize,
    },
    Append {
        operation: String,
        offset: usize,
    },
    Read {
        operation: String,
        offset: usize,
    },
    Close {
        operation: String,
    },
}

pub(super) struct Writer<W> {
    pub inner: W,
    pub bytes: usize,
    pub maximum: usize,
    pub overflowed: bool,
}

impl<W: std::io::Write> std::io::Write for Writer<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.maximum.saturating_sub(self.bytes) {
            self.overflowed = true;
            return Err(std::io::Error::other(
                "hydrated event exceeds the materialization byte budget",
            ));
        }
        let written = self.inner.write(bytes)?;
        self.bytes += written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
