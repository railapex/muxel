//! Bounded, nonblocking admission for bytes written to a terminal PTY.
//!
//! Admission is atomic at the transaction boundary: either every byte is
//! copied into the bounded queue or the caller receives an error before any
//! byte is accepted. The single writer drains one transaction completely
//! before starting the next, so a key can never land in the middle of a paste.

use parking_lot::{Condvar, Mutex};
use std::collections::VecDeque;
use std::fmt;
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

pub const INPUT_QUEUE_CAPACITY_BYTES: usize = 256 * 1024;
pub const INPUT_QUEUE_CAPACITY_CHUNKS: usize = 64;
pub const INPUT_WRITE_CHUNK_BYTES: usize = 8 * 1024;

const INTERACTIVE_RESERVE_BYTES: usize = 16 * 1024;
const PROTOCOL_RESERVE_BYTES: usize = 8 * 1024;
const INTERACTIVE_RESERVE_CHUNKS: usize = 2;
const PROTOCOL_RESERVE_CHUNKS: usize = 1;
pub const INPUT_MAX_BULK_BYTES: usize =
    INPUT_QUEUE_CAPACITY_BYTES - INTERACTIVE_RESERVE_BYTES - PROTOCOL_RESERVE_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InputClass {
    Bulk,
    Report,
    Interactive,
    Protocol,
}

impl InputClass {
    fn limits(self) -> (usize, usize) {
        match self {
            Self::Bulk | Self::Report => (
                INPUT_MAX_BULK_BYTES,
                INPUT_QUEUE_CAPACITY_CHUNKS - INTERACTIVE_RESERVE_CHUNKS - PROTOCOL_RESERVE_CHUNKS,
            ),
            Self::Interactive => (
                INPUT_QUEUE_CAPACITY_BYTES - PROTOCOL_RESERVE_BYTES,
                INPUT_QUEUE_CAPACITY_CHUNKS - PROTOCOL_RESERVE_CHUNKS,
            ),
            Self::Protocol => (INPUT_QUEUE_CAPACITY_BYTES, INPUT_QUEUE_CAPACITY_CHUNKS),
        }
    }
}

#[must_use]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputAdmissionError {
    Oversized { requested: usize, limit: usize },
    Full { requested: usize, available: usize },
    Closed,
}

impl fmt::Display for InputAdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Oversized { requested, limit } => write!(
                f,
                "terminal input is too large ({requested} bytes; limit {limit} bytes)"
            ),
            Self::Full {
                requested,
                available,
            } => write!(
                f,
                "terminal input queue is full ({requested} bytes requested; {available} available)"
            ),
            Self::Closed => f.write_str("terminal input is closed"),
        }
    }
}

impl std::error::Error for InputAdmissionError {}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PtyInputQueueStats {
    pub current_bytes: usize,
    pub current_chunks: usize,
    pub high_water_bytes: usize,
    pub high_water_chunks: usize,
    pub rejected_transactions: u64,
    pub canceled_bytes: u64,
    pub delivered_bytes: u64,
    pub closed: bool,
}

pub(crate) struct InputTransaction {
    pub(crate) bytes: Vec<u8>,
    pub(crate) queued_at: Option<Instant>,
    chunks: usize,
}

#[derive(Default)]
struct QueueState {
    queue: VecDeque<InputTransaction>,
    current_bytes: usize,
    current_chunks: usize,
    high_water_bytes: usize,
    high_water_chunks: usize,
    rejected_transactions: u64,
    canceled_bytes: u64,
    delivered_bytes: u64,
    closed: bool,
    inflight: Option<(usize, usize)>,
    warning: Option<InputAdmissionError>,
}

struct Shared {
    state: Mutex<QueueState>,
    ready: Condvar,
}

#[derive(Clone)]
pub(crate) struct InputSender {
    shared: Arc<Shared>,
    profile: bool,
}

pub(crate) struct InputReceiver {
    shared: Arc<Shared>,
}

