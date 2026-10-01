//! Compatibility inputs are read only; unavailable vaults never become empty replacements.

use super::dictionary::Record;
use zeroize::Zeroizing;

#[derive(Default)]
pub(crate) struct Snapshot {
    pub records: Vec<Record>,
    pub blocks: Vec<Zeroizing<String>>,
    pub allows: Vec<Zeroizing<String>>,
}
