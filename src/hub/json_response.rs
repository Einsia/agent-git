//! Hub JSON is bounded before and after decoding its negotiated content encoding.

use anyhow::{Context, bail};
use std::io::{self, Read};

const MAX_JSON_BYTES: u64 = 10 * 1024 * 1024;

pub(super) fn read<T: serde::de::DeserializeOwned>(
    response: ureq::http::Response<ureq::Body>,
) -> anyhow::Result<T> {
    read_with_limit(response, MAX_JSON_BYTES)
}

fn read_with_limit<T: serde::de::DeserializeOwned>(
    mut response: ureq::http::Response<ureq::Body>,
    limit: u64,
) -> anyhow::Result<T> {
    let mut encodings = response.headers().get_all("content-encoding").iter();
    let gzip = match encodings.next() {
        None => false,
        Some(value) => match value
            .to_str()
            .context("invalid Hub response encoding")?
            .trim()
        {
            value if value.eq_ignore_ascii_case("identity") => false,
            value if value.eq_ignore_ascii_case("gzip") => true,
            _ => bail!("unsupported Hub response encoding"),
        },
    };
    anyhow::ensure!(
        encodings.next().is_none(),
        "unsupported Hub response encoding"
    );
    // The encoded reader needs room to observe EOF at the size boundary.
    let reader = response
        .body_mut()
        .with_config()
        .limit(limit.saturating_add(1))
        .reader();
    if gzip {
        parse(flate2::read::MultiGzDecoder::new(reader), limit)
    } else {
        parse(reader, limit)
    }
}

fn parse<T: serde::de::DeserializeOwned>(reader: impl Read, limit: u64) -> anyhow::Result<T> {
    Ok(serde_json::from_reader(io::BufReader::new(DecodedLimit {
        reader,
        remaining: limit,
    }))?)
}

struct DecodedLimit<R> {
    reader: R,
    remaining: u64,
}

impl<R: Read> Read for DecodedLimit<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return match self.reader.read(&mut [0])? {
                0 => Ok(0),
                _ => Err(io::Error::other("Hub JSON response exceeds size limit")),
            };
        }
        let size = self.remaining.min(buffer.len() as u64) as usize;
        let count = self.reader.read(&mut buffer[..size])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn response(data: Vec<u8>, encoding: Option<&str>) -> ureq::http::Response<ureq::Body> {
        let mut response = ureq::http::Response::builder();
        if let Some(encoding) = encoding {
            response = response.header("content-encoding", encoding);
        }
        response.body(ureq::Body::builder().data(data)).unwrap()
    }

    #[test]
    fn encoded_and_decoded_json_obey_the_same_size_boundary() {
        let payload = serde_json::to_vec(&serde_json::json!({"text": "x".repeat(4096)})).unwrap();
        for encoding in [None, Some("gzip")] {
            let data = if encoding.is_some() {
                gzip(&payload)
            } else {
                payload.clone()
            };
            let value: serde_json::Value =
                read_with_limit(response(data.clone(), encoding), payload.len() as u64).unwrap();
            assert_eq!(value["text"].as_str().unwrap().len(), 4096);
            assert!(
                read_with_limit::<serde_json::Value>(
                    response(data, encoding),
                    payload.len() as u64 - 1,
                )
                .is_err()
            );
        }
        let mut encoded_padding = gzip(b"{}");
        for _ in 0..64 {
            encoded_padding.extend(gzip(b""));
        }
        assert!(encoded_padding.len() > 1024);
        assert!(
            read_with_limit::<serde_json::Value>(response(encoded_padding, Some("gzip")), 1024,)
                .is_err()
        );
        let mut padded = b"{}".to_vec();
        padded.extend(vec![b' '; 8192]);
        assert!(
            read_with_limit::<serde_json::Value>(response(gzip(&padded), Some("gzip")), 1024)
                .is_err()
        );
    }

    #[test]
    fn malformed_and_unadvertised_encodings_are_rejected() {
        let valid = gzip(br#"{"name":"repository"}"#);
        let mut corrupt = valid.clone();
        let checksum = corrupt.len() - 8;
        corrupt[checksum] ^= 0xff;
        for data in [corrupt, valid[..valid.len() - 4].to_vec()] {
            assert!(
                read_with_limit::<serde_json::Value>(response(data, Some("gzip")), 1024).is_err()
            );
        }
        for encoding in ["br", "gzip, gzip"] {
            assert!(
                read_with_limit::<serde_json::Value>(response(valid.clone(), Some(encoding)), 1024)
                    .is_err()
            );
        }
    }
}