pub(crate) fn input_queue(profile: bool) -> (InputSender, InputReceiver) {
    let shared = Arc::new(Shared {
        state: Mutex::new(QueueState::default()),
        ready: Condvar::new(),
    });
    (
        InputSender {
            shared: shared.clone(),
            profile,
        },
        InputReceiver { shared },
    )
}

fn chunk_count(bytes: usize) -> usize {
    bytes.div_ceil(INPUT_WRITE_CHUNK_BYTES)
}

pub(crate) fn write_in_chunks(
    writer: &mut dyn Write,
    bytes: &[u8],
    mut stopped: impl FnMut() -> bool,
) -> std::io::Result<usize> {
    let mut delivered = 0;
    for chunk in bytes.chunks(INPUT_WRITE_CHUNK_BYTES) {
        if stopped() {
            break;
        }
        writer.write_all(chunk)?;
        delivered += chunk.len();
    }
    Ok(delivered)
}

impl InputSender {
    pub(crate) fn preflight_bulk_len(&self, requested: usize) -> Result<(), InputAdmissionError> {
        if requested <= INPUT_MAX_BULK_BYTES {
            return Ok(());
        }
        let error = InputAdmissionError::Oversized {
            requested,
            limit: INPUT_MAX_BULK_BYTES,
        };
        let mut state = self.shared.state.lock();
        record_rejection(&mut state, &error);
        Err(error)
    }

    pub(crate) fn interactive(&self, bytes: &[u8]) -> Result<(), InputAdmissionError> {
        self.admit(bytes, InputClass::Interactive, false, || {})
    }

    /// Best-effort terminal reports (focus, pointer motion, wheel). These share
    /// bulk's limits so a stalled child cannot let many tiny reports consume the
    /// transaction slots reserved for actual keystrokes.
    pub(crate) fn report(&self, bytes: &[u8]) -> Result<(), InputAdmissionError> {
        self.admit(bytes, InputClass::Report, false, || {})
    }

    #[cfg(test)]
    pub(crate) fn user_interactive(&self, bytes: &[u8]) -> Result<(), InputAdmissionError> {
        self.user_interactive_with(bytes, || {})
    }

    pub(crate) fn user_interactive_with(
        &self,
        bytes: &[u8],
        on_admitted: impl FnOnce(),
    ) -> Result<(), InputAdmissionError> {
        self.admit(bytes, InputClass::Interactive, true, on_admitted)
    }

    #[cfg(test)]
    pub(crate) fn bulk(&self, bytes: &[u8]) -> Result<(), InputAdmissionError> {
        self.bulk_with(bytes, || {})
    }

    pub(crate) fn bulk_with(
        &self,
        bytes: &[u8],
        on_admitted: impl FnOnce(),
    ) -> Result<(), InputAdmissionError> {
        self.admit(bytes, InputClass::Bulk, true, on_admitted)
    }

    pub(crate) fn protocol(&self, bytes: &[u8]) -> Result<(), InputAdmissionError> {
        self.admit(bytes, InputClass::Protocol, false, || {})
    }

    fn admit(
        &self,
        bytes: &[u8],
        class: InputClass,
        clear_user_warning: bool,
        on_admitted: impl FnOnce(),
    ) -> Result<(), InputAdmissionError> {
        if bytes.is_empty() {
            return Ok(());
        }

        let chunks = chunk_count(bytes.len());
        let (byte_limit, chunk_limit) = class.limits();
        let mut state = self.shared.state.lock();
        let error = if state.closed {
            Some(InputAdmissionError::Closed)
        } else if bytes.len() > byte_limit || chunks > chunk_limit {
            Some(InputAdmissionError::Oversized {
                requested: bytes.len(),
                limit: byte_limit.min(chunk_limit * INPUT_WRITE_CHUNK_BYTES),
            })
        } else if state.current_bytes + bytes.len() > byte_limit
            || state.current_chunks + chunks > chunk_limit
        {
            Some(InputAdmissionError::Full {
                requested: bytes.len(),
                available: byte_limit.saturating_sub(state.current_bytes).min(
                    chunk_limit
                        .saturating_sub(state.current_chunks)
                        .saturating_mul(INPUT_WRITE_CHUNK_BYTES),
                ),
            })
        } else {
            None
        };

        if let Some(error) = error {
            record_rejection(&mut state, &error);
            return Err(error);
        }

        if clear_user_warning && state.current_bytes < INPUT_QUEUE_CAPACITY_BYTES / 2 {
            state.warning = None;
        }
        // Runtime facts must advance before the worker can observe the newly
        // accepted transaction. The queue lock keeps `recv` behind this hook.
        on_admitted();
        state.current_bytes += bytes.len();
        state.current_chunks += chunks;
        state.high_water_bytes = state.high_water_bytes.max(state.current_bytes);
        state.high_water_chunks = state.high_water_chunks.max(state.current_chunks);
        state.queue.push_back(InputTransaction {
            bytes: bytes.to_vec(),
            queued_at: self.profile.then(Instant::now),
            chunks,
        });
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }

