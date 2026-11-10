//! Input operations the app admitted that can still type into a pane later:
//! a ctl send, keys or answer, dictation, a remote upload's paste, the wake
//! sweep. Each holds a [`PendingInput`] for its whole life, so quitting can
//! tell they are there and ask instead of cutting them off.
//!
//! An operation takes its guard before it schedules anything and moves it into
//! the task that finishes it; the guard goes when the operation's last write
//! and bookkeeping are done. Each guard carries an [`Interrupt`]: once a
//! confirmed departure sets it, it stays set, and the operation stops at its
//! next stage boundary instead of starting new work. While a departure is
//! under way no new operation is admitted, so none starts between the
//! interruption and the exit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use uuid::Uuid;

/// What a pending operation is doing, for the quit confirmation's wording.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PendingKind {
    /// A ctl `send`, from admission to the end of its confirmation and retries.
    CtlSend,
    /// ctl `keys` or `answer`, from admission to the last press.
    CtlKeys,
    /// Dictation, from the start of the recording until the transcript is
    /// delivered or the recording is cancelled.
    Dictation,
    /// A remote file upload, until its paste lands or fails.
    Upload,
    /// Waking panes, until the sweep's last pane.
    WakeSweep,
}

/// One pending operation, as the app reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PendingEntry {
    pub kind: PendingKind,
    /// The pane it types into, when it has one.
    pub pane: Option<Uuid>,
}

/// Set when a confirmed departure interrupts an operation; never cleared, so
/// an operation that wakes after an aborted departure still stops.
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

/// A confirmed Quit or Restart is under way: no new input operation starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Departing;

#[derive(Default)]
struct Registry {
    next: u64,
    live: Vec<(u64, PendingEntry, Interrupt)>,
    departing: bool,
}

/// The app's pending input operations.
#[derive(Clone, Default)]
pub(crate) struct PendingInputs(Arc<Mutex<Registry>>);

impl PendingInputs {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Register an operation. Take this before scheduling anything it does.
    /// Refused while a departure is under way.
    pub(crate) fn begin(
        &self,
        kind: PendingKind,
        pane: Option<Uuid>,
    ) -> Result<PendingInput, Departing> {
        let interrupt = Interrupt::default();
        let mut registry = self.registry();
        if registry.departing {
            return Err(Departing);
        }
        registry.next += 1;
        let id = registry.next;
        registry
            .live
            .push((id, PendingEntry { kind, pane }, interrupt.clone()));
        Ok(PendingInput(Arc::new(Registration {
            registry: self.clone(),
            id,
            interrupt,
        })))
    }

    /// The operations still pending, oldest first.
    pub(crate) fn pending(&self) -> Vec<PendingEntry> {
        self.registry()
            .live
            .iter()
            .map(|(_, entry, _)| *entry)
            .collect()
    }

    /// Start a confirmed departure: interrupt every operation pending now, and
    /// admit no new one until [`Self::resume`]. Both happen under one lock, so
    /// an operation is either interrupted here or never starts. Returns the
    /// interrupted operations; their flags stay set even if the departure
    /// gives up.
    pub(crate) fn depart(&self) -> Vec<PendingEntry> {
        let mut registry = self.registry();
        registry.departing = true;
        for (_, _, interrupt) in &registry.live {
            interrupt.set();
        }
        registry.live.iter().map(|(_, entry, _)| *entry).collect()
    }

    /// The departure gave up: admit operations again. Those it interrupted
    /// stay interrupted.
    pub(crate) fn resume(&self) {
        self.registry().departing = false;
    }
}

/// One operation's registration. A clone is the same registration, so an
/// operation whose work passes between tasks stays pending, with no gap,
/// until its last clone drops.
#[must_use = "an operation is pending only while its guard is held"]
#[derive(Clone)]
pub(crate) struct PendingInput(Arc<Registration>);

struct Registration {
    registry: PendingInputs,
    id: u64,
    interrupt: Interrupt,
}

impl PendingInput {
    /// Whether a confirmed departure has interrupted this operation.
    pub(crate) fn interrupted(&self) -> bool {
        self.0.interrupt.is_set()
    }
}

impl std::fmt::Debug for PendingInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingInput")
            .field("id", &self.0.id)
            .field("interrupted", &self.interrupted())
            .finish()
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry
            .registry()
            .live
            .retain(|(id, _, _)| *id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::{Departing, PendingEntry, PendingInputs, PendingKind};
    use uuid::Uuid;

    #[test]
    fn an_interrupt_stays_set_and_reaches_only_operations_pending_then() {
        let inputs = PendingInputs::default();
        let pane = Uuid::new_v4();
        let send = inputs.begin(PendingKind::CtlSend, Some(pane)).unwrap();
        let sweep = inputs.begin(PendingKind::WakeSweep, None).unwrap();
        assert_eq!(
            inputs.depart(),
            [
                PendingEntry {
                    kind: PendingKind::CtlSend,
                    pane: Some(pane)
                },
                PendingEntry {
                    kind: PendingKind::WakeSweep,
                    pane: None
                },
            ]
        );
        // Nothing starts while the departure is under way.
        assert_eq!(
            inputs.begin(PendingKind::Dictation, None).unwrap_err(),
            Departing
        );
        assert_eq!(inputs.pending().len(), 2);
        // Once it gives up, a new operation starts uninterrupted.
        inputs.resume();
        let later = inputs.begin(PendingKind::Dictation, None).unwrap();
        assert!(send.interrupted() && sweep.interrupted());
        assert!(!later.interrupted());

        drop(sweep);
        assert_eq!(inputs.pending().len(), 2);
        // Nothing clears a flag once it is set.
        assert!(send.interrupted());
        // A clone is the same operation: listed once, pending until both go.
        let held = send.clone();
        drop(send);
        assert_eq!(inputs.pending().len(), 2);
        assert!(held.interrupted());
        drop((held, later));
        assert!(inputs.pending().is_empty());
    }
}
