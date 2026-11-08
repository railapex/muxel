//! Rescue of text someone typed but hadn't sent or saved: a prompt drafted in
//! an agent's input box or at a shell prompt, and an editor's unsaved buffer.
//!
//! Pure decisions only. The terminal crate reads the draft off the screen, the
//! store writes the files, and the app shows the offers. Draft text never goes
//! to a log: callers report only lengths and counts.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::Duration;

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

    fn clear(&mut self) -> DraftAction {
        match self.saved.take() {
            Some(_) => DraftAction::Clear,
            None => DraftAction::Keep,
        }
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

/// Join the rows of an input box back into the text they show. A row the next
/// row's first word couldn't have fit on was soft-wrapped, so the two are one
/// line; any other row break was a newline. `width` is the columns a row of
/// text can use; `lead` is what a row after the first already spends on the
/// space carried over from the wrap (Claude keeps it there, Codex drops it).
/// One unbroken chunk filling the row was a word cut in two.
pub fn join_rows(rows: &[&str], width: usize, lead: usize) -> String {
    let rows: Vec<&str> = rows.iter().map(|row| row.trim_end()).collect();
    let mut out = String::new();
    for (index, row) in rows.iter().enumerate() {
        out.push_str(row);
        let Some(next) = rows.get(index + 1) else {
            break;
        };
        let used = row.chars().count() + if index > 0 { lead } else { 0 };
        let next_word = next
            .split_whitespace()
            .next()
            .map_or(0, |w| w.chars().count());
        out.push_str(if row.is_empty() {
            "\n"
        } else if used >= width && !row.trim_start().contains(char::is_whitespace) {
            // A word too long for any row, cut at the edge.
            ""
        } else if next_word > 0 && used + 1 + next_word > width {
            " "
        } else {
            "\n"
        });
    }
    out
}

/// One paste muxel delivered to a pane, with the counts providers print in
/// the marker that replaces it on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PasteEntry {
    /// `None` when the paste was too large to keep.
    text: Option<String>,
    chars: usize,
    /// Characters once `\r\n` and `\r` become `\n`, as a TUI stores them.
    lf_chars: usize,
    newlines: usize,
}

/// A pane's recent pastes, oldest first, bounded by [`PASTE_LOG_ENTRIES`] and
/// [`PASTE_LOG_BYTES`].
#[derive(Clone, Debug, Default)]
pub struct PasteLog {
    entries: VecDeque<PasteEntry>,
    bytes: usize,
}

