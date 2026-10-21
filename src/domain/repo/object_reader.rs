//! Long-lived `cat-file` processes answer many object requests without starting a process per
//! request, and a large request is split across several of them so that reads waiting on storage
//! overlap.
//!
//! A scoped thread writes each request while the caller reads its answers, so neither side waits
//! on a full pipe. A failure inside a request leaves the protocol position unknown, so every
//! process of that kind is discarded and started again on the next request.

use super::{Repo, suppress_pipe_signal};
use crate::infra::local_git::Deadline;
use anyhow::{Context, Result, anyhow, bail, ensure};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Stdio};

const MAX_READERS: usize = 8;
/// Requests smaller than this stay on one process; more processes would only add startup cost.
const MIN_SHARE: usize = 64;
/// Object headers are an ID, a type name and a decimal size, or a spec and `missing`.
const MAX_HEADER: u64 = 64 * 1024;

/// One answer of a `<commit>:<path>` body request.
pub(crate) enum Body {
    Missing,
    /// Larger than the request's maximum; the body was drained, not kept.
    Large,
    Read(Vec<u8>),
}

struct Process {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) struct ObjectReaders<'a> {
    repo: &'a Repo,
    deadline: Deadline,
    readers: usize,
    locate: Vec<Process>,
    bodies: Vec<Process>,
}

impl<'a> ObjectReaders<'a> {
    pub(crate) fn new(repo: &'a Repo, deadline: Deadline) -> Self {
        Self {
            repo,
            deadline,
            readers: std::thread::available_parallelism()
                .map_or(1, |n| n.get())
                .min(MAX_READERS),
            locate: Vec::new(),
            bodies: Vec::new(),
        }
    }

    /// Blob IDs and sizes in request order; `None` when the spec names nothing.
    pub(crate) fn locate(&mut self, specs: &[String]) -> Result<Vec<Option<(String, u64)>>> {
        let (repo, deadline, readers) = (self.repo, self.deadline, self.readers);
        request(
            &mut self.locate,
            || start(repo, "--batch-check", deadline),
            readers,
            specs,
            |stdout| {
                Ok(match header(stdout)? {
                    None => None,
                    Some(Header { oid, kind, size }) => {
                        ensure!(kind == "blob", "Git object is a {kind}, not a blob");
                        ensure!(
                            oid.len() >= 40 && oid.bytes().all(|b| b.is_ascii_hexdigit()),
                            "Git object ID is malformed"
                        );
                        Some((oid, size))
                    }
                })
            },
        )
    }

    /// Blob bodies in request order. A body over `maximum` is drained without being kept.
    pub(crate) fn bodies(&mut self, specs: &[String], maximum: usize) -> Result<Vec<Body>> {
        let (repo, deadline, readers) = (self.repo, self.deadline, self.readers);
        request(
            &mut self.bodies,
            || start(repo, "--batch", deadline),
            readers,
            specs,
            |stdout| {
                let Some(Header { kind, size, .. }) = header(stdout)? else {
                    return Ok(Body::Missing);
                };
                ensure!(kind == "blob", "Git object is a {kind}, not a blob");
                let framed = size.checked_add(1).context("Git object size overflows")?;
                if size > maximum as u64 {
                    let drained =
                        std::io::copy(&mut Read::take(&mut *stdout, framed), &mut std::io::sink())?;
                    ensure!(drained == framed, "Git object reader closed early");
                    return Ok(Body::Large);
                }
                let mut body = Vec::with_capacity(usize::try_from(size)?);
                Read::take(&mut *stdout, size).read_to_end(&mut body)?;
                let mut end = [0u8];
                stdout.read_exact(&mut end)?;
                ensure!(
                    body.len() as u64 == size && end == *b"\n",
                    "Git object reader closed early"
                );
                Ok(Body::Read(body))
            },
        )
    }
}

fn start(repo: &Repo, mode: &str, deadline: Deadline) -> Result<Process> {
    let mut child = repo
        .object_reader_command(mode, deadline)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to run git cat-file {mode}"))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        bail!("git cat-file {mode} has no pipes");
    };
    Ok(Process {
        child,
        stdin,
        stdout: BufReader::with_capacity(256 * 1024, stdout),
    })
}

struct Header {
    oid: String,
    kind: String,
    size: u64,
}

/// `None` for a missing object.
fn header(stdout: &mut BufReader<ChildStdout>) -> Result<Option<Header>> {
    let mut line = Vec::new();
    Read::take(&mut *stdout, MAX_HEADER).read_until(b'\n', &mut line)?;
    let Some(line) = line.strip_suffix(b"\n") else {
        bail!("Git object reader closed early");
    };
    let line = std::str::from_utf8(line).context("Git object header is not UTF-8")?;
    match line.split(' ').collect::<Vec<_>>().as_slice() {
        [_, "missing"] => Ok(None),
        [oid, kind, size] => Ok(Some(Header {
            oid: (*oid).to_owned(),
            kind: (*kind).to_owned(),
            size: size.parse()?,
        })),
        _ => bail!("unexpected Git object header: {line}"),
    }
}

