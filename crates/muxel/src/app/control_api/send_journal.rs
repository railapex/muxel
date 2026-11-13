//! The on-disk record of detached `ctl send`s, which lets a send and its
//! `--once` key outlive a muxel restart.
//!
//! One file per workspace, beside its document, holding every detached key
//! the open workspace has: the sends still waiting, the one about to paste,
//! and how the finished ones ended. Each write replaces the whole file with a
//! snapshot of muxel's state when it was taken. Writes run off the UI thread,
//! one at a time, and a snapshot never overwrites a newer one.

use super::CtlResult;
use muxel_core::control as ctl;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// The journal's format. A file of another version is set aside, not read.
const JOURNAL_VERSION: u32 = 1;

/// Every detached key of one workspace.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct Journal {
    pub(super) version: u32,
    #[serde(default)]
    pub(super) keys: Vec<JournalKey>,
}

/// One pane's detached `--once` key and where its send had got to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct JournalKey {
    pub(super) pane: Uuid,
    pub(super) key: String,
    #[serde(flatten)]
    pub(super) state: JournalState,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum JournalState {
    /// Waiting for its pane, with the `queued` reply its caller got.
    Queued { receipt: Value, send: SendRecord },
    /// About to paste, or pasting: a restart can't tell whether it typed.
    Typing,
    /// It typed, and ended with this reply.
    Done { result: ctl::Response },
    /// It ended without typing, with this refusal.
    Dropped { result: ctl::Response },
}

/// What a restart needs to carry a waiting send on, or to say why it can't.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct SendRecord {
    /// The prompt, kept only for a send a restart carries on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) text: Option<String>,
    /// The caller's `--session-id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) session_id: Option<String>,
    /// The caller's `--launch-id`, which no launch after a restart can match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) launch_id: Option<Uuid>,
    /// When it was queued, and when it gives up (wall-clock ms).
    pub(super) queued_ms: i64,
    pub(super) expires_at: i64,
}

impl SendRecord {
    pub(super) fn new(
        text: &str,
        session_id: Option<&str>,
        launch_id: Option<Uuid>,
        queued_ms: i64,
        expires_at: i64,
    ) -> Self {
        let mut record = Self {
            text: None,
            session_id: session_id.map(str::to_owned),
            launch_id,
            queued_ms,
            expires_at,
        };
        if record.carries_on() {
            record.text = Some(text.to_owned());
        }
        record
    }

    /// A restart carries the send on only when it is pinned to a
    /// conversation, which the relaunched pane must prove it is still in.
    /// Unpinned, it could type into whatever the pane resumed; a launch id is
    /// gone with the launch.
    pub(super) fn carries_on(&self) -> bool {
        self.session_id.is_some() && self.launch_id.is_none()
    }
}

/// The result a finished key keeps, as the reply it was.
pub(super) fn response(result: &CtlResult<Value>) -> ctl::Response {
    match result {
        Ok(value) => ctl::Response::ok(value.clone()),
        Err(error) => ctl::Response::err(error.clone()),
    }
}

/// Read `path`'s journal: `None` when there is none. One muxel can't read is
/// renamed aside with `.unreadable`, so the next write doesn't erase it.
pub(super) fn read(path: &Path) -> Result<Option<Journal>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };
    let problem = match serde_json::from_slice::<Journal>(&bytes) {
        Ok(journal) if journal.version == JOURNAL_VERSION => return Ok(Some(journal)),
        Ok(journal) => format!("its version is {}", journal.version),
        Err(error) => error.to_string(),
    };
    let aside = path.with_extension("json.unreadable");
    let _ = std::fs::rename(path, &aside);
    Err(format!(
        "{} can't be read ({problem}); moved to {}",
        path.display(),
        aside.display()
    ))
}

/// Writes the journals, one at a time, in the order their snapshots were
/// taken: a snapshot that lands after a newer one for the same file is
/// skipped, since the newer one holds everything it did.
#[derive(Clone, Default)]
pub(in crate::app) struct JournalWriter {
    state: Arc<Mutex<WriterState>>,
}

#[derive(Default)]
struct WriterState {
    taken: u64,
    written: HashMap<PathBuf, u64>,
}

/// A snapshot to write: the file, its number, and its contents.
pub(super) struct Snapshot {
    path: PathBuf,
    number: u64,
    journal: Journal,
}

impl JournalWriter {
    /// Number `journal` for `path`, taken now.
    pub(super) fn snapshot(&self, path: PathBuf, mut journal: Journal) -> Snapshot {
        journal.version = JOURNAL_VERSION;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.taken += 1;
        Snapshot {
            path,
            number: state.taken,
            journal,
        }
    }

    /// Put `snapshot` on disk, unless a newer one is already there. Blocks:
    /// run it off the UI thread.
    pub(super) fn write(&self, snapshot: Snapshot) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&snapshot.journal).map_err(|e| e.to_string())?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state
            .written
            .get(&snapshot.path)
            .is_some_and(|written| *written >= snapshot.number)
        {
            return Ok(());
        }
        muxel_store::write_durably(&snapshot.path, &bytes).map_err(|e| format!("{e:#}"))?;
        state.written.insert(snapshot.path, snapshot.number);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Journal, JournalKey, JournalState, JournalWriter, SendRecord, read};
    use uuid::Uuid;

    fn journal(key: &str) -> Journal {
        Journal {
            version: 0,
            keys: vec![JournalKey {
                pane: Uuid::nil(),
                key: key.into(),
                state: JournalState::Typing,
            }],
        }
    }

    fn scratch() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("muxel-send-journal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Two snapshots whose writes run out of order leave the newer on disk.
    #[test]
    fn an_older_snapshot_never_overwrites_a_newer_one() {
        let dir = scratch();
        let path = dir.join("sends.json");
        let writer = JournalWriter::default();
        let older = writer.snapshot(path.clone(), journal("older"));
        let newer = writer.snapshot(path.clone(), journal("newer"));
        writer.write(newer).unwrap();
        writer.write(older).unwrap();
        let on_disk = read(&path).unwrap().unwrap();
        assert_eq!(on_disk.keys[0].key, "newer");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A journal muxel can't read is kept aside for a person to look at,
    /// instead of being overwritten by the next write.
    #[test]
    fn an_unreadable_journal_is_set_aside() {
        let dir = scratch();
        let path = dir.join("sends.json");
        std::fs::write(&path, b"{\"version\": 1, \"keys\": [{\"pane\": 7}]}").unwrap();
        assert!(read(&path).is_err());
        assert!(!path.exists());
        assert!(dir.join("sends.json.unreadable").exists());
        assert_eq!(read(&path), Ok(None));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Only a send a restart carries on keeps its prompt on disk.
    #[test]
    fn only_a_pinned_send_keeps_its_text() {
        let pinned = SendRecord::new("hello", Some("s-1"), None, 1, 2);
        assert_eq!(pinned.text.as_deref(), Some("hello"));
        for unkept in [
            SendRecord::new("hello", None, None, 1, 2),
            SendRecord::new("hello", Some("s-1"), Some(Uuid::nil()), 1, 2),
        ] {
            assert!(!unkept.carries_on());
            assert_eq!(unkept.text, None);
            assert!(!serde_json::to_string(&unkept).unwrap().contains("hello"));
        }
    }
}
