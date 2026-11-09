//! Input a terminal has admitted that can still reach its program later: a ctl
//! write queued for tmux, and the startup automation's timed writes. Each holds
//! a [`PendingOp`] until its last write and the facts that go with it are done,
//! so quitting can tell the terminal still has input on the way.
//!
//! Each op carries an [`Interrupt`]. Once a confirmed departure sets it, it
//! stays set, so the startup automation stops at its next stage even if the
//! departure gives up and input flows again. A write already queued for tmux
//! ignores it: admitted bytes still go.

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set when a confirmed departure interrupts an op; never cleared.
#[derive(Clone, Debug, Default)]
pub(crate) struct Interrupt(Arc<AtomicBool>);

impl Interrupt {
    pub(crate) fn is_set(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn set(&self) {
        self.0.store(true, Ordering::Release);
    }
}

#[derive(Default)]
struct Live {
    next: u64,
    ops: Vec<(u64, Interrupt)>,
}

#[derive(Default)]
pub(crate) struct PendingOps {
    live: Mutex<Live>,
    /// Run when the last op ends, so a test can check what was done by then.
    #[cfg(test)]
    drained: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl PendingOps {
    pub(crate) fn begin(self: &Arc<Self>) -> PendingOp {
        let interrupt = Interrupt::default();
        let mut live = self.live.lock();
        live.next += 1;
        let id = live.next;
        live.ops.push((id, interrupt.clone()));
        PendingOp {
            ops: self.clone(),
            id,
            interrupt,
        }
    }

    pub(crate) fn count(&self) -> usize {
        self.live.lock().ops.len()
    }

    /// Interrupt every op pending now, and return how many there were. An op
    /// begun later starts uninterrupted.
    pub(crate) fn interrupt_all(&self) -> usize {
        let live = self.live.lock();
        for (_, interrupt) in &live.ops {
            interrupt.set();
        }
        live.ops.len()
    }

    #[cfg(test)]
    pub(crate) fn on_drained(&self, probe: impl Fn() + Send + Sync + 'static) {
        *self.drained.lock() = Some(Box::new(probe));
    }
}

/// One pending piece of input; dropping it ends it.
#[must_use = "input is pending only while its guard is held"]
pub(crate) struct PendingOp {
    ops: Arc<PendingOps>,
    id: u64,
    interrupt: Interrupt,
}

impl PendingOp {
    /// Whether a confirmed departure has interrupted this op.
    pub(crate) fn interrupted(&self) -> bool {
        self.interrupt.is_set()
    }

    /// This op's interrupt, for code that checks it without owning the op.
    pub(crate) fn interrupt(&self) -> Interrupt {
        self.interrupt.clone()
    }
}

impl std::fmt::Debug for PendingOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingOp")
            .field("interrupted", &self.interrupted())
            .finish()
    }
}

impl Drop for PendingOp {
    fn drop(&mut self) {
        let drained = {
            let mut live = self.ops.live.lock();
            live.ops.retain(|(id, _)| *id != self.id);
            live.ops.is_empty()
        };
        #[cfg(test)]
        if drained && let Some(probe) = self.ops.drained.lock().as_ref() {
            probe();
        }
        #[cfg(not(test))]
        let _ = drained;
    }
}

#[cfg(test)]
mod tests {
    use super::PendingOps;
    use std::sync::Arc;

    #[test]
    fn an_interrupt_stays_set_and_reaches_only_ops_pending_then() {
        let ops = Arc::<PendingOps>::default();
        let startup = ops.begin();
        let queued = ops.begin();
        assert_eq!(ops.interrupt_all(), 2);
        let later = ops.begin();
        assert!(startup.interrupted() && queued.interrupt().is_set());
        assert!(!later.interrupted());
        drop(queued);
        assert_eq!(ops.count(), 2);
        assert!(startup.interrupted(), "nothing clears an interrupt");
        drop((startup, later));
        assert_eq!(ops.count(), 0);
    }
}
