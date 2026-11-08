//! Reading the prompt someone is typing, so muxel can keep it if it crashes
//! or the pane is closed before the prompt is sent.
//!
//! The watcher sleeps until the user types. About a second after typing stops
//! it copies the visible screen under the terminal lock, finds the provider's
//! input box in that copy, puts paste markers back, and hands the draft to the
//! app's sink, all on a background thread. It writes only when the text
//! changed. Covered: Claude's and Codex's input boxes, and a shell's command
//! line after its OSC 133 B mark. Grok, Muse, opencode and Amp draw boxes
//! muxel can't yet read, so their panes run no watcher.

use crate::listener::MuxelListener;
use crate::session::TerminalSession;
use crate::view::TitleProvider;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, NamedColor};
use muxel_core::rescue::{
    DraftAction, DraftRead, DraftSource, DraftTracker, PromptDraft, debounce_remaining, join_rows,
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
    matches!(
        provider,
        TitleProvider::Claude | TitleProvider::Codex | TitleProvider::Other
    )
}

/// Where a shell's OSC 133 B mark left the cursor, which is where its
/// command input starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ShellInputMark {
    /// The row counted from the top of the scrollback, so scrolling the
    /// screen doesn't move it.
    absolute_line: i64,
    column: usize,
    /// The prompt text to the left of the mark. A read checks it is still
    /// there, so a full scrollback (rows shift without the count changing)
    /// or a cleared screen reads as unknown, not as someone else's text.
    prefix: String,
}

