//! A missing transcript is empty only while its own new launch proves that it is unwritten.

use super::Target;
use crate::protocol::{NativeSourceRef, SessionInfo};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};

type Registry = HashMap<String, (u64, Weak<Entry>)>;
static LAUNCHES: LazyLock<Mutex<Registry>> = LazyLock::new(Mutex::default);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Empty,
    Submitting,
    Writing,
    Retired,
}

struct State {
    native: Option<String>,
    phase: Phase,
}

struct Entry {
    logical: String,
    workspace: String,
    generation: u64,
    runtime: String,
    source: Option<NativeSourceRef>,
    cwd: PathBuf,
    confinement: tokio::sync::watch::Receiver<crate::rc::Confinement>,
    state: Mutex<State>,
}

/// Owned by one supervisor generation; neither the roster nor a resumed session recreates it.
#[derive(Default)]
pub(crate) struct Launch(Option<Arc<Entry>>);

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Missing {
    Empty,
    Submitting,
    Initializing,
}

pub(crate) fn retire(logical: &str) {
    if let Ok(mut launches) = LAUNCHES.lock()
        && let Some((_, entry)) = launches.remove(logical)
        && let Some(entry) = entry.upgrade()
        && let Ok(mut state) = entry.state.lock()
    {
        state.phase = Phase::Retired;
    }
}

/// Apply revocation before broadcasting new roots; re-adding a folder cannot revive proof.
pub(crate) fn confine(workspace: &str, roots: &crate::rc::policy::CanonicalRoots) {
    for entry in entries() {
        if entry.workspace == workspace
            && crate::rc::policy::require_within(&entry.cwd, roots)
                .ok()
                .as_ref()
                != Some(&entry.cwd)
            && let Ok(mut state) = entry.state.lock()
        {
            state.phase = Phase::Retired;
        }
    }
}

impl Launch {
    pub(crate) fn new(
        info: &SessionInfo,
        cwd: &Path,
        generation: u64,
        confinement: tokio::sync::watch::Receiver<crate::rc::Confinement>,
    ) -> Self {
        let Ok(cwd) = crate::rc::policy::require_within(cwd, &confinement.borrow().roots) else {
            return Self::default();
        };
        let entry = Arc::new(Entry {
            logical: info.session_id.clone(),
            workspace: info.workspace_id.clone(),
            generation,
            runtime: info.runtime.clone(),
            source: info.native_source.clone(),
            cwd,
            confinement,
            state: Mutex::new(State {
                native: None,
                phase: Phase::Empty,
            }),
        });
        retire(&info.session_id);
        let Ok(mut launches) = LAUNCHES.lock() else {
            return Self::default();
        };
        launches.insert(
            info.session_id.clone(),
            (generation, Arc::downgrade(&entry)),
        );
        Self(Some(entry))
    }

    pub(crate) fn bind(&self, native: Option<String>, path: Option<PathBuf>) {
        let Some(entry) = &self.0 else { return };
        if let Some(native) = native {
            let Ok(mut state) = entry.state.lock() else {
                return;
            };
            match &state.native {
                Some(bound) if bound != &native => state.phase = Phase::Retired,
                None => state.native = Some(native),
                _ => {}
            }
        }
        if let Some(path) = path {
            self.observe(&path);
        }
    }

    pub(crate) fn observe(&self, path: &Path) {
        if !matches!(path.try_exists(), Ok(false)) {
            self.finish();
        }
    }

    pub(crate) fn submitting(&self) {
        if let Some(entry) = &self.0
            && let Ok(mut state) = entry.state.lock()
            && state.phase == Phase::Empty
        {
            state.phase = Phase::Submitting;
        }
    }

    pub(crate) fn accepted(&self, running: bool) {
        if let Some(entry) = &self.0
            && let Ok(mut state) = entry.state.lock()
        {
            if !running {
                state.phase = Phase::Retired;
            } else if state.phase != Phase::Retired {
                state.phase = Phase::Writing;
            }
        }
    }

    pub(crate) fn not_accepted(&self) {
        if let Some(entry) = &self.0
            && let Ok(mut state) = entry.state.lock()
            && state.phase == Phase::Submitting
        {
            state.phase = Phase::Empty;
        }
    }

    pub(crate) fn finish(&self) {
        if let Some(entry) = &self.0
            && let Ok(mut state) = entry.state.lock()
        {
            state.phase = Phase::Retired;
        }
    }
}

impl Drop for Launch {
    fn drop(&mut self) {
        self.finish();
        if let Some(entry) = &self.0
            && let Ok(mut launches) = LAUNCHES.lock()
            && launches
                .get(&entry.logical)
                .is_some_and(|(generation, current)| {
                    *generation == entry.generation && current.ptr_eq(&Arc::downgrade(entry))
                })
        {
            launches.remove(&entry.logical);
        }
    }
}

fn entries() -> Vec<Arc<Entry>> {
    let Ok(launches) = LAUNCHES.lock() else {
        return vec![];
    };
    launches
        .values()
        .filter_map(|(_, entry)| entry.upgrade())
        .collect()
}

fn matching(target: &Target<'_>) -> Vec<Arc<Entry>> {
    let cwd = Path::new(target.cwd).canonicalize().ok();
    entries()
        .into_iter()
        .filter(|entry| {
            if entry.runtime != target.runtime
                || entry.source.as_ref() != target.source
                || (target.entry.is_some() && entry.logical != target.session)
            {
                return false;
            }
            // The launch's own directory must stay authorized even when the requested path
            // cannot be resolved. Recreating a removed directory cannot revive its proof.
            if (entry.confinement.has_changed().is_err()
                || entry.cwd.canonicalize().ok().as_ref() != Some(&entry.cwd)
                || crate::rc::policy::require_within(&entry.cwd, &entry.confinement.borrow().roots)
                    .ok()
                    .as_ref()
                    != Some(&entry.cwd))
                && let Ok(mut state) = entry.state.lock()
            {
                state.phase = Phase::Retired;
            }
            cwd.as_ref() == Some(&entry.cwd)
        })
        .collect()
}

pub(super) fn missing(target: &Target<'_>) -> Option<Missing> {
    matching(target).into_iter().find_map(|entry| {
        let mut state = entry.state.lock().ok()?;
        if state.native.as_deref() != Some(target.native) {
            return None;
        }
        if entry.confinement.has_changed().is_err()
            || entry.cwd.canonicalize().ok().as_ref() != Some(&entry.cwd)
            || crate::rc::policy::require_within(&entry.cwd, &entry.confinement.borrow().roots)
                .ok()
                .as_ref()
                != Some(&entry.cwd)
        {
            state.phase = Phase::Retired;
        }
        match state.phase {
            Phase::Empty => Some(Missing::Empty),
            Phase::Submitting => Some(Missing::Submitting),
            Phase::Writing => Some(Missing::Initializing),
            Phase::Retired => None,
        }
    })
}

pub(super) fn seen(target: &Target<'_>) {
    for entry in matching(target) {
        if let Ok(mut state) = entry.state.lock()
            && state.native.as_deref() == Some(target.native)
        {
            state.phase = Phase::Retired;
        }
    }
}
