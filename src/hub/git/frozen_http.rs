//! Availability captures the ordinary HTTP client's proxy and WebPki policy.
//! Explicit request preferences are mapped where representable; unsupported effective
//! authentication or TLS constraints refuse this capability without changing Git transport.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::path::Path;
use std::time::Duration;
use ureq::tls::{PemItem, RootCerts, TlsConfig};

const CA_LIMIT: u64 = 1024 * 1024;

// Captured failures outlive their input; diagnostics must not retain config or PEM bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum Failure {
    #[error("cannot capture availability HTTP configuration")]
    Configuration,
    #[error("availability requires a captured HTTP endpoint")]
    Endpoint,
    #[error("availability proxy configuration is unsupported or invalid")]
    Proxy,
    #[error("availability cannot honor explicit proxy authentication")]
    ProxyAuthentication,
    #[error("availability cannot honor explicit TLS preferences")]
    Tls,
    #[error("availability CA bundle must be a readable bounded PEM certificate file")]
    CaBundle,
    #[error("availability cannot honor the explicit HTTP version")]
    HttpVersion,
    #[error("availability cannot honor the explicit low-speed deadline")]
    LowSpeed,
    #[error("availability user agent contains unsupported header characters")]
    UserAgent,
}

type Result<T> = std::result::Result<T, Failure>;

pub(super) fn prepare(
    url: &str,
    preferences: &BTreeMap<String, String>,
    environment: &BTreeMap<OsString, OsString>,
) -> Result<ureq::Agent> {
    let uri: ::http::Uri = url.parse().map_err(|_| Failure::Endpoint)?;
    if !matches!(uri.scheme_str(), Some("http" | "https")) || uri.host().is_none() {
        return Err(Failure::Endpoint);
    }
    let proxy = captured_proxy(&uri, preferences, environment)?;
    if proxy.is_some()
        && preferences
            .get("http.proxyauthmethod")
            .is_some_and(|value| !value.is_empty() && !value.eq_ignore_ascii_case("basic"))
    {
        return Err(Failure::ProxyAuthentication);
    }
    if preferences
        .get("http.version")
        .is_some_and(|value| value != "HTTP/1.1")
    {
        return Err(Failure::HttpVersion);
    }
    let positive = |key: &str| -> Result<bool> {
        preferences
            .get(key)
            .map(|value| {
                value
                    .parse::<i64>()
                    .map(|value| value > 0)
                    .map_err(|_| Failure::LowSpeed)
            })
            .transpose()
            .map(|value| value.unwrap_or(false))
    };
    if positive("http.lowspeedlimit")? && positive("http.lowspeedtime")? {
        return Err(Failure::LowSpeed);
    }
    let proxy_tls = proxy
        .as_ref()
        .is_some_and(|proxy| proxy.protocol() == ureq::ProxyProtocol::Https);
    if proxy_tls {
        refuse_tls(
            preferences,
            &[
                "http.proxysslcert",
                "http.proxysslkey",
                "http.proxysslcainfo",
            ],
        )?;
        require_verification(preferences, "http.proxysslverify")?;
    }
    let mut tls = TlsConfig::builder();
    if uri.scheme_str() == Some("https") {
        refuse_tls(
            preferences,
            &[
                "http.sslcapath",
                "http.sslcert",
                "http.sslkey",
                "http.sslversion",
                "http.sslcipherlist",
                "http.pinnedpubkey",
            ],
        )?;
        require_verification(preferences, "http.sslverify")?;
        if let Some(path) = preferences.get("http.sslcainfo") {
            // The Agent shares its roots with an HTTPS proxy, so independent trust is not representable.
            if proxy_tls {
                return Err(Failure::Tls);
            }
            tls = tls.root_certs(captured_ca(Path::new(path))?);
        }
    }
    let mut config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .max_redirects(0)
        .http_status_as_error(false)
        // Explicit None also replaces any proxy inferred by Config from the live environment.
        .proxy(proxy)
        .tls_config(tls.build());
    if let Some(value) = preferences.get("http.useragent") {
        if !value.is_ascii() || value.chars().any(char::is_control) {
            return Err(Failure::UserAgent);
        }
        config = config.user_agent(value.clone());
    }
    Ok(config.build().into())
}

fn refuse_tls(values: &BTreeMap<String, String>, keys: &[&str]) -> Result<()> {
    if keys
        .iter()
        .any(|key| values.get(*key).is_some_and(|value| !value.is_empty()))
    {
        return Err(Failure::Tls);
    }
    Ok(())
}

