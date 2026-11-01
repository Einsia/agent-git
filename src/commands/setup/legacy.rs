use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Only standalone, versioned blocks outside Markdown fences belong to the installer.
/// Nested or incomplete markers cannot establish a safe deletion boundary.
pub(super) fn without_versioned_blocks(text: &str, begin: &str, end: &str) -> Option<String> {
    without_blocks(text, begin, end, true)
}

/// The short sections `setup --agents-md` writes: standalone blocks outside Markdown fences that
/// carry no skill version, under the same boundary rules as [`without_versioned_blocks`]. Agit's
/// own [`LEGACY_SECTION`] goes as well, also when the setup that wrote it nested its markers.
pub(super) fn without_sections(text: &str, begin: &str, end: &str) -> Option<String> {
    without_blocks(text, begin, end, false)
}

/// The section `agit setup` wrote into the folder it ran in from 0.8.0 (08-08) until 09-10,
/// without its markers. It told agents to run `agit status` at the start of every session, so an
/// agent answering a plain greeting ran agit first (ledger 150). The 0.8.0 setup wrapped it in two
/// pairs of markers, which [`without_sections`] otherwise refuses as having no safe boundary, and
/// each later run of that setup left one more end marker after it; text that matches this exactly
/// is agit's own, so its boundary is known.
pub(super) const LEGACY_SECTION: &str = "## Session version control (agit)\n\nThis project's agent sessions are managed by agit. Rules:\n\n- Settle when a phase completes: `agit commit --milestone \"<summary>\"` (add `--code` when relevant).\n- `agit status` at session start; if you were resumed as a merge agent, follow the `AGIT_MERGE_TX` protocol (see the agit skill).\n- Never rebase / force-push; remove context with `agit revert @#n.k`.";

/// Whether `text` holds agit's [`LEGACY_SECTION`] outside a Markdown fence.
pub(super) fn has_legacy_section(text: &str, begin: &str, end: &str) -> bool {
    blocks(text, begin, end, false)
        .iter()
        .any(|(_, legacy)| *legacy)
}

fn without_blocks(text: &str, begin: &str, end: &str, versioned_wanted: bool) -> Option<String> {
    let ranges = blocks(text, begin, end, versioned_wanted);
    if ranges.is_empty() {
        return None;
    }
    let mut result = String::with_capacity(text.len());
    let mut kept = 0;
    for (range, _) in ranges {
        result.push_str(&text[kept..range.start]);
        kept = range.end;
    }
    result.push_str(&text[kept..]);
    Some(result)
}

/// The blocks [`without_blocks`] removes, each with whether it is the [`LEGACY_SECTION`].
fn blocks(
    text: &str,
    begin: &str,
    end: &str,
    versioned_wanted: bool,
) -> Vec<(std::ops::Range<usize>, bool)> {
    let mut offset = 0;
    let mut start = None;
    let mut depth = 0;
    let mut nested = false;
    let mut versioned = false;
    let mut selected = false;
    let mut body = String::new();
    // Right after a legacy section, the stray end markers its setup added belong to it.
    let mut absorbing = false;
    let mut fence: Option<(char, usize)> = None;
    let mut ranges: Vec<(std::ops::Range<usize>, bool)> = Vec::new();
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
                // Fenced lines inside a block are part of its text, so a code block someone added
                // inside an old section keeps the block from reading as agit's own.
                if depth > 0 {
                    body.push_str(content);
                    body.push('\n');
                }
                absorbing = false;
                offset += line.len();
                continue;
            }
        }
        if fence.is_none() && depth == 0 && absorbing && content == end {
            if let Some((range, _)) = ranges.last_mut() {
                range.end = offset + line.len();
            }
            offset += line.len();
            continue;
        }
        absorbing = false;
        if fence.is_none() {
            if matches!(content, "<!-- agit:begin -->" | "<!-- agit:skill-begin -->") {
                if depth == 0 {
                    start = Some(offset);
                    nested = false;
                    versioned = false;
                    selected = content == begin;
                    body.clear();
                } else {
                    nested = true;
                }
                depth += 1;
            } else if matches!(content, "<!-- agit:end -->" | "<!-- agit:skill-end -->")
                && depth > 0
            {
                depth -= 1;
                let legacy = !versioned_wanted && !versioned && body.trim() == LEGACY_SECTION;
                if depth == 0
                    && selected
                    && content == end
                    && versioned == versioned_wanted
                    && (!nested || legacy)
                {
                    ranges.push((start.take().unwrap()..offset + line.len(), legacy));
                    absorbing = legacy;
                }
            } else if depth > 0
                && content
                    .strip_prefix("<!-- agit:skill-version:")
                    .and_then(|value| value.strip_suffix(" -->"))
                    .is_some_and(|value| !value.trim().is_empty())
            {
                versioned = true;
            } else if depth > 0 {
                body.push_str(content);
                body.push('\n');
            }
        } else if depth > 0 {
            body.push_str(content);
            body.push('\n');
        }
        offset += line.len();
    }
    ranges
}

