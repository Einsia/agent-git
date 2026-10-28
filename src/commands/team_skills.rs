//! Skill folders as the runtimes load them, for doctor: a folder Claude Code or Codex skips
//! without saying so is named with the fix, and nothing is moved.

use std::path::{Path, PathBuf};

/// Text from the repository, made safe to print: a control character could otherwise draw a
/// line of its own inside a question or a notice, and a bidirectional override could reorder
/// what the person reads. A backslash is doubled, so an escape cannot be forged by a name.
pub(crate) fn escape(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        let hidden = c.is_control()
            || matches!(c, '\u{2028}' | '\u{2029}' | '\u{200E}' | '\u{200F}')
            || ('\u{202A}'..='\u{202E}').contains(&c)
            || ('\u{2066}'..='\u{2069}').contains(&c);
        if c == '\\' {
            shown.push_str("\\\\");
        } else if hidden && u32::from(c) < 0x100 {
            shown.push_str(&format!("\\x{:02X}", u32::from(c)));
        } else if hidden {
            shown.push_str(&format!("\\u{{{:04X}}}", u32::from(c)));
        } else {
            shown.push(c);
        }
    }
    shown
}

/// A skill folder a runtime skips without saying so.
#[derive(Debug, PartialEq)]
pub(crate) enum Unloadable {
    /// Claude Code reads `<skills>/<name>/SKILL.md` only, so skills one folder further down are
    /// invisible to it. Each skill comes with the free name it can move up to.
    Nested {
        folder: PathBuf,
        moves: Vec<(String, PathBuf)>,
    },
    /// Codex skips a SKILL.md whose frontmatter is missing or lacks `name` or `description`.
    Incomplete {
        file: PathBuf,
        missing: &'static str,
    },
    /// A link whose target is gone.
    Broken { link: PathBuf },
}

/// The folders `runtime` loads skills from for a conversation started in `cwd`: its own
/// user-level folders, then `.claude/skills` (Claude Code) or `.agents/skills` (Codex) in `cwd`
/// and each folder above it up to the git root, as both runtimes read them.
pub(crate) fn skill_dirs(runtime: &str, cwd: &Path) -> Vec<PathBuf> {
    let project = match runtime {
        "claude-code" => ".claude/skills",
        "codex" => ".agents/skills",
        _ => return Vec::new(),
    };
    // Claude Code reads the skills of the configuration folder it runs with, CLAUDE_CONFIG_DIR's
    // when that is set.
    let user = match runtime {
        "claude-code" => {
            crate::adapter::native_title::claude_user_config().map(|dir| dir.join("skills"))
        }
        _ => {
            super::setup::skill_path(runtime).and_then(|agit| agit.parent().map(Path::to_path_buf))
        }
    };
    let mut dirs: Vec<PathBuf> = user.into_iter().collect();
    if runtime == "codex"
        && let Some(home) = crate::infra::config::user_home()
    {
        dirs.push(home.join(".agents/skills"));
    }
    dirs.extend(
        crate::adapter::native_title::project_dirs(cwd)
            .into_iter()
            .map(|dir| dir.join(project)),
    );
    dirs.dedup();
    dirs
}

