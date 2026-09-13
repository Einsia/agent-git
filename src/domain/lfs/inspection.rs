//! Payload inspection preserves text scanning while bounding binary memory and total I/O.

use super::Pointer;
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::io::Read;

#[derive(Debug, PartialEq, Eq)]
pub enum Payload {
    Text(String),
    Binary,
    TooLarge,
}

/// A pointer is never evidence that its payload is clean, present, or unmodified.
pub fn read(
    mut input: impl Read,
    size: u64,
    pointer: Option<&Pointer>,
    text_limit: u64,
    remaining: &mut u64,
) -> Result<Payload> {
    if let Some(pointer) = pointer {
        pointer.validate()?;
        ensure!(pointer.size == size, "LFS inspection size mismatch");
    }
    ensure!(
        size <= *remaining,
        "LFS payload inspection exceeds the scan byte budget"
    );
    *remaining -= size;
    let mut digest = Sha256::new();
    let mut received = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    let mut text = Vec::new();
    let mut boundary = Vec::new();
    let mut binary = false;
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        received = received
            .checked_add(count as u64)
            .context("LFS inspection size overflow")?;
        ensure!(received <= size, "LFS payload exceeds its recorded size");
        digest.update(&buffer[..count]);
        if !binary {
            boundary.extend_from_slice(&buffer[..count]);
            match std::str::from_utf8(&boundary) {
                Ok(_) => boundary.clear(),
                Err(error) if error.error_len().is_some() => {
                    binary = true;
                    boundary.clear();
                    text.clear();
                }
                Err(error) => {
                    boundary.drain(..error.valid_up_to());
                }
            }
            if !binary {
                if received > text_limit {
                    return Ok(Payload::TooLarge);
                }
                text.extend_from_slice(&buffer[..count]);
            }
        }
    }
    ensure!(received == size, "LFS payload is truncated");
    if let Some(pointer) = pointer {
        ensure!(
            hex::encode(digest.finalize()) == pointer.oid,
            "LFS payload hash does not match its pointer"
        );
    }
    if binary || !boundary.is_empty() {
        Ok(Payload::Binary)
    } else {
        Ok(Payload::Text(String::from_utf8(text)?))
    }
}

pub fn cached(
    repo: &crate::domain::repo::Repo,
    pointer: &Pointer,
    text_limit: u64,
    remaining: &mut u64,
) -> Result<Payload> {
    let path = super::cached_object_path(repo, pointer)?;
    let input = std::fs::File::open(path)
        .context("cannot scan the missing LFS payload; fetch the object before publishing")?;
    read(input, pointer.size, Some(pointer), text_limit, remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect(bytes: &[u8], limit: u64, mut budget: u64) -> Result<Payload> {
        let pointer = Pointer {
            oid: hex::encode(Sha256::digest(bytes)),
            size: bytes.len() as u64,
        };
        read(bytes, pointer.size, Some(&pointer), limit, &mut budget)
    }

    #[test]
    fn binary_payloads_are_verified_without_becoming_unbounded_text() {
        let mut bytes = vec![0xff; 256 * 1024];
        assert_eq!(
            inspect(&bytes, 100, bytes.len() as u64).unwrap(),
            Payload::Binary
        );
        assert!(inspect(&bytes, 100, bytes.len() as u64 - 1).is_err());
        let pointer = Pointer {
            oid: hex::encode(Sha256::digest(&bytes)),
            size: bytes.len() as u64,
        };
        bytes[0] = 0;
        let mut remaining = u64::MAX;
        assert!(
            read(
                bytes.as_slice(),
                pointer.size,
                Some(&pointer),
                100,
                &mut remaining
            )
            .is_err()
        );
    }

    #[test]
    fn text_boundaries_preserve_multibyte_characters_and_refuse_oversized_text() {
        let mut text = "a".repeat(64 * 1024 - 1);
        text.push_str("\u{20ac}\0tail");
        assert_eq!(
            inspect(text.as_bytes(), u64::MAX, u64::MAX).unwrap(),
            Payload::Text(text.clone())
        );
        assert_eq!(
            inspect(text.as_bytes(), 100, u64::MAX).unwrap(),
            Payload::TooLarge
        );
        assert_eq!(inspect(&[b'a', 0xc2], 100, 100).unwrap(), Payload::Binary);
    }
}