impl ShellInputMark {
    pub(crate) fn at_cursor(term: &Term<MuxelListener>) -> Self {
        let grid = term.grid();
        let cursor = grid.cursor.point;
        let row = &grid[cursor.line];
        let prefix = (0..cursor.column.0.min(grid.columns()))
            .map(|column| row[Column(column)].c)
            .collect();
        Self {
            absolute_line: grid.history_size() as i64 + i64::from(cursor.line.0),
            column: cursor.column.0,
            prefix,
        }
    }
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
                    text.push(' ');
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
    /// prompt glyph and indent, and without trailing blank rows. Only blanks
    /// and the glyph are dropped: a row continuing a cut word can start a
    /// column early.
    fn text_rows(&self, range: std::ops::Range<usize>, indent: usize) -> Vec<String> {
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
        while rows.last().is_some_and(|row| row.is_empty()) {
            rows.pop();
        }
        rows
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

/// How a provider wraps its input box, measured on real screens: the columns
/// a row of text gets, and what a row after the first spends on the space
/// carried over from the wrap (see [`join_rows`]).
#[derive(Clone, Copy)]
struct Wrap {
    width: usize,
    lead: usize,
}

impl Wrap {
    /// Claude: text gets the rule's length less the prompt's columns and
    /// two more, and keeps the wrap's space at the start of the next row.
    fn claude(edge: usize, indent: usize) -> Self {
        Self {
            width: edge.saturating_sub(indent + 2),
            lead: 1,
        }
    }

    /// Codex: text gets every column after the prompt glyph; the wrap's
    /// space is dropped.
    fn codex(columns: usize, indent: usize) -> Self {
        Self {
            width: columns.saturating_sub(indent),
            lead: 0,
        }
    }
}

fn text_read(
    capture: &DraftCapture,
    rows: Vec<String>,
    first_row: usize,
    indent: usize,
    wrap: Wrap,
    clipped: bool,
) -> DraftRead {
    // A single muted row under the cursor is the box's placeholder.
    if rows.len() <= 1 && capture.cursor_at(first_row, indent) && capture.muted_row[first_row] {
        return DraftRead::Empty;
    }
    let lines: Vec<&str> = rows.iter().map(String::as_str).collect();
    let text = join_rows(&lines, wrap.width, wrap.lead);
    if text.trim().is_empty() {
        DraftRead::Empty
    } else {
        DraftRead::Text {
            text,
            partial: false,
            clipped,
        }
    }
}

fn rule(row: &str) -> bool {
    let row = row.trim();
    row.chars().count() >= 8 && row.chars().all(|ch| ch == '─' || ch == '━')
}

/// Claude's input box: the `❯` row and the rows under it, between two
/// full-width rules. When the box is taller than the pane its top rule is off
/// screen; the rows still visible are read and flagged clipped.
pub(crate) fn claude(capture: &DraftCapture) -> DraftRead {
    if !capture.bracketed {
        return DraftRead::Unknown;
    }
    let screen = capture.screen();
    if let Some(rows) = screen.claude_box_rows() {
        let indent = indent_of(&capture.raw[rows.start]) + 2;
        let edge = capture.raw[rows.end].trim_end().chars().count();
        let first = rows.start;
        return text_read(
            capture,
            capture.text_rows(rows, indent),
            first,
            indent,
            Wrap::claude(edge, indent),
            false,
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
    let rows = capture.text_rows(0..bottom, 2);
    match text_read(capture, rows, 0, 2, Wrap::claude(edge, 2), true) {
        DraftRead::Empty => DraftRead::Unknown,
        read => read,
    }
}

/// Codex's editor: the last `›` row above its footer and the indented rows
/// under it.
pub(crate) fn codex(capture: &DraftCapture) -> DraftRead {
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
    let lines = capture.text_rows(rows, indent);
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
        Wrap::codex(capture.columns, indent),
        clipped,
    )
}

/// A shell's command line: the text from its last OSC 133 B mark to the end
/// of that line, through soft wraps. A dim suggestion after the cursor (an
/// autosuggestion or inline prediction) isn't part of it.
pub(crate) fn shell(term: &Term<MuxelListener>, mark: &ShellInputMark) -> DraftRead {
    let grid = term.grid();
    let rows = grid.screen_lines() as i64;
    let columns = grid.columns();
    let line = mark.absolute_line - grid.history_size() as i64;
    if !(0..rows).contains(&line) || mark.column > columns {
        return DraftRead::Unknown;
    }
    let first = &grid[Line(line as i32)];
    let prefix: String = (0..mark.column).map(|c| first[Column(c)].c).collect();
    if prefix != mark.prefix {
        return DraftRead::Unknown;
    }
    let cursor = grid.cursor.point;
    // (char, muted, after the cursor)
    let mut cells: Vec<(char, bool, bool)> = Vec::new();
    let mut row = line;
    let mut start = mark.column;
    loop {
        let grid_row = &grid[Line(row as i32)];
        for column in start..columns {
            let cell = &grid_row[Column(column)];
            if spacer(cell) {
                continue;
            }
            let after = (row as i32, column) >= (cursor.line.0, cursor.column.0);
            cells.push((cell.c, muted(cell), after));
        }
        let wrapped = columns > 0
            && grid_row[Column(columns - 1)]
                .flags
                .contains(Flags::WRAPLINE);
        if !wrapped || row + 1 >= rows {
            break;
        }
        row += 1;
        start = 0;
    }
    while cells
        .last()
        .is_some_and(|&(ch, muted, after)| ch.is_whitespace() || (muted && after))
    {
        cells.pop();
    }
    let text: String = cells.into_iter().map(|(ch, _, _)| ch).collect();
    if text.is_empty() {
        DraftRead::Empty
    } else {
        DraftRead::Text {
            text,
            partial: false,
            clipped: false,
        }
    }
}

/// What kind of input box `provider`'s drafts come from.
pub(crate) fn source(provider: TitleProvider) -> DraftSource {
    match provider {
        TitleProvider::Claude | TitleProvider::Codex => DraftSource::Agent,
        _ => DraftSource::Shell,
    }
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
    let source = source(provider);
    let mut tracker = DraftTracker::default();
    while events.recv().await.is_ok() {
        loop {
            if session.take_draft_submitted() && tracker.submitted() == DraftAction::Clear {
                sink(DraftUpdate::Clear);
            }
            match debounce_remaining(session.draft_idle_for()) {
                Some(wait) => executor.timer(wait).await,
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
                    source,
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
        let screen = capture(
            &[
                "⏺ Earlier answer",
                "",
                RULE,
                "❯ please refactor the session module so",
                "  that it reads",
                "  second paragraph",
                RULE,
                "  ? for shortcuts",
            ],
            (5, 19),
            &[],
        );
        assert_eq!(
            super::claude(&screen),
            text(
                "please refactor the session module so that it reads\nsecond paragraph",
                false
            )
        );
    }

    #[test]
    fn claude_placeholder_and_empty_box_read_empty() {
        let placeholder = capture(
            &[RULE, "❯ Try \"fix lint errors\"", RULE, "  ? for shortcuts"],
            (1, 2),
            &[1],
        );
        assert_eq!(super::claude(&placeholder), DraftRead::Empty);
        let empty = capture(&[RULE, "❯ ", RULE], (1, 2), &[]);
        assert_eq!(super::claude(&empty), DraftRead::Empty);
        // Typed text with the cursor moved home is still a draft.
        let home = capture(&[RULE, "❯ keep me", RULE], (1, 2), &[]);
        assert_eq!(super::claude(&home), text("keep me", false));
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
            super::claude(&screen),
            text("line three of a long draft\nline four", true)
        );
        // An answer above a rule, with no box, is not a draft.
        let transcript = capture(&["⏺ Done.", "  All tests pass.", RULE], (2, 0), &[]);
        assert_eq!(super::claude(&transcript), DraftRead::Unknown);
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
        assert_eq!(
            super::codex(&screen),
            text("add a draft rescue\nfor crashed panes", false)
        );
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
        assert_eq!(super::codex(&empty), DraftRead::Empty);
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
        assert_eq!(
            super::claude(&capture(&screen, cursor, &[])),
            text(&typed, false)
        );

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
        assert_eq!(
            super::codex(&capture(&screen, cursor, &[])),
            text(&typed, false)
        );
    }

    #[test]
    fn a_covered_or_missing_box_says_nothing() {
        let menu = capture(
            &["Do you want to proceed?", "❯ 1. Yes", "  2. No"],
            (1, 0),
            &[],
        );
        assert_eq!(super::claude(&menu), DraftRead::Unknown);
        assert_eq!(super::codex(&menu), DraftRead::Unknown);
    }
}
