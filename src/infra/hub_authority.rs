//! The shared credential identity is a host and an explicitly supplied port.
//!
//! Scheme and path select transport and routing; they do not create another credential identity.
//! Repository remote-identity pins use their own, stricter URL identity.

use anyhow::{anyhow, ensure};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubAuthority {
    host: String,
    port: Option<u16>,
}

impl HubAuthority {
    pub fn parse(hub: &str) -> crate::Result<Self> {
        ensure!(
            !hub.chars().any(|c| c.is_control()) && !hub.contains(['?', '#', '\\']),
            "invalid Hub address"
        );
        let hub = hub.trim();
        let (scheme, rest) = hub
            .split_once("://")
            .ok_or_else(|| anyhow!("Hub address must use HTTP or HTTPS"))?;
        ensure!(
            scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"),
            "Hub address must use HTTP or HTTPS"
        );
        let raw_authority = rest.split('/').next().unwrap_or_default();
        ensure!(
            !raw_authority.is_empty() && !raw_authority.contains('@'),
            "Hub address must contain a host without user information"
        );
        let uri: http::Uri = format!("{}://{rest}", scheme.to_ascii_lowercase())
            .parse()
            .map_err(|_| anyhow!("invalid Hub address"))?;
        let authority = uri
            .authority()
            .ok_or_else(|| anyhow!("Hub address has no host"))?;
        ensure!(authority.as_str() == raw_authority, "invalid Hub authority");
        let raw_host = authority.host();
        ensure!(!raw_host.is_empty(), "Hub address has no host");
        let suffix = &raw_authority[raw_host.len()..];
        let port = if suffix.is_empty() {
            None
        } else {
            let digits = suffix
                .strip_prefix(':')
                .ok_or_else(|| anyhow!("invalid Hub port"))?;
            ensure!(
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()),
                "invalid Hub port"
            );
            Some(
                digits
                    .parse::<u16>()
                    .map_err(|_| anyhow!("invalid Hub port"))?,
            )
        };
        let host = if let Some(literal) = raw_host.strip_prefix('[') {
            let literal = literal
                .strip_suffix(']')
                .ok_or_else(|| anyhow!("invalid Hub IP address"))?;
            let (address, zone) = literal
                .split_once("%25")
                .map_or((literal, None), |(address, zone)| (address, Some(zone)));
            let address = address
                .parse::<std::net::Ipv6Addr>()
                .map_err(|_| anyhow!("invalid Hub IP address"))?;
            if let Some(zone) = zone {
                ensure!(
                    !zone.is_empty()
                        && zone.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric()
                                || matches!(byte, b'-' | b'_' | b'.' | b'~')
                        }),
                    "invalid Hub IP scope"
                );
                format!("[{address}%25{zone}]")
            } else {
                format!("[{address}]")
            }
        } else {
            raw_host.to_ascii_lowercase()
        };
        Ok(Self { host, port })
    }

    pub fn label(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }

    /// A bounded, case-stable filename component without URI routing or user information.
    pub fn storage_key(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"agit.hub-authority.v2\0");
        hash.update((self.host.len() as u64).to_be_bytes());
        hash.update(self.host.as_bytes());
        match self.port {
            Some(port) => {
                hash.update([1]);
                hash.update(port.to_be_bytes());
            }
            None => hash.update([0]),
        }
        format!("v2~{}", hex::encode(hash.finalize()))
    }

    pub fn matches(&self, hub: &str) -> bool {
        Self::parse(hub).is_ok_and(|other| other == *self)
    }
}

pub fn safe_label(hub: &str) -> String {
    HubAuthority::parse(hub)
        .map(|authority| authority.label())
        .unwrap_or_else(|_| "invalid Hub address".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_preserves_the_documented_credential_scope() {
        let expected = HubAuthority::parse("http://EXAMPLE.test:08177/base").unwrap();
        for hub in [
            "HTTP://example.TEST:8177",
            "hTtPs://example.test:8177/another/path/",
            " https://EXAMPLE.TEST:8177/ ",
        ] {
            assert_eq!(HubAuthority::parse(hub).unwrap(), expected);
        }
        for hub in [
            "http://example.test",
            "http://example.test:8178",
            "http://other.test:8177",
        ] {
            assert_ne!(HubAuthority::parse(hub).unwrap(), expected);
        }
        for (left, right) in [
            ("http://example.test", "http://example.test:80"),
            ("https://example.test", "https://example.test:443"),
        ] {
            assert_ne!(
                HubAuthority::parse(left).unwrap(),
                HubAuthority::parse(right).unwrap()
            );
        }
        assert_eq!(expected.label(), "example.test:8177");
    }

    #[test]
    fn literal_ip_spellings_keep_address_and_scope_identity() {
        assert_eq!(
            HubAuthority::parse("HTTP://[0:0:0:0:0:0:0:1]:8177").unwrap(),
            HubAuthority::parse("https://[::1]:8177/a").unwrap()
        );
        assert_ne!(
            HubAuthority::parse("http://[fe80::1%25Ethernet]:8177").unwrap(),
            HubAuthority::parse("http://[fe80::1%25ethernet]:8177").unwrap()
        );
    }

    #[test]
    fn malformed_addresses_never_become_a_credential_identity() {
        for hub in [
            "",
            "example.test",
            "ftp://example.test",
            "https:///path",
            "http://:80",
            "http://host:",
            "http://host:bad",
            "http://host:65536",
            "http://host:+80",
            "http://host:80:90",
            "http://user:secret@host",
            "http://host/?secret=value",
            "http://host/#secret",
            "http://host\n",
            "http://host\\other",
            "http://bad host",
            "http://[not-an-ip]",
            "http://[::1]:",
            "http://[::1]:bad",
            "http://[::1",
            "http://[fe80::1%25]",
            "http://[fe80::1%eth0]",
        ] {
            assert!(
                HubAuthority::parse(hub).is_err(),
                "unexpected valid identity"
            );
        }
        assert_eq!(
            safe_label("http://user:secret@host/?private"),
            "invalid Hub address"
        );
    }

    #[test]
    fn storage_keys_separate_legacy_collisions_and_bound_filename_size() {
        for (left, right) in [
            ("HTTP://127.0.0.1:8177", "HTTP://127.0.0.1:8178"),
            ("http://node:8177", "http://node_8177"),
            ("http://[::1]:8177", "http://___1__8177"),
        ] {
            assert_ne!(
                HubAuthority::parse(left).unwrap().storage_key(),
                HubAuthority::parse(right).unwrap().storage_key()
            );
        }
        let key = HubAuthority::parse(&format!("https://{}", "a".repeat(250)))
            .unwrap()
            .storage_key();
        assert_eq!(key.len(), 67);
        assert!(key.starts_with("v2~"));
        assert!(
            key[3..]
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }
}