fn require_verification(values: &BTreeMap<String, String>, key: &str) -> Result<()> {
    if let Some(value) = values.get(key)
        && !super::git_bool(value).map_err(|_| Failure::Tls)?
    {
        return Err(Failure::Tls);
    }
    Ok(())
}

fn captured_ca(path: &Path) -> Result<RootCerts> {
    if !path.is_absolute()
        || !std::fs::metadata(path)
            .map_err(|_| Failure::CaBundle)?
            .is_file()
    {
        return Err(Failure::CaBundle);
    }
    let file = std::fs::File::open(path).map_err(|_| Failure::CaBundle)?;
    if !file.metadata().map_err(|_| Failure::CaBundle)?.is_file() {
        return Err(Failure::CaBundle);
    }
    let mut bytes = Vec::new();
    file.take(CA_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Failure::CaBundle)?;
    if bytes.len() as u64 > CA_LIMIT {
        return Err(Failure::CaBundle);
    }
    let mut certificates = Vec::new();
    for item in ureq::tls::parse_pem(&bytes) {
        match item.map_err(|_| Failure::CaBundle)? {
            PemItem::Certificate(certificate) => certificates.push(certificate),
            _ => return Err(Failure::CaBundle),
        }
    }
    if certificates.is_empty() {
        return Err(Failure::CaBundle);
    }
    Ok(certificates.into())
}

