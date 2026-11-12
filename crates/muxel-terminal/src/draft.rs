//! Reading the prompt someone is typing, so muxel can keep it if it crashes
//! or the pane is closed before the prompt is sent.
//!
//! The watcher sleeps until the user types. About a second after typing stops
//! it copies the visible screen under the terminal lock, finds the provider's
//! input box in that copy, puts paste markers back, and hands the draft to the
//! app's sink, all on a background thread. It writes only when the text
//! changed. Covered: Claude's and Codex's input boxes. Grok, Muse, opencode
//! and Amp draw boxes muxel can't yet read, so their panes run no watcher. A
//! shell's command line is never read: shells keep their own history, and a
//! line typed at a prompt can hold a secret.
//!
//! A read that can't be sure of every character is flagged partial. The
//! screen alone never is: a terminal draws spaces at the end of a row, how
//! many spaces a wrap took, and spaces after the cursor as empty cells. When
//! what muxel typed into the box since it was last empty is known (see
//! [`muxel_core::rescue::TypedDraft`]) and the screen shows that text, the
//! typed text is the draft, whitespace and all. Even then a paste marker
//! muxel can't tie to one paste, or a box taller than the pane, leaves it
//! partial.

use crate::listener::MuxelListener;
use crate::session::TerminalSession;
use crate::view::TitleProvider;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use muxel_core::input_box::{ClaudeBox, read_claude_box};
use muxel_core::rescue::{
    DraftAction, DraftRead, DraftSource, DraftTracker, PromptDraft, RowWidths, debounce_remaining,
    has_paste_markers, join_rows, text_columns, typed_matches,
};
use std::sync::Arc;
use std::time::Instant;

/// What the draft watcher tells the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftUpdate {
    /// The pane's draft changed: replace the saved one.
    Save(PromptDraft),
    /// The box emptied or the draft was submitted: delete the saved one.
    Clear,
}

/// Receives a pane's draft updates on a background thread. It does the file
/// work itself; the UI thread is never involved.
pub type DraftSink = Arc<dyn Fn(DraftUpdate) + Send + Sync>;

/// Whether a pane running `provider` can have its draft read.
pub(crate) fn readable(provider: TitleProvider) -> bool {
    matches!(provider, TitleProvider::Claude | TitleProvider::Codex)
}

/// A suggestion, placeholder or hint drawn in the input area: dim or muted
/// text that isn't part of what was typed.
fn muted(cell: &Cell) -> bool {
    cell.flags.contains(Flags::DIM)
        || matches!(
            cell.fg,
            Color::Named(
                NamedColor::BrightBlack
                    | NamedColor::DimForeground
                    | NamedColor::DimBlack
                    | NamedColor::DimWhite
            ) | Color::Indexed(8)
        )
}

fn spacer(cell: &Cell) -> bool {
    cell.flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
}

/// The visible screen as two views of the same rows: `raw` keeps one char per
/// cell, as status detection reads it, to find the box; `content` drops wide
/// glyph spacers and blanks muted cells, to read the text in it.
pub(crate) struct DraftCapture {
    raw: Vec<String>,
    content: Vec<String>,
    /// Every non-blank cell of the row is muted: a placeholder row.
    muted_row: Vec<bool>,
    columns: usize,
    cursor_row: i32,
    cursor_column: usize,
    cursor_visible: bool,
    bracketed: bool,
}

impl DraftCapture {
    pub(crate) fn from_term(term: &Term<MuxelListener>) -> Self {
        let grid = term.grid();
        let rows = grid.screen_lines();
        let columns = grid.columns();
        let mut raw = Vec::with_capacity(rows);
        let mut content = Vec::with_capacity(rows);
        let mut muted_row = Vec::with_capacity(rows);
        for row in 0..rows {
            let line = &grid[Line(row as i32)];
            let mut chars = String::with_capacity(columns);
            let mut text = String::with_capacity(columns);
            let mut all_muted = true;
            for column in 0..columns {
                let cell = &line[Column(column)];
                chars.push(cell.c);
                if spacer(cell) {
                    continue;
                }
                if muted(cell) {
                    // Blank, keeping the columns the glyph took.
                    text.push(' ');
                    if cell.flags.contains(Flags::WIDE_CHAR) {
                        text.push(' ');
                    }
                    continue;
                }
                if !cell.c.is_whitespace() {
                    all_muted = false;
                }
                text.push(cell.c);
                if let Some(combining) = cell.zerowidth() {
                    text.extend(combining);
                }
            }
            raw.push(chars);
            content.push(text);
            muted_row.push(all_muted);
        }
        let cursor = grid.cursor.point;
        Self {
            raw,
            content,
            muted_row,
            columns,
            cursor_row: cursor.line.0,
            cursor_column: cursor.column.0,
            cursor_visible: term.mode().contains(TermMode::SHOW_CURSOR),
            bracketed: term.mode().contains(TermMode::BRACKETED_PASTE),
        }
    }

