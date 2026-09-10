//! Synchronization compares frozen branch tips with locally available tracking refs.

use crate::domain::repo::{ObjectBody, ReadPolicy, Repo};
use anyhow::ensure;
use std::collections::{BTreeMap, BTreeSet};

const MAX_REF_BYTES: usize = 1024 * 1024;
const MAX_REFS: usize = 4096;
const MAX_COMMITS: usize = 8192;
const MAX_COMMIT_BYTES: usize = 128 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Ref {
    sha: String,
    upstream: String,
    symbolic: String,
    tracking_config: Vec<(String, String)>,
}

#[derive(Debug)]
pub(super) struct Branch {
    pub name: String,
    pub head: String,
    pub tracking: String,
    pub state: String,
}

pub(super) struct Page {
    pub branches: Vec<Branch>,
    pub omitted: usize,
}

pub(super) fn inspect(repo: &Repo, limit: usize) -> crate::Result<Page> {
    let repo = repo.clone().local_objects_only();
    let before = refs(&repo, limit)?;
    let mut selected = Vec::new();
    let mut tracked = BTreeSet::new();
    for (reference, value) in &before {
        let Some(name) = reference.strip_prefix("refs/heads/") else {
            continue;
        };
        if !value.symbolic.is_empty() {
            selected.push((
                name.to_owned(),
                value.sha.clone(),
                value.symbolic.clone(),
                Some("symbolic local ref".to_owned()),
            ));
            continue;
        }
        if value.upstream.is_empty() && !value.tracking_config.is_empty() {
            selected.push((
                name.to_owned(),
                value.sha.clone(),
                String::new(),
                Some("tracking ref unavailable locally".to_owned()),
            ));
            continue;
        }
        let conventional = format!("refs/remotes/origin/{name}");
        let tracking = if !value.upstream.is_empty() {
            value.upstream.clone()
        } else if before.contains_key(&conventional) {
            conventional
        } else {
            String::new()
        };
        tracked.insert(tracking.clone());
        selected.push((name.to_owned(), value.sha.clone(), tracking, None));
    }
    for (reference, value) in &before {
        if !value.symbolic.is_empty() || tracked.contains(reference) {
            continue;
        }
        if let Some(name) = reference.strip_prefix("refs/remotes/") {
            selected.push((
                name.to_owned(),
                value.sha.clone(),
                reference.clone(),
                Some("remote only; no local tracking branch".to_owned()),
            ));
        }
    }
    let omitted = selected.len().saturating_sub(limit);
    selected.truncate(limit);
    let mut tips = BTreeSet::new();
    for (_, head, tracking, fixed_state) in &selected {
        if fixed_state.is_none()
            && let Some(remote) = before.get(tracking)
            && remote.sha != *head
        {
            tips.insert(head.clone());
            tips.insert(remote.sha.clone());
        }
    }
    let graph = ancestry(&repo, &tips);
    let mut rows = Vec::new();
    for (name, head, tracking, fixed_state) in selected {
        let state = if let Some(state) = fixed_state {
            state
        } else if tracking.is_empty() {
            "no known tracking ref".to_owned()
        } else if let Some(remote) = before.get(&tracking) {
            if head == remote.sha {
                "in sync (ahead 0, behind 0)".to_owned()
            } else {
                match &graph {
                    Ok(graph) => {
                        let local = reachable(graph, &head);
                        let upstream = reachable(graph, &remote.sha);
                        let ahead = local.difference(&upstream).count();
                        let behind = upstream.difference(&local).count();
                        if ahead > 0 && behind > 0 {
                            format!("diverged (ahead {ahead}, behind {behind})")
                        } else {
                            format!("ahead {ahead}, behind {behind}")
                        }
                    }
                    Err(error) => format!("comparison unavailable: {error:#}"),
                }
            }
        } else {
            "tracking ref unavailable locally".to_owned()
        };
        rows.push(Branch {
            name,
            head,
            tracking,
            state,
        });
    }
    ensure!(
        before == refs(&repo, limit)?,
        "branch refs changed during inspection; retry status"
    );
    Ok(Page {
        branches: rows,
        omitted,
    })
}

