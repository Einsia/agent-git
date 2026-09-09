//! Exact JSON facts for metadata equality across persisted readers.
//!
//! Object order and insignificant whitespace do not change a fact. Numeric spellings stay
//! intact because projecting unknown fields through floating-point values can erase changes.

use std::collections::BTreeMap;

use anyhow::{Context, ensure};

use crate::Result;

const MAX_DEPTH: usize = 128;

/// Number tokens stay exact so unknown metadata cannot compare equal through floating-point loss.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JsonFacts {
    Object(BTreeMap<String, JsonFacts>),
    Array(Vec<JsonFacts>),
    String(String),
    Atom(String),
}

struct JsonParser<'a> {
    input: &'a str,
    offset: usize,
}

impl JsonFacts {
    /// Parses unambiguous JSON while retaining exact scalar tokens and decoded object keys.
    /// Callers bound the input bytes before parsing; excessive nesting is rejected here.
    pub fn parse(input: &str) -> Result<Self> {
        JsonParser::parse(input)
    }
}

impl<'a> JsonParser<'a> {
    fn parse(input: &'a str) -> Result<JsonFacts> {
        let mut parser = Self { input, offset: 0 };
        let value = parser.value(0)?;
        parser.space();
        ensure!(parser.offset == input.len(), "metadata has trailing JSON");
        Ok(value)
    }

    fn space(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.offset)
            .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
        {
            self.offset += 1;
        }
    }

    fn take(&mut self, expected: u8) -> bool {
        self.space();
        if self.input.as_bytes().get(self.offset) == Some(&expected) {
            self.offset += 1;
            true
        } else {
            false
        }
    }

    fn string(&mut self) -> Result<String> {
        self.space();
        let start = self.offset;
        ensure!(self.take(b'"'), "metadata expects a JSON string");
        let mut escaped = false;
        while let Some(byte) = self.input.as_bytes().get(self.offset).copied() {
            self.offset += 1;
            if byte == b'"' && !escaped {
                return serde_json::from_str(&self.input[start..self.offset])
                    .context("metadata has an invalid JSON string");
            }
            escaped = byte == b'\\' && !escaped;
        }
        anyhow::bail!("metadata ends inside a JSON string")
    }

    fn value(&mut self, depth: usize) -> Result<JsonFacts> {
        ensure!(depth < MAX_DEPTH, "metadata exceeds its JSON depth limit");
        self.space();
        if self.take(b'{') {
            let mut object = BTreeMap::new();
            if self.take(b'}') {
                return Ok(JsonFacts::Object(object));
            }
            loop {
                let key = self.string()?;
                ensure!(self.take(b':'), "metadata omits a JSON field separator");
                let value = self.value(depth + 1)?;
                ensure!(
                    object.insert(key, value).is_none(),
                    "metadata repeats a JSON key"
                );
                if self.take(b'}') {
                    break;
                }
                ensure!(self.take(b','), "metadata omits a JSON object separator");
            }
            return Ok(JsonFacts::Object(object));
        }
        if self.take(b'[') {
            let mut array = Vec::new();
            if self.take(b']') {
                return Ok(JsonFacts::Array(array));
            }
            loop {
                array.push(self.value(depth + 1)?);
                if self.take(b']') {
                    break;
                }
                ensure!(self.take(b','), "metadata omits a JSON array separator");
            }
            return Ok(JsonFacts::Array(array));
        }
        if self.input.as_bytes().get(self.offset) == Some(&b'"') {
            return self.string().map(JsonFacts::String);
        }
        let start = self.offset;
        while self
            .input
            .as_bytes()
            .get(self.offset)
            .is_some_and(|b| !matches!(b, b' ' | b'\n' | b'\r' | b'\t' | b',' | b'}' | b']'))
        {
            self.offset += 1;
        }
        let token = &self.input[start..self.offset];
        let parsed: serde_json::Value =
            serde_json::from_str(token).context("metadata has an invalid JSON token")?;
        ensure!(
            parsed.is_number() || parsed.is_boolean() || parsed.is_null(),
            "metadata has an invalid scalar"
        );
        Ok(JsonFacts::Atom(token.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting_and_escaped_keys_preserve_facts() {
        assert_eq!(
            JsonFacts::parse(r#"{"b":true,"a":[null,"x"]}"#).unwrap(),
            JsonFacts::parse(r#" { "\u0061": [ null, "\u0078" ], "b": true } "#).unwrap()
        );
        assert_ne!(
            JsonFacts::parse(r#"{"a":[null,"x"]}"#).unwrap(),
            JsonFacts::parse(r#"{"a":["x",null]}"#).unwrap()
        );
    }

    #[test]
    fn numeric_facts_retain_digits_beyond_float_precision() {
        for (left, right) in [
            ("9007199254740992.0", "9007199254740993.0"),
            ("18446744073709551616", "18446744073709551617"),
            ("1.00000000000000001", "1.00000000000000002"),
            ("-0", "0"),
        ] {
            assert_ne!(
                JsonFacts::parse(&format!("{{\"future\":{{\"value\":[{left}]}}}}")).unwrap(),
                JsonFacts::parse(&format!("{{\"future\":{{\"value\":[{right}]}}}}")).unwrap()
            );
        }
    }

    #[test]
    fn malformed_or_ambiguous_facts_refuse() {
        for text in [
            r#"{"x":1,"x":2}"#,
            r#"{"x":{"k":1,"\u006b":2}}"#,
            r#"{"x":[{"k":1,"k":2}]}"#,
            r#"[] true"#,
            r#"{"x":}"#,
            r#"[1,]"#,
            r#"{"x":1,}"#,
            r#"{"x":"unterminated}"#,
            r#""\ud800""#,
            "",
            "[01]",
            "[NaN]",
        ] {
            assert!(JsonFacts::parse(text).is_err(), "accepted {text:?}");
        }
    }

    #[test]
    fn nesting_is_bounded_without_restricting_flat_metadata() {
        let nested = |depth: usize| format!("{}null{}", "[".repeat(depth), "]".repeat(depth));
        assert!(JsonFacts::parse(&nested(MAX_DEPTH - 1)).is_ok());
        assert!(JsonFacts::parse(&nested(MAX_DEPTH)).is_err());
        let flat = format!("[{}]", vec!["null"; MAX_DEPTH * 2].join(","));
        assert!(JsonFacts::parse(&flat).is_ok());
    }
}