/// What `runtime` would skip in the skills folder `dir`.
pub(crate) fn unloadable(runtime: &str, dir: &Path) -> Vec<Unloadable> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut entries: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    entries.sort();
    let mut found = Vec::new();
    // Names the moves offered so far go to: two moves to one name would put the second skill
    // inside the first, one level too deep again.
    let mut offered = std::collections::BTreeSet::new();
    for path in entries {
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with('.'))
        {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&path) else {
            if path
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                found.push(Unloadable::Broken { link: path });
            }
            continue;
        };
        if !metadata.is_dir() {
            continue;
        }
        match runtime {
            "claude-code" if !path.join("SKILL.md").is_file() => {
                let mut skills: Vec<String> = std::fs::read_dir(&path)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|inner| inner.path().join("SKILL.md").is_file())
                    .map(|inner| inner.file_name().to_string_lossy().into_owned())
                    .collect();
                skills.sort();
                if skills.is_empty() {
                    continue;
                }
                let prefix = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let moves = skills
                    .into_iter()
                    .map(|skill| {
                        // A name already taken would make `mv` put the skill inside that folder,
                        // so the first free name is offered instead.
                        let target = std::iter::once(format!("{prefix}-{skill}"))
                            .chain((2..).map(|n| format!("{prefix}-{skill}-{n}")))
                            .map(|name| dir.join(name))
                            .find(|target| {
                                !offered.contains(target)
                                    && std::fs::symlink_metadata(target).is_err()
                            })
                            .expect("an unbounded sequence of names has a free one");
                        offered.insert(target.clone());
                        (skill, target)
                    })
                    .collect();
                found.push(Unloadable::Nested {
                    folder: path,
                    moves,
                });
            }
            "codex" => incomplete(&path, 0, &mut Default::default(), &mut found),
            _ => {}
        }
    }
    found
}

/// The problem and its fix, for doctor: `indent` starts each line after the first.
pub(crate) fn describe(problem: &Unloadable, indent: &str) -> String {
    match problem {
        Unloadable::Nested { folder, moves } => {
            let names = moves
                .iter()
                .map(|(skill, _)| escape(skill))
                .collect::<Vec<_>>()
                .join(", ");
            let mut text = if moves.len() == 1 {
                format!(
                    "{} holds a skill one folder too deep, so Claude Code does not load it: {names}. To fix, move it up one level:",
                    shown(folder)
                )
            } else {
                format!(
                    "{} holds {} skills one folder too deep, so Claude Code loads none of them: {names}. To fix, move each up one level:",
                    shown(folder),
                    moves.len()
                )
            };
            for (skill, target) in moves {
                text.push_str(&format!(
                    "\n{indent}{}",
                    move_command(&folder.join(skill), target)
                ));
            }
            text
        }
        Unloadable::Incomplete { file, missing } => {
            let file = shown(file);
            if *missing == "frontmatter" {
                format!(
                    "Codex skips {file}: it has no frontmatter. To fix, start it with a frontmatter block holding name and description"
                )
            } else {
                format!(
                    "Codex skips {file}: its frontmatter has no {missing}. To fix, add `{missing}:` to the frontmatter at its top"
                )
            }
        }
        Unloadable::Broken { link } => format!(
            "{} is a link to {}, which is gone. To fix, remove the link or restore what it points at",
            shown(link),
            std::fs::read_link(link)
                .map(|target| shown(&target))
                .unwrap_or_else(|_| "nothing".to_owned())
        ),
    }
}

/// A path as doctor prints it: shortened to `~` and escaped like every name from disk.
fn shown(path: &Path) -> String {
    escape(&crate::ui::tilde(path))
}

/// A command that moves `from` to `to`, for the shell the person most likely has. A path that
/// cannot be printed as it is would not survive a copy, so it gets a plain instruction instead.
fn move_command(from: &Path, to: &Path) -> String {
    let printable = |path: &Path| escape(&path.to_string_lossy()) == path.to_string_lossy();
    if !printable(from) || !printable(to) {
        return format!(
            "move {} to {} yourself; its name holds characters a terminal cannot show",
            shown(from),
            shown(to)
        );
    }
    let quote = |path: &Path| {
        let short = crate::ui::tilde(path);
        if cfg!(windows) {
            format!("'{}'", path.to_string_lossy().replace('\'', "''"))
        } else if short
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '~'))
        {
            short
        } else {
            crate::ui::quote_posix_argument(&path.to_string_lossy())
        }
    };
    if cfg!(windows) {
        format!(
            "Move-Item -LiteralPath {} -Destination {}",
            quote(from),
            quote(to)
        )
    } else {
        format!("mv {} {}", quote(from), quote(to))
    }
}

