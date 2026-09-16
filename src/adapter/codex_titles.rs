//! Native names supplement indexed opening previews without reading rollouts.

use super::SessionRef;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const INDEX_BYTES: u64 = 4 * 1024 * 1024;
const RECORD_BYTES: usize = 64 * 1024;

pub(super) fn preview(title: &str) -> Option<String> {
    let title = super::preview::shorten(title, super::preview::SESSION_PREVIEW_CHARS);
    (!title.is_empty()).then_some(title)
}

pub(super) fn apply_names(path: &Path, sessions: &mut [SessionRef]) {
    let _ = read_names(path, sessions);
}

fn read_names(path: &Path, sessions: &mut [SessionRef]) -> std::io::Result<()> {
    if sessions.is_empty() || !std::fs::symlink_metadata(path)?.is_file() {
        return Ok(());
    }
    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(INDEX_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(INDEX_BYTES).read_to_end(&mut bytes)?;
    // Only complete append records establish a name; the budget may start inside a record.
    let begin = if start > 0 {
        bytes.iter().position(|byte| *byte == b'\n').map(|i| i + 1)
    } else {
        Some(0)
    };
    let Some((begin, end)) = begin.zip(bytes.iter().rposition(|byte| *byte == b'\n')) else {
        return Ok(());
    };
    if begin > end {
        return Ok(());
    }
    let mut pending: HashMap<_, _> = sessions
        .iter()
        .enumerate()
        .map(|(index, session)| (session.id.clone(), index))
        .collect();
    #[derive(serde::Deserialize)]
    struct Name {
        id: String,
        thread_name: String,
    }
    // Codex appends renames, so a newer name must hide every older entry for that identity.
    for line in bytes[begin..end].rsplit(|byte| *byte == b'\n') {
        if line.len() > RECORD_BYTES {
            continue;
        }
        let Ok(name) = serde_json::from_slice::<Name>(line) else {
            continue;
        };
        if let Some(index) = pending.remove(&name.id) {
            sessions[index].title = preview(&name.thread_name);
            if pending.is_empty() {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_titles_ignore_partial_records_and_stay_within_the_index_budget() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session_index.jsonl");
        let mut bytes = serde_json::json!({"id":"older", "thread_name":"Outside the read budget"})
            .to_string()
            .into_bytes();
        bytes.push(b'\n');
        bytes.extend(vec![b'x'; INDEX_BYTES as usize]);
        bytes.extend_from_slice(b"\n{\"id\":\"recent\",\"thread_name\":\"Complete rename\"}\n{\"id\":\"recent\",\"thread_name\":\"Partial rename\"}");
        std::fs::write(&path, &bytes).unwrap();
        let mut sessions = ["older", "recent"].map(|id| SessionRef {
            id: id.into(),
            path: "missing-rollout".into(),
            runtime: "codex",
            cwd: None,
            mtime: std::time::UNIX_EPOCH,
            gist: Some("Opening prompt".into()),
            title: Some("Indexed title".into()),
        });
        read_names(&path, &mut sessions).unwrap();
        assert_eq!(sessions[0].title.as_deref(), Some("Indexed title"));
        assert_eq!(sessions[1].title.as_deref(), Some("Complete rename"));
        assert!(
            sessions
                .iter()
                .all(|session| session.gist.as_deref() == Some("Opening prompt"))
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}
