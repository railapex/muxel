//! Rescue of text someone typed but hadn't sent or saved: a prompt drafted in
//! an agent's input box or at a shell prompt, and an editor's unsaved buffer.
//!
//! Pure decisions only. The terminal crate reads the draft off the screen, the
//! store writes the files, and the app shows the offers. Draft text never goes
//! to a log: callers report only lengths and counts.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::Duration;
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

/// How long input must stop before a pane's draft is read and saved.
pub const DRAFT_DEBOUNCE: Duration = Duration::from_millis(1000);

/// The largest rescue file written. A draft whose pastes would push it past
/// this is saved with its paste markers instead, flagged partial.
pub const MAX_RESCUE_BYTES: usize = 2 * 1024 * 1024;

/// The largest editor backup written, matching the largest file an editor
/// opens. A larger buffer isn't backed up.
pub const MAX_EDITOR_BACKUP_BYTES: usize = 8 * 1024 * 1024;

/// How many recent pastes a pane remembers for marker substitution.
pub const PASTE_LOG_ENTRIES: usize = 16;

/// How much paste text a pane remembers in total. A paste larger than this is
/// still counted, so the markers around it keep their order, but its text is
/// not kept.
pub const PASTE_LOG_BYTES: usize = 2 * 1024 * 1024;

/// Where a draft was typed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftSource {
    /// A coding agent's input box (Claude, Codex).
    #[default]
    Agent,
    /// A shell's command line, after its OSC 133 B mark.
    Shell,
}

/// A prompt that was typed but not sent, as saved for one pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptDraft {
    pub text: String,
    /// Some of the draft couldn't be recovered: its top was scrolled out of
    /// the pane, or a paste marker couldn't be matched to a paste muxel made.
    #[serde(default)]
    pub partial: bool,
    /// Unix milliseconds when the draft was read.
    pub saved_at_ms: i64,
    #[serde(default)]
    pub source: DraftSource,
    /// The pane's title, kept once the pane itself is closed for good.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
}

/// What one read of a pane's input box found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftRead {
    /// No input box muxel recognizes is on screen (a menu or permission
    /// question covers it, or the program draws none). Says nothing about the
    /// draft, so a saved one is kept.
    Unknown,
    /// The input box is on screen and holds nothing.
    Empty,
    /// The input box holds text.
    Text {
        text: String,
        /// A paste marker couldn't be matched, or an image or truncation
        /// marker stands in for content muxel never saw as text.
        partial: bool,
        /// The box's top edge is off screen, so earlier lines may be missing.
        clipped: bool,
    },
}

/// What to do with a pane's saved draft after a read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DraftAction {
    /// Write this text (it differs from what was last saved).
    Save { text: String, partial: bool },
    /// Delete the saved draft.
    Clear,
    /// Leave the file as it is.
    Keep,
}

/// The last draft saved for one pane in this launch, so a read that finds the
/// same text writes nothing.
#[derive(Clone, Debug, Default)]
pub struct DraftTracker {
    saved: Option<(String, bool)>,
}

impl DraftTracker {
    /// Decide what a read after [`DRAFT_DEBOUNCE`] does to the saved draft.
    pub fn observe(&mut self, read: DraftRead) -> DraftAction {
        let (text, partial, clipped) = match read {
            DraftRead::Unknown => return DraftAction::Keep,
            DraftRead::Empty => return self.clear(),
            DraftRead::Text {
                text,
                partial,
                clipped,
            } => (text, partial, clipped),
        };
        if text.trim().is_empty() {
            return self.clear();
        }
        let (text, partial) = match (&self.saved, clipped) {
            (Some((previous, _)), true) => (merge_clipped(previous, &text), true),
            (_, clipped) => (text, partial || clipped),
        };
        if self
            .saved
            .as_ref()
            .is_some_and(|(saved, was_partial)| *saved == text && *was_partial == partial)
        {
            return DraftAction::Keep;
        }
        self.saved = Some((text.clone(), partial));
        DraftAction::Save { text, partial }
    }

    /// A submission was admitted. The draft it sent is gone, so drop the file
    /// now; the next read saves anything still left in the box.
    pub fn submitted(&mut self) -> DraftAction {
        self.clear()
    }

    /// Forget what was saved, after the file was deleted or replaced elsewhere
    /// (a Restore or Discard), so the next read writes again.
    pub fn forget(&mut self) {
        self.saved = None;
    }

    /// Always a delete, even when this tracker saved nothing: the app can
    /// write the pane's file too (a restored offer), and deleting a missing
    /// file costs nothing.
    fn clear(&mut self) -> DraftAction {
        self.saved = None;
        DraftAction::Clear
    }
}

/// How much longer to wait for input to stop, `since_input` after the last
/// keystroke. `None` means read the draft now.
pub fn debounce_remaining(since_input: Duration) -> Option<Duration> {
    DRAFT_DEBOUNCE
        .checked_sub(since_input)
        .filter(|left| !left.is_zero())
}

/// The draft a clipped read shows, with the lines above the pane's top taken
/// from the draft saved before. `visible` starts somewhere inside `previous`
/// (its first line may be the tail of a wrapped line); when it can't be lined
/// up, only `visible` is provable.
pub fn merge_clipped(previous: &str, visible: &str) -> String {
    let prev: Vec<&str> = previous.split('\n').collect();
    let vis: Vec<&str> = visible.split('\n').collect();
    let Some(first) = vis.first().filter(|line| !line.trim().is_empty()) else {
        return visible.to_string();
    };
    let aligns = |i: usize| {
        if !prev[i].ends_with(first) {
            return false;
        }
        let rest = &prev[i + 1..];
        rest.iter()
            .enumerate()
            .all(|(j, line)| match vis.get(j + 1) {
                None => false,
                // Typing extends the last line; every earlier one is unchanged.
                Some(seen) if j + 1 == rest.len() => seen.starts_with(line),
                Some(seen) => seen == line,
            })
    };
    match (0..prev.len()).rev().find(|&i| aligns(i)) {
        Some(i) => {
            // The aligned line is whole in `previous`; only its tail shows.
            let head = &prev[i][..prev[i].len() - first.len()];
            let mut out: Vec<String> = prev[..i].iter().map(|line| line.to_string()).collect();
            out.push(format!("{head}{first}"));
            out.extend(vis[1..].iter().map(|line| line.to_string()));
            out.join("\n")
        }
        None => visible.to_string(),
    }
}

/// The terminal columns a provider's input box gives a row of text: its first
/// row, and each row after it (Claude spends one of those on the space the
/// wrap carried over; Codex doesn't).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowWidths {
    pub first: usize,
    pub rest: usize,
}

/// An input box's rows joined back into the text they show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Joined {
    pub text: String,
    /// Each row break had only one possible reading. When false, `text` is
    /// the likeliest reading and the draft must be offered as partial.
    pub exact: bool,
}

/// The readings of one row break.
const NEWLINE: u8 = 1;
const SPACE: u8 = 2;
const CUT: u8 = 4;

