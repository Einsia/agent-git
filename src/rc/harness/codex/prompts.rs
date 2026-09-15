//! Expand explicitly selected local prompts using the documented Codex placeholders.
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{collections::HashMap, io::Read, path::PathBuf};

fn directory() -> crate::Result<PathBuf> {
    Ok(std::env::var_os("CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::infra::config::user_home().map(|home| home.join(".codex")))
        .context("Codex home is unavailable")?
        .join("prompts"))
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || "_-".contains(c))
}

fn read(name: &str) -> crate::Result<(String, String, String)> {
    ensure!(valid_name(name), "Invalid custom prompt name");
    let mut source = String::new();
    let file = std::fs::File::open(directory()?.join(format!("{name}.md")))?;
    ensure!(
        file.metadata()?.is_file(),
        "Custom prompt must be a Markdown file"
    );
    file.take(512 * 1024 + 1).read_to_string(&mut source)?;
    ensure!(
        source.len() <= 512 * 1024,
        "Custom prompt exceeds the message size limit"
    );
    Ok(parse(&source))
}

fn parse(source: &str) -> (String, String, String) {
    let normalized = source.replace("\r\n", "\n");
    let Some(rest) = normalized.strip_prefix("---\n") else {
        return (normalized, String::new(), String::new());
    };
    let Some((metadata, body)) = rest.split_once("\n---\n") else {
        return (normalized, String::new(), String::new());
    };
    let field = |name: &str| {
        metadata
            .lines()
            .find_map(|line| line.strip_prefix(&format!("{name}:")))
            .map(str::trim)
            .map(|value| {
                serde_json::from_str::<String>(value)
                    .unwrap_or_else(|_| value.trim_matches('\'').to_string())
            })
            .unwrap_or_default()
    };
    (
        body.to_string(),
        field("description"),
        field("argument-hint"),
    )
}

pub(super) fn catalog() -> crate::Result<Vec<Value>> {
    let root = directory()?;
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(root)?.take(1024) {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|v| v.to_str()) != Some("md") || !path.is_file() {
            continue;
        }
        let Some(name) = path
            .file_stem()
            .and_then(|v| v.to_str())
            .filter(|name| valid_name(name))
        else {
            continue;
        };
        if let Ok((_, description, hint)) = read(name) {
            entries.push(json!({"name":format!("prompts:{name}"),"description":description,"argument_hint":hint,"kind":"prompt"}));
        }
    }
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(entries)
}

pub(super) fn expand(name: &str, arguments: &str) -> crate::Result<String> {
    let (body, _, _) = read(name)?;
    substitute(&body, arguments)
}

fn substitute(body: &str, arguments: &str) -> crate::Result<String> {
    let words =
        shlex::split(arguments).context("Custom prompt arguments contain unmatched quotes")?;
    let named: HashMap<_, _> = words
        .iter()
        .filter_map(|word| word.split_once('='))
        .collect();
    let mut output = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        ensure!(
            output.len() <= 512 * 1024,
            "Expanded custom prompt exceeds the message size limit"
        );
        if c != '$' {
            output.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('$') => {
                chars.next();
                output.push('$');
            }
            Some(c @ '1'..='9') => {
                chars.next();
                output.push_str(
                    words
                        .get((c as u8 - b'1') as usize)
                        .map(String::as_str)
                        .unwrap_or_default(),
                );
            }
            Some('A'..='Z' | '_') => {
                let mut name = String::new();
                while chars
                    .peek()
                    .is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                {
                    name.push(chars.next().unwrap());
                }
                if name == "ARGUMENTS" {
                    output.push_str(arguments)
                } else {
                    output.push_str(
                        named
                            .get(name.as_str())
                            .with_context(|| format!("Missing custom prompt argument: {name}"))?,
                    )
                }
            }
            _ => output.push('$'),
        }
    }
    ensure!(
        output.len() <= 512 * 1024,
        "Expanded custom prompt exceeds the message size limit"
    );
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_prompt_arguments_preserve_quotes_and_do_not_expand_inserted_placeholders() {
        assert_eq!(
            substitute(
                "$FILE / $1 / $ARGUMENTS / $$ / $skill",
                "FILE='a $2.txt' second"
            )
            .unwrap(),
            "a $2.txt / FILE=a $2.txt / FILE='a $2.txt' second / $ / $skill"
        );
        assert!(substitute("$FILE", "OTHER=x").is_err());
        assert!(substitute("$1", "'unclosed").is_err());
        assert_eq!(substitute("$9", "").unwrap(), "");
        assert!(!valid_name("../foreign"));
    }
    #[test]
    fn prompt_metadata_is_removed_from_the_submitted_instructions() {
        assert_eq!(
            parse(
                "---\ndescription: \"Check changes\"\nargument-hint: FILE=<path>\n---\nReview $FILE"
            ),
            (
                "Review $FILE".into(),
                "Check changes".into(),
                "FILE=<path>".into()
            )
        );
    }
}