    fn screen(&self) -> crate::startup::Screen {
        let mut text = String::with_capacity(self.raw.iter().map(|row| row.len() + 1).sum());
        for row in &self.raw {
            text.push_str(row);
            text.push('\n');
        }
        crate::startup::Screen {
            text,
            cursor_column: self.cursor_column,
            cursor_row: self.cursor_row,
            cursor_visible: self.cursor_visible,
            bracketed_paste: self.bracketed,
        }
    }

    /// Rows `range` of the content without their first `indent` columns of
    /// prompt glyph and indent, and without the blank rows after row `keep`
    /// (padding under the text). Only blanks and the glyph are dropped: a row
    /// continuing a cut word can start a column early.
    fn text_rows(&self, range: std::ops::Range<usize>, indent: usize, keep: usize) -> Vec<String> {
        let first = range.start;
        let mut rows: Vec<String> = range
            .map(|row| {
                let content = &self.content[row];
                let margin = content
                    .chars()
                    .take(indent)
                    .take_while(|ch| ch.is_whitespace() || matches!(ch, '❯' | '›'))
                    .count();
                content
                    .chars()
                    .skip(margin)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        while rows.len() > 1
            && first + rows.len() - 1 > keep
            && rows.last().is_some_and(String::is_empty)
        {
            rows.pop();
        }
        rows
    }

    /// The spaces between the end of `row`'s text and the cursor when the
    /// cursor is on that row after the text: they were typed. Text starts
    /// `indent` columns in. A hidden cursor can be anywhere, so it shows
    /// none. Spaces after the cursor can't be seen at all.
    fn blanks_before_cursor(&self, row: usize, indent: usize) -> usize {
        if !self.cursor_visible || usize::try_from(self.cursor_row).ok() != Some(row) {
            return 0;
        }
        let end = text_columns(self.content[row].trim_end()).max(indent);
        self.cursor_column.saturating_sub(end)
    }

    /// The cursor sits at the start of the box's first row: an empty box
    /// shows its placeholder there.
    fn cursor_at(&self, row: usize, column: usize) -> bool {
        usize::try_from(self.cursor_row).ok() == Some(row) && self.cursor_column == column
    }
}

fn indent_of(row: &str) -> usize {
    row.chars().take_while(|ch| ch.is_whitespace()).count()
}

/// How a provider wraps its input box, measured on real 166-column screens
/// (Claude Code 2.1.319, Codex 0.167.0): the columns a row of text gets.
///
/// Claude: the rule's length less the prompt's columns and two more, one
/// fewer on later rows (the wrap carries its space over). Codex: every column
/// after the prompt glyph; the wrap's space is dropped.
fn claude_widths(edge: usize, indent: usize) -> RowWidths {
    let first = edge.saturating_sub(indent + 2);
    RowWidths {
        first,
        rest: first.saturating_sub(1),
    }
}

fn codex_widths(columns: usize, indent: usize) -> RowWidths {
    let first = columns.saturating_sub(indent);
    RowWidths { first, rest: first }
}

/// `newlines`: how many line breaks the pane's input could have typed into
/// the box, `None` when unknown (see [`muxel_core::rescue::input_newlines`]).
/// `typed`: what muxel typed into the box since it was last empty, when
/// known.
#[allow(clippy::too_many_arguments)]
fn text_read(
    capture: &DraftCapture,
    rows: Vec<String>,
    first_row: usize,
    indent: usize,
    widths: RowWidths,
    clipped: bool,
    newlines: Option<u64>,
    typed: Option<&str>,
) -> DraftRead {
    // A single muted row under the cursor is the box's placeholder.
    if rows.len() <= 1 && capture.cursor_at(first_row, indent) && capture.muted_row[first_row] {
        return DraftRead::Empty;
    }
    let lines: Vec<&str> = rows.iter().map(String::as_str).collect();
    let joined = join_rows(&lines, widths, newlines);
    if joined.text.trim().is_empty() {
        return DraftRead::Empty;
    }
    // The typed text, when the screen shows it, is the draft as typed. A
    // paste the provider shows as a marker isn't on screen to compare.
    if !clipped
        && let Some(typed) = typed
        && !has_paste_markers(&joined.text)
        && typed_matches(typed, &lines, widths)
    {
        return DraftRead::Text {
            text: typed.to_string(),
            partial: false,
            clipped: false,
        };
    }
    // Only the screen: spaces before the cursor were typed, but nothing
    // shows whether more whitespace ends a row, the cursor's included.
    let blanks = capture.blanks_before_cursor(first_row + rows.len() - 1, indent);
    let mut text = joined.text;
    text.extend(std::iter::repeat_n(' ', blanks));
    DraftRead::Text {
        text,
        partial: true,
        clipped,
    }
}

fn rule(row: &str) -> bool {
    let row = row.trim();
    row.chars().count() >= 8 && row.chars().all(|ch| ch == '─' || ch == '━')
}

/// Claude's input box: the `❯` row and the rows under it, between two
/// full-width rules. When the box is taller than the pane its top rule is off
/// screen; the rows still visible are read and flagged clipped.
pub(crate) fn claude(
    capture: &DraftCapture,
    newlines: Option<u64>,
    typed: Option<&str>,
) -> DraftRead {
    if !capture.bracketed {
        return DraftRead::Unknown;
    }
    let screen = capture.screen();
    if let Some(rows) = screen.claude_box_rows() {
        let indent = indent_of(&capture.raw[rows.start]) + 2;
        let edge = capture.raw[rows.end].trim_end().chars().count();
        let first = rows.start;
        // Every row between the rules is the text's, blank ones too.
        let keep = rows.end - 1;
        let lines = capture.text_rows(rows, indent, keep);
        let widths = claude_widths(edge, indent);
        // A Claude drawn without colour (NO_COLOR) doesn't dim its suggestion,
        // so only the suggestion's shape and muxel's copy of what was typed
        // tell it from a draft whose cursor went home.
        let shown: Vec<&str> = lines.iter().map(String::as_str).collect();
        if read_claude_box(&shown, capture.cursor_at(first, indent)) == ClaudeBox::Suggestion
            && !typed.is_some_and(|typed| typed_matches(typed, &shown, widths))
        {
            return DraftRead::Empty;
        }
        return text_read(
            capture, lines, first, indent, widths, false, newlines, typed,
        );
    }
    // Clipped: the last rule is the box's bottom edge, everything above it is
    // box (continuation rows are indented two columns), and the cursor is in
    // it. The footer below a box is short.
    let Some(bottom) = capture.raw.iter().rposition(|row| rule(row)) else {
        return DraftRead::Unknown;
    };
    let in_box = |row: &str| {
        let trimmed = row.trim_start();
        row.trim().is_empty() || row.starts_with("  ") || trimmed.starts_with("❯ ")
    };
    let cursor_inside = usize::try_from(capture.cursor_row).is_ok_and(|row| row < bottom);
    if bottom == 0
        || capture.raw.len() - bottom > 6
        || !cursor_inside
        || !capture.raw[..bottom].iter().all(|row| in_box(row))
        || capture.raw[..bottom]
            .iter()
            .all(|row| row.trim().is_empty())
    {
        return DraftRead::Unknown;
    }
    let edge = capture.raw[bottom].trim_end().chars().count();
    let rows = capture.text_rows(0..bottom, 2, bottom - 1);
    match text_read(
        capture,
        rows,
        0,
        2,
        claude_widths(edge, 2),
        true,
        newlines,
        typed,
    ) {
        DraftRead::Empty => DraftRead::Unknown,
        read => read,
    }
}

/// Codex's editor: the last `›` row above its footer and the indented rows
/// under it.
pub(crate) fn codex(
    capture: &DraftCapture,
    newlines: Option<u64>,
    typed: Option<&str>,
) -> DraftRead {
    let screen = capture.screen();
    let Some(rows) = screen.codex_editor_rows() else {
        return DraftRead::Unknown;
    };
    let indent = indent_of(&capture.raw[rows.start]) + 2;
    // Every row after the first is a continuation (indented) or blank; a
    // transcript row means the region isn't the editor alone.
    if capture.raw[rows.start + 1..rows.end]
        .iter()
        .any(|row| !row.trim().is_empty() && !row.chars().take(indent).all(char::is_whitespace))
    {
        return DraftRead::Unknown;
    }
    let first = rows.start;
    let clipped = first == 0;
    // Blank rows under the cursor are the editor's padding.
    let keep = usize::try_from(capture.cursor_row).unwrap_or(first);
    let lines = capture.text_rows(rows, indent, keep);
    // Codex's placeholder, in case it isn't drawn muted.
    if lines.len() == 1
        && lines[0] == "Ask Codex to do anything"
        && capture.cursor_at(first, indent)
    {
        return DraftRead::Empty;
    }
    text_read(
        capture,
        lines,
        first,
        indent,
        codex_widths(capture.columns, indent),
        clipped,
        newlines,
        typed,
    )
}

/// Whether `provider`'s input box is in `capture`: what a draft read can
/// find, holding text or empty. A dialog drawn in its place (a permission
/// question, a picker) hides it. `None` when muxel can't tell: a program
/// whose box it doesn't read, or a screen without the bracketed paste
/// Claude's and Codex's boxes ask for.
pub(crate) fn box_shown(capture: &DraftCapture, provider: TitleProvider) -> Option<bool> {
    let read = match provider {
        TitleProvider::Claude if capture.bracketed => claude(capture, None, None),
        TitleProvider::Codex if capture.bracketed => codex(capture, None, None),
        _ => return None,
    };
    Some(read != DraftRead::Unknown)
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Keep `session`'s saved draft current until the task is dropped. Parked on
/// the wake channel while nobody types, so an idle pane costs nothing.
pub(crate) async fn watch(
    session: Arc<TerminalSession>,
    provider: TitleProvider,
    executor: gpui::BackgroundExecutor,
    sink: DraftSink,
) {
    let events = session.draft_events();
    let mut tracker = DraftTracker::default();
    while events.recv().await.is_ok() {
        loop {
            if session.take_draft_submitted() && tracker.submitted() == DraftAction::Clear {
                sink(DraftUpdate::Clear);
            }
            match debounce_remaining(session.draft_idle_for()) {
                Some(wait) => {
                    // A key or a submission ends the wait early: a submission
                    // clears the file now, not a second later.
                    let woken = futures_lite::future::or(
                        async {
                            executor.timer(wait).await;
                            true
                        },
                        async { events.recv().await.is_ok() },
                    )
                    .await;
                    if !woken {
                        return;
                    }
                }
                None => {
                    // Wakeups queued by keys during the wait are covered by
                    // the read below, so it runs once per pause. Input or a
                    // submission that landed since the check re-arms the wait.
                    while events.try_recv().is_ok() {}
                    if debounce_remaining(session.draft_idle_for()).is_none() {
                        break;
                    }
                }
            }
        }
        let started = Instant::now();
        let read = session.read_draft(provider);
        let read_took = started.elapsed();
        match tracker.observe(read) {
            DraftAction::Save { text, partial } => {
                let bytes = text.len();
                sink(DraftUpdate::Save(PromptDraft {
                    text,
                    partial,
                    saved_at_ms: now_ms(),
                    source: DraftSource::Agent,
                    pane: None,
                }));
                log::debug!(
                    "draft saved: {bytes} bytes partial={partial} read={read_took:?} total={:?}",
                    started.elapsed()
                );
            }
            DraftAction::Clear => {
                sink(DraftUpdate::Clear);
                log::debug!("draft cleared");
            }
            DraftAction::Keep => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::DraftCapture;
    use muxel_core::rescue::DraftRead;

    /// A capture from plain rows; `muted` rows are placeholder rows.
    fn capture(rows: &[&str], cursor: (i32, usize), muted: &[usize]) -> DraftCapture {
        let columns = rows.iter().map(|r| r.chars().count()).max().unwrap_or(0);
        let raw: Vec<String> = rows.iter().map(|r| format!("{r:<columns$}")).collect();
        let content = raw
            .iter()
            .enumerate()
            .map(|(i, r)| {
                if muted.contains(&i) {
                    // Placeholder cells read as blank, the glyph stays.
                    r.chars()
                        .enumerate()
                        .map(|(c, ch)| if c < 2 { ch } else { ' ' })
                        .collect()
                } else {
                    r.clone()
                }
            })
            .collect();
        let muted_row = (0..raw.len()).map(|i| muted.contains(&i)).collect();
        DraftCapture {
            raw,
            content,
            muted_row,
            columns,
            cursor_row: cursor.0,
            cursor_column: cursor.1,
            cursor_visible: true,
            bracketed: true,
        }
    }

    fn text(t: &str, clipped: bool) -> DraftRead {
        DraftRead::Text {
            text: t.into(),
            partial: false,
            clipped,
        }
    }

    const RULE: &str = "────────────────────────────────────────";

    #[test]
    fn claude_box_draft_rejoins_wraps_and_keeps_newlines() {
        // Rows of 38 columns: "that" couldn't have fit after the first row.
        let rule = "─".repeat(42);
        let screen = capture(
            &[
                "⏺ Earlier answer",
                "",
                &rule,
                "❯ please refactor the session module so",
                "  that it reads",
                "  second paragraph",
                &rule,
                "  ? for shortcuts",
            ],
            (5, 18),
            &[],
        );
        let typed = "please refactor the session module so that it reads\nsecond paragraph";
        // One newline typed, and the short row is it: the first break was the
        // wrap's space. How many spaces the wrap took, and whether spaces end
        // the first line, the screen can't show; what was typed can.
        assert_eq!(super::claude(&screen, Some(1), None), partial(typed));
        assert_eq!(
            super::claude(&screen, Some(1), Some(typed)),
            text(typed, false)
        );
        // Without knowing how many were typed, the first break could have been
        // a newline at the edge too.
        assert_eq!(
            super::claude(&screen, None, None),
            DraftRead::Text {
                text: typed.into(),
                partial: true,
                clipped: false,
            }
        );
    }

    #[test]
    fn claude_placeholder_and_empty_box_read_empty() {
        let placeholder = capture(
            &[RULE, "❯ Try \"fix lint errors\"", RULE, "  ? for shortcuts"],
            (1, 2),
            &[1],
        );
        assert_eq!(super::claude(&placeholder, None, None), DraftRead::Empty);
        let empty = capture(&[RULE, "❯ ", RULE], (1, 2), &[]);
        assert_eq!(super::claude(&empty, None, None), DraftRead::Empty);
        // Typed text with the cursor moved home is still a draft; spaces after
        // its end can't be seen.
        let home = capture(&[RULE, "❯ keep me", RULE], (1, 2), &[]);
        assert_eq!(super::claude(&home, None, None), partial("keep me"));
        assert_eq!(
            super::claude(&home, None, Some("keep me")),
            text("keep me", false)
        );
    }

    /// Claude Code 2.1.322 drawn without colour (NO_COLOR), as the
    /// composer-tour cast recorded it at 120 columns: the box after End and
    /// Ctrl+U (`draft-cleared`) and with a draft and the cursor at Home
    /// (`draft-home`). Neither row is dim and the cursor is at column 2 in
    /// both, so only the suggestion's shape tells them apart.
    #[test]
    fn claude_suggestion_drawn_without_colour_reads_empty_and_a_homed_draft_does_not() {
        let rule = "─".repeat(118);
        let cleared = capture(
            &[&rule, "❯\u{a0}Try \"write a test for <filepath>\"", &rule],
            (1, 2),
            &[],
        );
        assert_eq!(super::claude(&cleared, None, None), DraftRead::Empty);
        // muxel's copy says the box was emptied.
        assert_eq!(super::claude(&cleared, None, Some("")), DraftRead::Empty);
        let home = capture(&[&rule, "❯\u{a0}fix the login bug", &rule], (1, 2), &[]);
        assert_eq!(
            super::claude(&home, None, None),
            partial("fix the login bug")
        );
        // Typed by the person, the suggestion's shape is a draft.
        let typed = "Try \"write a test for <filepath>\"";
        assert_eq!(
            super::claude(&cleared, None, Some(typed)),
            text(typed, false)
        );
    }

    #[test]
    fn claude_box_taller_than_the_pane_is_read_as_clipped() {
        let screen = capture(
            &[
                "  line three of a long draft",
                "  line four",
                RULE,
                "  ? for shortcuts",
            ],
            (1, 11),
            &[],
        );
        assert_eq!(
            super::claude(&screen, None, None),
            DraftRead::Text {
                text: "line three of a long draft\nline four".into(),
                partial: true,
                clipped: true,
            }
        );
        // An answer above a rule, with no box, is not a draft.
        let transcript = capture(&["⏺ Done.", "  All tests pass.", RULE], (2, 0), &[]);
        assert_eq!(super::claude(&transcript, None, None), DraftRead::Unknown);
    }

    #[test]
    fn codex_editor_draft_and_placeholder() {
        let rows = [
            "• Ran cargo test",
            "",
            "› add a draft rescue",
            "  for crashed panes",
            "",
            "  gpt-5.4 high · ~/src/muxel",
        ];
        let screen = capture(&rows, (3, 19), &[]);
        let typed = "add a draft rescue\nfor crashed panes";
        assert_eq!(super::codex(&screen, None, None), partial(typed));
        assert_eq!(super::codex(&screen, None, Some(typed)), text(typed, false));
        let empty = capture(
            &[
                "",
                "› Ask Codex to do anything",
                "",
                "  ? for shortcuts            98% context left",
            ],
            (1, 2),
            &[],
        );
        assert_eq!(super::codex(&empty, None, None), DraftRead::Empty);
    }

    /// A long typed draft and the rows it wrapped into on a real 166-column
    /// pane (Claude Code 2.1.319, Codex 0.167.0), breaking before word
    /// `breaks[i]`.
    fn wrapped(breaks: &[usize]) -> (String, Vec<String>) {
        let words: Vec<String> = (0..70)
            .map(|i| format!("w{i}x{}", &"abcdefg"[..i % 7]))
            .collect();
        let typed = format!("{} keep me", words.join(" "));
        let mut rows = Vec::new();
        let mut start = 0;
        for &end in breaks.iter().chain([&words.len()]) {
            rows.push(words[start..end].join(" "));
            start = end;
        }
        rows.last_mut().unwrap().push_str(" keep me");
        (typed, rows)
    }

    #[test]
    fn long_drafts_rejoin_where_the_providers_wrapped_them() {
        let rule = "─".repeat(166);
        let (typed, rows) = wrapped(&[22, 41, 61]);
        let mut screen: Vec<String> = vec![String::new(), rule.clone()];
        screen.extend(
            rows.iter()
                .enumerate()
                .map(|(i, row)| format!("{}{row}", if i == 0 { "❯ " } else { "  " })),
        );
        screen.push(rule);
        screen.push("  ⏸ plan mode on (shift+tab to cycle)".into());
        let cursor = (5, 2 + rows[3].chars().count());
        let screen: Vec<&str> = screen.iter().map(String::as_str).collect();
        let claude = capture(&screen, cursor, &[]);
        assert_eq!(
            super::claude(&claude, Some(0), Some(&typed)),
            text(&typed, false)
        );
        // How many spaces each wrap took can't be seen.
        assert_eq!(super::claude(&claude, Some(0), None), partial(&typed));
        // A newline at a row's edge looks the same as a wrap.
        assert!(matches!(
            super::claude(&claude, None, None),
            DraftRead::Text { partial: true, .. }
        ));

        let (typed, rows) = wrapped(&[22, 42, 62]);
        let mut screen: Vec<String> = vec![String::new()];
        screen.extend(
            rows.iter()
                .enumerate()
                .map(|(i, row)| format!("{}{row}", if i == 0 { "› " } else { "  " })),
        );
        screen.push(String::new());
        screen.push(format!(
            "{:<166}",
            "  GPT-6.1-Sol high · Context 100% left · ~/src/app · app · main"
        ));
        let cursor = (4, 2 + rows[3].chars().count());
        let screen: Vec<&str> = screen.iter().map(String::as_str).collect();
        let codex = capture(&screen, cursor, &[]);
        assert_eq!(
            super::codex(&codex, Some(0), Some(&typed)),
            text(&typed, false)
        );
        assert_eq!(super::codex(&codex, Some(0), None), partial(&typed));
        assert!(matches!(
            super::codex(&codex, None, None),
            DraftRead::Text { partial: true, .. }
        ));
    }

    #[test]
    fn a_covered_or_missing_box_says_nothing() {
        let menu = capture(
            &["Do you want to proceed?", "❯ 1. Yes", "  2. No"],
            (1, 0),
            &[],
        );
        assert_eq!(super::claude(&menu, None, None), DraftRead::Unknown);
        assert_eq!(super::codex(&menu, None, None), DraftRead::Unknown);
    }

    fn partial(t: &str) -> DraftRead {
        DraftRead::Text {
            text: t.into(),
            partial: true,
            clipped: false,
        }
    }

    #[test]
    fn whitespace_the_screen_cannot_show_is_never_offered_as_exact() {
        // `line  \nnext`: the two spaces before the newline draw as empty
        // cells, so the screen alone reads `line\nnext`.
        let screen = capture(&[RULE, "❯ line", "  next", RULE], (2, 6), &[]);
        assert_eq!(super::claude(&screen, Some(1), None), partial("line\nnext"));
        // What muxel saw typed, when the screen shows it, is the draft.
        assert_eq!(
            super::claude(&screen, Some(1), Some("line  \nnext")),
            text("line  \nnext", false)
        );
        // Typed text the screen doesn't show isn't trusted.
        assert_eq!(
            super::claude(&screen, Some(1), Some("other\nnext")),
            partial("line\nnext")
        );

        // Two spaces where a line wrapped draw as one break.
        let rule = "─".repeat(16);
        let wrapped = capture(&[&rule, "❯ hello world", "  again", &rule], (2, 7), &[]);
        assert_eq!(
            super::claude(&wrapped, Some(0), None),
            partial("hello world again")
        );
        assert_eq!(
            super::claude(&wrapped, Some(0), Some("hello world  again")),
            text("hello world  again", false)
        );
    }

    #[test]
    fn blank_rows_and_spaces_before_the_cursor_are_part_of_the_draft() {
        // Enter at the end of a line: the cursor sits on the new blank row.
        let screen = capture(&[RULE, "❯ hello", "  ", RULE], (2, 2), &[]);
        assert_eq!(super::claude(&screen, Some(1), None), partial("hello\n"));
        assert_eq!(
            super::claude(&screen, Some(1), Some("hello\n")),
            text("hello\n", false)
        );
        // A space typed last: the cursor is after it.
        let mut screen = capture(&[RULE, "❯ hello ", RULE], (1, 8), &[]);
        assert_eq!(super::claude(&screen, Some(0), None), partial("hello "));
        assert_eq!(
            super::claude(&screen, Some(0), Some("hello ")),
            text("hello ", false)
        );
        // A hidden cursor can be anywhere: it doesn't show where the draft ends.
        screen.cursor_visible = false;
        assert_eq!(super::claude(&screen, Some(0), None), partial("hello"));
    }

    #[test]
    fn spaces_after_the_cursor_are_never_dropped_unlabelled() {
        // `hello   `, then Home and Right five times: the cursor is after
        // `hello`, and the three spaces after it draw as empty cells.
        let screen = capture(&[RULE, "❯ hello   ", RULE], (1, 7), &[]);
        assert_eq!(super::claude(&screen, Some(0), None), partial("hello"));
        let codex = capture(
            &[
                "",
                "› hello   ",
                "",
                "  ? for shortcuts            98% context left",
            ],
            (1, 7),
            &[],
        );
        assert_eq!(super::codex(&codex, Some(0), None), partial("hello"));
        // What was typed, when known, is the draft.
        assert_eq!(
            super::claude(&screen, Some(0), Some("hello   ")),
            text("hello   ", false)
        );
    }
}
