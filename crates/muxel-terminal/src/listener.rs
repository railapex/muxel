//! `alacritty_terminal` event listener: handles title/bell, queues OSC-52
//! copies, answers color queries, and writes PTY responses (cursor-position
//! reports, query replies) back to the child.

use crate::input::InputSender;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::ClipboardType;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

const MAX_TITLE_EVENTS: usize = 64;
const MAX_TITLE_EVENT_BYTES: usize = 16 * 1024;

#[derive(Default)]
pub(crate) struct TitleEventBuffer {
    events: VecDeque<Option<String>>,
    bytes: usize,
    overflowed: bool,
}

impl TitleEventBuffer {
    pub(crate) fn push(&mut self, title: Option<String>) {
        let bytes = title.as_ref().map_or(0, String::len);
        if bytes > MAX_TITLE_EVENT_BYTES {
            self.events.clear();
            self.bytes = 0;
            self.overflowed = true;
            return;
        }
        while self.events.len() >= MAX_TITLE_EVENTS
            || self.bytes.saturating_add(bytes) > MAX_TITLE_EVENT_BYTES
        {
            if let Some(dropped) = self.events.pop_front() {
                self.bytes -= dropped.as_ref().map_or(0, String::len);
                self.overflowed = true;
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.events.push_back(title);
    }

    pub(crate) fn take(&mut self) -> (Vec<Option<String>>, bool) {
        let events = self.events.drain(..).collect();
        self.bytes = 0;
        let overflowed = std::mem::take(&mut self.overflowed);
        (events, overflowed)
    }
}

/// Forwards terminal events to shared state. Lives inside the `Term`, so its
/// `send_event` runs wherever the VTE processor is advanced (the GPUI thread) —
/// and while the `Term` mutex is held, so it must never call back into the
/// session; it only touches its own shared state and the PTY writer.
pub(crate) struct MuxelListener {
    pub input: InputSender,
    pub title: Arc<Mutex<Option<String>>>,
    pub title_events: Arc<Mutex<TitleEventBuffer>>,
    /// Monotonic sequence for OSC title changes, including ResetTitle. Consumers
    /// use it to distinguish a repeated title event from an unchanged snapshot.
    pub title_generation: Arc<AtomicU64>,
    /// Advances only when a provider publishes a different UUID-shaped binding.
    /// Ordinary spinner/title frames for the same conversation do not churn it.
    pub binding_generation: Arc<AtomicU64>,
    pub title_changed_at: Arc<Mutex<Option<Instant>>>,
    /// Latest OSC title whose complete thread field is UUID-shaped. The app
    /// validates the provider's full title contract before adopting it.
    pub session_id_hint: Arc<Mutex<Option<String>>>,
    pub bell: Arc<AtomicBool>,
    /// OSC-52 copies from the child, drained by the view onto the system
    /// clipboard (a clipboard write needs a gpui context this thread lacks).
    pub clipboard_store: Arc<Mutex<Vec<(ClipboardType, String)>>>,
}

fn uuid_thread_field(title: &str) -> Option<&str> {
    let title = title.trim();
    let thread = title
        .split_once(" | ")
        .map_or(title, |(thread, _)| thread.trim());
    uuid::Uuid::parse_str(thread).is_ok().then_some(thread)
}

impl MuxelListener {
    fn write_reply(&self, reply: &str) {
        // InputSender latches and reports the first rejection in an episode.
        let _ = self.input.protocol(reply.as_bytes());
    }
}

impl EventListener for MuxelListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                let mut hint = self.session_id_hint.lock();
                if let Some(next) = uuid_thread_field(&title) {
                    let changed = hint.as_deref().and_then(uuid_thread_field) != Some(next);
                    *hint = Some(title.clone());
                    if changed {
                        // Frames queued before this title belong to the retired
                        // provider binding and cannot seed the new reducer.
                        self.title_events.lock().take();
                        self.binding_generation.fetch_add(1, Ordering::Relaxed);
                    }
                }
                self.title_events.lock().push(Some(title.clone()));
                *self.title.lock() = Some(title);
                *self.title_changed_at.lock() = Some(Instant::now());
                self.title_generation.fetch_add(1, Ordering::Relaxed);
            }
            Event::ResetTitle => {
                self.title_events.lock().push(None);
                *self.title.lock() = None;
                *self.title_changed_at.lock() = Some(Instant::now());
                self.title_generation.fetch_add(1, Ordering::Relaxed);
            }
            Event::Bell => self.bell.store(true, Ordering::Relaxed),
            // Apps query the terminal (cursor position, device attributes, …);
            // the reply must go back to the PTY's stdin.
            Event::PtyWrite(text) => self.write_reply(&text),
            // OSC-52 copy: alacritty hands over the already-base64-decoded text;
            // queue it for the view to land on the system clipboard.
            Event::ClipboardStore(ty, text) => self.clipboard_store.lock().push((ty, text)),
            // OSC-52 read: answer with a well-formed EMPTY reply. Returning real
            // clipboard contents would let any program that can write to this
            // PTY's stdout — including a compromised remote over SSH — silently
            // exfiltrate whatever the user last copied (often a password). The
            // empty reply keeps that hardening while TUIs that probe OSC-52
            // support with `52;c;?` (e.g. vim autodetect) get an answer instead
            // of hanging on a timeout.
            Event::ClipboardLoad(_, format) => {
                let reply = format("");
                self.write_reply(&reply);
            }
            // Color queries are answered synchronously on the PTY reader thread.
            // Waiting for this listener means waiting for the GPUI drain task,
            // which is late enough for a TUI to treat the reply as typed text.
            Event::ColorRequest(_, _) => {}
            // `Wakeup` and `ChildExit` are emitted only by alacritty's own
            // EventLoop, which muxel doesn't run: repaints are driven by the
            // view's drain task, and exit (with its code) comes from the PTY
            // reader's EOF. The remaining events (cursor blink, pointer shape,
            // text-area pixel size for CSI 14t) are intentionally unhandled.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_title_hint_keeps_only_complete_thread_candidates() {
        let hint = Arc::new(Mutex::new(None));
        let listener = MuxelListener {
            input: crate::input::input_queue(false).0,
            title: Arc::new(Mutex::new(None)),
            title_events: Arc::new(Mutex::new(TitleEventBuffer::default())),
            title_generation: Arc::new(AtomicU64::new(0)),
            binding_generation: Arc::new(AtomicU64::new(0)),
            title_changed_at: Arc::new(Mutex::new(None)),
            session_id_hint: hint.clone(),
            bell: Arc::new(AtomicBool::new(false)),
            clipboard_store: Arc::new(Mutex::new(Vec::new())),
        };
        let first = "019f95d7-db31-7db0-904d-9e08330e0000";
        let resumed = "019f95d7-db31-7db0-904d-9e08330e0001";

