//! A shared boundary between accepted RPC work and automatic daemon replacement.

use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub(super) struct Admission(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    closed: bool,
    active: usize,
}

pub(super) struct Work(Admission);

pub(super) struct Frozen {
    admission: Admission,
    committed: bool,
}

impl Admission {
    pub(super) fn enter(&self) -> Option<Work> {
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return None;
        }
        state.active += 1;
        Some(Work(self.clone()))
    }

    pub(super) fn freeze(&self) -> Result<Frozen, String> {
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return Err("daemon replacement is already in progress".into());
        }
        if state.active != 0 {
            return Err("accepted RPC requests or replies are still in flight".into());
        }
        state.closed = true;
        Ok(Frozen {
            admission: self.clone(),
            committed: false,
        })
    }
}

impl Drop for Work {
    fn drop(&mut self) {
        self.0.0.lock().unwrap().active -= 1;
    }
}

impl Frozen {
    pub(super) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        if !self.committed {
            self.admission.0.lock().unwrap().closed = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_work_and_replacement_exclude_each_other() {
        let gate = Admission::default();
        let work = gate.enter().unwrap();
        assert!(gate.freeze().is_err());
        drop(work);
        let frozen = gate.freeze().unwrap();
        assert!(gate.enter().is_none());
        drop(frozen);
        assert!(gate.enter().is_some());
        gate.freeze().unwrap().commit();
        assert!(gate.enter().is_none());
    }
}
