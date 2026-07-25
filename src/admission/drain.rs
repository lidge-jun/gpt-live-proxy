//! Draining: refuse new work while shutting down, without dropping in-flight calls.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Clone, Debug, Default)]
pub struct DrainState(Arc<AtomicBool>);

impl DrainState {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn begin(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_draining(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draining_starts_false_and_latches_true() {
        let state = DrainState::new();
        assert!(!state.is_draining());
        state.begin();
        assert!(state.is_draining());
    }

    #[test]
    fn clones_share_one_flag() {
        let state = DrainState::new();
        let clone = state.clone();
        state.begin();
        assert!(
            clone.is_draining(),
            "a clone must observe the same shutdown"
        );
    }
}
