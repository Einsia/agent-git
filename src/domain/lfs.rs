//! Git LFS pointers identify immutable payloads independently of Git blob identities.

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::io::Read;

pub mod history;
pub mod inspection;
#[cfg(feature = "cli")]
pub mod local;

pub const VERSION: &str = "https://git-lfs.github.com/spec/v1";
pub const POINTER_LIMIT: usize = 1024;

pub fn cached_object_path(
    repo: &crate::domain::repo::Repo,
    pointer: &Pointer,
) -> Result<std::path::PathBuf> {
    pointer.validate()?;
    let environment = repo.git(&["lfs", "env"])?;
    let directory = environment
        .lines()
        .find_map(|line| line.strip_prefix("LocalMediaDir="))
        .filter(|value| !value.is_empty())
        .context("Git LFS did not report its local object cache")?;
    let directory = std::path::PathBuf::from(directory);
    ensure!(
        directory.is_absolute(),
        "Git LFS returned a relative object cache"
    );
    Ok(directory
        .join(&pointer.oid[..2])
        .join(&pointer.oid[2..4])
        .join(&pointer.oid))
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Pointer {
    pub oid: String,
    pub size: u64,
}

impl Pointer {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.oid.len() == 64
                && self
                    .oid
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "invalid Git LFS object identity"
        );
        ensure!(self.size <= i64::MAX as u64, "Git LFS object is too large");
        Ok(())
    }

    /// Recognized but invalid pointers cannot fall back to ordinary file contents.
    pub fn parse(bytes: &[u8]) -> Result<Option<Self>> {
        let decoded = String::from_utf8_lossy(bytes);
        let header = decoded
            .trim_start()
            .lines()
            .find(|line| !line.is_empty() && !line.starts_with("ext-"));
        if !header.is_some_and(|line| {
            line.starts_with("version https://git-lfs.github.com/spec/")
                || line.starts_with("version https://hawser.github.com/spec/")
                || line.starts_with("version http://git-media.io/v/")
        }) {
            return Ok(None);
        }
        ensure!(
            bytes.len() < POINTER_LIMIT,
            "Git LFS pointer exceeds its format limit"
        );
        let text = std::str::from_utf8(bytes).context("Git LFS pointer is not UTF-8")?;
        ensure!(
            text.ends_with('\n') && !text.contains('\r'),
            "invalid Git LFS pointer line endings"
        );
        let mut lines = text.lines();
        ensure!(
            matches!(
                lines.next(),
                Some(
                    "version https://git-lfs.github.com/spec/v1"
                        | "version https://hawser.github.com/spec/v1"
                )
            ),
            "unsupported Git LFS pointer version"
        );
        let mut oid = None;
        let mut size = None;
        let mut previous = "";
        for line in lines {
            let (key, value) = line
                .split_once(' ')
                .context("invalid Git LFS pointer line")?;
            ensure!(
                !key.is_empty()
                    && key != "version"
                    && key.bytes().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || matches!(byte, b'.' | b'-'))
                    && key > previous
                    && !value.starts_with(' '),
                "invalid Git LFS pointer key order"
            );
            ensure!(
                !key.starts_with("ext-"),
                "Git LFS pointer extensions are not supported"
            );
            previous = key;
            match key {
                "oid" => {
                    oid = Some(
                        value
                            .strip_prefix("sha256:")
                            .context("unsupported Git LFS hash algorithm")?
                            .to_owned(),
                    )
                }
                "size" => {
                    let parsed: u64 = value.parse().context("invalid Git LFS object size")?;
                    ensure!(
                        parsed.to_string() == value,
                        "noncanonical Git LFS object size"
                    );
                    size = Some(parsed);
                }
                _ => {}
            }
        }
        let pointer = Self {
            oid: oid.context("Git LFS pointer has no object identity")?,
            size: size.context("Git LFS pointer has no size")?,
        };
        pointer.validate()?;
        Ok(Some(pointer))
    }

    /// Verification reads a bounded stream and rejects both truncation and trailing bytes.
    pub fn verify(&self, mut input: impl Read) -> Result<()> {
        self.validate()?;
        let mut digest = Sha256::new();
        let mut size = 0_u64;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            size = size
                .checked_add(count as u64)
                .context("Git LFS size overflow")?;
            ensure!(
                size <= self.size,
                "Git LFS payload exceeds its declared size"
            );
            digest.update(&buffer[..count]);
        }
        ensure!(size == self.size, "Git LFS payload is truncated");
        ensure!(
            hex::encode(digest.finalize()) == self.oid,
            "Git LFS payload hash does not match its pointer"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pointer() -> Pointer {
        Pointer {
            oid: hex::encode(Sha256::digest(b"artifact")),
            size: 8,
        }
    }

    #[test]
    fn pointers_are_distinct_from_text_and_keep_their_exact_object_identity() {
        let expected = pointer();
        let text = format!("version {VERSION}\noid sha256:{}\nsize 8\n", expected.oid);
        assert_eq!(Pointer::parse(text.as_bytes()).unwrap(), Some(expected));
        assert_eq!(Pointer::parse(b"an ordinary document").unwrap(), None);
        assert_eq!(Pointer::parse(b"").unwrap(), None);
        let future_key = text.replace("\nsize", "\nreserved metadata\nsize");
        assert!(Pointer::parse(future_key.as_bytes()).unwrap().is_some());
    }

    #[test]
    fn malformed_pointers_and_transform_extensions_are_not_plain_files() {
        let text = format!("version {VERSION}\noid sha256:{}\nsize 8\n", pointer().oid);
        for invalid in [
            format!("\n{text}"),
            format!("\u{2003}\t{text}"),
            format!("ext-0-custom sha256:{}\n{text}", pointer().oid),
            text.replace(VERSION, "http://git-media.io/v/2"),
            text.trim_end().to_owned(),
            text.replace("\n", "\r\n"),
            text.replace("size 8", "size 08"),
            text.replace("size 8", "size -1"),
            text.replace("sha256:", "sha1:"),
            text.replace("\noid", "\next-0-custom sha256:unused\noid"),
            text.replace("size 8", "size 8\nsize 8"),
            text.replace("size 8", "size 18446744073709551615"),
            text.replace("spec/v1", "spec/v1-other"),
            text.replace("spec/v1", "spec/v2"),
            format!("{text}version {VERSION}\n"),
        ] {
            assert!(
                Pointer::parse(invalid.as_bytes()).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn payload_integrity_rejects_truncation_trailing_data_and_same_size_substitution() {
        let expected = pointer();
        expected.verify(&b"artifact"[..]).unwrap();
        assert!(expected.verify(&b"artifac"[..]).is_err());
        assert!(expected.verify(&b"artifacts"[..]).is_err());
        assert!(expected.verify(&b"modified"[..]).is_err());
    }
}
