//! Optional local acceleration data never grants a claim or proves a source by itself.

use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::path::PathBuf;

pub const STATE_BYTES: usize = 64 * 1024;
pub const CONTEXT_BYTES: usize = 1024 * 1024;
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Carrier {
    pub path: PathBuf,
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub version: u32,
    pub source: String,
    pub session: String,
    pub owner: String,
    pub repository: String,
    pub branch: String,
    pub directory: PathBuf,
    pub cwd: Option<String>,
    pub binding: Option<super::NativeBinding>,
    pub claim: String,
    pub tip: String,
    pub bytes: u64,
    pub sha256: String,
    pub origin: u64,
    pub prompts_before: u32,
    pub events_before: usize,
    pub seen_message: bool,
    pub max_timestamp: Option<i64>,
    pub view_start: usize,
    pub projection: String,
    pub log: String,
    pub view: String,
    pub header: Option<Record>,
    #[serde(deserialize_with = "bounded_vec::<_, _, 3>")]
    pub titles: Vec<Record>,
    #[serde(deserialize_with = "bounded_vec::<_, _, 128>")]
    pub carriers: Vec<Carrier>,
}

fn bounded_vec<'de, D, T, const N: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Bounded<T, const N: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const N: usize> serde::de::Visitor<'de> for Bounded<T, N> {
        type Value = Vec<T>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a bounded continuation sequence")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> Result<Self::Value, A::Error> {
            let mut values = Vec::new();
            while values.len() < N {
                match sequence.next_element()? {
                    Some(value) => values.push(value),
                    None => return Ok(values),
                }
            }
            if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom(
                    "continuation sequence exceeds its budget",
                ));
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Bounded::<T, N>(std::marker::PhantomData))
}

impl State {
    pub fn decode(raw: &serde_json::value::RawValue) -> Option<Self> {
        if raw.get().len() > STATE_BYTES {
            return None;
        }
        let state: Self = serde_json::from_str(raw.get()).ok()?;
        let identifiers = [
            &state.source,
            &state.session,
            &state.owner,
            &state.repository,
            &state.branch,
            &state.claim,
            &state.tip,
            &state.sha256,
            &state.projection,
            &state.log,
            &state.view,
        ];
        if state.version != VERSION
            || identifiers.iter().any(|value| value.len() > 4096)
            || state.directory.as_os_str().len() > 4096
            || state.cwd.as_ref().is_some_and(|value| value.len() > 4096)
            || state
                .carriers
                .iter()
                .any(|carrier| carrier.path.as_os_str().len() > 4096)
        {
            return None;
        }
        Some(state)
    }

    pub fn encode(&self) -> Option<Box<serde_json::value::RawValue>> {
        struct Limited(Vec<u8>);
        impl std::io::Write for Limited {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > STATE_BYTES.saturating_sub(self.0.len()) {
                    return Err(std::io::Error::other(
                        "continuation state exceeds its budget",
                    ));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut output = Limited(Vec::with_capacity(STATE_BYTES));
        serde_json::to_writer(&mut output, self).ok()?;
        serde_json::value::RawValue::from_string(String::from_utf8(output.0).ok()?).ok()
    }
}

/// Borrow the optional JSON before admitting it to typed decoding. An oversized
/// cache must not allocate its vectors and strings merely to discover its size.
pub(super) fn borrow_state<'de, D>(
    deserializer: D,
) -> Result<Option<Cow<'de, serde_json::value::RawValue>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = Option::<&'de serde_json::value::RawValue>::deserialize(deserializer)?;
    Ok(raw.map(Cow::Borrowed))
}
