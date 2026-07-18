//! Server-side secret scan (pre-receive hook). Verbatim from the monolith.
use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::io::Write as _;
use std::io::BufRead as _;

use agit::hub::audit;

use crate::cli::repo_path;
use crate::gitplumb::{clip, git};
use crate::flag;

// ─────────────────── server-side secret scan (pre-receive) ───────────────────
//
// The client-side hook is `agit`'s, and `git push --no-verify` skips it — by design, that flag
// exists precisely to skip local hooks. So a client hook is a **reminder**, not a gate: the only
// place a push can actually be refused is the server, and this is it. A pre-receive hook runs before
// any ref is updated, so a rejected push leaves nothing behind to clean up.
//
// The scanner is the library's (`agit::scan`), so a rule fixed for `agit` is fixed here too.

// Every bound below now **refuses** the push it cannot cover rather than waving it through, so each
// one is an outage if it trips on ordinary work. They are set to bound cost, not to be reached.

/// Blobs scanned per push. A push is a pack of arbitrary size; without a ceiling a single push can
/// keep a core busy for as long as the pusher likes.
pub(crate) const SCAN_MAX_BLOBS: usize = 2_000;
/// Bytes scanned per blob. Generous: a session transcript is routinely megabytes, and the scan is one
/// linear pass, so the old 1MiB ceiling bought little and refused a lot.
pub(crate) const SCAN_MAX_BLOB_BYTES: u64 = 16 * 1024 * 1024;
/// Bytes scanned per push, across all blobs. `cat-file --batch` is buffered whole, so this is a
/// memory ceiling before it is a time one — which is why it does not simply follow the per-blob bound
/// upwards.
pub(crate) const SCAN_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// The operator's escape hatch from fail-closed: paths accepted as unscannable, one per line, read
/// from the **bare repo** on the server. Same placement as the allowlist and for the same reason — a
/// file the pusher controls is not a gate, it is a form to fill in.
pub(crate) const SCAN_SKIP_FILE: &str = ".agit-scan-skip";
/// Bytes of a blob sniffed for NUL before calling it binary. Matches `agit::scan`'s own sniff.
pub(crate) const BINARY_SNIFF_BYTES: usize = 8192;
/// Shortest printable run worth scanning inside a binary blob. A credential has to survive being
/// copied through a config file or an env var, so it is printable and it is long.
pub(crate) const MIN_PRINTABLE_RUN: usize = 6;