/// Split `specs` into contiguous shares, one per process, and join the answers in request order.
fn request<T: Send>(
    pool: &mut Vec<Process>,
    start: impl Fn() -> Result<Process>,
    readers: usize,
    specs: &[String],
    answer: impl Fn(&mut BufReader<ChildStdout>) -> Result<T> + Sync,
) -> Result<Vec<T>> {
    // Callers bound their own time; a read failing on a clock would look like unreadable data.
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let shares = specs.len().div_ceil(MIN_SHARE).clamp(1, readers);
    while pool.len() < shares {
        pool.push(start()?);
    }
    let share = specs.len().div_ceil(shares);
    let results: Vec<Result<Vec<T>>> = if shares == 1 {
        vec![answer_all(&mut pool[0], specs, &answer)]
    } else {
        std::thread::scope(|scope| {
            let handles: Vec<_> = pool
                .iter_mut()
                .zip(specs.chunks(share))
                .map(|(process, specs)| scope.spawn(|| answer_all(process, specs, &answer)))
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| Err(anyhow!("Git object reader panicked")))
                })
                .collect()
        })
    };
    let mut answers = Vec::with_capacity(specs.len());
    for result in results {
        match result {
            Ok(part) => answers.extend(part),
            Err(error) => {
                pool.clear();
                return Err(error);
            }
        }
    }
    Ok(answers)
}

fn answer_all<T>(
    process: &mut Process,
    specs: &[String],
    answer: &(impl Fn(&mut BufReader<ChildStdout>) -> Result<T> + Sync),
) -> Result<Vec<T>> {
    let Process {
        child,
        stdin,
        stdout,
    } = process;
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || -> Result<()> {
            suppress_pipe_signal(stdin)?;
            let mut input = std::io::BufWriter::new(stdin);
            for spec in specs {
                input.write_all(spec.as_bytes())?;
                input.write_all(b"\n")?;
            }
            input.flush()?;
            Ok(())
        });
        let mut answers = Vec::with_capacity(specs.len());
        let mut failure = None;
        for _ in specs {
            match answer(stdout) {
                Ok(value) => answers.push(value),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        // A writer blocked on a reader that stopped would never return.
        if failure.is_some() {
            let _ = child.kill();
        }
        let written = writer
            .join()
            .map_err(|_| anyhow!("Git input writer panicked"))?;
        match failure {
            Some(error) => Err(error),
            None => written.map(|()| answers),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers stay aligned with requests across parallel shares, after an oversized body is
    /// drained, and on a reused process. Concatenating shares out of order, or leaving an
    /// oversized body's bytes in the stream, would hand one object's bytes to another request
    /// without any error.
    #[test]
    fn answers_follow_request_order_across_shares_and_drained_bodies() {
        let directory = tempfile::tempdir().unwrap();
        let repo = Repo::init(directory.path()).unwrap();
        let count = MIN_SHARE * 3;
        let mut names = Vec::new();
        for index in 0..count {
            let name = format!("blob-{index}");
            let body = if index == MIN_SHARE + 1 {
                "x".repeat(4096)
            } else {
                format!("body {index}\n")
            };
            std::fs::write(directory.path().join(&name), body).unwrap();
            names.push(name);
        }
        let mut args = vec!["hash-object", "-w", "--"];
        args.extend(names.iter().map(String::as_str));
        let mut specs: Vec<String> = repo
            .git(&args)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(specs.len(), count);
        let missing = MIN_SHARE * 2;
        specs[missing] = "0".repeat(specs[0].len());
        let mut readers = ObjectReaders::new(&repo, Deadline::new());
        for _ in 0..2 {
            let bodies = readers.bodies(&specs, 1024).unwrap();
            assert_eq!(bodies.len(), count);
            for (index, body) in bodies.iter().enumerate() {
                match body {
                    Body::Large => assert_eq!(index, MIN_SHARE + 1),
                    Body::Missing => assert_eq!(index, missing),
                    Body::Read(raw) => assert_eq!(raw, format!("body {index}\n").as_bytes()),
                }
            }
        }
        let located = readers.locate(&specs).unwrap();
        assert_eq!(located[0].as_ref().map(|(oid, _)| oid), Some(&specs[0]));
        assert_eq!(
            located[MIN_SHARE + 1].as_ref().map(|(_, size)| *size),
            Some(4096)
        );
        assert!(located[missing].is_none());
    }
}
