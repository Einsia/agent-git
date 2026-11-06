//! File-only pulls retain a manual transaction only when reconciliation needs a person.

use super::*;
use std::io::IsTerminal as _;

/// How many conflicting places of one file are shown, and how many lines of each side.
const SHOWN_HUNKS: usize = 5;
const SHOWN_LINES: usize = 3;

pub(in crate::commands) fn reconcile(
    repo: &Repo,
    store: &crate::domain::store::Store,
    slug: &str,
    branch: &str,
    target: &str,
    source: &str,
) -> CmdResult {
    #[cfg(windows)]
    use crate::ui::quote_powershell_argument as quote_arg;
    #[cfg(not(windows))]
    use crate::ui::session::shell_arg as quote_arg;
    let target_ref = format!("{slug}@{branch}");
    let quoted_target = quote_arg(&target_ref);
    let guard = crate::domain::link::lock_branch(store, slug, branch)?;
    anyhow::ensure!(
        repo.git(&["rev-parse", &format!("refs/heads/{branch}")])? == target,
        "the pull target changed; run `agit pull {quoted_target}` again"
    );
    anyhow::ensure!(
        meta::is_file_line_at(repo, target) && meta::is_file_line_at(repo, source),
        "automatic reconciliation requires file branches on both sides"
    );
    if let Some(open) = mergetx::read(repo.root())? {
        let selected = quote_arg(&format!("{slug}@{}", open.target));
        anyhow::bail!(
            "a merge transaction is already open; inspect it with `agit merge --into {selected} --status`"
        );
    }
    let base = repo.git(&["merge-base", "--all", target, source]).context(
        "file branches have no available common ancestor; reconcile their histories explicitly",
    )?;
    anyhow::ensure!(
        !base.is_empty() && !base.contains('\n'),
        "file branches require a unique common ancestor for automatic pull reconciliation"
    );
    super::super::resume::require_merge_claims(repo, store, slug, branch, target, false)?;
    let checkout = super::super::worktree::checkout(repo, branch)?;
    let dirty =
        checkout.git_bytes_result(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
    anyhow::ensure!(
        dirty.is_empty(),
        "file checkout has uncommitted or staged changes; commit or save them before `agit pull {quoted_target}`"
    );
    super::super::plumbing::ensure_v1_upgrade_preflight(&checkout, target)?;
    super::super::plumbing::ensure_v1_namespace_available_at(repo, source)?;
    let conflicts = file_reconciliation::conflicts(repo, target, source)?;
    let tx = Tx {
        mode: Some(mergetx::Mode::Manual),
        exploration: None,
        generation: Some(uuid::Uuid::now_v7().to_string()),
        target: branch.into(),
        source: format!("{slug}@{source}"),
        source_repo: Some(slug.into()),
        source_branch: None,
        base,
        target_head: target.into(),
        source_head: source.into(),
        picked: vec![],
        summary: Some("Merge remote file changes".into()),
    };
    mergetx::create(repo.root(), &tx)?;
    drop(guard);
    if conflicts.is_empty() {
        return continue_tx(checkout.root(), Some(&target_ref), &[], &Choices::default()).with_context(|| format!(
            "file merge did not finish cleanly; inspect it with `agit merge --into {quoted_target} --status` or cancel its open transaction with `agit merge --into {quoted_target} --abort`"
        ));
    }
    ui::error("file changes conflict; the target branch has not moved");
    // The conflicting lines are someone's memory: shown to the person at a terminal only, never to
    // an agent's tool output or a JSON document, which are captured and can be published.
    let shown = std::io::stderr().is_terminal() && !crate::commands::json::is_capturing();
    for path in &conflicts {
        eprintln!("  {}", quote_arg(path));
        if shown {
            for line in describe(repo, &tx.base, target, source, path) {
                eprintln!("    {line}");
            }
        }
    }
    if !shown {
        ui::hint(
            "run the same pull in a terminal to see the conflicting lines and who made each side",
        );
    }
    let target_ref = quoted_target;
    for path in &conflicts {
        let path = quote_arg(path);
        ui::hint(&format!(
            "keep yours: `agit merge --into {target_ref} --continue --keep-mine {path}`"
        ));
        ui::hint(&format!(
            "take theirs: `agit merge --into {target_ref} --continue --take-theirs {path}`"
        ));
        ui::hint(&format!(
            "merge by hand: `agit merge --into {target_ref} --edit {path}`, then `agit merge --into {target_ref} --continue --resolved {path}`"
        ));
    }
    if conflicts.len() > 1 {
        ui::hint(
            "give every file its side in the same `--continue` (repeat `--keep-mine` / `--take-theirs` / `--resolved`)",
        );
    }
    ui::hint(&format!(
        "cancel: `agit merge --into {target_ref} --abort` (checkout edits are kept)"
    ));
    Ok(ExitCode::Precondition)
}

/// The conflicting places of `path` between the pull's two sides, each as its line range in yours,
/// the lines on each side, and who last changed them. Every line is made inert and cut. Anything
/// that can't be read (a binary file, a side that deleted the file) is named as such.
fn describe(repo: &Repo, base: &str, target: &str, source: &str, path: &str) -> Vec<String> {
    let blob = |rev: &str| {
        repo.git_bytes_result(&["show", &format!("{rev}:{path}")])
            .ok()
    };
    let (Some(mine), Some(theirs)) = (blob(target), blob(source)) else {
        return vec!["one side deleted this file".into()];
    };
    if [&mine, &theirs]
        .iter()
        .any(|bytes| std::str::from_utf8(bytes).is_err())
    {
        return vec!["not text; compare the two sides with `agit file get`".into()];
    }
    let base = blob(base).unwrap_or_default();
    let (Ok(yours), Ok(their_hunks)) = (
        crate::commands::memory::hunks(repo, &base, &mine),
        crate::commands::memory::hunks(repo, &base, &theirs),
    ) else {
        return vec![];
    };
    let mut out = Vec::new();
    let clashing: Vec<_> = yours
        .iter()
        .filter_map(|mine| {
            their_hunks
                .iter()
                .find(|other| mine.clashes(other))
                .map(|other| (mine, other))
        })
        .collect();
    for (mine, other) in clashing.iter().take(SHOWN_HUNKS) {
        let end = mine.new_at + mine.lines.len().saturating_sub(1);
        out.push(if mine.lines.len() > 1 {
            format!("lines {}–{end}", mine.new_at)
        } else {
            format!("line {}", mine.new_at)
        });
        for (label, hunk, head) in [("yours ", *mine, target), ("theirs", *other, source)] {
            let who = last_change(repo, head, path, hunk);
            if hunk.lines.is_empty() {
                out.push(format!("  {label}: (deleted){who}"));
            }
            for (index, line) in hunk.lines.iter().take(SHOWN_LINES).enumerate() {
                let tail = if index == 0 { who.as_str() } else { "" };
                out.push(format!("  {label}: {}{tail}", ui::printable(line)));
            }
            if hunk.lines.len() > SHOWN_LINES {
                out.push(format!(
                    "  {label}: … {} more lines",
                    hunk.lines.len() - SHOWN_LINES
                ));
            }
        }
    }
    if clashing.len() > SHOWN_HUNKS {
        out.push(format!(
            "… {} more conflicting places",
            clashing.len() - SHOWN_HUNKS
        ));
    }
    out
}

/// " — <author> (<commit>, <date>)" for the commit that last changed `hunk`'s lines at `head`, or
/// the newest change of the file when the hunk deleted them; empty when git can't tell.
fn last_change(
    repo: &Repo,
    head: &str,
    path: &str,
    hunk: &crate::commands::memory::Hunk,
) -> String {
    let commit = if hunk.lines.is_empty() {
        repo.git_opt(&["log", "-1", "--format=%H", head, "--", path])
    } else {
        let lines = format!("{},+{}", hunk.new_at, hunk.lines.len());
        repo.git_opt(&["blame", "--porcelain", "-L", &lines, head, "--", path])
            .and_then(|blame| {
                blame
                    .lines()
                    .next()
                    .and_then(|line| line.split(' ').next())
                    .map(str::to_owned)
            })
    };
    commit
        .map(|sha| sha.trim().to_owned())
        .filter(|sha| crate::commands::memory::is_object_id(sha))
        .and_then(|sha| {
            repo.git_opt(&["log", "-1", "--format=%an (%h, %ad)", "--date=short", &sha])
        })
        .map(|who| format!(" — {}", ui::printable(who.trim())))
        .unwrap_or_default()
}

/// The side the person chose for each conflicted shared file: `mine` keeps the target's version,
/// `theirs` takes the source's.
#[derive(Default)]
pub(in crate::commands) struct Choices {
    pub mine: Vec<String>,
    pub theirs: Vec<String>,
}

impl Choices {
    pub(in crate::commands) fn is_empty(&self) -> bool {
        self.mine.is_empty() && self.theirs.is_empty()
    }
}

/// Puts the chosen side of each conflicted file into `checkout`, for the transaction `tx` the
/// caller holds the control lock of, and returns the paths it resolved. Every conflict must be
/// settled by this one command (a choice, `--resolved`, or an edit already in the checkout), and
/// each choice must name a conflict of this transaction: otherwise nothing is written and `None`
/// says why. A keep-mine leaves the checkout as it is, so a confirmation spread over several
/// commands would be lost between them.
pub(in crate::commands) fn apply_choices(
    repo: &Repo,
    checkout: &Repo,
    tx: &Tx,
    source_head: &str,
    choices: &Choices,
    resolved: &[String],
    exclude: &[&str],
) -> crate::Result<Option<Vec<String>>> {
    let conflicts = file_reconciliation::conflicts(repo, &tx.target_head, source_head)?;
    for path in choices.mine.iter().chain(&choices.theirs) {
        if !conflicts.contains(path) {
            ui::error(&format!(
                "{} is not a conflicting file of this merge",
                ui::printable(path)
            ));
            return Ok(None);
        }
        if choices.mine.contains(path) && choices.theirs.contains(path) {
            ui::error(&format!(
                "{} can't be both kept and taken",
                ui::printable(path)
            ));
            return Ok(None);
        }
    }
    let edited = super::super::plumbing::worktree_changes(checkout, &tx.target_head, exclude)?;
    let open: Vec<&String> = conflicts
        .iter()
        .filter(|path| {
            !choices.mine.contains(path)
                && !choices.theirs.contains(path)
                && !resolved.contains(path)
                && !edited.contains(path)
        })
        .collect();
    if !open.is_empty() {
        #[cfg(windows)]
        use crate::ui::quote_powershell_argument as quote_arg;
        #[cfg(not(windows))]
        use crate::ui::session::shell_arg as quote_arg;
        ui::error("every conflicting file needs its side in the same command; nothing was changed");
        for path in &open {
            eprintln!("  {}", quote_arg(path));
        }
        ui::hint(
            "add `--keep-mine <path>` or `--take-theirs <path>` for each of them to this command",
        );
        return Ok(None);
    }
    for (paths, head) in [
        (&choices.mine, tx.target_head.as_str()),
        (&choices.theirs, source_head),
    ] {
        for path in paths {
            let entry = entry(repo, head, path)?;
            replace_inside(
                checkout.root(),
                path,
                entry
                    .as_ref()
                    .map(|(bytes, executable)| (bytes.as_slice(), *executable)),
            )?;
        }
    }
    Ok(Some(
        choices
            .mine
            .iter()
            .chain(&choices.theirs)
            .cloned()
            .collect(),
    ))
}

/// The bytes of `path` at `head` and whether Git records it as executable, or `None` when that side
/// has no such file. A link on either side is refused: choosing one would have to restore the link
/// itself, which these ways out don't do.
fn entry(repo: &Repo, head: &str, path: &str) -> crate::Result<Option<(Vec<u8>, bool)>> {
    let listing = repo.git_bytes_result(&["ls-tree", "-z", head, "--", path])?;
    let Some(record) = listing
        .split(|byte| *byte == 0)
        .find(|record| !record.is_empty())
    else {
        return Ok(None);
    };
    let mode = record
        .split(|byte| *byte == b' ')
        .next()
        .unwrap_or_default();
    anyhow::ensure!(
        mode == b"100644" || mode == b"100755",
        "{} is not a regular file on one side; resolve it with `--edit` or by hand",
        ui::printable(path)
    );
    let bytes = repo.git_bytes_result(&["show", &format!("{head}:{path}")])?;
    Ok(Some((bytes, mode == b"100755")))
}

/// agit's home with its own links resolved (a home on another volume, macOS's `/var`): the one
/// prefix trusted as it is. The checkout must lie inside it, and every folder below it is opened
/// without following a link, the checkout's own folder included.
fn trusted_home_and_parts(
    root: &std::path::Path,
) -> crate::Result<(std::path::PathBuf, Vec<std::ffi::OsString>)> {
    let home = crate::infra::config::agit_home()?;
    // In the form agit's checkout paths take, so both compare part by part.
    let trusted = super::super::worktree::canonical(&home);
    anyhow::ensure!(
        trusted.is_dir(),
        "agit's home {} is not a folder",
        home.display()
    );
    let below = root
        .strip_prefix(&trusted)
        .or_else(|_| root.strip_prefix(&home))
        .map_err(|_| anyhow::anyhow!("the checkout {} is outside agit's home", root.display()))?;
    let parts = below
        .components()
        .map(|component| match component {
            std::path::Component::Normal(part) => Ok(part.to_owned()),
            _ => Err(anyhow::anyhow!(
                "the checkout {} is not a plain path",
                root.display()
            )),
        })
        .collect::<crate::Result<Vec<_>>>()?;
    Ok((trusted, parts))
}

/// The parts of a plain relative path; anything else (`..`, an absolute path, a prefix) is refused.
fn plain_parts(relative: &str) -> crate::Result<Vec<&std::ffi::OsStr>> {
    use std::path::Component;
    let mut parts = Vec::new();
    for component in std::path::Path::new(relative).components() {
        match component {
            Component::Normal(part) => parts.push(part),
            _ => anyhow::bail!(
                "{} is not a plain path inside the checkout",
                ui::printable(relative)
            ),
        }
    }
    anyhow::ensure!(!parts.is_empty(), "an empty path names no file");
    Ok(parts)
}

/// Writes `content` (bytes, and whether it is executable) to `relative` inside `root`, or removes
/// the file when it is `None`, without ever following a link. Every step works on a directory
/// already opened without following links, so nothing that changes the path meanwhile can send
/// the write elsewhere: only agit's own home is taken with its links resolved, and from there every
/// folder down to the file, the checkout's own folder included, is opened from its parent's handle
/// without following a link (a missing one below the checkout is created there). The file is
/// replaced by renaming a new file over it within the same directory handle, which replaces a link
/// rather than writing through it. A link anywhere below agit's home is refused.
#[cfg(unix)]
fn replace_inside(
    root: &std::path::Path,
    relative: &str,
    content: Option<(&[u8], bool)>,
) -> crate::Result<()> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let parts = plain_parts(relative)?;
    let (name, directories) = parts.split_last().expect("plain_parts is never empty");
    let c = |part: &std::ffi::OsStr| CString::new(part.as_bytes()).map_err(anyhow::Error::from);
    let open_dir = |parent: &std::fs::File, part: &CString| -> std::io::Result<std::fs::File> {
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let descriptor = unsafe { libc::openat(parent.as_raw_fd(), part.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
    };
    let (trusted, below) = trusted_home_and_parts(root)?;
    let mut directory = std::fs::File::open(&trusted)?;
    for part in &below {
        let part = c(part)?;
        directory = open_dir(&directory, &part).map_err(|error| {
            anyhow::anyhow!(
                "the checkout's folder is not a plain folder inside agit's home: {error}"
            )
        })?;
    }
    for part in directories {
        let part = c(part)?;
        directory = match open_dir(&directory, &part) {
            Ok(opened) => opened,
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                if content.is_none() {
                    return Ok(());
                }
                if unsafe { libc::mkdirat(directory.as_raw_fd(), part.as_ptr(), 0o777) } != 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(error.into());
                    }
                }
                open_dir(&directory, &part)?
            }
            Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                anyhow::bail!(
                    "{} has a part that is not a directory, so nothing is written under it",
                    ui::printable(relative)
                )
            }
            Err(error) => return Err(error.into()),
        };
    }
    let name = c(name)?;
    let Some((bytes, executable)) = content else {
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ENOENT) {
                return Err(error.into());
            }
        }
        return Ok(());
    };
    let temporary = CString::new(format!(".agit-merge-{}", uuid::Uuid::new_v4()))?;
    let mode: libc::c_uint = if executable { 0o777 } else { 0o666 };
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let written = (|| -> std::io::Result<()> {
        let mut file = unsafe { std::fs::File::from_raw_fd(descriptor) };
        std::io::Write::write_all(&mut file, bytes)?;
        file.sync_all()?;
        if unsafe {
            libc::renameat(
                directory.as_raw_fd(),
                temporary.as_ptr(),
                directory.as_raw_fd(),
                name.as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    })();
    if let Err(error) = written {
        unsafe { libc::unlinkat(directory.as_raw_fd(), temporary.as_ptr(), 0) };
        if error.raw_os_error() == Some(libc::EISDIR) {
            anyhow::bail!("{} is a directory", ui::printable(relative));
        }
        return Err(error.into());
    }
    Ok(())
}

/// [`replace_inside`] on Windows, which has no descriptor-relative calls: agit's home and every
/// folder below it on the way, the checkout's own included, is opened and held without the right
/// for anyone else to delete or rename it, and refused if it is a link (a reparse point). While those handles are
/// open no part of the way can be swapped, so the path-based write that follows lands where they
/// were checked.
#[cfg(windows)]
fn replace_inside(
    root: &std::path::Path,
    relative: &str,
    content: Option<(&[u8], bool)>,
) -> crate::Result<()> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const FILE_SHARE_READ: u32 = 0x1;
    const FILE_SHARE_WRITE: u32 = 0x2;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    let hold = |path: &std::path::Path| -> crate::Result<std::fs::File> {
        let handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let metadata = handle.metadata()?;
        anyhow::ensure!(
            metadata.is_dir() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0,
            "{} is not a plain folder, so nothing is written under it",
            path.display()
        );
        Ok(handle)
    };
    let parts = plain_parts(relative)?;
    let (name, directories) = parts.split_last().expect("plain_parts is never empty");
    let (trusted, below) = trusted_home_and_parts(root)?;
    let mut held = vec![hold(&trusted)?];
    let mut parent = trusted;
    for part in &below {
        parent.push(part);
        held.push(hold(&parent)?);
    }
    for directory in directories {
        parent.push(directory);
        match std::fs::symlink_metadata(&parent) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && content.is_some() => {
                std::fs::create_dir(&parent)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        held.push(hold(&parent)?);
    }
    let destination = parent.join(name);
    if std::fs::symlink_metadata(&destination).is_ok_and(|metadata| metadata.is_dir()) {
        anyhow::bail!("{} is a directory", ui::printable(relative));
    }
    match content {
        Some((bytes, _)) => {
            let mut temporary = tempfile::NamedTempFile::new_in(&parent)?;
            std::io::Write::write_all(&mut temporary, bytes)?;
            temporary
                .persist(&destination)
                .map_err(|error| error.error)?;
        }
        None => match std::fs::remove_file(&destination) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        },
    }
    drop(held);
    Ok(())
}