/// `agit-hub pre-receive --root <root> --agent <name>` — run by git as the repo's pre-receive hook,
/// with the pushed ref updates on stdin.
///
/// Exit non-zero = the push is refused, and everything on stderr reaches the pusher's terminal.
pub(crate) fn pre_receive_cmd(root: &Path, args: &[String]) -> i32 {
    let Some(agent) = flag(args, "--agent") else {
        eprintln!("pre-receive: --agent is required");
        return 2;
    };
    let Some(owner) = flag(args, "--owner") else {
        eprintln!("pre-receive: --owner is required");
        return 2;
    };
    // The scoped id `<owner>/<name>` is what the audit log keys on now.
    let scoped = format!("{owner}/{agent}");
    // git runs the hook with cwd = the bare repo.
    let repo = std::env::current_dir().unwrap_or_else(|_| repo_path(root, &owner, &agent));
    let mut news = vec![];
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let mut f = line.split_whitespace();
        let (_old, new) = (f.next().unwrap_or_default(), f.next().unwrap_or_default());
        // All-zero = a deletion: nothing new arrived to scan.
        if new.is_empty() || new.bytes().all(|b| b == b'0') {
            continue;
        }
        news.push(new.to_string());
    }
    if news.is_empty() {
        return 0;
    }

    let report = scan_push(&repo, &news);
    // REMOTE_USER is set for http-backend and inherited all the way down to this hook.
    let actor = std::env::var("REMOTE_USER").unwrap_or_else(|_| "unknown".into());
    if report.findings.is_empty() && !report.incomplete() {
        return 0;
    }

    let mut detail: Vec<String> = report.findings.iter().take(20).map(|f| format!("{} in {}:{}", f.0, f.1, f.2)).collect();
    detail.extend(report.unscanned.iter().take(20).map(|(path, why)| format!("unscanned {path}: {why}")));
    if let Some(e) = &report.errored {
        detail.push(format!("scan failed: {e}"));
    }
    audit::append(
        root,
        &actor,
        audit::GIT_PUSH_REJECTED,
        Some(&scoped),
        &format!(
            "secret scan: {} finding(s), {} unscanned blob(s){}; {}",
            report.findings.len(),
            report.unscanned.len(),
            if report.errored.is_some() { ", the scan itself failed" } else { "" },
            detail.join(", "),
        ),
    );

    eprintln!();
    if !report.findings.is_empty() {
        eprintln!("agit-hub: push REFUSED — {} possible secret(s) in the pushed objects.", report.findings.len());
        eprintln!();
        for (rule, path, line, excerpt) in report.findings.iter().take(20) {
            eprintln!("  {rule}  {path}:{line}");
            eprintln!("      {excerpt}");
        }
        if report.findings.len() > 20 {
            eprintln!("  ... and {} more", report.findings.len() - 20);
        }
        eprintln!();
    }
    if report.incomplete() {
        // The reason this refuses instead of warning: a gate that clears what it could not read is
        // worse than no gate, because it is trusted. One NUL byte used to buy exactly that.
        eprintln!("agit-hub: push REFUSED — this push could not be scanned in full.");
        eprintln!();
        if let Some(e) = &report.errored {
            eprintln!("  the scan itself failed: {e}");
            eprintln!("      nothing is known about ANY object in this push.");
        }
        for (path, why) in report.unscanned.iter().take(20) {
            eprintln!("  NOT SCANNED  {path}");
            eprintln!("      {why}");
        }
        if report.unscanned.len() > 20 {
            eprintln!("  ... and {} more", report.unscanned.len() - 20);
        }
        eprintln!();
        eprintln!("A push that could not be read cannot be cleared — that is what this gate is for.");
        eprintln!("If a path above is genuinely fine, add it to {} in the bare repo on the", SCAN_SKIP_FILE);
        eprintln!("server (one path per line) and it will be skipped rather than refused.");
        eprintln!();
    }
    if !report.findings.is_empty() {
        eprintln!("If a finding is wrong: add the line's literal to {} in the bare repo on the server,", agit::scan::ALLOW_FILE);
        eprintln!("or mark the line with the `{}` pragma before committing.", agit::scan::ALLOW_PRAGMA);
        eprintln!("Rewrite the history that carries the secret — and rotate it; a pushed secret is a burnt secret.");
        eprintln!();
    }
    eprintln!("Nothing was written — no ref moved. This gate is on the server, so --no-verify does not reach it.");
    eprintln!();
    1
}

pub(crate) struct ScanReport {
    /// (rule, path, line, excerpt)
    pub(crate) findings: Vec<(String, String, usize, String)>,
    /// Blobs no rule ever ran over: (path, the bound or failure that stopped it). The path is the
    /// actionable half — an operator who cannot tell which limit hit which file cannot act on either.
    pub(crate) unscanned: Vec<(String, String)>,
    /// The scan broke before it could reach any blob. Unlike `unscanned` there is no file to name:
    /// nothing at all is known about the push. A `bool` would do, but the message is the point.
    pub(crate) errored: Option<String>,
}

impl ScanReport {
    /// Anything the scan did not cover. `pre_receive_cmd` refuses on this: "found nothing" and
    /// "looked at nothing" are different claims, and only one of them clears a push.
    pub(crate) fn incomplete(&self) -> bool {
        !self.unscanned.is_empty() || self.errored.is_some()
    }
}

