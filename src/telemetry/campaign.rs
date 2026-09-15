//! Locally retained campaign fields exclude authorization and arbitrary URL data.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Campaign {
    pub recorded_at: chrono::DateTime<chrono::Utc>,
    pub parameters: BTreeMap<String, Vec<String>>,
}

impl Campaign {
    pub fn from_url(raw: &str) -> Option<Self> {
        if raw.len() > 8192 {
            return None;
        }
        let url = url::Url::parse(raw).ok()?;
        if !matches!(url.scheme(), "http" | "https") {
            return None;
        }
        let mut parameters: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (key, value) in url.query_pairs() {
            let campaign_key = key.starts_with("utm_")
                && key.len() <= 48
                && key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
            let click_key = matches!(
                key.as_ref(),
                "gclid"
                    | "dclid"
                    | "gbraid"
                    | "wbraid"
                    | "fbclid"
                    | "msclkid"
                    | "ttclid"
                    | "twclid"
                    | "li_fat_id"
                    | "campaign"
            );
            if !(campaign_key || click_key)
                || value.is_empty()
                || value.len() > 1024
                || value.chars().any(char::is_control)
            {
                continue;
            }
            if !parameters.contains_key(key.as_ref()) && parameters.len() >= 32 {
                continue;
            }
            let values = parameters.entry(key.into_owned()).or_default();
            if values.len() < 8 && !values.iter().any(|v| v == value.as_ref()) {
                values.push(value.into_owned());
            }
        }
        (!parameters.is_empty()).then(|| Self {
            recorded_at: chrono::Utc::now(),
            parameters,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn campaign_fields_preserve_encoding_and_repeated_values_without_auth_material() {
        let campaign = Campaign::from_url("https://example.test/path?utm_source=search&utm_campaign=hello%20world&utm_content=a%2Bb&utm_content=second&utm_source_platform=ads&utm_custom=test&gclid=click&campaign=launch&code=secret&token=secret#secret").unwrap();
        assert_eq!(campaign.parameters["utm_campaign"], ["hello world"]);
        assert_eq!(campaign.parameters["utm_content"], ["a+b", "second"]);
        assert_eq!(campaign.parameters["utm_source_platform"], ["ads"]);
        assert_eq!(campaign.parameters["utm_custom"], ["test"]);
        assert_eq!(campaign.parameters["gclid"], ["click"]);
        assert!(!serde_json::to_string(&campaign).unwrap().contains("secret"));
        assert!(Campaign::from_url("https://example.test/?token=secret").is_none());
        assert!(Campaign::from_url("file:///tmp/?utm_source=test").is_none());
        assert!(
            Campaign::from_url(&format!(
                "https://example.test/?utm_source={}",
                "x".repeat(8192)
            ))
            .is_none()
        );
    }
}
