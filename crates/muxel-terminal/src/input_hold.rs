//! Launch-local input cancellation and bounded OSC 133 shell evidence.

use muxel_core::prompt_marks::BUSY_MARK;
use muxel_core::recoverable_close::ShellReading;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub(crate) struct InputGate(AtomicU64);

impl InputGate {
    pub fn epoch(&self) -> u64 {
        self.0.load(Ordering::Acquire) >> 1
    }

    pub fn accepts(&self, epoch: u64) -> bool {
        self.0.load(Ordering::Acquire) == epoch << 1
    }

    pub fn hold(&self) {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                Some(state.wrapping_add(2) | 1)
            })
            .expect("input gate update always succeeds");
    }

    pub fn resume(&self) {
        self.0.fetch_and(!1, Ordering::AcqRel);
    }
}

/// What the shell's marks and the pane's input say about its prompt. The
/// reader thread applies marks and the input path applies writes, so the
/// fields change together under one lock.
#[derive(Default)]
pub(super) struct ShellPrompt(parking_lot::Mutex<PromptState>);

#[derive(Default)]
struct PromptState {
    /// The last OSC 133 mark was A or B, with no Enter written since.
    at_prompt: bool,
    /// That mark was [`BUSY_MARK`]: muxel's PowerShell wrapper found work
    /// running inside the shell when it drew this prompt, or couldn't check.
    busy: bool,
    /// Input that may still be waiting on a prompt: a draft, a recalled
    /// command, an unsent last line of a paste, or keys typed ahead while a
    /// command ran. Marks don't clear it, since a redraw (PSReadLine's Ctrl+L)
    /// keeps the line, and typeahead lands after the prompt is drawn. Only
    /// input that ends with Enter clears it, or a prompt drawn after Ctrl+C.
    typed: bool,
    /// The last input ended with Ctrl+C, so the line is being discarded. The
    /// next prompt the shell draws completes that.
    cancel: bool,
    /// Every OSC 133 mark the shell has printed, counted, whatever it changed.
    marks: u64,
}

impl ShellPrompt {
    pub fn at_prompt(&self) -> bool {
        self.0.lock().at_prompt
    }

    /// Whether the shell sits at a prompt not marked busy, with nothing typed
    /// since that could still run, and how many marks it has printed.
    pub fn reading(&self) -> ShellReading {
        let state = self.0.lock();
        ShellReading {
            idle: state.at_prompt && !state.busy && !state.typed,
            marks: state.marks,
        }
    }

    #[cfg(test)]
    fn idle_prompt(&self) -> bool {
        self.reading().idle
    }

    pub fn submitted(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut state = self.0.lock();
        let line_end = bytes
            .iter()
            .rposition(|&byte| matches!(byte, b'\r' | b'\n'));
        if line_end.is_some() {
            state.at_prompt = false;
        }
        // Anything after the last Enter may still run: text, an arrow or
        // history recall, Esc (a mode switch in PSReadLine's vi mode, not a
        // clear), Ctrl+L. Ctrl+C at the end discards the line, but only once
        // the shell draws its next prompt.
        let last_line = &bytes[line_end.map_or(0, |end| end + 1)..];
        state.typed = !last_line.is_empty();
        state.cancel = last_line.last() == Some(&0x03);
    }

    fn mark(&self, at_prompt: bool, busy: bool) {
        let mut state = self.0.lock();
        state.marks = state.marks.wrapping_add(1);
        state.at_prompt = at_prompt;
        state.busy = busy;
        if at_prompt && state.cancel {
            state.typed = false;
            state.cancel = false;
        }
    }
}

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Escape,
    Osc {
        /// The payload's first bytes, enough to match [`BUSY_MARK`].
        prefix: [u8; BUSY_MARK.len()],
        /// The payload's whole length so far.
        len: usize,
        escaped: bool,
    },
    String {
        escaped: bool,
    },
}