/// Scan the objects these refs bring that the repo does not already have.
///
/// `--not --all` is what keeps this proportional to the **push** rather than to the repo: during
/// pre-receive no ref has moved yet, so `--all` is the history already on the server, and the
/// difference is exactly what is arriving. Re-scanning history already accepted would make every
/// push cost the size of the repo.
pub(crate) fn scan_push(repo: &Path, news: &[String]) -> ScanReport {
    // The allowlist is the **server's**, read from the bare repo directory — deliberately not from
    // the pushed tree. An allowlist the pusher controls is not a gate, it is a form to fill in.
    let allow = agit::scan::Allowlist::load(repo);
    let skip = load_scan_skip(repo);
    let mut out = ScanReport { findings: vec![], unscanned: vec![], errored: None };

    let mut list_args: Vec<&str> = vec!["rev-list", "--objects"];
    for n in news {
        list_args.push(n);
    }
    list_args.push("--not");
    list_args.push("--all");
    let Some(listing) = git(repo, &list_args) else {
        out.errored = Some("`git rev-list` could not list the objects this push brings".into());
        return out;
    };

    // "<sha> [path]" — the path is git's best guess at a name for the object, which is what makes a
    // finding reportable to a human.
    let mut want: Vec<(String, String)> = vec![];
    for line in listing.lines() {
        let (sha, path) = match line.split_once(' ') {
            Some((s, p)) => (s, p),
            None => (line, ""),
        };
        if sha.len() < 7 || path.is_empty() {
            continue; // commits/tags have no path here; only blobs (and trees) do
        }
        if skip.iter().any(|s| s == path) {
            continue;
        }
        if want.len() >= SCAN_MAX_BLOBS {
            // One entry, not one per blob: the tail of an oversized push is unbounded, and naming the
            // first object past the bound is what an operator needs to act on it anyway.
            out.unscanned.push((
                path.to_string(),
                format!("this push carries more than {SCAN_MAX_BLOBS} blobs — this one and every blob after it went unscanned"),
            ));
            break;
        }
        want.push((sha.to_string(), path.to_string()));
    }
    // Scan the blob CONTENT (may be empty — a tag or a metadata-only push brings no new blobs; that must
    // NOT skip the metadata scan below, which was the bug that let a tag message through).
    scan_blob_content(repo, &want, &allow, &mut out);
    // Blobs are not the only channel a secret rides in on. A commit MESSAGE, an AUTHOR or COMMITTER
    // name/email, and an annotated TAG message all travel with the push and are readable back off the
    // server — and none of them has a path in `rev-list --objects`, so the loop above never saw them.
    // A gate that advertises "a pushed secret is a burnt secret" but scans only file content is blind to
    // three channels; verified live, an AKIA key in a commit message pushed clean.
    scan_meta(repo, news, &allow, &skip, &mut out);
    out
}

/// Scan the CONTENT of the blobs a push brings. Extracted so `scan_push` can run it and the metadata
/// scan unconditionally: a push with no new blobs (a tag, or a ref that only moves metadata) must not
/// short-circuit before the message/author/tag channels are checked.
pub(crate) fn scan_blob_content(repo: &Path, want: &[(String, String)], allow: &agit::scan::Allowlist, out: &mut ScanReport) {
    if want.is_empty() {
        return;
    }
    // One `cat-file --batch-check` for every candidate: types and sizes in a single process, so the
    // size bound can be applied *before* any content is read.
    let shas: String = want.iter().map(|(s, _)| format!("{s}\n")).collect();
    let Some(check) = git_stdin(repo, &["cat-file", "--batch-check"], shas.as_bytes()) else {
        out.errored.get_or_insert_with(|| "`git cat-file --batch-check` could not size the pushed objects".into());
        return;
    };
    let mut budget = SCAN_MAX_TOTAL_BYTES;
    let mut todo: Vec<(String, String)> = vec![];
    // --batch-check answers every input line with exactly one output line, so this zip stays aligned
    // even for an object git has lost (`<sha> missing`).
    for (line, (sha, path)) in String::from_utf8_lossy(&check).lines().zip(want.iter()) {
        let mut f = line.split_whitespace();
        let (_s, kind, size) = (f.next(), f.next().unwrap_or(""), f.next().unwrap_or("0"));
        if kind == "missing" {
            out.unscanned.push((path.clone(), "git no longer has this object".into()));
            continue;
        }
        if kind != "blob" {
            continue; // a tree has no content of its own to scan
        }
        let size: u64 = size.parse().unwrap_or(0);
        if size > SCAN_MAX_BLOB_BYTES {
            out.unscanned.push((path.clone(), format!("{size} bytes — past the {SCAN_MAX_BLOB_BYTES}-byte per-blob scan bound")));
            continue;
        }
        if size > budget {
            out.unscanned.push((
                path.clone(),
                format!("{size} bytes — past what is left of this push's {SCAN_MAX_TOTAL_BYTES}-byte total scan budget"),
            ));
            continue;
        }
        budget -= size;
        todo.push((sha.clone(), path.clone()));
    }
    if todo.is_empty() {
        return;
    }

    // ...and one `cat-file --batch` for the survivors' contents.
    let shas: String = todo.iter().map(|(s, _)| format!("{s}\n")).collect();
    let Some(blobs) = git_stdin(repo, &["cat-file", "--batch"], shas.as_bytes()) else {
        out.errored.get_or_insert_with(|| "`git cat-file --batch` could not read the pushed blobs".into());
        return;
    };
    // Keyed by sha, never by position: a missing object yields no body, and a positional zip would
    // then pair every later blob's content with the *previous* blob's path — and the path is the whole
    // actionable part of "rewrite the history that carries this secret".
    let bodies = parse_batch(&blobs);
    for (sha, path) in todo.iter() {
        let Some(content) = bodies.get(sha) else {
            out.unscanned.push((path.clone(), "git returned no content for this object".into()));
            continue;
        };
        // A NUL byte used to skip the blob whole and silently: `printf '\000' > f; cat key >> f` was a
        // complete bypass of this gate. Binary holds a key just as well as text does, so scan its
        // printable runs instead — and with the entropy heuristic off, which over the strings of a
        // compressed or compiled file is a false-positive generator, not a rule.
        let binary = content.iter().take(BINARY_SNIFF_BYTES).any(|&b| b == 0);
        let text = match binary {
            false => String::from_utf8_lossy(content).into_owned(),
            true => printable_runs(content),
        };
        for f in agit::scan::scan_text_allow(&text, !binary, allow) {
            // For a binary blob `line` counts printable runs, not file lines — the rule and the path
            // are what the operator acts on either way.
            out.findings.push((f.rule.to_string(), path.clone(), f.line, clip(&f.excerpt, 120)));
        }
    }
}

