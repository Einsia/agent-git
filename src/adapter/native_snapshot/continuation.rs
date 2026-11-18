//! A logical native stream keeps only admitted replay ranges and its appended tail.

use super::{Unavailable, finish_unchanged, open_unchanged};
use crate::domain::link::continuation::{CONTEXT_BYTES, Carrier, Record};
use sha2::{Digest as _, Sha256};
use std::io::{Read as _, Seek as _, SeekFrom};

pub(crate) struct Read {
    pub context: Vec<Vec<u8>>,
    pub prefix: Sha256,
}

pub(crate) fn read(
    carriers: &[Carrier],
    checkpoint: u64,
    expected: &str,
    records: &[Record],
    mut inspect_prefix: impl FnMut(&[u8]) -> bool,
) -> super::Result<Option<Read>> {
    if carriers.is_empty() || carriers.len() > 128 || records.is_empty() || records.len() > 5 {
        return Err(Unavailable::BudgetExceeded);
    }
    if records[0].end != checkpoint {
        return Err(Unavailable::Incomplete);
    }
    let mut context_size = 0u64;
    for record in records {
        let size = record
            .end
            .checked_sub(record.start)
            .ok_or(Unavailable::Incomplete)?;
        context_size = context_size
            .checked_add(size)
            .ok_or(Unavailable::BudgetExceeded)?;
        if record.end > checkpoint || context_size > CONTEXT_BYTES as u64 {
            return Err(Unavailable::BudgetExceeded);
        }
    }
    let mut length = 0u64;
    let mut pinned = Vec::new();
    for carrier in carriers {
        let size = carrier
            .end
            .checked_sub(carrier.start)
            .ok_or(Unavailable::Incomplete)?;
        length = length
            .checked_add(size)
            .ok_or(Unavailable::BudgetExceeded)?;
        if length > super::MAX_CAPTURE_BYTES as u64 {
            return Err(Unavailable::BudgetExceeded);
        }
        let (file, metadata) = open_unchanged(&carrier.path, |_| Ok(()))?;
        if carrier.end > metadata.len() {
            return Ok(None);
        }
        pinned.push((file, metadata));
    }
    let Some(tail_size) = length.checked_sub(checkpoint) else {
        return Ok(None);
    };
    let mut context = Vec::new();
    for (index, record) in records.iter().enumerate() {
        let size = (record.end - record.start)
            .checked_add(if index == 0 { tail_size } else { 0 })
            .ok_or(Unavailable::BudgetExceeded)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(usize::try_from(size).map_err(|_| Unavailable::BudgetExceeded)?)
            .map_err(|_| Unavailable::BudgetExceeded)?;
        context.push(bytes);
    }
    let mut prefix = Sha256::new();
    let mut at = 0u64;
    let mut previous = None;
    let mut chunk = [0u8; 8192];
    for (carrier, (file, _)) in carriers.iter().zip(&mut pinned) {
        file.seek(SeekFrom::Start(carrier.start))
            .map_err(|_| Unavailable::Read)?;
        let mut left = carrier.end - carrier.start;
        while left > 0 {
            let count = usize::try_from(left.min(chunk.len() as u64)).unwrap();
            file.read_exact(&mut chunk[..count])
                .map_err(|_| Unavailable::Read)?;
            let end = at + count as u64;
            let prefix_end = end.min(checkpoint);
            if at < prefix_end {
                let count = (prefix_end - at) as usize;
                if !inspect_prefix(&chunk[..count]) {
                    return Ok(None);
                }
                prefix.update(&chunk[..count]);
                previous = Some(chunk[count - 1]);
            }
            for (record, collected) in records.iter().zip(&mut context) {
                let start = at.max(record.start);
                let stop = end.min(record.end);
                if start < stop {
                    collected
                        .extend_from_slice(&chunk[(start - at) as usize..(stop - at) as usize]);
                }
            }
            if end > checkpoint {
                let start = checkpoint.saturating_sub(at) as usize;
                context[0].extend_from_slice(&chunk[start..count]);
            }
            at = end;
            left -= count as u64;
        }
    }
    for (carrier, (file, opened)) in carriers.iter().zip(&pinned) {
        // The logical stream may select only a fork prefix of this pinned file.
        // Its entire physical identity and length must nevertheless stay fixed.
        finish_unchanged(
            file,
            &carrier.path,
            opened,
            usize::try_from(opened.len()).map_err(|_| Unavailable::BudgetExceeded)?,
        )?;
    }
    if previous != Some(b'\n') || hex::encode(prefix.clone().finalize()) != expected {
        return Ok(None);
    }
    if records
        .iter()
        .zip(&context)
        .enumerate()
        .any(|(index, (range, bytes))| {
            bytes.len() as u64 != range.end - range.start + if index == 0 { tail_size } else { 0 }
        })
    {
        return Err(Unavailable::Incomplete);
    }
    Ok(Some(Read { context, prefix }))
}
