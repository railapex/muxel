//! What a provider's input box holds, read from its text alone.
//!
//! Colour can't decide it. A Claude started with `NO_COLOR` set (agent tool
//! shells set it, and panes inherit it) draws the suggestion in its empty box
//! in the same colour as typed text.

/// What Claude's input box holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaudeBox {
    /// Nothing visible in the box.
    Blank,
    /// Only the suggestion Claude draws in an empty box, one quoted phrase
    /// such as `Try "write a test for <filepath>"`, with the cursor at its start.
    Suggestion,
    /// Anything else: text a person or a paste put there.
    Typed,
}

/// What Claude's input box holds, from its text alone, so a Claude run with
/// `NO_COLOR` reads the same as one drawn in colour.
///
/// `rows` are the box's text rows, prompt row first, with the `❯` glyph
/// removed. Whitespace around each row and blank rows are ignored.
/// `cursor_at_start`: the cursor sits where the prompt row's text begins.
///
/// Every empty box recorded (Claude Code 2.1.310 to 2.1.322) is blank or
/// shows one quoted suggestion, wrapped or not, with the cursor at its start.
/// A draft that is itself one such phrase (`Try "…"`, closing quote and all)
/// reads as the suggestion while its cursor is at the start; only a record of
/// what was typed can tell the two apart.
pub fn read_claude_box(rows: &[&str], cursor_at_start: bool) -> ClaudeBox {
    let text = rows
        .iter()
        .map(|row| row.trim())
        .filter(|row| !row.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return ClaudeBox::Blank;
    }
    let quoted = text
        .strip_prefix("Try \"")
        .is_some_and(|phrase| phrase.len() > 1 && phrase.ends_with('"'));
    if cursor_at_start && quoted {
        ClaudeBox::Suggestion
    } else {
        ClaudeBox::Typed
    }
}

#[cfg(test)]
mod tests {
    use super::{ClaudeBox, read_claude_box};

    /// Box rows as recorded (casts on Claude Code 2.1.310, 2.1.312 and 2.1.322,
    /// colourless, and muxel's own spawn under `NO_COLOR`), glyph removed.
    #[test]
    fn claude_box_reads_blank_suggestion_or_typed_from_text() {
        use ClaudeBox::{Blank, Suggestion, Typed};
        for (rows, cursor_at_start, expected) in [
            // composer-tour idle-first and draft-cleared
            (
                &[r#" Try "write a test for <filepath>""#][..],
                true,
                Suggestion,
            ),
            // permission-parked-draft idle-first
            (&[r#" Try "refactor <filepath>""#], true, Suggestion),
            // a fresh pane through muxel's spawn
            (&[r#" Try "fix lint errors""#], true, Suggestion),
            // a suggestion wrapped in a narrow pane
            (
                &[
                    r#" Try "write a test for a function in"#,
                    r#"  <filepath>""#,
                ],
                true,
                Suggestion,
            ),
            // idle-after-picker, and every box after a turn
            (&[""], true, Blank),
            (&["", "  "], true, Blank),
            // draft-end, draft-mid and draft-home
            (&[" fix the login bug"], false, Typed),
            (&[" fix the login bug"], true, Typed),
            (
                &[" fix the login bug and", "  then the signup form"],
                true,
                Typed,
            ),
            // slash-menu-model, and the draft Claude restores after a dialog
            (&[" /model"], false, Typed),
            (&[" check it after"], false, Typed),
            // a draft that opens like the suggestion but isn't one phrase
            (&[r#" Try "x" first, then y"#], true, Typed),
            (&[r#" Try "unclosed"#], true, Typed),
            // the suggestion's text with the cursor moved off its start was typed
            (&[r#" Try "fix lint errors""#], false, Typed),
        ] {
            assert_eq!(read_claude_box(rows, cursor_at_start), expected, "{rows:?}");
        }
    }
}
