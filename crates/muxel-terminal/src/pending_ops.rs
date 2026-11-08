//! Input a terminal has admitted that can still reach its program later: a ctl
//! write queued for tmux, and the startup automation's timed writes. Each holds
//! a [`PendingOp`] until its last write and the facts that go with it are done,
//! so quitting can tell the terminal still has input on the way.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
pub(crate) struct PendingOps {
    count: AtomicUsize,
    /// Run when the count drops to zero, so a test can check what was done by
    /// then.
    #[cfg(test)]
    drained: parking_lot::Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl PendingOps {
    pub(crate) fn begin(self: &Arc<Self>) -> PendingOp {
        self.count.fetch_add(1, Ordering::AcqRel);
        PendingOp(self.clone())
    }

    pub(crate) fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn on_drained(&self, probe: impl Fn() + Send + Sync + 'static) {
        *self.drained.lock() = Some(Box::new(probe));
    }
}

/// One pending piece of input; dropping it ends it.
#[must_use = "input is pending only while its guard is held"]
pub(crate) struct PendingOp(Arc<PendingOps>);

impl std::fmt::Debug for PendingOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingOp")
    }
}

impl Drop for PendingOp {
    fn drop(&mut self) {
        let before = self.0.count.fetch_sub(1, Ordering::AcqRel);
        #[cfg(test)]
        if before == 1
            && let Some(probe) = self.0.drained.lock().as_ref()
        {
            probe();
        }
        #[cfg(not(test))]
        let _ = before;
    }
}
