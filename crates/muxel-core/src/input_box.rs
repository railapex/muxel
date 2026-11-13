//! What a provider's input box holds, read from its text alone.
//!
//! Colour can't decide it. A Claude started with `NO_COLOR` set (agent tool
//! shells set it, and panes inherit it) draws the suggestion in its empty box
//! in the same colour as typed text. Codex's placeholder is read by its text
//! too, so a caller that blanks dim cells first and one that doesn't agree.

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

/// What Codex's input box holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodexBox {
    /// Nothing visible in the box.
    Blank,
    /// Only the placeholder Codex draws in an empty box.
    Placeholder,
    /// Anything else: text a person or a paste put there.
    Typed,
}

/// The placeholder in every empty Codex box recorded (Codex 0.164.0 and
/// 0.167.2, default model).
const CODEX_PLACEHOLDER: &str = "Ask Codex to do anything";

/// What Codex's input box holds, from its text alone.
///
/// `rows` are the editor's rows from the `›` prompt row down to the row above
/// its footer, with the glyph removed. Whitespace around each row and blank
/// rows are ignored. The cursor isn't consulted: a draft with its cursor at
/// Home sits where the placeholder's cursor does, and is still a draft.
///
/// A draft that is exactly the placeholder's text reads as the placeholder;
/// only a record of what was typed can tell the two apart. Any other
/// placeholder reads as typed.
pub fn read_codex_box(rows: &[&str]) -> CodexBox {
    let mut text = rows
        .iter()
        .map(|row| row.trim())
        .filter(|row| !row.is_empty());
    match (text.next(), text.next()) {
        (None, _) => CodexBox::Blank,
        (Some(CODEX_PLACEHOLDER), None) => CodexBox::Placeholder,
        _ => CodexBox::Typed,
    }
}

#[cfg(test)]
mod tests {
    use super::{ClaudeBox, CodexBox, read_claude_box, read_codex_box};

    /// Editor rows as recorded (casts on Codex 0.164.0 and the 0.167.2
    /// composer tour), glyph removed.
    #[test]
    fn codex_box_reads_blank_placeholder_or_typed_from_text() {
        use CodexBox::{Blank, Placeholder, Typed};
        for (rows, expected) in [
            // idle-first, draft-backspaced, ctrl-u-cleared, after-turn, idle-last
            (&["  Ask Codex to do anything"][..], Placeholder),
            (&["  Ask Codex to do anything", ""], Placeholder),
            // a placeholder whose dim cells were blanked by the reader's caller
            (&[""], Blank),
            (&["", "   "], Blank),
            // draft-end, draft-mid and draft-home: the same text, any cursor
            (&[" fix the login bug"], Typed),
            // up-recall
            (&[" Reply with the single word ok."], Typed),
            // slash-menu-open and slash-menu-model
            (&[" /"], Typed),
            (&[" /mo"], Typed),
            // a draft over more than one row
            (&[" fix the login bug and", "then the signup form"], Typed),
            // drafts that open with the placeholder's words
            (&[" Ask Codex to do anything else"], Typed),
            (&[" Ask Codex to do anything", "and then stop"], Typed),
            // another placeholder is not on the list
            (&[" Explain this codebase"], Typed),
        ] {
            assert_eq!(read_codex_box(rows), expected, "{rows:?}");
        }
    }

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