    pub(crate) fn close(&self) {
        let mut state = self.shared.state.lock();
        if state.closed {
            return;
        }
        state.closed = true;
        let mut canceled_bytes = 0usize;
        let mut canceled_chunks = 0usize;
        for transaction in state.queue.drain(..) {
            canceled_bytes += transaction.bytes.len();
            canceled_chunks += transaction.chunks;
        }
        state.current_bytes = state.current_bytes.saturating_sub(canceled_bytes);
        state.current_chunks = state.current_chunks.saturating_sub(canceled_chunks);
        state.canceled_bytes = state.canceled_bytes.saturating_add(canceled_bytes as u64);
        drop(state);
        self.shared.ready.notify_all();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.shared.state.lock().closed
    }

    pub(crate) fn stats(&self) -> PtyInputQueueStats {
        let state = self.shared.state.lock();
        PtyInputQueueStats {
            current_bytes: state.current_bytes,
            current_chunks: state.current_chunks,
            high_water_bytes: state.high_water_bytes,
            high_water_chunks: state.high_water_chunks,
            rejected_transactions: state.rejected_transactions,
            canceled_bytes: state.canceled_bytes,
            delivered_bytes: state.delivered_bytes,
            closed: state.closed,
        }
    }

    pub(crate) fn warning(&self) -> Option<InputAdmissionError> {
        self.shared.state.lock().warning.clone()
    }
}

fn record_rejection(state: &mut QueueState, error: &InputAdmissionError) {
    state.rejected_transactions = state.rejected_transactions.saturating_add(1);
    if state.warning.is_none() && !matches!(error, InputAdmissionError::Closed) {
        log::warn!("terminal input admission failed: {error}");
    }
    state.warning = Some(error.clone());
}

impl InputReceiver {
    pub(crate) fn recv(&self) -> Option<InputTransaction> {
        let mut state = self.shared.state.lock();
        loop {
            if state.closed {
                return None;
            }
            if let Some(transaction) = state.queue.pop_front() {
                debug_assert!(state.inflight.is_none());
                state.inflight = Some((transaction.bytes.len(), transaction.chunks));
                return Some(transaction);
            }
            self.shared.ready.wait(&mut state);
        }
    }