/// Retains at most 16 bytes, regardless of the size of an OSC payload.
#[derive(Default)]
pub(super) struct ShellMarks {
    state: State,
}

impl ShellMarks {
    pub fn advance(&mut self, bytes: &[u8], prompt: &ShellPrompt) {
        for &byte in bytes {
            let state = std::mem::take(&mut self.state);
            self.state = match state {
                State::Ground => {
                    if byte == 0x1b {
                        State::Escape
                    } else {
                        State::Ground
                    }
                }
                State::Escape => Self::escape(byte),
                State::String { escaped } => {
                    if matches!(byte, 0x18 | 0x1a) || (escaped && byte == b'\\') {
                        State::Ground
                    } else {
                        State::String {
                            escaped: byte == 0x1b,
                        }
                    }
                }
                State::Osc {
                    mut prefix,
                    mut len,
                    escaped,
                } => {
                    if matches!(byte, 0x18 | 0x1a) {
                        State::Ground
                    } else if (!escaped && byte == 0x07) || (escaped && byte == b'\\') {
                        if len >= 5 && &prefix[..4] == b"133;" {
                            let valid = len == 5 || prefix[5] == b';';
                            prompt.mark(
                                valid && matches!(prefix[4], b'A' | b'B'),
                                len == BUSY_MARK.len() && prefix == BUSY_MARK.as_bytes(),
                            );
                        }
                        State::Ground
                    } else if escaped {
                        Self::escape(byte)
                    } else if byte == 0x1b {
                        State::Osc {
                            prefix,
                            len,
                            escaped: true,
                        }
                    } else {
                        if len < prefix.len() {
                            prefix[len] = byte;
                        }
                        len = len.saturating_add(1);
                        State::Osc {
                            prefix,
                            len,
                            escaped: false,
                        }
                    }
                }
            };
        }
    }