/// Scan the metadata the push brings — commit messages, author/committer identity, tag messages — for
/// the same secrets as blob content. Reuses the blob path's batch machinery and bounds.
///
/// Entropy is OFF here on purpose: a raw commit object carries `tree`/`parent` 40-hex lines, which the
/// entropy heuristic would flag as high-entropy strings. The named rules (AKIA, `ghp_…`, and the rest)
/// do not need entropy and do not match a hex sha, so metadata is scanned by rule only.
pub(crate) fn scan_meta(repo: &Path, news: &[String], allow: &agit::scan::Allowlist, skip: &[String], out: &mut ScanReport) {
    // Commits the push introduces, exactly the same range as the blob scan.
    let mut list_args: Vec<&str> = vec!["rev-list"];
    for n in news {
        list_args.push(n);
    }
    list_args.push("--not");
    list_args.push("--all");
    let mut shas: Vec<String> = match git(repo, &list_args) {
        Some(listing) => listing.lines().map(str::to_string).collect(),
        None => {
            out.errored.get_or_insert_with(|| "`git rev-list` could not list the pushed commits".into());
            return;
        }
    };
    // Annotated tags carry their own message/tagger and are not reachable as commits. A ref tip that is
    // itself a tag object gets scanned too.
    for n in news {
        if let Some(check) = git(repo, &["cat-file", "-t", n]) {
            if check.trim() == "tag" {
                shas.push(n.clone());
            }
        }
    }
    if shas.is_empty() {
        return;
    }
    if shas.len() > SCAN_MAX_BLOBS {
        out.unscanned.push((
            format!("commit {}", &shas[SCAN_MAX_BLOBS][..shas[SCAN_MAX_BLOBS].len().min(12)]),
            format!("this push carries more than {SCAN_MAX_BLOBS} commits — this one and every commit after it went unscanned"),
        ));
        shas.truncate(SCAN_MAX_BLOBS);
    }

    let batch: String = shas.iter().map(|s| format!("{s}\n")).collect();
    let Some(raw) = git_stdin(repo, &["cat-file", "--batch"], batch.as_bytes()) else {
        out.errored.get_or_insert_with(|| "`git cat-file --batch` could not read the pushed commits".into());
        return;
    };
    let bodies = parse_batch(&raw);
    // The same byte bounds the blob path applies: a commit message can be arbitrarily large, so cap
    // each object and the metadata scan as a whole rather than regex-scanning unbounded text. Oversize
    // objects are reported as unscanned (fail-closed), never waved through.
    let mut budget = SCAN_MAX_TOTAL_BYTES;
    for sha in &shas {
        let label = format!("commit {}", &sha[..sha.len().min(12)]);
        if skip.iter().any(|s| s == &label) {
            continue;
        }
        let Some(content) = bodies.get(sha) else {
            out.unscanned.push((label, "git returned no content for this commit".into()));
            continue;
        };
        let size = content.len() as u64;
        if size > SCAN_MAX_BLOB_BYTES {
            out.unscanned.push((label, format!("{size} bytes — past the {SCAN_MAX_BLOB_BYTES}-byte per-object scan bound")));
            continue;
        }
        if size > budget {
            out.unscanned.push((label, format!("{size} bytes — past what is left of this push's {SCAN_MAX_TOTAL_BYTES}-byte total scan budget")));
            continue;
        }
        budget -= size;
        let text = String::from_utf8_lossy(content);
        for f in agit::scan::scan_text_allow(&text, false, allow) {
            out.findings.push((f.rule.to_string(), label.clone(), f.line, clip(&f.excerpt, 120)));
        }
    }
}