fn columns(text: &str) -> usize {
    text.width()
}

/// The terminal columns `text` takes.
pub fn text_columns(text: &str) -> usize {
    columns(text)
}

fn char_columns(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

/// A line can break here without a space: after a hyphen or a slash, or
/// beside a wide (CJK) character.
fn breaks_without_space(before: char, after: char) -> bool {
    matches!(before, '-' | '/' | '\\') || char_columns(before) > 1 || char_columns(after) > 1
}

/// The start of `row` up to where a line could next break: its first word,
/// ending early after a hyphen or slash, or one wide character.
fn first_segment(row: &str) -> &str {
    let mut end = 0;
    for (at, ch) in row.char_indices() {
        if ch.is_whitespace() {
            break;
        }
        if char_columns(ch) > 1 {
            if at == 0 {
                end = ch.len_utf8();
            }
            break;
        }
        end = at + ch.len_utf8();
        if matches!(ch, '-' | '/' | '\\') {
            break;
        }
    }
    &row[..end]
}

/// Join the rows of an input box back into the text they show, measuring in
/// terminal columns. A break between two rows was a typed newline, the space
/// a word wrap consumed, or a word cut at the edge, and each is possible only
/// where the rows allow it: a space only when the next row's first word
/// couldn't have fit on the row, and a cut only when the row is full and the
/// word is longer than a row, or where a line breaks without a space.
/// `newlines` is how many line breaks the pane's input could have put in the
/// box (`None`: unknown). When the breaks that can only be newlines account
/// for all of them, no other break is one.
///
/// The result is exact only when every break has one reading. A full row
/// followed by more text, a long unbroken word, or wide text that wraps is
/// usually ambiguous; the screen alone can't settle it.
pub fn join_rows(rows: &[&str], widths: RowWidths, newlines: Option<u64>) -> Joined {
    let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
    let (breaks, mut exact) = readings(&rows, widths, newlines);
    let mut text = String::with_capacity(rows.iter().map(|row| row.len() + 1).sum());
    for (index, row) in rows.iter().enumerate() {
        text.push_str(row);
        let Some(&can) = breaks.get(index) else {
            break;
        };
        if can.count_ones() != 1 {
            exact = false;
        }
        let next = rows[index + 1];
        let wide = row.chars().last().is_some_and(|ch| char_columns(ch) > 1)
            || next.chars().next().is_some_and(|ch| char_columns(ch) > 1);
        let one_word = !row.contains(char::is_whitespace);
        // The likeliest reading: a cut through wide text or one long word,
        // otherwise the space of a wrap, otherwise a newline.
        text.push_str(if can & CUT != 0 && (can == CUT || wide || one_word) {
            ""
        } else if can & SPACE != 0 {
            " "
        } else if can & CUT != 0 {
            ""
        } else {
            "\n"
        });
    }
    Joined { text, exact }
}

/// The readings each break between `rows` (trimmed) can have, and whether
/// the rows fit the box and the typed newlines at all.
fn readings(rows: &[&str], widths: RowWidths, newlines: Option<u64>) -> (Vec<u8>, bool) {
    let room = |index: usize| {
        if index == 0 {
            widths.first
        } else {
            widths.rest
        }
    };
    // A row wider than its box means the widths are wrong for this screen.
    let mut exact = rows
        .iter()
        .enumerate()
        .all(|(index, row)| columns(row) <= room(index));
    let mut breaks: Vec<u8> = rows
        .windows(2)
        .enumerate()
        .map(|(index, pair)| {
            let (row, next) = (pair[0], pair[1]);
            let (Some(last), Some(first)) = (row.chars().last(), next.chars().next()) else {
                // A blank row is a line of its own.
                return NEWLINE;
            };
            let used = columns(row);
            let next_word = next.split(char::is_whitespace).next().unwrap_or("");
            let mut can = NEWLINE;
            if used + 1 + columns(next_word) > room(index) {
                can |= SPACE;
            }
            if !first.is_whitespace() {
                let full = used + char_columns(first) > room(index);
                let tail = row.rsplit(char::is_whitespace).next().unwrap_or(row);
                let long_word = columns(tail) + columns(next_word) > room(index + 1);
                let segment_left = used + columns(first_segment(next)) > room(index);
                if (full && long_word) || (breaks_without_space(last, first) && segment_left) {
                    can |= CUT;
                }
            }
            can
        })
        .collect();
    let forced = breaks.iter().filter(|&&can| can == NEWLINE).count() as u64;
    match newlines {
        Some(limit) if forced > limit => exact = false,
        Some(limit) if forced == limit => {
            for can in breaks.iter_mut().filter(|can| **can != NEWLINE) {
                *can &= !NEWLINE;
            }
        }
        _ => {}
    }
    (breaks, exact)
}

/// Whether `typed` is text the box's `rows` show: the same characters on
/// every row, and between rows and after the last only whitespace the
/// screen can't show, at breaks whose reading the rows allow (a typed
/// newline where a line break fits, spaces where a wrap fits, nothing where
/// a cut fits).
pub fn typed_matches(typed: &str, rows: &[&str], widths: RowWidths) -> bool {
    let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
    let (breaks, _) = readings(&rows, widths, None);
    let blank = |ch: char| ch == ' ' || ch == '\t';
    let mut at = 0;
    for (index, row) in rows.iter().enumerate() {
        let Some(rest) = typed[at..].strip_prefix(row) else {
            return false;
        };
        let run = rest.len() - rest.trim_start_matches(blank).len();
        let after = &rest[run..];
        let Some(next) = rows.get(index + 1) else {
            return after.is_empty();
        };
        let can = breaks[index];
        if let Some(line) = after.strip_prefix('\n') {
            if can & NEWLINE == 0 {
                return false;
            }
            at = typed.len() - line.len();
            continue;
        }
        // A wrap: the run's end is the next row's own leading blanks.
        let lead = next.len() - next.trim_start_matches(blank).len();
        if !rest[..run].ends_with(&next[..lead]) {
            return false;
        }
        let taken = run - lead;
        if can & (if taken == 0 { CUT } else { SPACE }) == 0 {
            return false;
        }
        at = typed.len() - rest.len() + taken;
    }
    // No rows: only blanks were typed.
    typed[at..].chars().all(blank)
}

/// The text muxel typed into a pane's input box since the box was last seen
/// empty, while every key it sent typed or deleted at a cursor it can
/// follow: text, Backspace, Delete, Left and Right inside the text,
/// Shift+Enter, and bracketed pastes. Any other key (Enter, history,
/// completion, Home and End, a word jump, Right at the end where it can take
/// a suggestion) makes it unknown until the box is seen empty again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedDraft {
    text: Option<String>,
    /// Byte offset of the cursor in `text`.
    cursor: usize,
}

impl Default for TypedDraft {
    /// An empty box.
    fn default() -> Self {
        Self {
            text: Some(String::new()),
            cursor: 0,
        }
    }
}

/// A key that edits at the cursor.
enum EditKey {
    Left,
    Right,
    Delete,
    Newline,
}