pub(super) fn remove(path: &Path, begin: &str, end: &str) -> io::Result<Option<PathBuf>> {
    remove_with(path, |text| without_versioned_blocks(text, begin, end))
}

/// [`remove`] for the short sections ([`without_sections`]).
pub(super) fn remove_sections(path: &Path, begin: &str, end: &str) -> io::Result<Option<PathBuf>> {
    remove_with(path, |text| without_sections(text, begin, end))
}

/// Rewrites the file without what `select` drops, keeping a backup of the original next to it.
fn remove_with(
    path: &Path,
    select: impl Fn(&str) -> Option<String>,
) -> io::Result<Option<PathBuf>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let existing = std::fs::read_to_string(path)?;
    let Some(updated) = select(&existing) else {
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

    /// The section `--agents-md` writes is the unversioned block. Removing it keeps the user's own
    /// text, a versioned skill block (removed only once a native skill has replaced it) and an
    /// example inside a fence.
    #[test]
    fn removing_a_section_keeps_everything_else() {
        let (begin, end) = ("<!-- agit:begin -->", "<!-- agit:end -->");
        let section = format!("{begin}\n## Session version control (agit)\nrules\n{end}\n");
        let versioned = format!("{begin}\n<!-- agit:skill-version:old -->\nold\n{end}\n");
        let fenced = format!("```md\n{section}```\n");
        let text = format!("mine\n{section}{versioned}{fenced}");
        assert_eq!(
            without_sections(&text, begin, end),
            Some(format!("mine\n{versioned}{fenced}"))
        );
        assert_eq!(
            without_sections(&format!("mine\n{versioned}"), begin, end),
            None
        );
    }

    /// The section agit's setup wrote from 0.8.0 until 09-10 told agents to run `agit status` at
    /// the start of every session. It goes in each shape a setup of that time left: the 0.8.0
    /// double markers, the stray end marker each later 0.8.0 run added, and the single pair from
    /// 08-28, also with CRLF line ends. The user's text stays. A nested block whose text is not
    /// agit's own, the same text inside a fence, and today's section are not legacy; only today's
    /// section is removed as a section. A removal that kept the nested rule would leave the old
    /// instruction in the home folder after every upgrade.
    #[test]
    fn the_old_session_start_section_goes_in_every_shape_agit_wrote() {
        let (begin, end) = ("<!-- agit:begin -->", "<!-- agit:end -->");
        let body = LEGACY_SECTION;
        let shapes = [
            format!("{begin}\n{begin}\n{body}\n{end}\n{end}\n"),
            format!("{begin}\n{begin}\n{body}\n{end}\n{end}\n{end}\n{end}\n"),
            format!("{begin}\n{body}\n{end}\n"),
            format!("{begin}\n{begin}\n{body}\n{end}\n{end}\n").replace('\n', "\r\n"),
        ];
        for shape in &shapes {
            let text = format!("User rules\n\n{shape}after\n");
            assert!(has_legacy_section(&text, begin, end), "{text}");
            assert_eq!(
                without_sections(&text, begin, end),
                Some("User rules\n\nafter\n".to_owned()),
                "{text}"
            );
        }

        let other = format!("{begin}\n{begin}\nProject rules\n{end}\n{end}\n");
        let fenced = format!("```md\n{}```\n", shapes[0]);
        // Someone's code block inside the old section makes it theirs: a match that skipped
        // fenced lines would delete the code block with the section.
        let added = |fence: &str| {
            format!("{begin}\n{begin}\n{body}\n{fence}sh\necho keep\n{fence}\n{end}\n{end}\n")
        };
        let (backticks, tildes) = (added("```"), added("~~~"));
        for kept in [&other, &fenced, &backticks, &tildes] {
            assert!(!has_legacy_section(kept, begin, end), "{kept}");
            assert_eq!(without_sections(kept, begin, end), None, "{kept}");
        }
        let today = format!("{begin}\n## Session version control (agit)\nrules\n{end}\n");
        assert!(!has_legacy_section(&today, begin, end));
        assert_eq!(without_sections(&today, begin, end), Some(String::new()));
        // A stray end marker is taken only right after a legacy section.
        let stray = format!("{begin}\n{body}\n{end}\nmine\n{end}\n");
        assert_eq!(
            without_sections(&stray, begin, end),
            Some(format!("mine\n{end}\n"))
        );
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