    pub(crate) fn finish(&self, delivered: usize, failed: bool) {
        let mut state = self.shared.state.lock();
        let Some((bytes, chunks)) = state.inflight.take() else {
            return;
        };
        state.current_bytes = state.current_bytes.saturating_sub(bytes);
        state.current_chunks = state.current_chunks.saturating_sub(chunks);
        state.delivered_bytes = state.delivered_bytes.saturating_add(delivered as u64);
        state.canceled_bytes = state
            .canceled_bytes
            .saturating_add(bytes.saturating_sub(delivered) as u64);
        if failed {
            state.closed = true;
            let mut canceled_bytes = 0usize;
            let mut canceled_chunks = 0usize;
            for transaction in state.queue.drain(..) {
                canceled_bytes += transaction.bytes.len();
                canceled_chunks += transaction.chunks;
            }
            state.current_bytes = state.current_bytes.saturating_sub(canceled_bytes);
            state.current_chunks = state.current_chunks.saturating_sub(canceled_chunks);
            state.canceled_bytes = state.canceled_bytes.saturating_add(canceled_bytes as u64);
        }
        drop(state);
        if failed {
            self.shared.ready.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingWriter {
        bytes: Vec<u8>,
        writes: Vec<usize>,
    }

    impl Write for RecordingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes.push(buf.len());
            self.bytes.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn large_transaction_is_physically_chunked_without_changing_bytes() {
        let payload = vec![b'p'; 128 * 1024 + 17];
        let mut writer = RecordingWriter::default();
        let delivered = write_in_chunks(&mut writer, &payload, || false).unwrap();
        assert_eq!(delivered, payload.len());
        assert_eq!(writer.bytes, payload);
        assert!(
            writer
                .writes
                .iter()
                .all(|&len| len <= INPUT_WRITE_CHUNK_BYTES)
        );
        assert_eq!(writer.writes.len(), chunk_count(payload.len()));
    }

    #[test]
    fn oversized_bulk_rejects_atomically() {
        let (tx, rx) = input_queue(false);
        let limit = INPUT_QUEUE_CAPACITY_BYTES - INTERACTIVE_RESERVE_BYTES - PROTOCOL_RESERVE_BYTES;
        assert!(matches!(
            tx.bulk(&vec![b'x'; limit + 1]),
            Err(InputAdmissionError::Oversized { .. })
        ));
        assert_eq!(tx.stats().current_bytes, 0);
        tx.close();
        assert!(rx.recv().is_none());
    }

    #[test]
    fn admission_hook_runs_only_for_an_accepted_transaction() {
        let (tx, _rx) = input_queue(false);
        let bulk_limit =
            INPUT_QUEUE_CAPACITY_BYTES - INTERACTIVE_RESERVE_BYTES - PROTOCOL_RESERVE_BYTES;
        let mut admitted = 0;

        assert!(matches!(
            tx.bulk_with(&vec![b'x'; bulk_limit + 1], || admitted += 1),
            Err(InputAdmissionError::Oversized { .. })
        ));
        assert_eq!(admitted, 0);

        tx.bulk_with(b"accepted", || admitted += 1).unwrap();
        assert_eq!(admitted, 1);

        tx.bulk(&vec![b'p'; bulk_limit - b"accepted".len()])
            .unwrap();
        assert!(matches!(
            tx.bulk_with(b"full", || admitted += 1),
            Err(InputAdmissionError::Full { .. })
        ));
        assert_eq!(admitted, 1);
    }

    #[test]
    fn background_admission_keeps_warning_until_explicit_low_water_clear() {
        let (tx, rx) = input_queue(false);
        assert!(matches!(
            tx.preflight_bulk_len(INPUT_MAX_BULK_BYTES + 1),
            Err(InputAdmissionError::Oversized { .. })
        ));
        assert!(matches!(
            tx.warning(),
            Some(InputAdmissionError::Oversized { .. })
        ));
        assert_eq!(tx.stats().rejected_transactions, 1);
        assert_eq!(tx.stats().current_bytes, 0);

        tx.protocol(b"background").unwrap();
        assert!(tx.warning().is_some());
        let retry = rx.recv().unwrap();
        rx.finish(retry.bytes.len(), false);
        tx.user_interactive(b"retry").unwrap();
        assert!(tx.warning().is_none());
        let retry = rx.recv().unwrap();
        rx.finish(retry.bytes.len(), false);
    }

    #[test]
    fn admission_is_bounded_and_reserves_interactive_and_protocol_space() {
        let (tx, _rx) = input_queue(false);
        let bulk_limit =
            INPUT_QUEUE_CAPACITY_BYTES - INTERACTIVE_RESERVE_BYTES - PROTOCOL_RESERVE_BYTES;
        tx.bulk(&vec![b'p'; bulk_limit]).unwrap();
        assert!(matches!(
            tx.bulk(b"x"),
            Err(InputAdmissionError::Full { .. })
        ));
        tx.interactive(&vec![b'k'; INTERACTIVE_RESERVE_BYTES])
            .unwrap();
        tx.protocol(&vec![b'r'; PROTOCOL_RESERVE_BYTES]).unwrap();
        let stats = tx.stats();
        assert_eq!(stats.current_bytes, INPUT_QUEUE_CAPACITY_BYTES);
        assert_eq!(stats.high_water_bytes, INPUT_QUEUE_CAPACITY_BYTES);
        assert!(matches!(
            tx.protocol(b"!"),
            Err(InputAdmissionError::Full { .. })
        ));
    }

    #[test]
    fn accepted_paste_and_later_key_are_fifo_transactions() {
        let (tx, rx) = input_queue(false);
        let paste = vec![b'p'; 128 * 1024];
        tx.bulk(&paste).unwrap();
        tx.interactive(b"k").unwrap();

        let first = rx.recv().unwrap();
        assert_eq!(first.bytes, paste);
        rx.finish(first.bytes.len(), false);
        let second = rx.recv().unwrap();
        assert_eq!(second.bytes, b"k");
        rx.finish(second.bytes.len(), false);
        assert_eq!(tx.stats().current_bytes, 0);
    }

    #[test]
    fn protocol_reserve_does_not_change_fifo_dispatch() {
        let (tx, rx) = input_queue(false);
        tx.bulk(b"paste").unwrap();
        tx.protocol(b"reply").unwrap();
        tx.interactive(b"key").unwrap();

        for expected in [b"paste".as_slice(), b"reply", b"key"] {
            let transaction = rx.recv().unwrap();
            assert_eq!(transaction.bytes, expected);
            rx.finish(transaction.bytes.len(), false);
        }
    }

    #[test]
    fn reports_cannot_consume_interactive_chunk_reserve() {
        let (tx, _rx) = input_queue(false);
        let report_limit =
            INPUT_QUEUE_CAPACITY_CHUNKS - INTERACTIVE_RESERVE_CHUNKS - PROTOCOL_RESERVE_CHUNKS;
        for _ in 0..report_limit {
            tx.report(b"r").unwrap();
        }
        assert!(matches!(
            tx.report(b"r"),
            Err(InputAdmissionError::Full { .. })
        ));
        for _ in 0..INTERACTIVE_RESERVE_CHUNKS {
            tx.interactive(b"k").unwrap();
        }
        tx.protocol(b"p").unwrap();
        assert_eq!(tx.stats().current_chunks, INPUT_QUEUE_CAPACITY_CHUNKS);
    }

    #[test]
    fn rejected_submission_does_not_run_admission_hook() {
        let (tx, _rx) = input_queue(false);
        for _ in 0..(INPUT_QUEUE_CAPACITY_CHUNKS - PROTOCOL_RESERVE_CHUNKS) {
            tx.interactive(b"k").unwrap();
        }
        let mut admitted = false;
        assert!(matches!(
            tx.user_interactive_with(b"\r", || admitted = true),
            Err(InputAdmissionError::Full { .. })
        ));
        assert!(!admitted);
    }

    #[test]
    fn close_cancels_queued_and_inflight_bytes() {
        let (tx, rx) = input_queue(false);
        tx.interactive(b"inflight").unwrap();
        tx.interactive(b"queued").unwrap();
        let inflight = rx.recv().unwrap();
        tx.close();
        assert_eq!(tx.stats().current_bytes, inflight.bytes.len());
        rx.finish(0, false);
        let stats = tx.stats();
        assert_eq!(stats.current_bytes, 0);
        assert_eq!(stats.canceled_bytes, 14);
        assert!(stats.closed);
    }

    #[test]
    fn write_failure_closes_queue_and_accounts_every_byte() {
        let (tx, rx) = input_queue(false);
        tx.interactive(b"first").unwrap();
        tx.interactive(b"second").unwrap();
        let _first = rx.recv().unwrap();
        rx.finish(2, true);
        let stats = tx.stats();
        assert!(stats.closed);
        assert_eq!(stats.current_bytes, 0);
        assert_eq!(stats.delivered_bytes, 2);
        assert_eq!(stats.canceled_bytes, 9);
        assert!(rx.recv().is_none());
    }
}