/// The `strings(1)` of a blob: its printable runs, one per line, so the text rules can see them.
///
/// A credential has to survive being copied through a config file, an env var or a header, so it is
/// printable ASCII by construction — the bytes around it cannot hide it.
pub(crate) fn printable_runs(content: &[u8]) -> String {
    let mut out = String::new();
    let mut run: Vec<u8> = vec![];
    // Tab included: an indent does not end a run a human would read as one line.
    let printable = |b: u8| (0x20..0x7f).contains(&b) || b == b'\t';
    for &b in content.iter().chain(std::iter::once(&0)) {
        match printable(b) {
            true => run.push(b),
            false => {
                if run.len() >= MIN_PRINTABLE_RUN {
                    out.push_str(&String::from_utf8_lossy(&run));
                    out.push('\n');
                }
                run.clear();
            }
        }
    }
    out
}

/// Paths the operator has accepted as unscannable. Absent file = empty list, which is the safe
/// direction: fail-closed stays closed until someone says otherwise.
pub(crate) fn load_scan_skip(repo: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(repo.join(SCAN_SKIP_FILE)) else {
        return vec![];
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

/// `git cat-file --batch` output: `<sha> <type> <size>\n<size bytes>\n`, repeated. Split on the
/// declared size rather than on newlines — blob content contains newlines, and a "missing" line has
/// no size at all.
///
/// Keyed by the sha in the header rather than returned in order: an object git has lost contributes no
/// body, so position is not identity here.
pub(crate) fn parse_batch(raw: &[u8]) -> HashMap<String, Vec<u8>> {
    let mut out = HashMap::new();
    let mut i = 0;
    while i < raw.len() {
        let Some(nl) = raw[i..].iter().position(|&b| b == b'\n') else {
            break;
        };
        let header = String::from_utf8_lossy(&raw[i..i + nl]).to_string();
        i += nl + 1;
        let mut f = header.split_whitespace();
        let Some(sha) = f.next() else {
            continue;
        };
        let Some(size) = f.nth(1).and_then(|s| s.parse::<usize>().ok()) else {
            continue; // "<sha> missing" — no content follows
        };
        let end = (i + size).min(raw.len());
        out.insert(sha.to_string(), raw[i..end].to_vec());
        i = end + 1; // the trailing newline git adds after the content
    }
    out
}

pub(crate) fn git_stdin(repo: &Path, args: &[&str], input: &[u8]) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(input).ok()?;
    let out = child.wait_with_output().ok()?;
    Some(out.stdout)
}

/// Install the pre-receive hook into a bare repo, pointing at this very binary.
///
/// The absolute path of the running executable is baked in: the hook runs from git's environment,
/// where PATH is whatever the service inherited, and a hook that cannot find its binary is a gate
/// that silently isn't there.
pub(crate) fn install_pre_receive(repo: &Path, root: &Path, owner_ns: &str, name: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let hook = repo.join("hooks").join("pre-receive");
    let script = format!(
        "#!/bin/sh\n\
         # Installed by agit-hub. The server-side secret gate: `git push --no-verify` skips the\n\
         # client's hook, not this one. Regenerated on demand — edit agit-hub, not this file.\n\
         exec {} pre-receive --root {} --owner {} --agent {}\n",
        shell_quote(&exe.to_string_lossy()),
        shell_quote(&root.to_string_lossy()),
        shell_quote(owner_ns),
        shell_quote(name),
    );
    // Rewrite whenever it differs: the binary may have moved since the repo was created, and a hook
    // pointing at a path that no longer exists fails the push rather than passing it (git treats a
    // hook that cannot execute as a failure) — but silently wrong is still worth correcting.
    if std::fs::read_to_string(&hook).ok().as_deref() == Some(script.as_str()) {
        return;
    }
    let _ = std::fs::create_dir_all(repo.join("hooks"));
    if std::fs::write(&hook, &script).is_ok() {
        // Make the hook executable on Unix; Windows git runs hooks without a mode bit.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700));
        }
    }
}

/// Single-quote for /bin/sh. Paths come from the filesystem and the agent name is validated, but a
/// hook script is code — quoting it is not where to save a line.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
