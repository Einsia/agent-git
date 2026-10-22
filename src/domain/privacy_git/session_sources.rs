//! Unclaimed birth records inherit only an unambiguous descendant session identity.

use super::*;
use crate::domain::session_privacy::{AccountIdentity, SessionPrivacy};

pub(super) struct Birth {
    pub privacy: Option<SessionPrivacy>,
    pub committer: Option<AccountIdentity>,
}

pub(super) struct History {
    pub births: BTreeMap<String, Birth>,
    aliases: BTreeMap<String, String>,
    /// Metadata of every frozen commit that has readable metadata, read once.
    metadata: BTreeMap<String, Meta>,
}

impl History {
    pub fn read(repo: &Repo, plan: &PublicationPlan) -> Result<Self> {
        let parents = commit_parent_map(repo, plan.commit_objects())?;
        let mut records = BTreeMap::new();
        let mut cached = BTreeMap::new();
        for oid in plan.commit_objects() {
            // Accepted opaque ancestors need no metadata interpretation; admission validates new objects.
            let Ok(metadata) = super::super::super::storage::metadata_local(repo.root(), oid)
            else {
                continue;
            };
            cached.insert(oid.clone(), metadata.clone());
            if metadata.is_session_line() {
                records.insert(
                    oid.clone(),
                    (
                        metadata.session,
                        Birth {
                            privacy: metadata.session_privacy,
                            committer: metadata.committer_account,
                        },
                    ),
                );
            }
        }
        let order = parent_first(&parents)?;
        let mut descendants = BTreeMap::<String, BTreeSet<String>>::new();
        let mut aliases = BTreeMap::new();
        for oid in order.iter().rev() {
            let Some((session, _)) = records.get(oid) else {
                continue;
            };
            let identities = if session.is_empty() {
                descendants.get(oid).cloned().unwrap_or_default()
            } else {
                BTreeSet::from([session.clone()])
            };
            if session.is_empty() && identities.len() == 1 {
                aliases.insert(oid.clone(), identities.iter().next().unwrap().clone());
            }
            for parent in &parents[oid] {
                if records
                    .get(parent)
                    .is_some_and(|(session, _)| session.is_empty())
                {
                    descendants
                        .entry(parent.clone())
                        .or_default()
                        .extend(identities.iter().cloned());
                }
            }
        }
        let mut births = BTreeMap::<String, Birth>::new();
        for oid in &order {
            let Some((session, birth)) = records.remove(oid) else {
                continue;
            };
            let logical = aliases.get(oid).unwrap_or(&session);
            if !logical.is_empty() {
                match births.entry(logical.clone()) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(birth);
                    }
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        if let Some(privacy) = birth.privacy {
                            if let Some(previous) = &entry.get().privacy {
                                ensure!(
                                    previous.scheme == privacy.scheme,
                                    "source history changes session encryption mode"
                                );
                                if previous.owner_source
                                    != crate::domain::session_privacy::OwnerResolution::Unresolved
                                {
                                    ensure!(
                                        previous == &privacy,
                                        "source history changes recorded session ownership"
                                    );
                                }
                            }
                            entry.get_mut().privacy = Some(privacy);
                        }
                    }
                }
            }
        }
        Ok(Self {
            births,
            aliases,
            metadata: cached,
        })
    }

    /// Stored metadata of a frozen commit, without resolving an unclaimed birth's session.
    pub fn stored(&self, oid: &str) -> Option<&Meta> {
        self.metadata.get(oid)
    }

    pub fn metadata(&self, repo: &Repo, oid: &str) -> Result<Meta> {
        let mut metadata = match self.metadata.get(oid) {
            Some(metadata) => metadata.clone(),
            None => super::super::super::storage::metadata_local(repo.root(), oid)?,
        };
        if metadata.is_session_line() && metadata.session.is_empty() {
            metadata.session = self
                .aliases
                .get(oid)
                .context("unclaimed birth has no unambiguous descendant session")?
                .clone();
        }
        Ok(metadata)
    }
}