impl TypedDraft {
    /// Nothing is known about what the box holds.
    pub fn unknown() -> Self {
        Self {
            text: None,
            cursor: 0,
        }
    }

    /// What was typed, when every key since the box was empty is known.
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }

    /// What was typed before the cursor, when known.
    pub fn before_cursor(&self) -> Option<&str> {
        self.text.as_deref().map(|text| &text[..self.cursor])
    }

    /// `input` was written to the pane.
    pub fn feed(&mut self, input: &[u8]) {
        if self.apply(input).is_none() {
            *self = Self::unknown();
        }
    }

    fn apply(&mut self, input: &[u8]) -> Option<()> {
        let text = self.text.as_mut()?;
        let mut at = 0;
        while at < input.len() {
            match input[at] {
                0x1b => {
                    let rest = &input[at..];
                    if let Some(body) = rest.strip_prefix(b"\x1b[200~") {
                        let end = body.windows(6).position(|w| w == b"\x1b[201~")?;
                        let paste = std::str::from_utf8(&body[..end]).ok()?;
                        let paste = paste.replace("\r\n", "\n").replace('\r', "\n");
                        text.insert_str(self.cursor, &paste);
                        self.cursor += paste.len();
                        at += 6 + end + 6;
                    } else {
                        let (len, key) = edit_key(rest)?;
                        match key {
                            EditKey::Left => {
                                let back = text[..self.cursor].chars().next_back();
                                self.cursor -= back.map_or(0, char::len_utf8);
                            }
                            EditKey::Right => {
                                // At the end, Right can take a suggestion.
                                self.cursor += text[self.cursor..].chars().next()?.len_utf8();
                            }
                            EditKey::Delete => {
                                if self.cursor < text.len() {
                                    text.remove(self.cursor);
                                }
                            }
                            EditKey::Newline => {
                                text.insert(self.cursor, '\n');
                                self.cursor += 1;
                            }
                        }
                        at += len;
                    }
                }
                0x7f | 0x08 => {
                    if let Some(back) = text[..self.cursor].chars().next_back() {
                        self.cursor -= back.len_utf8();
                        text.remove(self.cursor);
                    }
                    at += 1;
                }
                0x00..=0x1f => return None,
                _ => {
                    let end = input[at..]
                        .iter()
                        .position(|&byte| byte < 0x20 || byte == 0x7f)
                        .map_or(input.len(), |offset| at + offset);
                    let typed = std::str::from_utf8(&input[at..end]).ok()?;
                    text.insert_str(self.cursor, typed);
                    self.cursor += typed.len();
                    at = end;
                }
            }
            if text.len() > MAX_RESCUE_BYTES {
                return None;
            }
        }
        Some(())
    }
}

/// The editing key at the start of `input` (an escape sequence) and its
/// length; `None` for any other.
fn edit_key(input: &[u8]) -> Option<(usize, EditKey)> {
    match input.get(1)? {
        b'\r' => return Some((2, EditKey::Newline)),
        b'O' => {
            return match input.get(2)? {
                b'D' => Some((3, EditKey::Left)),
                b'C' => Some((3, EditKey::Right)),
                _ => None,
            };
        }
        b'[' => {}
        _ => return None,
    }
    let body = &input[2..];
    let params_len = body.iter().position(|byte| !(0x20..=0x3f).contains(byte))?;
    let params = std::str::from_utf8(&body[..params_len]).ok()?;
    let len = 2 + params_len + 1;
    let numbers: Vec<u32> = params
        .split(';')
        .map(|part| part.parse().unwrap_or(0))
        .collect();
    let key = match (body[params_len], numbers.as_slice()) {
        (b'D', [0]) => EditKey::Left,
        (b'C', [0]) => EditKey::Right,
        (b'~', [3]) => EditKey::Delete,
        // Shift+Enter, as the kitty and xterm keyboard protocols send it.
        (b'u', [13, 2]) | (b'~', [27, 2, 13]) => EditKey::Newline,
        _ => return None,
    };
    Some((len, key))
}

/// How many line breaks writing `input` to a pane can put in its input box:
/// each Enter however a terminal encodes it, and each line break inside a
/// paste. `None` when the input can bring back text muxel never saw typed
/// (history recall, search, undo, a yank, an external editor) or isn't
/// understood, so the box can hold any number.
pub fn input_newlines(input: &[u8]) -> Option<u64> {
    let mut count = 0u64;
    let mut at = 0;
    let mut pasting = false;
    while at < input.len() {
        let byte = input[at];
        if pasting {
            if input[at..].starts_with(b"\x1b[201~") {
                pasting = false;
                at += 6;
                continue;
            }
            if byte == b'\n' || (byte == b'\r' && input.get(at + 1) != Some(&b'\n')) {
                count += 1;
            }
            at += 1;
            continue;
        }
        match byte {
            b'\r' | b'\n' => {
                if !(byte == b'\r' && input.get(at + 1) == Some(&b'\n')) {
                    count += 1;
                }
                at += 1;
            }
            0x1b => match input.get(at + 1) {
                None | Some(0x1b) => at += 1,
                Some(b'[') => {
                    let (len, key) = csi(&input[at..])?;
                    match key {
                        Key::Safe => {}
                        Key::Enter => count += 1,
                        Key::PasteStart => pasting = true,
                    }
                    at += len;
                }
                Some(b'O') => match input.get(at + 2) {
                    Some(b'M') => {
                        count += 1;
                        at += 3;
                    }
                    Some(b'C' | b'D' | b'H' | b'F' | b'P' | b'Q' | b'R' | b'S') => at += 3,
                    _ => return None,
                },
                // Alt+Enter.
                Some(b'\r' | b'\n') => {
                    count += 1;
                    at += 2;
                }
                // Alt+Y cycles the kill ring.
                Some(b'y' | b'Y') => return None,
                Some(0x20..=0x7e) => at += 2,
                Some(_) => return None,
            },
            // Moving the cursor, deleting and clearing only take text away;
            // Tab completes a name; Ctrl+V pastes an image's marker.
            0x01..=0x06 | 0x08 | 0x09 | 0x0b | 0x0c | 0x14..=0x17 | 0x7f => at += 1,
            0x00..=0x1f => return None,
            _ => at += 1,
        }
    }
    Some(count)
}

enum Key {
    Safe,
    Enter,
    PasteStart,
}