        let first_frame = format!("{first} | Starting · Booting");
        listener.send_event(Event::Title(first_frame.clone()));
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 1);
        assert_eq!(listener.binding_generation.load(Ordering::Relaxed), 1);
        assert_eq!(hint.lock().as_deref(), Some(first_frame.as_str()));
        listener.send_event(Event::Title(format!("Review {resumed} | Ready ·")));
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 2);
        assert_eq!(listener.binding_generation.load(Ordering::Relaxed), 1);
        assert_eq!(hint.lock().as_deref(), Some(first_frame.as_str()));

        listener.send_event(Event::Title(resumed.to_string()));
        assert_eq!(hint.lock().as_deref(), Some(resumed));
        assert_eq!(listener.binding_generation.load(Ordering::Relaxed), 2);
        let (events, overflowed) = listener.title_events.lock().take();
        assert!(!overflowed);
        assert_eq!(events, vec![Some(resumed.to_string())]);
        listener.send_event(Event::ResetTitle);
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 4);
        assert_eq!(*listener.title.lock(), None);
    }

    #[test]
    fn title_edges_are_bounded_and_report_overflow() {
        let mut buffer = TitleEventBuffer::default();
        for index in 0..=MAX_TITLE_EVENTS {
            buffer.push(Some(format!("title-{index}")));
        }
        let (events, overflowed) = buffer.take();
        assert_eq!(events.len(), MAX_TITLE_EVENTS);
        assert!(overflowed);
        assert_eq!(events.last().and_then(Option::as_deref), Some("title-64"));

        buffer.push(Some("x".repeat(MAX_TITLE_EVENT_BYTES + 1)));
        let (events, overflowed) = buffer.take();
        assert!(events.is_empty());
        assert!(overflowed);
    }
}