/// Opens a conflicted file of the open transaction in the person's editor. The first time, the
/// checkout's copy (still the target's version) is replaced by the three-way merge with both sides
/// marked; once it differs from the target's version it is the person's work, and is opened as it
/// is, never regenerated over. `--continue --resolved` confirms it after.
pub(in crate::commands) fn edit(
    cwd: &std::path::Path,
    into: Option<&str>,
    path: &str,
) -> CmdResult {
    let (repo, slug, tx, control) = match open_lifecycle(cwd, into)? {
        Some(Lifecycle::Ordinary {
            repo,
            slug,
            tx,
            control,
        }) if tx.generation.is_some() && meta::is_file_line_at(&repo, &tx.target_head) => {
            (repo, slug, tx, control)
        }
        _ => {
            ui::error("--edit needs an open file-line merge transaction");
            return Ok(ExitCode::Precondition);
        }
    };
    let conflicts = file_reconciliation::conflicts(&repo, &tx.target_head, &tx.source_head)?;
    if !conflicts.contains(path) {
        ui::error(&format!(
            "{} is not a conflicting file of this merge",
            ui::printable(path)
        ));
        return Ok(ExitCode::Usage);
    }
    let blob = |rev: &str| {
        repo.git_bytes_result(&["show", &format!("{rev}:{path}")])
            .ok()
    };
    let checkout = super::super::worktree::checkout(&repo, &tx.target)?;
    let destination = checkout.root().join(path);
    let current = match std::fs::symlink_metadata(&destination) {
        Ok(metadata) if metadata.file_type().is_file() => Some(std::fs::read(&destination)?),
        Ok(_) => anyhow::bail!("{} is not a regular file", destination.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    if current == blob(&tx.target_head) {
        let dir = tempfile::tempdir()?;
        let write = |name: &str, bytes: Option<Vec<u8>>| -> crate::Result<String> {
            let file = dir.path().join(name);
            std::fs::write(&file, bytes.unwrap_or_default())?;
            Ok(file.to_string_lossy().into_owned())
        };
        let (mine, base, theirs) = (
            write("yours", blob(&tx.target_head))?,
            write("base", blob(&tx.base))?,
            write("theirs", blob(&tx.source_head))?,
        );
        let (code, _, stderr) = repo.git_status(&[
            "merge-file",
            "-q",
            "--diff3",
            "-L",
            "yours",
            "-L",
            "common",
            "-L",
            "theirs",
            &mine,
            &base,
            &theirs,
        ])?;
        anyhow::ensure!(
            matches!(code, Some(0..=127)),
            "cannot merge the two sides: {stderr}"
        );
        let executable =
            entry(&repo, &tx.target_head, path)?.is_some_and(|(_, executable)| executable);
        replace_inside(
            checkout.root(),
            path,
            Some((&std::fs::read(&mine)?, executable)),
        )?;
    }
    // The file is written; the editor runs without the transaction's lock, as any edit of the
    // checkout does.
    drop(control);
    #[cfg(windows)]
    use crate::ui::quote_powershell_argument as quote_arg;
    #[cfg(not(windows))]
    use crate::ui::session::shell_arg as quote_arg;
    let target_ref = quote_arg(&format!("{slug}@{}", tx.target));
    let quoted = quote_arg(path);
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .ok()
        .filter(|e| !e.trim().is_empty());
    match editor {
        Some(editor) if std::io::stdin().is_terminal() => {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{editor} \"$1\""))
                .arg("sh")
                .arg(&destination)
                .status()?;
            if !status.success() {
                ui::warning("the editor did not exit cleanly; the file keeps what was saved");
            }
        }
        _ => ui::info(format_args!(
            "the merged file with conflict markers is at {}",
            ui::tilde(&destination)
        )),
    }
    ui::hint(&format!(
        "when its markers are gone: `agit merge --into {target_ref} --continue --resolved {quoted}`"
    ));
    Ok(ExitCode::Ok)
}