/// The CSI sequence at the start of `input` and what it does to the box.
/// `None` for one that can bring in text, or one that isn't complete.
fn csi(input: &[u8]) -> Option<(usize, Key)> {
    let body = &input[2..];
    let params_len = body.iter().position(|byte| !(0x20..=0x3f).contains(byte))?;
    let final_byte = body[params_len];
    if !(0x40..=0x7e).contains(&final_byte) {
        return None;
    }
    let params = std::str::from_utf8(&body[..params_len]).ok()?;
    let len = 2 + params_len + 1;
    let numbers: Vec<u32> = params
        .split(';')
        .map(|part| part.split(':').next().unwrap_or("").parse().unwrap_or(0))
        .collect();
    let first = numbers.first().copied().unwrap_or(0);
    // A modifier beyond Shift (Alt, Ctrl, Super) on a letter can recall or
    // yank text in some programs.
    let chorded_letter = |code: u32, modifiers: u32| {
        modifiers > 2 && char::from_u32(code).is_some_and(|ch| ch.is_ascii_alphabetic())
    };
    let key = match final_byte {
        // Up and Down recall history.
        b'A' | b'B' => return None,
        b'C' | b'D' | b'E' | b'F' | b'G' | b'H' | b'I' | b'O' | b'P' | b'Q' | b'R' | b'S'
        | b'Z' => Key::Safe,
        // Mouse reports.
        b'M' | b'm' if params.starts_with('<') => Key::Safe,
        b'~' => match first {
            200 => Key::PasteStart,
            201 => Key::Safe,
            27 => {
                let modifiers = numbers.get(1).copied().unwrap_or(1);
                let code = numbers.get(2).copied().unwrap_or(0);
                match code {
                    10 | 13 => Key::Enter,
                    _ if chorded_letter(code, modifiers) => return None,
                    _ => Key::Safe,
                }
            }
            _ => Key::Safe,
        },
        b'u' => {
            let modifiers = numbers.get(1).copied().unwrap_or(1);
            match first {
                13 => Key::Enter,
                _ if chorded_letter(first, modifiers) => return None,
                _ => Key::Safe,
            }
        }
        _ => return None,
    };
    Some((len, key))
}

/// One paste muxel delivered to a pane, with the counts providers print in
/// the marker that replaces it on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PasteEntry {
    /// Pastes are numbered in the order they were made, from 1.
    seq: u64,
    /// `None` when the paste was too large to keep.
    text: Option<String>,
    chars: usize,
    /// Characters once `\r\n` and `\r` become `\n`, as a TUI stores them.
    lf_chars: usize,
    newlines: usize,
}

/// A pane's pastes since its last submission, oldest first, bounded by
/// [`PASTE_LOG_ENTRIES`] and [`PASTE_LOG_BYTES`], and which paste each of
/// Claude's numbered markers stands for.
///
/// Claude numbers a marker when the paste lands, so muxel ties each new
/// number to its paste the first time it reads the box after it: the new
/// numbers and the pastes made since the last read must match one to one, in
/// order, with agreeing line counts. Anything else (a paste deleted before
/// the read, a paste Claude kept inline, a paste from another client) leaves
/// the new markers unbound, and an unbound marker is never replaced. Codex's
/// markers carry no number; one is replaced only when exactly one remembered
/// paste and one marker in the draft have its character count, and no paste
/// with that count and other text was pushed out of the log since the last
/// submission.
#[derive(Clone, Debug, Default)]
pub struct PasteLog {
    entries: VecDeque<PasteEntry>,
    /// Pastes pushed out of the log since the last submission, newest last,
    /// at most [`PASTE_LOG_ENTRIES`] times four: still pastes a Codex marker
    /// can stand for.
    evicted: Vec<Evicted>,
    /// More were pushed out than `evicted` holds: no Codex marker is
    /// replaced until the next submission.
    evicted_overflow: bool,
    bytes: usize,
    /// The last paste number given out.
    last_seq: u64,
    /// Pastes up to this number were made before the last read of the box.
    read_through: u64,
    /// Claude marker numbers seen in the box, each with its paste (`None`
    /// when it couldn't be told).
    claude: HashMap<u64, Option<u64>>,
}

/// A paste pushed out of the log: its counts, and a digest of its text
/// (`None` when it was too large to keep).
#[derive(Clone, Debug, PartialEq, Eq)]
struct Evicted {
    chars: usize,
    lf_chars: usize,
    digest: Option<u64>,
}

fn digest(text: &str) -> u64 {
    use std::hash::Hasher as _;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hasher.write(text.as_bytes());
    hasher.finish()
}

impl PasteLog {
    /// Remember a paste. Empty pastes put no marker on screen and aren't kept.
    pub fn record(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let keep = text.len() <= PASTE_LOG_BYTES;
        self.last_seq += 1;
        let entry = PasteEntry {
            seq: self.last_seq,
            text: keep.then(|| text.to_string()),
            chars: text.chars().count(),
            lf_chars: normalized.chars().count(),
            newlines: normalized.matches('\n').count(),
        };
        self.bytes += entry.text.as_ref().map_or(0, String::len);
        self.entries.push_back(entry);
        while self.entries.len() > PASTE_LOG_ENTRIES || self.bytes > PASTE_LOG_BYTES {
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= old.text.as_ref().map_or(0, String::len);
            if self.evicted.len() >= PASTE_LOG_ENTRIES * 4 {
                self.evicted.clear();
                self.evicted_overflow = true;
            }
            if !self.evicted_overflow {
                self.evicted.push(Evicted {
                    chars: old.chars,
                    lf_chars: old.lf_chars,
                    digest: old.text.as_deref().map(digest),
                });
            }
        }
    }

    /// Whether a paste pushed out of the log can be what a Codex marker of
    /// `chars` characters stands for, other than one with `text`.
    fn evicted_other(&self, chars: usize, text: Option<&str>) -> bool {
        if self.evicted_overflow {
            return true;
        }
        let digest = text.map(digest);
        self.evicted.iter().any(|old| {
            (old.chars == chars || old.lf_chars == chars)
                && (old.digest.is_none() || old.digest != digest)
        })
    }

    /// A read of the box found `draft`: tie the Claude markers that are new
    /// since the last read to the pastes made since then, when they match.
    pub fn observe(&mut self, draft: &str) {
        let mut fresh: Vec<(u64, Option<usize>)> = markers(draft)
            .into_iter()
            .filter_map(|(_, marker)| match marker {
                Marker::Claude { id, lines } if !self.claude.contains_key(&id) => Some((id, lines)),
                _ => None,
            })
            .collect();
        fresh.sort_unstable_by_key(|&(id, _)| id);
        fresh.dedup_by_key(|&mut (id, _)| id);
        let pasted: Vec<&PasteEntry> = self
            .entries
            .iter()
            .filter(|entry| entry.seq > self.read_through)
            .collect();
        // Pastes evicted from the log before this read can't be told apart.
        let complete = self.last_seq - self.read_through == pasted.len() as u64;
        let matched = complete
            && fresh.len() == pasted.len()
            && fresh
                .iter()
                .zip(&pasted)
                .all(|(&(id, lines), entry)| consistent(Marker::Claude { id, lines }, entry));
        let bound: Vec<(u64, Option<u64>)> = fresh
            .iter()
            .enumerate()
            .map(|(index, &(id, _))| (id, matched.then(|| pasted[index].seq)))
            .collect();
        self.claude.extend(bound);
        self.read_through = self.last_seq;
    }

