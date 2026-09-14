use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Only standalone, versioned blocks outside Markdown fences belong to the installer.
/// Nested or incomplete markers cannot establish a safe deletion boundary.
pub(super) fn without_versioned_blocks(text: &str, begin: &str, end: &str) -> Option<String> {
    let mut offset = 0;
    let mut start = None;
    let mut depth = 0;
    let mut nested = false;
    let mut versioned = false;
    let mut selected = false;
    let mut fence: Option<(char, usize)> = None;
    let mut ranges = Vec::new();
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\r', '\n']);
        let trimmed = content.trim_start_matches(' ');
        let fence_char = trimmed
            .chars()
            .next()
            .filter(|c| content.len() - trimmed.len() <= 3 && matches!(c, '`' | '~'));
        if let Some(ch) = fence_char {
            let count = trimmed.chars().take_while(|c| *c == ch).count();
            if count >= 3 {
                match fence {
                    Some((open, width))
                        if open == ch
                            && count >= width
                            && trimmed[count..].trim_matches([' ', '\t']).is_empty() =>
                    {
                        fence = None
                    }
                    None if ch != '`' || !trimmed[count..].contains('`') => {
                        fence = Some((ch, count))
                    }
                    _ => {}
                }
                offset += line.len();
                continue;
            }
        }
        if fence.is_none() {
            if matches!(content, "<!-- agit:begin -->" | "<!-- agit:skill-begin -->") {
                if depth == 0 {
                    start = Some(offset);
                    nested = false;
                    versioned = false;
                    selected = content == begin;
                } else {
                    nested = true;
                }
                depth += 1;
            } else if matches!(content, "<!-- agit:end -->" | "<!-- agit:skill-end -->")
                && depth > 0
            {
                depth -= 1;
                if depth == 0 && selected && content == end && versioned && !nested {
                    ranges.push(start.take().unwrap()..offset + line.len());
                }
            } else if depth > 0
                && content
                    .strip_prefix("<!-- agit:skill-version:")
                    .and_then(|value| value.strip_suffix(" -->"))
                    .is_some_and(|value| !value.trim().is_empty())
            {
                versioned = true;
            }
        }
        offset += line.len();
    }
    if ranges.is_empty() {
        return None;
    }
    let mut result = String::with_capacity(text.len());
    let mut kept = 0;
    for range in ranges {
        result.push_str(&text[kept..range.start]);
        kept = range.end;
    }
    result.push_str(&text[kept..]);
    Some(result)
}

