//! Only an active native creator can prove that an unmaterialized transcript is empty.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};

type Key = (String, PathBuf);

struct Source {
    active: AtomicBool,
    path: PathBuf,
}

static SOURCES: OnceLock<Mutex<HashMap<Key, Arc<Source>>>> = OnceLock::new();

pub(crate) struct FreshCodex {
    key: Key,
    source: Arc<Source>,
}

impl FreshCodex {
    pub(crate) fn new(native: &str, cwd: &Path, path: PathBuf) -> Option<Self> {
        let mut sources = SOURCES.get_or_init(Default::default).lock().ok()?;
        let guard = Self {
            key: (native.to_owned(), cwd.to_owned()),
            source: Arc::new(Source {
                active: AtomicBool::new(true),
                path,
            }),
        };
        sources.insert(guard.key.clone(), guard.source.clone());
        Some(guard)
    }
}

impl Drop for FreshCodex {
    fn drop(&mut self) {
        self.source.active.store(false, Ordering::Release);
        if let Ok(mut sources) = SOURCES.get_or_init(Default::default).lock()
            && sources
                .get(&self.key)
                .is_some_and(|source| Arc::ptr_eq(source, &self.source))
        {
            sources.remove(&self.key);
        }
    }
}

pub(crate) fn is_empty(native: &str, cwd: &Path) -> bool {
    let Ok(sources) = SOURCES.get_or_init(Default::default).lock() else {
        return false;
    };
    let key = (native.to_owned(), cwd.to_owned());
    let Some(source) = sources.get(&key).cloned() else {
        return false;
    };
    drop(sources);
    if !source.active.load(Ordering::Acquire) {
        return false;
    }
    // Filesystem access holds no shared lock. Revocation precedes native input;
    // a final active observation captures the empty snapshot before that input.
    // Observing a source permanently retires the proof even if it is later deleted.
    if matches!(std::fs::metadata(&source.path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return source.active.load(Ordering::Acquire);
    }
    source.active.store(false, Ordering::Release);
    false
}