    /// A submission sent the box's contents: forget its pastes and markers.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.evicted.clear();
        self.evicted_overflow = false;
        self.bytes = 0;
        self.claude.clear();
        self.read_through = self.last_seq;
    }

    fn entry(&self, seq: u64) -> Option<&PasteEntry> {
        self.entries.iter().find(|entry| entry.seq == seq)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A placeholder a provider draws in its input box instead of pasted content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    /// Claude's `[Pasted text #N]` or `[Pasted text #N +L lines]`.
    Claude { id: u64, lines: Option<usize> },
    /// Codex's `[Pasted Content N chars]`.
    Codex { chars: usize },
    /// Content that was never text muxel delivered: Claude's `[Image #N]`
    /// and `[...Truncated text #N +L lines...]`.
    Opaque,
}

fn digits(s: &str) -> Option<(u64, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let value = s[..end].parse().ok()?;
    Some((value, &s[end..]))
}

/// The marker at the start of `s`, and its length in bytes.
fn marker_at(s: &str) -> Option<(Marker, usize)> {
    let consumed = |rest: &str| s.len() - rest.len();
    if let Some(rest) = s.strip_prefix("[Pasted text #") {
        let (id, rest) = digits(rest)?;
        if let Some(rest) = rest.strip_prefix(']') {
            return Some((Marker::Claude { id, lines: None }, consumed(rest)));
        }
        let (lines, rest) = digits(rest.strip_prefix(" +")?)?;
        let rest = rest.strip_prefix(" lines]")?;
        let lines = Some(usize::try_from(lines).ok()?);
        return Some((Marker::Claude { id, lines }, consumed(rest)));
    }
    if let Some(rest) = s.strip_prefix("[Pasted Content ") {
        let (chars, rest) = digits(rest)?;
        let rest = rest.strip_prefix(" chars]")?;
        let chars = usize::try_from(chars).ok()?;
        return Some((Marker::Codex { chars }, consumed(rest)));
    }
    if let Some(rest) = s.strip_prefix("[Image #") {
        let (_, rest) = digits(rest)?;
        let rest = rest.strip_prefix(']')?;
        return Some((Marker::Opaque, consumed(rest)));
    }
    if let Some(rest) = s.strip_prefix("[...Truncated text #") {
        let (_, rest) = digits(rest)?;
        let (_, rest) = digits(rest.strip_prefix(" +")?)?;
        let rest = rest.strip_prefix(" lines...]")?;
        return Some((Marker::Opaque, consumed(rest)));
    }
    None
}

/// Whether `text` holds a paste, image or truncation marker.
pub fn has_paste_markers(text: &str) -> bool {
    !markers(text).is_empty()
}

fn markers(text: &str) -> Vec<(std::ops::Range<usize>, Marker)> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = text[at..].find('[') {
        let start = at + offset;
        match marker_at(&text[start..]) {
            Some((marker, len)) => {
                found.push((start..start + len, marker));
                at = start + len;
            }
            None => at = start + 1,
        }
    }
    found
}

fn consistent(marker: Marker, entry: &PasteEntry) -> bool {
    match marker {
        // Claude counts line breaks; accept a count of lines as well, since
        // only the order of pastes and markers has to line up.
        Marker::Claude {
            lines: Some(lines), ..
        } => entry.newlines == lines || entry.newlines + 1 == lines,
        Marker::Claude { lines: None, .. } => true,
        Marker::Codex { chars } => entry.chars == chars || entry.lf_chars == chars,
        Marker::Opaque => false,
    }
}

/// A draft with its paste markers replaced by the pastes they stand for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Substituted {
    pub text: String,
    /// A marker was left in place: it matched no remembered paste, its paste
    /// was too large to keep, or it stands for an image or truncated text.
    pub partial: bool,
}

/// Replace each paste marker in `draft` with the paste `log` ties it to (see
/// [`PasteLog`]). A marker it can't tie to one paste stays as it is and flags
/// the draft partial, as does an image or truncation marker. A result larger
/// than [`MAX_RESCUE_BYTES`] keeps every marker and is flagged partial.
pub fn substitute_pastes(draft: &str, log: &PasteLog) -> Substituted {
    let found = markers(draft);
    if found.is_empty() {
        return Substituted {
            text: draft.to_string(),
            partial: false,
        };
    }
    // Counted once: a read holds the log, which Enter and paste wait for.
    let mut codex_markers: HashMap<usize, usize> = HashMap::new();
    for (_, marker) in &found {
        if let Marker::Codex { chars } = marker {
            *codex_markers.entry(*chars).or_default() += 1;
        }
    }
    let paste_for = |marker: Marker| -> Option<&PasteEntry> {
        match marker {
            Marker::Claude { id, .. } => log.entry(log.claude.get(&id).copied().flatten()?),
            Marker::Codex { chars } => {
                if codex_markers.get(&chars) != Some(&1) {
                    return None;
                }
                let mut matching = log.entries.iter().filter(|entry| consistent(marker, entry));
                let only = matching.next()?;
                (matching.next().is_none() && !log.evicted_other(chars, only.text.as_deref()))
                    .then_some(only)
            }
            Marker::Opaque => None,
        }
    };
    let mut partial = false;
    let assigned: Vec<Option<&str>> = found
        .iter()
        .map(|&(_, marker)| {
            let text = paste_for(marker).and_then(|entry| entry.text.as_deref());
            partial |= text.is_none();
            text
        })
        .collect();
    let mut text = String::with_capacity(draft.len());
    let mut at = 0;
    for ((range, _), paste) in found.iter().zip(&assigned) {
        text.push_str(&draft[at..range.start]);
        text.push_str(paste.unwrap_or(&draft[range.clone()]));
        at = range.end;
    }
    text.push_str(&draft[at..]);
    if text.len() > MAX_RESCUE_BYTES {
        return Substituted {
            text: draft.to_string(),
            partial: true,
        };
    }
    Substituted { text, partial }
}

/// A file's size and modification time, as a backup records the version it
/// was based on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    /// Unix milliseconds, when the platform reports it.
    pub modified_ms: Option<i64>,
    pub len: u64,
}

/// An editor buffer with unsaved changes, as backed up for one pane.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditorBackup {
    /// `None` for an Untitled buffer.
    pub path: Option<PathBuf>,
    pub text: String,
    /// The on-disk version the edits started from (`None` for Untitled, a
    /// remote file, or a file that didn't exist yet).
    pub base: Option<FileStamp>,
    /// Unix milliseconds when the buffer was backed up.
    pub saved_at_ms: i64,
}

/// What an editor reopened with a backup offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupOffer {
    /// The file is still the version the edits started from.
    Restore,
    /// The file changed (or went away) since: restoring keeps both, the disk
    /// version on disk and the backup in the buffer.
    ChangedOnDisk,
    /// The file already holds this text, so there is nothing to restore.
    Stale,
}

