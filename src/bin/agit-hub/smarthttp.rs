//! git smart-http, rewritten onto tokio::process with a streamed request body and a streamed
//! response, preserving every env var and the CGI-header normalization. prepare_repo/ensure_exportable
//! /find_subslice stay verbatim.
#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]
use std::path::Path;
use std::process::Stdio;

use axum::body::Body;
use axum::response::{IntoResponse, Response};
use futures_util::{StreamExt, TryStreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::{ReaderStream, StreamReader};

use crate::cli::repo_path;
use crate::http::{Req, Resp};
use crate::scan::install_pre_receive;
use crate::limits::{MAX_BODY, MAX_CGI_HEADERS};
use crate::server::Ctx;

/// **`name` must already be authorized before calling this** (see git_or_spa). This only shuttles
/// bytes: the request body is streamed into `git http-backend`'s stdin (capped at MAX_BODY) and its
/// stdout is streamed straight back out (close-delimited, never buffered whole).
pub(crate) async fn git_http(ctx: &Ctx, req: &Req, body: Body, name: &str, actor: &str) -> Response {
    let (path, query) = match req.target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (req.target.clone(), String::new()),
    };
    let ctype = req.header("content-type").unwrap_or("").to_string();
    let method = req.method.clone();
    let clen = req.content_length;
    let root = ctx.root().to_path_buf();
    let name_s = name.to_string();

    // Export marker + secret gate (fs writes) on the blocking pool.
    {
        let root2 = root.clone();
        let n = name_s.clone();
        tokio::task::spawn_blocking(move || prepare_repo(&repo_path(&root2, &n), &root2, &n)).await.unwrap();
    }

    let mut child = match tokio::process::Command::new("git")
        .arg("http-backend")
        .env("GIT_PROJECT_ROOT", &root)
        // GIT_HTTP_EXPORT_ALL is **deliberately unset**: http-backend only serves the repo marked by
        // ensure_exportable (= the one that just passed acl::decide). The real gate ran above.
        .env("REQUEST_METHOD", &method)
        .env("PATH_INFO", &path)
        .env("QUERY_STRING", &query)
        .env("CONTENT_TYPE", &ctype)
        .env("CONTENT_LENGTH", clen.to_string())
        // Who pushed goes into the reflog; it also puts a person on http-backend's errors.
        .env("REMOTE_USER", actor)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Resp::text(500, "git http-backend unavailable").into_response(),
    };

    // Stream the request body into stdin, capped so total <= MAX_BODY; never buffer the pack.
    let mut stdin = child.stdin.take().unwrap();
    tokio::spawn(async move {
        let byte_stream = body.into_data_stream().map_err(std::io::Error::other);
        let mut reader = StreamReader::new(byte_stream).take(MAX_BODY as u64);
        let _ = tokio::io::copy(&mut reader, &mut stdin).await;
        let _ = stdin.shutdown().await; // EOF so http-backend can wrap up
    });

    // Read only the CGI header block into memory, then stream the body straight out.
    let mut cout = child.stdout.take().unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let sep = loop {
        if let Some(hit) = find_subslice(&buf, b"\r\n\r\n")
            .map(|i| (i, 4))
            .or_else(|| find_subslice(&buf, b"\n\n").map(|i| (i, 2)))
        {
            break Some(hit);
        }
        if buf.len() >= MAX_CGI_HEADERS {
            break None;
        }
        match cout.read(&mut chunk).await {
            Ok(0) => break None,
            Ok(k) => buf.extend_from_slice(&chunk[..k]),
            Err(_) => break None,
        }
    };
    let (raw_headers, body_prefix): (&[u8], &[u8]) = match sep {
        Some((i, n)) => (&buf[..i], &buf[i + n..]),
        None => (&[][..], &buf[..]),
    };
    // Normalize the CGI headers: pull git's Status: as the real status, drop its Content-Length (the
    // response is close-delimited), forward the rest verbatim.
    let mut status = 200u16;
    let mut fwd: Vec<(String, String)> = Vec::new();
    for line in String::from_utf8_lossy(raw_headers).split('\n') {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            if key.eq_ignore_ascii_case("status") {
                status = v.split_whitespace().next().and_then(|c| c.parse().ok()).unwrap_or(200);
                continue;
            }
            if key.eq_ignore_ascii_case("content-length") {
                continue;
            }
            fwd.push((key.to_string(), v.trim().to_string()));
        }
    }

    let prefix = bytes::Bytes::copy_from_slice(body_prefix);
    let tail = ReaderStream::new(cout);
    let stream = futures_util::stream::once(async move { Ok::<_, std::io::Error>(prefix) }).chain(tail);

    let mut builder = Response::builder().status(status);
    for (k, v) in fwd {
        builder = builder.header(k, v);
    }
    let response = match builder.body(Body::from_stream(stream)) {
        Ok(r) => r,
        Err(_) => return Resp::text(500, "git http-backend produced an unusable response").into_response(),
    };

    // Reap the child once it exits (kill_on_drop covers an early client hangup).
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    response
}

/// Make a repo ready to be served: the export marker, and the server-side secret gate.
///
/// Both are done here, right before http-backend runs, rather than only at create time — that is
/// what brings repos made by an older agit-hub (or `git init --bare` by hand) under the same rules
/// instead of leaving them as quiet exceptions.
pub(crate) fn prepare_repo(repo: &Path, root: &Path, agent: &str) {
    ensure_exportable(repo);
    install_pre_receive(repo, root, agent);
}

/// Put http-backend's export marker on **this one** repo.
///
/// Without GIT_HTTP_EXPORT_ALL, http-backend only serves repos carrying `git-daemon-export-ok`.
/// The marker is written only after authorization passes, which also brings old repos (created
/// before `agit-hub add`) along automatically.
/// Note: the marker is **not** a security boundary — it only tells http-backend "this repo is meant
/// to be served". Who may access it is decided by acl::decide, and that step already ran above.
pub(crate) fn ensure_exportable(repo: &Path) {
    let marker = repo.join("git-daemon-export-ok");
    if !marker.exists() {
        let _ = std::fs::write(&marker, b"");
    }
}

pub(crate) fn find_subslice(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}