/// Every SKILL.md under `dir` whose frontmatter Codex would reject. Codex follows a linked skill
/// folder wherever it sits, so the walk does too, once per real folder.
fn incomplete(
    dir: &Path,
    depth: usize,
    seen: &mut std::collections::HashSet<PathBuf>,
    found: &mut Vec<Unloadable>,
) {
    const DEPTH: usize = 6;
    if !dir.canonicalize().is_ok_and(|real| seen.insert(real)) {
        return;
    }
    let file = dir.join("SKILL.md");
    if let Ok(text) = std::fs::read_to_string(&file) {
        let missing = match frontmatter(&text) {
            None => Some("frontmatter"),
            Some(keys) if !keys.contains(&"name") => Some("name"),
            Some(keys) if !keys.contains(&"description") => Some("description"),
            Some(_) => None,
        };
        if let Some(missing) = missing {
            found.push(Unloadable::Incomplete { file, missing });
        }
    }
    if depth == DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut inner: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| std::fs::metadata(path).is_ok_and(|kind| kind.is_dir()))
        .collect();
    inner.sort();
    for path in inner {
        incomplete(&path, depth + 1, seen, found);
    }
}

/// The top-level keys of a SKILL.md's YAML frontmatter that hold a value, or `None` when it has
/// no frontmatter. A key with nothing after it but a comment holds one only when indented lines
/// follow (a block or folded string); a quoted string of only blanks, `~` and `null` are empty,
/// which Codex rejects for a required field.
fn frontmatter(text: &str) -> Option<Vec<&str>> {
    let body = text.strip_prefix("---")?;
    let body = body
        .strip_prefix('\n')
        .or_else(|| body.strip_prefix("\r\n"))?;
    let end = body.find("\n---")?;
    let lines: Vec<&str> = body[..end].lines().collect();
    let mut keys = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.starts_with([' ', '\t', '#', '-']) {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let raw = value.trim();
        // A quoted string is its contents; an unquoted value ends where a comment begins.
        let quoted = raw.chars().next().filter(|c| matches!(c, '"' | '\''));
        let filled = match quoted {
            Some(quote) => {
                let rest = std::iter::once(&raw[1..])
                    .chain(lines[index + 1..].iter().copied())
                    .collect::<Vec<_>>()
                    .join("\n");
                !quoted_scalar(&rest, quote).trim().is_empty()
            }
            None => {
                let plain = if raw.starts_with('#') {
                    ""
                } else {
                    raw.split(" #").next().unwrap_or_default().trim()
                };
                match plain {
                    "" | "|" | ">" | "|-" | ">-" | "|+" | ">+" => lines[index + 1..]
                        .iter()
                        .take_while(|next| next.starts_with([' ', '\t']) || next.trim().is_empty())
                        .any(|next| !next.trim().is_empty()),
                    plain => !matches!(plain, "~" | "null" | "Null" | "NULL"),
                }
            }
        };
        if filled {
            keys.push(key.trim());
        }
    }
    Some(keys)
}