type Graph = BTreeMap<String, Vec<String>>;
type TrackingConfig = BTreeMap<String, Vec<(String, String)>>;

fn ancestry(repo: &Repo, tips: &BTreeSet<String>) -> crate::Result<Graph> {
    if tips.is_empty() {
        return Ok(Graph::new());
    }
    let maximum = format!("--max-count={}", MAX_COMMITS + 1);
    let mut args = vec!["rev-list", maximum.as_str()];
    args.extend(tips.iter().map(String::as_str));
    let output = repo.inspection_output(&args, (MAX_COMMITS + 1) * 66)?;
    ensure!(
        output.status.success(),
        "local ancestry cannot be enumerated"
    );
    let oids = std::str::from_utf8(&output.stdout)?
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ensure!(
        oids.len() <= MAX_COMMITS,
        "history exceeds the status comparison budget"
    );
    ensure!(
        oids.iter().all(|oid| valid_oid(oid)),
        "ancestry contains an invalid commit identity"
    );
    let mut graph = Graph::new();
    let mut bytes_read = 0usize;
    repo.git_cat_file_batch_with_policy(
        oids,
        MAX_COMMIT_BYTES,
        ReadPolicy::LocalOnly,
        |oid, kind, body| {
            ensure!(kind == "commit", "ancestry contains a non-commit object");
            let ObjectBody::Read(bytes) = body else {
                anyhow::bail!("commit exceeds the status inspection budget");
            };
            bytes_read = bytes_read
                .checked_add(bytes.len())
                .ok_or_else(|| anyhow::anyhow!("history size overflow"))?;
            ensure!(
                bytes_read <= 32 * 1024 * 1024,
                "history exceeds the status byte budget"
            );
            let parents = bytes
                .split(|byte| *byte == b'\n')
                .take_while(|line| !line.is_empty())
                .filter_map(|line| line.strip_prefix(b"parent "))
                .map(|parent| Ok(std::str::from_utf8(parent)?.to_owned()))
                .collect::<crate::Result<Vec<_>>>()?;
            ensure!(
                parents.iter().all(|parent| valid_oid(parent)),
                "commit has an invalid parent identity"
            );
            ensure!(
                graph.insert(oid.to_owned(), parents).is_none(),
                "ancestry contains a duplicate commit"
            );
            Ok(())
        },
    )?;
    // Enumeration is only a candidate set. Immutable parent headers must close the graph;
    // otherwise shallow or grafted history can manufacture divergence or hide unpublished work.
    ensure!(
        tips.iter().all(|tip| graph.contains_key(tip))
            && graph
                .values()
                .flatten()
                .all(|parent| graph.contains_key(parent)),
        "complete immutable ancestry is unavailable locally"
    );
    Ok(graph)
}

fn reachable<'a>(graph: &'a Graph, head: &'a str) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![head];
    while let Some(oid) = pending.pop() {
        if seen.insert(oid) {
            pending.extend(graph[oid].iter().map(String::as_str));
        }
    }
    seen
}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn refs(repo: &Repo, limit: usize) -> crate::Result<BTreeMap<String, Ref>> {
    let tracking_config = tracking_configuration(repo)?;
    let output = repo.inspection_output(
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(upstream)%00%(symref)",
            "refs/heads/",
            "refs/remotes/",
        ],
        MAX_REF_BYTES,
    )?;
    ensure!(
        output.status.success() && output.stderr.is_empty(),
        "branch refs cannot be completely inspected"
    );
    let mut ref_count = 0;
    let mut result = parse_refs(&output.stdout, &tracking_config, None, &mut ref_count)?;
    let upstreams = result
        .iter()
        .filter(|(name, _)| name.starts_with("refs/heads/"))
        .take(limit)
        .filter(|(_, value)| {
            value.symbolic.is_empty()
                && !value.upstream.is_empty()
                && !value.upstream.starts_with("refs/heads/")
                && !value.upstream.starts_with("refs/remotes/")
        })
        .map(|(_, value)| value.upstream.clone())
        .collect::<BTreeSet<_>>();
    if !upstreams.is_empty() {
        let mut args = vec![
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(upstream)%00%(symref)",
            "--",
        ];
        args.extend(upstreams.iter().map(String::as_str));
        let output =
            repo.inspection_output(&args, MAX_REF_BYTES.saturating_sub(output.stdout.len()))?;
        ensure!(
            output.status.success() && output.stderr.is_empty(),
            "branch refs cannot be completely inspected"
        );
        for (name, value) in parse_refs(
            &output.stdout,
            &tracking_config,
            Some(&upstreams),
            &mut ref_count,
        )? {
            ensure!(
                result.insert(name, value).is_none(),
                "branch inventory contains a duplicate ref"
            );
        }
    }
    Ok(result)
}