pub(super) fn remove(path: &Path, begin: &str, end: &str) -> io::Result<Option<PathBuf>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let existing = std::fs::read_to_string(path)?;
    let Some(updated) = without_versioned_blocks(&existing, begin, end) else {
        return Ok(None);
    };
    if !metadata.file_type().is_file() {
        return Err(io::Error::other("instruction path is not a regular file"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent directory"))?;
    let filename = path
        .file_name()
        .ok_or_else(|| io::Error::other("missing filename"))?;
    let mut backup_name = filename.to_os_string();
    backup_name.push(format!(
        ".agit-backup-{}",
        hex::encode(Sha256::digest(existing.as_bytes()))
    ));
    let backup = parent.join(backup_name);
    let mut saved = tempfile::NamedTempFile::new_in(parent)?;
    saved.as_file().set_permissions(metadata.permissions())?;
    saved.write_all(existing.as_bytes())?;
    saved.as_file().sync_all()?;
    if let Err(error) = saved.persist_noclobber(&backup)
        && (error.error.kind() != io::ErrorKind::AlreadyExists
            || std::fs::read(&backup)? != existing.as_bytes())
    {
        return Err(error.error);
    }
    let mut replacement = tempfile::NamedTempFile::new_in(parent)?;
    replacement
        .as_file()
        .set_permissions(metadata.permissions())?;
    replacement.write_all(updated.as_bytes())?;
    replacement.as_file().sync_all()?;
    if std::fs::read(path)? != existing.as_bytes() {
        return Err(io::Error::other(
            "instruction file changed during migration",
        ));
    }
    replacement.persist(path).map_err(|error| error.error)?;
    Ok(Some(backup))
}

#[cfg(test)]
mod tests {
    use super::*;
    const BEGIN: &str = "<!-- agit:skill-begin -->";
    const END: &str = "<!-- agit:skill-end -->";

    fn block(newline: &str) -> String {
        [BEGIN, "<!-- agit:skill-version:old -->", "old", END, ""].join(newline)
    }

    #[test]
    fn removal_preserves_surrounding_bytes_and_backs_up_original_once() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("AGENTS.md");
        let original = format!("before\r\n{}between\n{}after", block("\r\n"), block("\n"));
        std::fs::write(&path, &original).unwrap();
        let backup = remove(&path, BEGIN, END).unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "before\r\nbetween\nafter"
        );
        assert_eq!(std::fs::read_to_string(backup).unwrap(), original);
        assert_eq!(remove(&path, BEGIN, END).unwrap(), None);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 2);
    }

    #[test]
    fn examples_unversioned_and_ambiguous_blocks_are_preserved() {
        for original in [
            format!("```md\n{}```\n", block("\n")),
            format!("~~~md\n{}~~~\n", block("\n")),
            format!("{BEGIN}\nProject rules\n{END}\n"),
            format!("{BEGIN}\n<!-- agit:skill-version:old -->\nunterminated\n"),
            format!("{BEGIN}\n{}{END}\n", block("\n")),
            format!("<!-- agit:begin -->\n{}<!-- agit:end -->\n", block("\n")),
            format!("Example: {}", block("\n")),
        ] {
            assert_eq!(
                without_versioned_blocks(&original, BEGIN, END),
                None,
                "{original}"
            );
        }
    }

    #[test]
    fn unversioned_block_does_not_hide_a_later_owned_block() {
        let kept = format!("{BEGIN}\nProject rules\n{END}\n");
        assert_eq!(
            without_versioned_blocks(&format!("{kept}{}", block("\n")), BEGIN, END),
            Some(kept)
        );
    }

    #[test]
    fn fence_content_cannot_close_the_surrounding_example() {
        for marker in ["```", "~~~"] {
            for indent in ["    ", "\t", " \t", "\u{00a0}"] {
                let original = format!("{marker}text\n{indent}{marker}\n{}{marker}\n", block("\n"));
                assert_eq!(without_versioned_blocks(&original, BEGIN, END), None);
            }
            let original = format!("{marker}text\n{marker}\u{00a0}\n{}{marker}\n", block("\n"));
            assert_eq!(without_versioned_blocks(&original, BEGIN, END), None);
        }
        for (open, content) in [
            ("````", "```"),
            ("```", "~~~"),
            ("~~~", "```"),
            ("```", "```text"),
        ] {
            let original = format!("{open}\n{content}\n{}{open}\n", block("\n"));
            assert_eq!(without_versioned_blocks(&original, BEGIN, END), None);
        }
    }

    #[test]
    fn valid_indented_fences_preserve_examples_without_hiding_owned_blocks() {
        for marker in ["```", "~~~"] {
            for indent in ["", " ", "  ", "   "] {
                let example = format!("{indent}{marker}text\n{}{indent}{marker} \t\n", block("\n"));
                assert_eq!(
                    without_versioned_blocks(&format!("{example}{}", block("\n")), BEGIN, END),
                    Some(example)
                );
            }
        }
    }

    #[test]
    fn inline_backticks_cannot_invert_the_fences_of_a_later_example() {
        for prefix in ["```literal```", "```lang`", "   ````text `code`"] {
            let example = format!("{prefix}\n\n```\n{}```\n", block("\n"));
            assert_eq!(without_versioned_blocks(&example, BEGIN, END), None);
            assert_eq!(
                without_versioned_blocks(&format!("{example}{}", block("\n")), BEGIN, END),
                Some(example)
            );
        }
        let example = format!("~~~ `code`\n{}~~~\n", block("\n"));
        assert_eq!(without_versioned_blocks(&example, BEGIN, END), None);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_are_not_rewritten() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.md");
        let path = root.path().join("AGENTS.md");
        let original = block("\n");
        std::fs::write(&target, &original).unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(remove(&path, BEGIN, END).is_err());
        assert_eq!(std::fs::read_to_string(target).unwrap(), original);
    }
}