fn captured_proxy(
    endpoint: &::http::Uri,
    preferences: &BTreeMap<String, String>,
    environment: &BTreeMap<OsString, OsString>,
) -> Result<Option<ureq::Proxy>> {
    let value = |name: &str| {
        environment
            .get(OsStr::new(name))
            .and_then(|value| value.to_str())
    };
    let proxy = match preferences.get("http.proxy") {
        Some(value) if value.is_empty() => None,
        Some(value) => Some(ureq::Proxy::new(value).map_err(|_| Failure::Proxy)?),
        // These priorities and invalid-value fallback follow ureq's environment policy.
        None => [
            "ALL_PROXY",
            "all_proxy",
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "http_proxy",
        ]
        .into_iter()
        .filter_map(value)
        .find_map(|value| ureq::Proxy::new(value).ok()),
    };
    let Some(proxy) = proxy else { return Ok(None) };
    let mut builder = ureq::Proxy::builder(proxy.protocol())
        .host(proxy.host())
        .port(proxy.port())
        .resolve_target(proxy.resolve_target());
    if let Some(username) = proxy.username() {
        builder = builder.username(username);
    }
    if let Some(password) = proxy.password() {
        builder = builder.password(password);
    }
    if let Some(bypass) = ["NO_PROXY", "no_proxy"].into_iter().find_map(value) {
        for item in bypass.split(',') {
            builder = builder.no_proxy(item);
        }
    }
    let proxy = builder.build().map_err(|_| Failure::Proxy)?;
    if proxy.is_no_proxy(endpoint) {
        return Ok(None);
    }
    if !matches!(
        proxy.protocol(),
        ureq::ProxyProtocol::Http | ureq::ProxyProtocol::Https
    ) {
        return Err(Failure::Proxy);
    }
    Ok(Some(proxy))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preferences(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect()
    }

    #[test]
    fn prepared_agent_retains_captured_policy_and_ca_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ca.pem");
        let pem = "-----BEGIN CERTIFICATE-----\nAQID\n-----END CERTIFICATE-----\n";
        std::fs::write(&path, pem.repeat(2)).unwrap();
        let mut values = preferences(&[
            ("http.proxy", "http://proxy.invalid:8080"),
            ("http.sslcainfo", path.to_str().unwrap()),
            ("http.useragent", "prepared-agent"),
        ]);
        let mut environment =
            BTreeMap::from([("ALL_PROXY".into(), "http://ambient.invalid".into())]);
        let agent = prepare("https://hub.invalid/r.git", &values, &environment).unwrap();
        values.clear();
        environment.clear();
        std::fs::remove_file(path).unwrap();
        let config = agent.config();
        assert_eq!(config.proxy().unwrap().host(), "proxy.invalid");
        assert_eq!(config.proxy().unwrap().port(), 8080);
        assert!(
            matches!(config.user_agent(), ureq::config::AutoHeaderValue::Provided(value) if value.as_str() == "prepared-agent")
        );
        assert_eq!(config.max_redirects(), 0);
        assert_eq!(config.timeouts().global, Some(Duration::from_secs(30)));
        assert!(!config.http_status_as_error());
        let RootCerts::Specific(roots) = config.tls_config().root_certs() else {
            panic!("CA snapshot missing")
        };
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().all(|root| root.der() == [1, 2, 3]));
    }

    #[test]
    fn captured_proxy_uses_ureq_precedence_and_bypass_matching() {
        let empty = BTreeMap::new();
        let mut env = BTreeMap::from([
            ("ALL_PROXY".into(), "http://first.invalid".into()),
            ("all_proxy".into(), "http://lower.invalid".into()),
            ("HTTPS_PROXY".into(), "http://scheme.invalid".into()),
        ]);
        for url in ["http://sub.example.com", "https://sub.example.com"] {
            let uri = url.parse().unwrap();
            assert_eq!(
                captured_proxy(&uri, &empty, &env).unwrap().unwrap().host(),
                "first.invalid"
            );
        }
        let uri = "https://sub.example.com".parse().unwrap();
        for invalid in ["", "http://["] {
            env.insert("ALL_PROXY".into(), invalid.into());
            assert_eq!(
                captured_proxy(&uri, &empty, &env).unwrap().unwrap().host(),
                "lower.invalid"
            );
        }
        env.insert("NO_PROXY".into(), "".into());
        env.insert("no_proxy".into(), "*".into());
        assert!(captured_proxy(&uri, &empty, &env).unwrap().is_some());
        for pattern in [
            "example.com",
            ".example.com",
            "*.example.com",
            "other,*",
            " .example.com",
            "10.0.0.0/8",
        ] {
            env.insert("NO_PROXY".into(), pattern.into());
            let library = ureq::Proxy::builder(ureq::ProxyProtocol::Http).host("lower.invalid");
            let library = pattern
                .split(',')
                .fold(library, |builder, item| builder.no_proxy(item))
                .build()
                .unwrap();
            assert_eq!(
                captured_proxy(&uri, &empty, &env).unwrap().is_none(),
                library.is_no_proxy(&uri),
                "{pattern}"
            );
        }
        env.remove(OsStr::new("NO_PROXY"));
        env.remove(OsStr::new("no_proxy"));
        assert!(
            captured_proxy(&uri, &preferences(&[("http.proxy", "")]), &env)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            captured_proxy(
                &uri,
                &preferences(&[("http.proxy", "http://explicit.invalid")]),
                &env
            )
            .unwrap()
            .unwrap()
            .host(),
            "explicit.invalid"
        );
        assert_eq!(
            captured_proxy(&uri, &preferences(&[("http.proxy", "http://[")]), &env).unwrap_err(),
            Failure::Proxy
        );
    }

    #[test]
    fn proxy_reconstruction_preserves_library_fields_and_raw_credentials() {
        let uri = "https://hub.invalid".parse().unwrap();
        for raw in [
            "http://proxy.invalid",
            "https://proxy.invalid",
            "http://[::1]:8080",
            "http://user%40name:p%3Ass@proxy.invalid",
            "http://user:@proxy.invalid",
            "http://@proxy.invalid",
            "http://user@proxy.invalid",
        ] {
            let original = ureq::Proxy::new(raw).unwrap();
            let captured =
                captured_proxy(&uri, &preferences(&[("http.proxy", raw)]), &BTreeMap::new())
                    .unwrap()
                    .unwrap();
            assert_eq!(captured.protocol(), original.protocol());
            assert_eq!(captured.host(), original.host());
            assert_eq!(captured.port(), original.port());
            assert_eq!(captured.username(), original.username());
            assert_eq!(captured.password(), original.password());
            assert_eq!(captured.resolve_target(), original.resolve_target());
        }
    }

    #[test]
    fn refusal_applies_only_to_effective_request_preferences() {
        let empty = BTreeMap::new();
        let url = "https://hub.invalid/r.git";
        let mut values = preferences(&[
            ("http.proxy", "http://user:pass@proxy.invalid"),
            ("http.proxyauthmethod", "basic"),
        ]);
        assert!(prepare(url, &values, &empty).is_ok());
        values.insert("http.proxyauthmethod".into(), "negotiate".into());
        assert_eq!(
            prepare(url, &values, &empty).unwrap_err(),
            Failure::ProxyAuthentication
        );
        assert!(
            prepare(
                url,
                &values,
                &BTreeMap::from([("NO_PROXY".into(), "*".into())])
            )
            .is_ok()
        );
        values.insert("http.proxy".into(), "http://proxy.invalid".into());
        assert_eq!(
            prepare(url, &values, &empty).unwrap_err(),
            Failure::ProxyAuthentication
        );
        values.insert("http.proxy".into(), "".into());
        assert!(prepare(url, &values, &empty).is_ok());
        values.insert("http.proxy".into(), "socks5://proxy.invalid".into());
        assert_eq!(prepare(url, &values, &empty).unwrap_err(), Failure::Proxy);
        assert!(
            prepare(
                url,
                &values,
                &BTreeMap::from([("NO_PROXY".into(), "*".into())])
            )
            .is_ok()
        );
        for key in [
            "http.sslcapath",
            "http.sslcert",
            "http.sslkey",
            "http.pinnedpubkey",
            "http.sslcipherlist",
            "http.sslversion",
        ] {
            let values = preferences(&[(key, "explicit")]);
            assert_eq!(
                prepare(url, &values, &empty).unwrap_err(),
                Failure::Tls,
                "{key}"
            );
            assert!(
                prepare("http://hub.invalid", &values, &empty).is_ok(),
                "{key}"
            );
        }
        for key in [
            "http.proxysslcert",
            "http.proxysslkey",
            "http.proxysslcainfo",
        ] {
            let mut values = preferences(&[(key, "explicit")]);
            assert!(prepare(url, &values, &empty).is_ok(), "{key}");
            values.insert("http.proxy".into(), "https://proxy.invalid".into());
            assert_eq!(
                prepare(url, &values, &empty).unwrap_err(),
                Failure::Tls,
                "{key}"
            );
        }
        assert_eq!(
            prepare(
                url,
                &preferences(&[
                    ("http.proxy", "https://proxy.invalid"),
                    ("http.sslcainfo", "/captured.pem")
                ]),
                &empty
            )
            .unwrap_err(),
            Failure::Tls
        );
        for (key, value, expected) in [
            ("http.sslverify", "false", Failure::Tls),
            ("http.version", "HTTP/2", Failure::HttpVersion),
            (
                "http.useragent",
                "agent\r\nInjected: yes",
                Failure::UserAgent,
            ),
        ] {
            assert_eq!(
                prepare(url, &preferences(&[(key, value)]), &empty).unwrap_err(),
                expected
            );
        }
        assert_eq!(
            prepare(
                url,
                &preferences(&[("http.lowspeedlimit", "1"), ("http.lowspeedtime", "1")]),
                &empty
            )
            .unwrap_err(),
            Failure::LowSpeed
        );
        let agent = prepare(
            url,
            &preferences(&[
                ("http.version", "HTTP/1.1"),
                ("http.maxrequests", "7"),
                ("http.postbuffer", "1"),
                ("http.sslbackend", "schannel"),
            ]),
            &empty,
        )
        .unwrap();
        assert!(agent.config().proxy().is_none());
        assert!(matches!(
            agent.config().tls_config().root_certs(),
            RootCerts::WebPki
        ));
    }

    #[test]
    fn ca_failures_do_not_retain_input_bytes_or_parser_details() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ca.pem");
        let secret = "PRIVATE_PEM_SENTINEL";
        let malformed = format!("-----BEGIN {secret}\n");
        std::fs::write(&path, &malformed).unwrap();
        let failure = captured_ca(&path).unwrap_err();
        assert_eq!(failure, Failure::CaBundle);
        assert_eq!(
            failure.to_string(),
            "availability CA bundle must be a readable bounded PEM certificate file"
        );
        let diagnostic = format!("{failure:#} {failure:?}");
        assert!(!diagnostic.contains(secret));
        assert!(!diagnostic.contains(&format!("{:?}", malformed.as_bytes())));
        assert!(std::error::Error::source(&failure).is_none());
        std::fs::write(&path, vec![b'x'; CA_LIMIT as usize + 1]).unwrap();
        assert_eq!(captured_ca(&path).unwrap_err(), Failure::CaBundle);
        assert_eq!(
            captured_ca(Path::new("relative.pem")).unwrap_err(),
            Failure::CaBundle
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(captured_ca(&path).unwrap_err(), Failure::CaBundle);
    }
}
