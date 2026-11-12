//! The pane bar: one row at the bottom of a pane for what happened to it and
//! what waits for it (stop-and-return design, § 3). Each line is a glyph, a
//! state and an action. This module decides which lines a pane shows and in
//! what order; the app words and draws them.

/// How a pane's program ended. Its last screen stays above the bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ending {
    /// Exit code 0.
    Clean,
    /// Any other exit code.
    Code(i32),
    /// Killed by a signal, named as the platform reports it.
    Signal(String),
    /// The terminal stopped reading before an exit status arrived.
    ReadFailed,
    /// No exit status at all.
    Unknown,
}

impl Ending {
    /// What the terminal knows about its exit: a read failure outranks a
    /// signal, and a signal outranks the code, since a signalled child also
    /// reports code 1.
    pub fn from_exit(read_failed: bool, signal: Option<&str>, code: Option<i32>) -> Self {
        if read_failed {
            Ending::ReadFailed
        } else if let Some(signal) = signal {
            Ending::Signal(signal.to_string())
        } else {
            match code {
                Some(0) => Ending::Clean,
                Some(code) => Ending::Code(code),
                None => Ending::Unknown,
            }
        }
    }
}

/// One line of a pane's bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BarLine {
    /// The program ended and the pane stays on its last screen.
    Ended(Ending),
    /// A remote pane lost its connection and muxel is reconnecting it.
    Reconnecting,
    /// Prompts typed and not sent, offered back.
    Unsent { count: usize },
}

/// What a line asks of the person, most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    /// Something was cut off or lost.
    Loss,
    /// Something waits on the person.
    Waiting,
    /// Worth knowing, nothing to do.
    Info,
}

impl BarLine {
    pub fn urgency(&self) -> Urgency {
        match self {
            BarLine::Ended(Ending::Clean) | BarLine::Reconnecting => Urgency::Info,
            BarLine::Ended(_) => Urgency::Loss,
            BarLine::Unsent { .. } => Urgency::Waiting,
        }
    }
}

/// A pane shows at most this many lines, so the bar never buries its screen.
pub const MAX_LINES: usize = 3;

/// The lines a pane shows: most urgent first, lines of equal urgency in the
/// order given, at most [`MAX_LINES`].
pub fn arrange(mut lines: Vec<BarLine>) -> Vec<BarLine> {
    lines.sort_by_key(BarLine::urgency);
    lines.truncate(MAX_LINES);
    lines
}

#[cfg(test)]
mod tests {
    use super::{BarLine, Ending, MAX_LINES, Urgency, arrange};

    #[test]
    fn an_exit_reads_as_its_most_telling_fact() {
        assert_eq!(
            Ending::from_exit(true, Some("SIGKILL"), Some(1)),
            Ending::ReadFailed
        );
        assert_eq!(
            Ending::from_exit(false, Some("SIGKILL"), Some(1)),
            Ending::Signal("SIGKILL".into())
        );
        assert_eq!(Ending::from_exit(false, None, Some(0)), Ending::Clean);
        assert_eq!(Ending::from_exit(false, None, Some(3)), Ending::Code(3));
        assert_eq!(Ending::from_exit(false, None, None), Ending::Unknown);
    }

    #[test]
    fn the_bar_puts_a_loss_first_and_keeps_three_lines() {
        let lines = arrange(vec![
            BarLine::Reconnecting,
            BarLine::Unsent { count: 2 },
            BarLine::Ended(Ending::Clean),
            BarLine::Ended(Ending::Code(1)),
        ]);
        assert_eq!(lines.len(), MAX_LINES);
        assert_eq!(lines[0], BarLine::Ended(Ending::Code(1)));
        assert_eq!(lines[1], BarLine::Unsent { count: 2 });
        // Equal urgency keeps the order given; the last info line is cut.
        assert_eq!(lines[2], BarLine::Reconnecting);
        // Only a clean exit is no loss.
        for ending in [
            Ending::Code(1),
            Ending::Signal("SIGTERM".into()),
            Ending::ReadFailed,
            Ending::Unknown,
        ] {
            assert_eq!(BarLine::Ended(ending).urgency(), Urgency::Loss);
        }
    }
}