    fn escape(byte: u8) -> State {
        match byte {
            b']' => State::Osc {
                prefix: [0; BUSY_MARK.len()],
                len: 0,
                escaped: false,
            },
            b'P' | b'X' | b'^' | b'_' => State::String { escaped: false },
            0x1b => State::Escape,
            // C0 controls execute without leaving VTE's Escape state.
            0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f..=0xff => State::Escape,
            _ => State::Ground,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{InputGate, ShellMarks, ShellPrompt};

    #[test]
    fn hold_changes_the_epoch_and_resume_only_clears_the_hold() {
        let gate = InputGate::default();
        let queued = gate.epoch();
        assert!(gate.accepts(queued));
        gate.hold();
        assert!(!gate.accepts(queued));
        assert!(!gate.accepts(gate.epoch()));
        gate.resume();
        assert!(!gate.accepts(queued));
        assert!(gate.accepts(gate.epoch()));
    }

    #[test]
    fn windows_shell_without_osc133_asks() {
        let prompt = ShellPrompt::default();
        let mut marks = ShellMarks::default();
        marks.advance(b"PS C:\\> ", &prompt);
        assert!(!prompt.at_prompt());
    }

    #[test]
    fn shell_marks_survive_every_read_split_and_both_terminators() {
        for bytes in [b"\x1b]133;A\x07".as_slice(), b"\x1b]133;B;aid=1\x1b\\"] {
            for split in 0..=bytes.len() {
                let prompt = ShellPrompt::default();
                let mut marks = ShellMarks::default();
                marks.advance(&bytes[..split], &prompt);
                marks.advance(&bytes[split..], &prompt);
                assert!(prompt.at_prompt(), "split {split}");
                prompt.submitted(b"command\r");
                assert!(!prompt.at_prompt());
                marks.advance(bytes, &prompt);
                assert!(prompt.at_prompt());
                marks.advance(b"\x1b]133;C\x07", &prompt);
                assert!(!prompt.at_prompt());
                marks.advance(bytes, &prompt);
                marks.advance(b"\x1b]133;D;0\x07", &prompt);
                assert!(!prompt.at_prompt());
            }
        }
    }

    /// A prompt as muxel's PowerShell wrapper draws it when the shell found
    /// nothing running, around a user prompt that prints its own marks.
    const IDLE: &[u8] = b"\x1b]133;A\x1b\\\x1b]133;A\x07PS> \x1b]133;B\x07\x1b]133;B\x1b\\";
    /// The same prompt when the shell found work running inside itself.
    const BUSY: &[u8] =
        b"\x1b]133;A\x1b\\\x1b]133;A\x07PS> \x1b]133;B\x07\x1b]133;B;muxel=busy\x1b\\";

    #[test]
    fn the_wrappers_closing_mark_decides_whether_the_prompt_is_busy() {
        for split in 0..=BUSY.len() {
            let prompt = ShellPrompt::default();
            let mut marks = ShellMarks::default();
            marks.advance(&BUSY[..split], &prompt);
            marks.advance(&BUSY[split..], &prompt);
            assert!(prompt.at_prompt(), "split {split}");
            assert!(!prompt.idle_prompt(), "split {split}");
            marks.advance(IDLE, &prompt);
            assert!(prompt.idle_prompt(), "split {split}, then an idle prompt");
        }
        // Only the wrapper's exact payload is busy; a B with other options is
        // a plain prompt mark, as from a prompt that prints its own.
        for plain in [
            b"\x1b]133;B;muxel=busy2\x07".as_slice(),
            b"\x1b]133;B;muxel=bus\x07",
            b"\x1b]133;B;aid=1\x07",
        ] {
            let prompt = ShellPrompt::default();
            let mut marks = ShellMarks::default();
            marks.advance(BUSY, &prompt);
            marks.advance(plain, &prompt);
            assert!(prompt.idle_prompt(), "{plain:?}");
        }
        // A command start, as a prompt module prints it and as muxel's line
        // editor wrap does, ends the prompt.
        for start in [b"\x1b]133;C\x07".as_slice(), b"\x1b]133;C\x1b\\"] {
            let prompt = ShellPrompt::default();
            let mut marks = ShellMarks::default();
            marks.advance(IDLE, &prompt);
            marks.advance(start, &prompt);
            assert!(!prompt.idle_prompt(), "a command started: {start:?}");
        }
    }

    #[test]
    fn every_mark_the_shell_prints_counts_even_one_that_changes_nothing() {
        let prompt = ShellPrompt::default();
        let mut marks = ShellMarks::default();
        marks.advance(IDLE, &prompt);
        let drawn = prompt.reading();
        assert!(drawn.idle);
        // The same idle prompt drawn again.
        marks.advance(IDLE, &prompt);
        let again = prompt.reading();
        assert!(again.idle);
        assert_ne!(again, drawn);
        // A mark that leaves the prompt's state as it was.
        marks.advance(b"\x1b]133;A\x07", &prompt);
        let unchanged = prompt.reading();
        assert!(unchanged.idle);
        assert_ne!(unchanged, again);
        // A command that started and ended between two readings.
        marks.advance(b"\x1b]133;C\x07", &prompt);
        marks.advance(IDLE, &prompt);
        assert_ne!(prompt.reading(), unchanged);
        // Other strings aren't marks.
        let settled = prompt.reading();
        marks.advance(b"\x1b]0;title\x07\x1b]1333;A\x07\x1b]2;133;A\x07", &prompt);
        assert_eq!(prompt.reading(), settled);
    }

    #[test]
    fn input_that_may_still_run_outlasts_every_prompt_redraw() {
        let drawn = |prompt: &ShellPrompt| ShellMarks::default().advance(IDLE, prompt);
        for pending in [
            b"Remove-Item *".as_slice(),
            // Ctrl+L redraws the prompt and keeps the line.
            b"Remove-Item *\x0c",
            // Up arrow recalls a command onto the line.
            b"\x1b[A",
            // A paste whose first line runs and whose last line waits.
            b"'FIRST'\rUNSENT",
            b"'FIRST'\r\nUNSENT",
            // Esc is a mode switch in PSReadLine's vi mode, not a clear.
            b"Remove-Item *\x1b",
            // Ctrl+C with input after it.
            b"\x03Remove-Item *",
        ] {
            let prompt = ShellPrompt::default();
            drawn(&prompt);
            prompt.submitted(pending);
            for _ in 0..3 {
                drawn(&prompt);
                assert!(!prompt.idle_prompt(), "{pending:?}");
            }
            // Enter sends it; the next idle prompt is idle.
            prompt.submitted(b"\r");
            assert!(!prompt.idle_prompt(), "{pending:?} sent");
            drawn(&prompt);
            assert!(prompt.idle_prompt(), "{pending:?} ran");
        }
        // Typed ahead while a command runs: the prompt it finishes with drew
        // before the shell read the keys.
        let prompt = ShellPrompt::default();
        drawn(&prompt);
        prompt.submitted(b"Start-Sleep 5\r");
        prompt.submitted(b"Remove-Item *");
        drawn(&prompt);
        assert!(!prompt.idle_prompt(), "typed ahead");
    }

    #[test]
    fn ctrl_c_clears_the_line_once_the_shell_draws_a_fresh_prompt() {
        let prompt = ShellPrompt::default();
        let mut marks = ShellMarks::default();
        marks.advance(IDLE, &prompt);
        prompt.submitted(b"Remove-Item *");
        prompt.submitted(b"\x03");
        assert!(!prompt.idle_prompt(), "before the shell redraws");
        marks.advance(IDLE, &prompt);
        assert!(prompt.idle_prompt(), "the fresh prompt");
        // Ctrl+C at an idle prompt is busy only until the redraw.
        prompt.submitted(b"\x03");
        assert!(!prompt.idle_prompt());
        marks.advance(BUSY, &prompt);
        assert!(prompt.at_prompt() && !prompt.idle_prompt(), "a busy redraw");
        marks.advance(IDLE, &prompt);
        assert!(prompt.idle_prompt());
    }

    #[test]
    fn shell_marks_reject_lookalikes_cancellation_and_nested_strings() {
        for bytes in [
            b"\x1b]133;Afoo\x07".as_slice(),
            b"\x1b]133;Z\x07",
            b"\x1b]133;A\x18\x07",
            b"\x1b]133;A\x1a\x07",
            b"\x1b]133;A\x1b\x07",
            b"\x1b]1333;A\x07",
            b"\x1b]2;133;A\x07",
            b"133;A",
            b"\x1bP\x1b]133;A\x07\x1b\\",
            b"\x1b_\x1b]133;A\x07\x1b\\",
        ] {
            let prompt = ShellPrompt::default();
            let mut marks = ShellMarks::default();
            marks.advance(bytes, &prompt);
            assert!(!prompt.at_prompt(), "{bytes:?}");
        }
    }

    #[test]
    fn shell_prompt_observation_ignores_paint_synchronization_and_bounds_payloads() {
        let prompt = ShellPrompt::default();
        let mut marks = ShellMarks::default();
        marks.advance(b"\x1b[?2026h\x1b]133;A\x07", &prompt);
        assert!(prompt.at_prompt());
        prompt.submitted(b"draft");
        assert!(prompt.at_prompt());
        marks.advance(b"\x1b]133;C;", &prompt);
        for _ in 0..100_000 {
            marks.advance(b"x", &prompt);
        }
        marks.advance(b"\x07", &prompt);
        assert!(!prompt.at_prompt());
        marks.advance(b"\x1b]133;B\x1b\\\x1b[?2026l", &prompt);
        assert!(prompt.at_prompt());
        assert!(
            !ShellPrompt::default().at_prompt(),
            "a new launch has no shell evidence"
        );
    }
}