fn parse_refs(
    bytes: &[u8],
    tracking_config: &TrackingConfig,
    exact_names: Option<&BTreeSet<String>>,
    ref_count: &mut usize,
) -> crate::Result<BTreeMap<String, Ref>> {
    let mut result = BTreeMap::new();
    for line in std::str::from_utf8(bytes)?.split_terminator('\n') {
        let fields = line.as_bytes().split(|byte| *byte == 0).collect::<Vec<_>>();
        ensure!(fields.len() == 4, "branch ref framing is incomplete");
        ensure!(
            *ref_count < MAX_REFS,
            "branch inventory exceeds the inspection limit"
        );
        *ref_count += 1;
        let name = std::str::from_utf8(fields[0])?.to_owned();
        let configured = name
            .strip_prefix("refs/heads/")
            .and_then(|branch| tracking_config.get(branch))
            .cloned()
            .unwrap_or_default();
        let sha = std::str::from_utf8(fields[1])?;
        ensure!(
            matches!(sha.len(), 40 | 64) && sha.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "branch ref has an invalid object identity"
        );
        // Git ref patterns include descendants; only the selected ref itself is its upstream.
        if exact_names.is_some_and(|names| !names.contains(&name)) {
            continue;
        }
        let previous = result.insert(
            name,
            Ref {
                sha: sha.to_owned(),
                upstream: std::str::from_utf8(fields[2])?.to_owned(),
                symbolic: std::str::from_utf8(fields[3])?.to_owned(),
                tracking_config: configured,
            },
        );
        ensure!(
            previous.is_none(),
            "branch inventory contains a duplicate ref"
        );
    }
    if !bytes.is_empty() {
        ensure!(
            bytes.last() == Some(&b'\n'),
            "branch inventory is truncated"
        );
    }
    ensure!(
        result
            .values()
            .all(|value| value.upstream.is_empty() || value.upstream.starts_with("refs/")),
        "tracking ref has an invalid namespace"
    );
    Ok(result)
}

fn tracking_configuration(repo: &Repo) -> crate::Result<TrackingConfig> {
    let output = repo.inspection_output(
        &[
            "config",
            "--null",
            "--get-regexp",
            r"^branch\..*\.(remote|merge)$",
        ],
        MAX_REF_BYTES,
    )?;
    if output.status.code() == Some(1) && output.stdout.is_empty() && output.stderr.is_empty() {
        return Ok(BTreeMap::new());
    }
    ensure!(
        output.status.success() && output.stderr.is_empty() && output.stdout.last() == Some(&0),
        "branch tracking configuration cannot be completely inspected"
    );
    let mut result = TrackingConfig::new();
    for record in std::str::from_utf8(&output.stdout)?.split_terminator('\0') {
        let (key, value) = record.split_once('\n').ok_or_else(|| {
            anyhow::anyhow!("branch tracking configuration framing is incomplete")
        })?;
        let key = key
            .strip_prefix("branch.")
            .ok_or_else(|| anyhow::anyhow!("tracking configuration has an unexpected section"))?;
        let (branch, field) = key
            .rsplit_once('.')
            .ok_or_else(|| anyhow::anyhow!("tracking configuration has an incomplete key"))?;
        ensure!(
            matches!(field, "remote" | "merge"),
            "tracking configuration has an unexpected field"
        );
        result
            .entry(branch.to_owned())
            .or_default()
            .push((field.to_owned(), value.to_owned()));
    }
    Ok(result)
}