/// The value of a quoted YAML scalar whose text, after the opening quote, starts `rest`: it runs
/// to the closing quote, across lines, with a double-quoted string's escapes decoded and a
/// single-quoted string's doubled quote read as one. Line breaks inside fold to spaces.
fn quoted_scalar(rest: &str, quote: char) -> String {
    let mut value = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' if quote == '\'' => {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    value.push('\'');
                } else {
                    break;
                }
            }
            '"' if quote == '"' => break,
            '\\' if quote == '"' => {
                let hex = |chars: &mut std::iter::Peekable<std::str::Chars>, n: usize| {
                    let digits: String = chars.by_ref().take(n).collect();
                    u32::from_str_radix(&digits, 16)
                        .ok()
                        .and_then(char::from_u32)
                };
                match chars.next() {
                    Some('n') => value.push('\n'),
                    Some('t' | '\t') => value.push('\t'),
                    Some('r') => value.push('\r'),
                    Some('v') => value.push('\u{b}'),
                    Some('f') => value.push('\u{c}'),
                    Some('a') => value.push('\u{7}'),
                    Some('b') => value.push('\u{8}'),
                    Some('e') => value.push('\u{1b}'),
                    Some('0') => value.push('\0'),
                    Some(' ') => value.push(' '),
                    Some('N') => value.push('\u{85}'),
                    Some('_') => value.push('\u{a0}'),
                    Some('L') => value.push('\u{2028}'),
                    Some('P') => value.push('\u{2029}'),
                    Some('x') => value.extend(hex(&mut chars, 2)),
                    Some('u') => value.extend(hex(&mut chars, 4)),
                    Some('U') => value.extend(hex(&mut chars, 8)),
                    // An escaped line break joins the lines without a space.
                    Some('\n') => {}
                    // `\"`, `\\` and `\/` stand for themselves.
                    Some(other) => value.push(other),
                    None => break,
                }
            }
            '\n' => value.push(' '),
            c => value.push(c),
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Doctor names exactly the folders a runtime skips: skills one folder too deep for Claude
    /// Code (the reported `~/.claude/skills/agentgit/<name>/` layout), a SKILL.md Codex rejects
    /// (no frontmatter, a missing or empty required field, also behind a linked folder Codex
    /// follows), and a dangling link, while a working layout, a folded description included,
    /// produces nothing. The fix it prints moves each nested skill up under a free name that keeps
    /// the folder's prefix; a name taken on disk, or by another move the same fix prints, would
    /// nest it again.
    #[test]
    fn doctor_names_the_skill_folders_a_runtime_skips() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path();
        let write = |path: &str, text: &str| {
            let path = skills.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        let good = "---\nname: ok\ndescription: works\n---\nbody\n";
        write("ok/SKILL.md", good);
        write("agentgit/deploy/SKILL.md", good);
        write("agentgit/review/SKILL.md", good);
        write("agentgit/review-2/SKILL.md", good);
        write("plain/SKILL.md", "no frontmatter\n");
        write("half/SKILL.md", "---\nname: half\n---\nbody\n");
        write(
            "empty/SKILL.md",
            "---\nname: empty\ndescription:\n---\nbody\n",
        );
        for (folder, value) in [
            ("todo", "# TODO"),
            ("nulled", "null # TODO"),
            ("blank", "\"   \""),
        ] {
            write(
                &format!("{folder}/SKILL.md"),
                &format!("---\nname: {folder}\ndescription: {value}\n---\nbody\n"),
            );
        }
        write(
            "spread/SKILL.md",
            "---\nname: spread\ndescription: \"\n  Run deployment checks\"\n---\nbody\n",
        );
        write(
            "escaped/SKILL.md",
            "---\nname: escaped\ndescription: \"\\n\\t\\v\\f\\r\\ \\N\\_\\L\\P\"\n---\nbody\n",
        );
        write(
            "commented/SKILL.md",
            "---\nname: commented\ndescription: Fix issue #12 # who wrote this\n---\nbody\n",
        );
        write(
            "folded/SKILL.md",
            "---\nname: folded\ndescription: >\n  spans\n  lines\n---\nbody\n",
        );
        // A name already taken: moving into it would nest the skill again.
        std::fs::create_dir_all(skills.join("agentgit-review")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(skills.join("gone"), skills.join("dangling")).unwrap();
            // A team's skill linked into a grouping folder, which Codex follows.
            let shared = tempfile::tempdir().unwrap();
            std::fs::write(shared.path().join("SKILL.md"), "no frontmatter\n").unwrap();
            std::fs::create_dir_all(skills.join("team")).unwrap();
            std::os::unix::fs::symlink(shared.path(), skills.join("team/linked")).unwrap();
            let codex = unloadable("codex", skills);
            assert!(
                codex.contains(&Unloadable::Incomplete {
                    file: skills.join("team/linked/SKILL.md"),
                    missing: "frontmatter"
                }),
                "{codex:?}"
            );
        }

        let claude = unloadable("claude-code", skills);
        assert!(claude.contains(&Unloadable::Nested {
            folder: skills.join("agentgit"),
            moves: vec![
                ("deploy".into(), skills.join("agentgit-deploy")),
                ("review".into(), skills.join("agentgit-review-2")),
                ("review-2".into(), skills.join("agentgit-review-2-2")),
            ],
        }));
        let fix = describe(
            &claude[claude
                .iter()
                .position(|p| matches!(p, Unloadable::Nested { .. }))
                .unwrap()],
            "  ",
        );
        assert!(fix.contains("holds 3 skills one folder too deep"), "{fix}");
        #[cfg(unix)]
        assert!(
            fix.contains(&format!(
                "\n  mv {} {}",
                skills.join("agentgit/deploy").display(),
                skills.join("agentgit-deploy").display()
            )) && fix.contains(&format!(
                "\n  mv {} {}",
                skills.join("agentgit/review").display(),
                skills.join("agentgit-review-2").display()
            )),
            "{fix}"
        );

        // A name with an apostrophe is quoted for a POSIX shell, and one that holds a terminal
        // control sequence is printed escaped, with no command a copy would mangle.
        #[cfg(unix)]
        {
            write("quoted/deploy's/SKILL.md", good);
            write("bad\u{1b}[2Jname/inner/SKILL.md", good);
            let claude = unloadable("claude-code", skills);
            let described = |folder: &str| {
                let problem = claude
                    .iter()
                    .find(|p| matches!(p, Unloadable::Nested { folder: f, .. } if f == &skills.join(folder)))
                    .unwrap_or_else(|| panic!("{folder}: {claude:?}"));
                describe(problem, "  ")
            };
            let fix = described("quoted");
            let quoted =
                |path: std::path::PathBuf| crate::ui::quote_posix_argument(&path.to_string_lossy());
            assert!(
                fix.contains(&format!(
                    "mv {} {}",
                    quoted(skills.join("quoted/deploy's")),
                    quoted(skills.join("quoted-deploy's"))
                )),
                "{fix}"
            );
            let fix = described("bad\u{1b}[2Jname");
            assert!(!fix.contains('\u{1b}'), "{fix:?}");
            assert!(fix.contains("bad\\x1B[2Jname"), "{fix:?}");
            assert!(
                fix.contains("yourself; its name holds characters"),
                "{fix:?}"
            );
        }

        let codex = unloadable("codex", skills);
        assert!(codex.contains(&Unloadable::Incomplete {
            file: skills.join("plain/SKILL.md"),
            missing: "frontmatter"
        }));
        assert!(codex.contains(&Unloadable::Incomplete {
            file: skills.join("half/SKILL.md"),
            missing: "description"
        }));
        for folder in ["empty", "todo", "nulled", "blank", "escaped"] {
            assert!(
                codex.contains(&Unloadable::Incomplete {
                    file: skills.join(format!("{folder}/SKILL.md")),
                    missing: "description"
                }),
                "{folder}: {codex:?}"
            );
        }
        assert!(!codex.iter().any(|p| format!("{p:?}").contains("commented")));
        assert!(!codex.iter().any(|p| format!("{p:?}").contains("spread")));
        assert!(!codex.iter().any(|p| format!("{p:?}").contains("folded")));
        assert!(!codex.iter().any(|p| matches!(p, Unloadable::Nested { .. })));
        #[cfg(unix)]
        assert!(codex.contains(&Unloadable::Broken {
            link: skills.join("dangling")
        }));
        assert!(
            !claude
                .iter()
                .chain(&codex)
                .any(|p| format!("{p:?}").contains("\"ok"))
        );
    }
}