/// Classify a backup against the file as it is now. `disk_text` is the file's
/// current text when it could be read.
pub fn backup_offer(
    backup: &EditorBackup,
    current: Option<FileStamp>,
    disk_text: Option<&str>,
) -> BackupOffer {
    if disk_text == Some(backup.text.as_str()) {
        BackupOffer::Stale
    } else if backup.path.is_none() || current == backup.base {
        BackupOffer::Restore
    } else {
        BackupOffer::ChangedOnDisk
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BackupOffer, DRAFT_DEBOUNCE, DraftAction, DraftRead, DraftTracker, EditorBackup, FileStamp,
        Joined, MAX_RESCUE_BYTES, PASTE_LOG_BYTES, PASTE_LOG_ENTRIES, PasteLog, PromptDraft,
        RowWidths, TypedDraft, backup_offer, debounce_remaining, input_newlines, join_rows,
        merge_clipped, substitute_pastes, typed_matches,
    };
    use std::time::Duration;

    fn text(t: &str) -> DraftRead {
        DraftRead::Text {
            text: t.to_string(),
            partial: false,
            clipped: false,
        }
    }

    #[test]
    fn debounce_waits_out_the_second_after_the_last_keystroke() {
        assert_eq!(debounce_remaining(Duration::ZERO), Some(DRAFT_DEBOUNCE));
        assert_eq!(
            debounce_remaining(Duration::from_millis(400)),
            Some(Duration::from_millis(600))
        );
        assert_eq!(debounce_remaining(DRAFT_DEBOUNCE), None);
        assert_eq!(debounce_remaining(Duration::from_secs(5)), None);
    }

    #[test]
    fn a_draft_is_written_once_per_change_and_cleared_when_the_box_empties() {
        let mut tracker = DraftTracker::default();
        // An empty box deletes the file whoever wrote it.
        assert_eq!(tracker.observe(DraftRead::Empty), DraftAction::Clear);
        assert_eq!(
            tracker.observe(text("fix the")),
            DraftAction::Save {
                text: "fix the".into(),
                partial: false
            }
        );
        // The same text again writes nothing, whatever woke the read.
        assert_eq!(tracker.observe(text("fix the")), DraftAction::Keep);
        // A covered box (a menu, a permission question) keeps the file.
        assert_eq!(tracker.observe(DraftRead::Unknown), DraftAction::Keep);
        assert_eq!(
            tracker.observe(text("fix the bug")),
            DraftAction::Save {
                text: "fix the bug".into(),
                partial: false
            }
        );
        assert_eq!(tracker.observe(text("   ")), DraftAction::Clear);
        assert_eq!(tracker.observe(DraftRead::Empty), DraftAction::Clear);
    }

    #[test]
    fn submission_clears_at_once_and_a_draft_left_behind_is_saved_again() {
        let mut tracker = DraftTracker::default();
        assert!(matches!(
            tracker.observe(text("ship it")),
            DraftAction::Save { .. }
        ));
        assert_eq!(tracker.submitted(), DraftAction::Clear);
        // The app may have written the file itself (a restored offer).
        assert_eq!(tracker.submitted(), DraftAction::Clear);
        // Enter that only picked a completion leaves the text in the box.
        assert!(matches!(
            tracker.observe(text("ship it")),
            DraftAction::Save { .. }
        ));
        tracker.forget();
        assert!(matches!(
            tracker.observe(text("ship it")),
            DraftAction::Save { .. }
        ));
    }

    #[test]
    fn a_clipped_read_keeps_the_lines_scrolled_off_the_top() {
        let mut tracker = DraftTracker::default();
        tracker.observe(text("one\ntwo\nthree"));
        let action = tracker.observe(DraftRead::Text {
            text: "two\nthree and more\nfour".into(),
            partial: false,
            clipped: true,
        });
        assert_eq!(
            action,
            DraftAction::Save {
                text: "one\ntwo\nthree and more\nfour".into(),
                partial: true
            }
        );
        // Without an earlier read only the visible lines are provable.
        let mut fresh = DraftTracker::default();
        assert_eq!(
            fresh.observe(DraftRead::Text {
                text: "four".into(),
                partial: false,
                clipped: true,
            }),
            DraftAction::Save {
                text: "four".into(),
                partial: true
            }
        );
    }

    #[test]
    fn clipped_merge_lines_up_a_wrapped_tail_and_refuses_a_mismatch() {
        assert_eq!(
            merge_clipped("alpha beta\ngamma", "beta\ngamma delta"),
            "alpha beta\ngamma delta"
        );
        assert_eq!(merge_clipped("a\nb\na\nb", "a\nb\nc"), "a\nb\na\nb\nc");
        assert_eq!(merge_clipped("one\ntwo", "zzz\nyyy"), "zzz\nyyy");
        assert_eq!(merge_clipped("one\ntwo", "\nyyy"), "\nyyy");
    }

    const TWELVE: RowWidths = RowWidths {
        first: 12,
        rest: 12,
    };

    fn exact(text: &str) -> Joined {
        Joined {
            text: text.into(),
            exact: true,
        }
    }

    fn guess(text: &str) -> Joined {
        Joined {
            text: text.into(),
            exact: false,
        }
    }

    #[test]
    fn rows_rejoin_where_only_one_reading_fits() {
        // "again" couldn't have fit after "hello world": with no newline
        // typed, the break was the wrap's space.
        assert_eq!(
            join_rows(&["hello world ", "again"], TWELVE, Some(0)),
            exact("hello world again")
        );
        // "next" would have fit after "short": a newline, whatever was typed.
        assert_eq!(
            join_rows(&["short", "next line"], TWELVE, None),
            exact("short\nnext line")
        );
        assert_eq!(
            join_rows(&["", "after blank"], TWELVE, None),
            exact("\nafter blank")
        );
        // The one newline typed is the short row's, so the full row before it
        // was wrapped at a space.
        assert_eq!(
            join_rows(
                &["ab", "cdefghij", "klm"],
                RowWidths {
                    first: 12,
                    rest: 11
                },
                Some(1)
            ),
            exact("ab\ncdefghij klm")
        );
        assert_eq!(
            join_rows(&["ab", "cdefghij", "klm"], TWELVE, None),
            exact("ab\ncdefghij\nklm")
        );
    }

    #[test]
    fn a_break_more_than_one_reading_fits_is_never_offered_as_exact() {
        // A newline typed at the edge looks like a wrap: unknown which.
        assert_eq!(
            join_rows(&["hello world!", "x"], TWELVE, Some(1)),
            guess("hello world! x")
        );
        assert_eq!(
            join_rows(&["hello world!", "x"], TWELVE, None),
            guess("hello world! x")
        );
        // With no newline typed it was the wrap's space.
        assert_eq!(
            join_rows(&["hello world!", "x"], TWELVE, Some(0)),
            exact("hello world! x")
        );
        // A word longer than a row, starting mid-row, cut at the edge, looks
        // like a word that ended there.
        assert_eq!(
            join_rows(&["say abcdefgh", "ijklmn"], TWELVE, Some(0)),
            guess("say abcdefgh ijklmn")
        );
        assert_eq!(
            join_rows(&["abcdefghijkl", "mn"], TWELVE, Some(0)),
            guess("abcdefghijklmn")
        );
        // A newline where the screen shows none breaks the count: unknown.
        assert!(!join_rows(&["short", "next line"], TWELVE, Some(0)).exact);
        // A row wider than the box: the widths don't match this screen.
        assert!(!join_rows(&["a row much too wide"], TWELVE, Some(0)).exact);
    }

    #[test]
    fn wide_characters_are_measured_in_terminal_columns() {
        // Six CJK characters fill 12 columns; the seventh wrapped. Whether a
        // space stood between them can't be told, so it's offered as partial,
        // with no newline put in.
        let first = "界".repeat(6);
        assert_eq!(
            join_rows(&[&first, "文"], TWELVE, Some(0)),
            guess(&format!("{first}文"))
        );
        // Two of them leave room the next two would have used: a newline.
        assert_eq!(
            join_rows(&["界界", "文字"], TWELVE, None),
            exact("界界\n文字")
        );
    }

    #[test]
    fn input_counts_the_newlines_it_can_type_and_gives_up_on_recall() {
        assert_eq!(input_newlines(b"hello world"), Some(0));
        assert_eq!(input_newlines(b"\r"), Some(1));
        // Shift+Enter in its encodings, and Ctrl+J.
        assert_eq!(input_newlines(b"\x1b\r"), Some(1));
        assert_eq!(input_newlines(b"\x1b[13;2u"), Some(1));
        assert_eq!(input_newlines(b"\x1b[27;2;13~"), Some(1));
        assert_eq!(input_newlines(b"\n"), Some(1));
        // A bracketed paste counts its line breaks, CRLF once.
        assert_eq!(
            input_newlines(b"\x1b[200~one\r\ntwo\nthree\x1b[201~"),
            Some(2)
        );
        // Editing keys take text away.
        assert_eq!(input_newlines(b"\x7f\x1b[D\x1b[3~\x15\x17\x0b"), Some(0));
        // History, search, undo, yank and external editors bring text back.
        for recall in [
            b"\x1b[A".as_slice(),
            b"\x1bOB",
            b"\x10",
            b"\x0e",
            b"\x12",
            b"\x1f",
            b"\x19",
            b"\x07",
            b"\x1by",
            b"\x1b[112;5u",
            b"\x1b[",
        ] {
            assert_eq!(input_newlines(recall), None, "{recall:?}");
        }
    }

    fn log(pastes: &[&str]) -> PasteLog {
        let mut log = PasteLog::default();
        for paste in pastes {
            log.record(paste);
        }
        log
    }

    #[test]
    fn markers_are_replaced_by_the_pastes_they_stand_for() {
        let codex = "x".repeat(1096);
        let pastes = log(&["an older paste", &codex]);
        let draft = "look at [Pasted Content 1096 chars] please";
        let out = substitute_pastes(draft, &pastes);
        assert_eq!(out.text, format!("look at {codex} please"));
        assert!(!out.partial);

        let three = "a\nb\nc\nd";
        let two = "e\nf\ng";
        let mut pastes = log(&[three, two]);
        // Claude numbers pastes in the order they were made, wherever the
        // cursor put them; the first read after both binds the numbers.
        let draft = "[Pasted text #2 +2 lines] then [Pasted text #1 +3 lines]";
        pastes.observe(draft);
        let out = substitute_pastes(draft, &pastes);
        assert_eq!(out.text, format!("{two} then {three}"));
        assert!(!out.partial);
    }

    #[test]
    fn a_deleted_paste_never_stands_in_for_the_marker_left_behind() {
        // Both pasted, the second deleted, all before muxel read the box:
        // which paste the remaining marker is can't be told.
        let mut pastes = log(&["public\ntext\nhere", "secret\nleak\nhere"]);
        pastes.observe("[Pasted text #1 +2 lines]");
        let out = substitute_pastes("[Pasted text #1 +2 lines]", &pastes);
        assert_eq!(out.text, "[Pasted text #1 +2 lines]");
        assert!(out.partial);

        // Read after each paste, the numbers are bound and survive a delete.
        let mut pastes = PasteLog::default();
        pastes.record("public\ntext\nhere");
        pastes.observe("[Pasted text #1 +2 lines]");
        pastes.record("secret\nleak\nhere");
        pastes.observe("[Pasted text #1 +2 lines] [Pasted text #2 +2 lines]");
        pastes.observe("[Pasted text #1 +2 lines]");
        let out = substitute_pastes("[Pasted text #1 +2 lines]", &pastes);
        assert_eq!(out.text, "public\ntext\nhere");
        assert!(!out.partial);

        // Codex: two pastes with the same count, one deleted.
        let same = log(&["aaaa", "bbbb"]);
        let out = substitute_pastes("[Pasted Content 4 chars]", &same);
        assert_eq!(out.text, "[Pasted Content 4 chars]");
        assert!(out.partial);

        // A submission forgets the pastes it sent.
        let mut sent = PasteLog::default();
        sent.record("one\ntwo");
        sent.observe("[Pasted text #1 +1 lines]");
        sent.clear();
        let out = substitute_pastes("[Pasted text #1 +1 lines]", &sent);
        assert!(out.partial);
        assert!(sent.is_empty());
    }

    #[test]
    fn an_unmatched_or_opaque_marker_stays_and_flags_the_draft_partial() {
        let pastes = log(&["short"]);
        let out = substitute_pastes("see [Pasted Content 1096 chars]", &pastes);
        assert_eq!(out.text, "see [Pasted Content 1096 chars]");
        assert!(out.partial);

        let out = substitute_pastes("[Image #1] what is this", &log(&[]));
        assert_eq!(out.text, "[Image #1] what is this");
        assert!(out.partial);

        // Literal brackets that aren't markers are just text.
        let out = substitute_pastes("[Pasted text] and [x]", &log(&[]));
        assert_eq!(out.text, "[Pasted text] and [x]");
        assert!(!out.partial);
    }

    #[test]
    fn a_paste_too_large_to_keep_still_holds_its_place_in_order() {
        let huge = "y".repeat(PASTE_LOG_BYTES + 1);
        let pastes = log(&["z".repeat(10).as_str(), &huge]);
        let marker = format!("[Pasted Content {} chars]", huge.len());
        let out = substitute_pastes(&marker, &pastes);
        // The huge paste is counted but not kept: the older 10-char paste
        // must not be substituted for it.
        assert_eq!(out.text, marker);
        assert!(out.partial);
    }

    #[test]
    fn the_paste_log_is_bounded_by_count_and_bytes() {
        let mut pastes = PasteLog::default();
        for i in 0..PASTE_LOG_ENTRIES + 5 {
            pastes.record(&format!("paste {i}"));
        }
        assert_eq!(pastes.len(), PASTE_LOG_ENTRIES);
        let half = "h".repeat(PASTE_LOG_BYTES / 2 + 1);
        pastes.record(&half);
        pastes.record(&half);
        assert!(pastes.len() < PASTE_LOG_ENTRIES);
        pastes.record("");
        let out = substitute_pastes(&format!("[Pasted Content {} chars]", half.len()), &pastes);
        assert_eq!(out.text, half);
    }

    #[test]
    fn a_paste_pushed_out_of_the_log_still_makes_its_count_ambiguous() {
        // Two pastes of one size, the second deleted from the box; more
        // pastes push the first out of the log. The marker left in the box is
        // the first one's, and the second must never stand in for it.
        let first = "a".repeat(40);
        let deleted = "b".repeat(40);
        let mut pastes = log(&[first.as_str(), deleted.as_str()]);
        for i in 0..PASTE_LOG_ENTRIES - 1 {
            pastes.record(&"x".repeat(100 + i));
        }
        let draft = "[Pasted Content 40 chars]";
        let out = substitute_pastes(draft, &pastes);
        assert_eq!(out.text, draft);
        assert!(out.partial);
    }

    #[test]
    fn markers_sharing_a_count_are_counted_in_one_pass() {
        // A read holds the paste log, which Enter and paste wait for. A draft
        // merged from clipped reads can reach the size cap: 80,000 markers.
        // Counted once per marker, that's 6.4 billion comparisons; once per
        // read, milliseconds.
        let pastes = log(&["c".repeat(40).as_str()]);
        let draft = "[Pasted Content 40 chars] ".repeat(80_000);
        assert!(draft.len() <= MAX_RESCUE_BYTES);
        let started = std::time::Instant::now();
        let out = substitute_pastes(&draft, &pastes);
        let took = started.elapsed();
        assert!(out.partial, "a count markers share stands for no one paste");
        assert!(
            took < Duration::from_millis(500),
            "{took:?} for 80,000 markers"
        );
    }

    fn typed(keys: &[&[u8]]) -> Option<String> {
        let mut draft = TypedDraft::default();
        for key in keys {
            draft.feed(key);
        }
        draft.text().map(str::to_string)
    }

    #[test]
    fn keys_muxel_can_follow_keep_the_typed_text() {
        assert_eq!(
            typed(&[b"fix ", b"teh", b"\x7f\x7f\x7f", b"the bug"]).as_deref(),
            Some("fix the bug")
        );
        // Left and Right inside the text, Delete, and a key in the middle.
        assert_eq!(
            typed(&[b"ac", b"\x1b[D", b"b", b"\x1b[C"]).as_deref(),
            Some("abc")
        );
        assert_eq!(
            typed(&[b"abc", b"\x1bOD\x1bOD", b"\x1b[3~"]).as_deref(),
            Some("ac")
        );
        // Shift+Enter in its encodings, and a paste with its CRLF as the box keeps it.
        assert_eq!(
            typed(&[
                b"one",
                b"\x1b\r",
                b"two",
                b"\x1b[13;2u",
                b"\x1b[200~three\r\nfour  \x1b[201~"
            ])
            .as_deref(),
            Some("one\ntwo\nthree\nfour  ")
        );
        assert_eq!(
            typed(&["界".as_bytes(), b"\x7f", b"x"]).as_deref(),
            Some("x")
        );
        // Keys whose effect muxel can't follow.
        for key in [
            b"\r".as_slice(),
            b"\t",
            b"\x1b[A",
            b"\x1b[H",
            b"\x1b[1;5D",
            b"\x17",
            b"\x1b",
            b"\x1b[200~unfinished",
        ] {
            assert_eq!(typed(&[b"abc", key]), None, "{key:?}");
        }
        // Right at the end can take a suggestion.
        assert_eq!(typed(&[b"abc", b"\x1b[C"]), None);
    }

    #[test]
    fn typed_text_matches_the_rows_only_where_the_screen_could_hide_its_whitespace() {
        let rows = ["line", "next"];
        assert!(typed_matches("line\nnext", &rows, TWELVE));
        assert!(typed_matches("line  \nnext ", &rows, TWELVE));
        assert!(
            !typed_matches("line next", &rows, TWELVE),
            "next would have fit: no wrap"
        );
        assert!(!typed_matches("lines\nnext", &rows, TWELVE));
        assert!(!typed_matches("line\nnext\n", &rows, TWELVE));
        let wrapped = ["hello world", "again"];
        assert!(typed_matches("hello world  again", &wrapped, TWELVE));
        assert!(typed_matches("hello world\nagain", &wrapped, TWELVE));
        assert!(
            !typed_matches("hello worldagain", &wrapped, TWELVE),
            "no cut fits there"
        );
        // Visible blanks must match exactly.
        assert!(typed_matches("a\n  b", &["a", "  b"], TWELVE));
        assert!(!typed_matches("a\n b", &["a", "  b"], TWELVE));
        assert!(typed_matches("hello\n", &["hello", ""], TWELVE));
    }

    #[test]
    fn a_substitution_past_the_size_cap_keeps_its_markers() {
        // Both pastes fit the log; with the typed text around them the
        // result is just over the cap.
        let big = "q".repeat(MAX_RESCUE_BYTES / 2 - 10);
        let bigger = "r".repeat(MAX_RESCUE_BYTES / 2 - 9);
        let pastes = log(&[&big, &bigger]);
        assert_eq!(pastes.len(), 2);
        let draft = format!(
            "[Pasted Content {} chars][Pasted Content {} chars]{}",
            big.len(),
            bigger.len(),
            "x".repeat(30)
        );
        let out = substitute_pastes(&draft, &pastes);
        assert_eq!(out.text, draft);
        assert!(out.partial);
    }

    #[test]
    fn a_draft_file_round_trips_and_older_files_load() {
        let draft = PromptDraft {
            text: "secret? no, just a prompt".into(),
            partial: true,
            saved_at_ms: 1_700_000_000_000,
            source: super::DraftSource::Shell,
            pane: None,
        };
        let json = serde_json::to_string(&draft).unwrap();
        assert_eq!(serde_json::from_str::<PromptDraft>(&json).unwrap(), draft);
        let minimal: PromptDraft =
            serde_json::from_str(r#"{"text":"hi","saved_at_ms":1}"#).unwrap();
        assert!(!minimal.partial);
        assert_eq!(minimal.source, super::DraftSource::Agent);
    }

    #[test]
    fn a_backup_is_offered_by_how_the_file_changed_since() {
        let base = FileStamp {
            modified_ms: Some(10),
            len: 4,
        };
        let backup = EditorBackup {
            path: Some("notes.md".into()),
            text: "edited".into(),
            base: Some(base),
            saved_at_ms: 20,
        };
        assert_eq!(
            backup_offer(&backup, Some(base), Some("orig")),
            BackupOffer::Restore
        );
        let later = FileStamp {
            modified_ms: Some(30),
            len: 9,
        };
        assert_eq!(
            backup_offer(&backup, Some(later), Some("someone else")),
            BackupOffer::ChangedOnDisk
        );
        assert_eq!(
            backup_offer(&backup, None, None),
            BackupOffer::ChangedOnDisk
        );
        assert_eq!(
            backup_offer(&backup, Some(later), Some("edited")),
            BackupOffer::Stale
        );
        let untitled = EditorBackup {
            path: None,
            base: None,
            ..backup
        };
        assert_eq!(backup_offer(&untitled, None, None), BackupOffer::Restore);
    }
}
