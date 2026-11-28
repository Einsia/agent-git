use super::{HistoryPoint, MAX_FORK_DEPTH, id_from_filename, next_parent_ordinal, split_rollout};
use crate::adapter::native_snapshot::{self, Limit, Limits, Source, Stage, Unavailable};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};

const RECORD_BYTES: usize = 8 * 1024 * 1024;

fn read_record(reader: &mut impl BufRead, line: &mut Vec<u8>) -> native_snapshot::Result<usize> {
    line.clear();
    let count = reader
        .take(RECORD_BYTES as u64 + 1)
        .read_until(b'\n', line)
        .map_err(|_| Unavailable::Read)?;
    if count > RECORD_BYTES {
        return Err(Unavailable::hit(
            Stage::Stream,
            Limit::RecordBytes,
            count,
            RECORD_BYTES,
        ));
    }
    if count != 0 && line.last() != Some(&b'\n') {
        return Err(Unavailable::Incomplete);
    }
    Ok(count)
}

pub(crate) fn visit(
    source: &Source,
    emit: impl FnMut(&[u8]) -> crate::Result<()>,
) -> crate::Result<()> {
    visit_with_lookup(source, false, native_snapshot::lookup_codex_rollout, emit)
}

pub(crate) fn visit_with_lookup(
    source: &Source,
    bound: bool,
    lookup: impl Fn(&str, Limits) -> native_snapshot::Result<Source>,
    mut emit: impl FnMut(&[u8]) -> crate::Result<()>,
) -> crate::Result<()> {
    let mut current = source.clone();
    let mut cut: Option<HistoryPoint> = None;
    let mut nodes = Vec::new();
    let mut visited = HashSet::new();
    let mut same_thread = true;
    let mut selected_header = None;
    loop {
        if nodes.len() >= MAX_FORK_DEPTH {
            return Err(Unavailable::hit(
                Stage::Reconstruct,
                Limit::Depth,
                nodes.len() + 1,
                MAX_FORK_DEPTH,
            )
            .into());
        }
        if !visited.insert(id_from_filename(&current.path).ok_or(Unavailable::Incomplete)?) {
            return Err(Unavailable::Incomplete.into());
        }
        let (file, opened) = native_snapshot::open_unchanged(&current.path, |_| Ok(()))?;
        let end = cut.as_ref().map_or(opened.len(), |point| point.byte_offset);
        if end > opened.len() {
            return Err(Unavailable::Incomplete.into());
        }
        let mut reader = BufReader::new(file);
        let mut header = Vec::new();
        let consumed = read_record(&mut reader, &mut header)? as u64;
        let (parts, history) =
            split_rollout(header, nodes.is_empty().then_some(&source.session_id))
                .map_err(|_| Unavailable::Incomplete)?;
        if consumed > end {
            return Err(Unavailable::Incomplete.into());
        }
        same_thread &= serde_json::from_slice::<serde_json::Value>(&parts.header)?
            .pointer("/payload/id")
            .and_then(|id| id.as_str())
            == Some(source.session_id.as_str());
        if nodes.is_empty() || (bound && same_thread) {
            selected_header = Some(parts.header);
        }
        let boundary = cut
            .map(|point| {
                Ok::<_, Unavailable>((
                    parts.header_ordinal.ok_or(Unavailable::Incomplete)?,
                    point.ordinal_exclusive,
                ))
            })
            .transpose()?;
        let extra = if bound && same_thread && opened.len() > end {
            let body = super::complete_body_len(
                &current.path,
                usize::try_from(consumed)?,
                usize::try_from(opened.len())?,
            )?;
            (consumed + body as u64)
                .checked_sub(end)
                .ok_or(Unavailable::Incomplete)?
        } else {
            0
        };
        nodes.push((
            reader,
            current.path,
            opened,
            end - consumed,
            boundary,
            extra,
        ));
        let Some(point) = history else {
            break;
        };
        current = lookup(
            &point.source_id,
            Limits {
                lookup_entries: Limits::default().lookup_entries.saturating_sub(nodes.len()),
                ..Limits::default()
            },
        )?;
        cut = Some(point);
    }
    let mut line = Vec::new();
    emit(&selected_header.ok_or(Unavailable::Incomplete)?)?;
    for (mut reader, path, opened, mut remaining, mut boundary, mut extra) in
        nodes.into_iter().rev()
    {
        while remaining != 0 {
            let count = read_record(&mut reader, &mut line)?;
            if count == 0 || count as u64 > remaining {
                return Err(Unavailable::Changed.into());
            }
            remaining -= count as u64;
            if let Some((previous, _)) = &mut boundary
                && line != b"\n"
            {
                *previous = next_parent_ordinal(*previous, &line)?;
            }
            emit(&line)?;
        }
        if boundary.is_some_and(|(last, exclusive)| last.checked_add(1) != Some(exclusive)) {
            return Err(Unavailable::Incomplete.into());
        }
        while extra != 0 {
            let count = read_record(&mut reader, &mut line)? as u64;
            if count == 0 || count > extra {
                return Err(Unavailable::Incomplete.into());
            }
            extra -= count;
            emit(&line)?;
        }
        native_snapshot::verify_unchanged(reader.get_ref(), &path, &opened)?;
    }
    Ok(())
}