impl PasteLog {
    /// Remember a paste. Empty pastes put no marker on screen and aren't kept.
    pub fn record(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let keep = text.len() <= PASTE_LOG_BYTES;
        let entry = PasteEntry {
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
        }
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

/// Replace each paste marker in `draft` with the paste it stands for. Markers
/// are matched newest first against `log`, in the order the provider numbered
/// them (Claude) or wrote them (Codex), and only to a paste whose line or
/// character count agrees with the marker. A result larger than
/// [`MAX_RESCUE_BYTES`] keeps every marker and is flagged partial.
pub fn substitute_pastes(draft: &str, log: &PasteLog) -> Substituted {
    let found = markers(draft);
    if found.is_empty() {
        return Substituted {
            text: draft.to_string(),
            partial: false,
        };
    }
    let mut order: Vec<usize> = (0..found.len())
        .filter(|&i| found[i].1 != Marker::Opaque)
        .collect();
    if found
        .iter()
        .all(|(_, marker)| matches!(marker, Marker::Claude { .. } | Marker::Opaque))
    {
        order.sort_by_key(|&i| match found[i].1 {
            Marker::Claude { id, .. } => id,
            _ => 0,
        });
    }
    let entries: Vec<&PasteEntry> = log.entries.iter().collect();
    let mut assigned: Vec<Option<&str>> = vec![None; found.len()];
    let mut partial = found.iter().any(|(_, marker)| *marker == Marker::Opaque);
    let mut upper = entries.len();
    for &index in order.iter().rev() {
        let marker = found[index].1;
        match (0..upper).rev().find(|&e| consistent(marker, entries[e])) {
            Some(e) => {
                upper = e;
                match &entries[e].text {
                    Some(text) => assigned[index] = Some(text),
                    None => partial = true,
                }
            }
            None => partial = true,
        }
    }
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
        MAX_RESCUE_BYTES, PASTE_LOG_BYTES, PASTE_LOG_ENTRIES, PasteLog, PromptDraft, backup_offer,
        debounce_remaining, join_rows, merge_clipped, substitute_pastes,
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
        assert_eq!(tracker.observe(DraftRead::Empty), DraftAction::Keep);
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
        assert_eq!(tracker.observe(DraftRead::Empty), DraftAction::Keep);
    }

    #[test]
    fn submission_clears_at_once_and_a_draft_left_behind_is_saved_again() {
        let mut tracker = DraftTracker::default();
        assert!(matches!(
            tracker.observe(text("ship it")),
            DraftAction::Save { .. }
        ));
        assert_eq!(tracker.submitted(), DraftAction::Clear);
        assert_eq!(tracker.submitted(), DraftAction::Keep);
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

    #[test]
    fn rows_rejoin_soft_wraps_and_keep_real_newlines() {
        // Width 12: "hello world" fits, "again" would not have fit after it.
        assert_eq!(
            join_rows(&["hello world ", "again"], 12, 0),
            "hello world again"
        );
        assert_eq!(
            join_rows(&["short", "next line"], 12, 0),
            "short\nnext line"
        );
        assert_eq!(join_rows(&["", "after blank"], 12, 0), "\nafter blank");
        // Words that end exactly at the edge still wrapped at a space.
        assert_eq!(join_rows(&["hello world!", "x"], 12, 0), "hello world! x");
        // One chunk filling the row was a word cut in half.
        assert_eq!(join_rows(&["abcdefghijkl", "mn"], 12, 0), "abcdefghijklmn");
        // A carried-over space makes a later row one column fuller.
        assert_eq!(
            join_rows(&["ab", "cdefghij", "klm"], 12, 1),
            "ab\ncdefghij klm"
        );
        assert_eq!(
            join_rows(&["ab", "cdefghij", "klm"], 12, 0),
            "ab\ncdefghij\nklm"
        );
    }

    fn log(pastes: &[&str]) -> PasteLog {
        let mut log = PasteLog::default();
        for paste in pastes {
            log.record(paste);
        }
        log
    }

    #[test]
    fn markers_are_replaced_by_the_pastes_they_count() {
        let codex = "x".repeat(1096);
        let pastes = log(&["an older paste", &codex]);
        let draft = "look at [Pasted Content 1096 chars] please";
        let out = substitute_pastes(draft, &pastes);
        assert_eq!(out.text, format!("look at {codex} please"));
        assert!(!out.partial);

        let three = "a\nb\nc\nd";
        let two = "e\nf\ng";
        let pastes = log(&[three, two]);
        // Claude numbers pastes in the order they were made, wherever the
        // cursor put them.
        let draft = "[Pasted text #2 +2 lines] then [Pasted text #1 +3 lines]";
        let out = substitute_pastes(draft, &pastes);
        assert_eq!(out.text, format!("{two} then {three}"));
        assert!(!out.partial);
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
    fn a_substitution_past_the_size_cap_keeps_its_markers() {
        // Both pastes fit the log; with the typed text around them the
        // result is just over the cap.
        let big = "q".repeat(MAX_RESCUE_BYTES / 2 - 10);
        let pastes = log(&[&big, &big]);
        assert_eq!(pastes.len(), 2);
        let marker = format!("[Pasted Content {} chars]", big.len());
        let draft = format!("{marker}{marker}{}", "x".repeat(30));
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
