//! Local Codex child turns that run beside a pane's root conversation.

use chrono::{DateTime, Local, NaiveDate, Utc};
use notify::{RecursiveMode, Watcher};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, SystemTime};

const READ_BYTES: u64 = 256 * 1024;
const RECORD_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub struct CodexSubagents {
    store: Option<PathBuf>,
    scanned_through: Option<NaiveDate>,
    directories: HashMap<PathBuf, Option<SystemTime>>,
    children: HashMap<PathBuf, Child>,
    new_rollouts: Option<NewRollouts>,
    watch_attempted: bool,
    last_backstop: Option<SystemTime>,
}

impl CodexSubagents {
    /// Return parents with a child turn started during the current launch.
    /// Call on a worker; remote panes must not query the local rollout store.
    pub fn refresh(
        &mut self,
        store: &Path,
        parents: &[(String, SystemTime)],
        now: SystemTime,
    ) -> HashSet<(String, SystemTime)> {
        let Some(oldest) = parents.iter().map(|(_, launched)| *launched).min() else {
            *self = Self::default();
            return HashSet::new();
        };
        if self.store.as_deref() != Some(store) {
            *self = Self {
                store: Some(store.to_path_buf()),
                ..Self::default()
            };
        }
        if !self.watch_attempted {
            self.new_rollouts = NewRollouts::watch(store);
            self.watch_attempted = true;
        }
        let utc: DateTime<Utc> = now.into();
        let local_day = utc.with_timezone(&Local).date_naive();
        let utc_day = utc.date_naive();
        let first: DateTime<Utc> = oldest.into();
        let first_day = first
            .date_naive()
            .min(first.with_timezone(&Local).date_naive());
        let last_day = local_day.max(utc_day);
        // Scan each historical day once, then only directories that can receive
        // new files. Keep discovered child paths across a midnight boundary.
        let mut next = self
            .scanned_through
            .and_then(|day| day.succ_opt())
            .unwrap_or(first_day);
        for _ in 0..32 {
            if next > last_day {
                break;
            }
            self.discover(store, next, oldest);
            self.scanned_through = Some(next);
            let Some(day) = next.succ_opt() else {
                break;
            };
            next = day;
        }
        let (paths, lost) = self.new_rollouts.as_ref().map_or_else(
            || (Vec::new(), false),
            |watch| {
                (
                    watch.changes.try_iter().take(256).flatten().collect(),
                    watch.lost.swap(false, Ordering::Relaxed),
                )
            },
        );
        for path in paths {
            self.discover_path(path, oldest);
        }
        if self.new_rollouts.is_none()
            || lost
            || self.last_backstop.is_none_or(|at| {
                now.duration_since(at).unwrap_or_default() >= Duration::from_secs(5)
            })
        {
            for day in [Some(local_day), Some(utc_day), local_day.pred_opt()]
                .into_iter()
                .flatten()
            {
                if lost {
                    self.directories
                        .remove(&store.join(day.format("%Y/%m/%d").to_string()));
                }
                self.discover(store, day, oldest);
            }
            self.last_backstop = Some(now);
        }
        let mut requested: HashMap<&str, Vec<SystemTime>> = HashMap::new();
        for (id, launched) in parents {
            requested.entry(id).or_default().push(*launched);
        }
        let mut running = HashSet::new();
        for (path, child) in &mut self.children {
            // Read a new file's metadata once, then follow only requested parents.
            if (child.ordinal > 0 && child.parent.is_none())
                || child
                    .parent
                    .as_deref()
                    .is_some_and(|id| !requested.contains_key(id))
            {
                continue;
            }
            child.refresh(path);
            if let (Some(parent), Some((_, started))) = (&child.parent, &child.active) {
                for launched in requested.get(parent.as_str()).into_iter().flatten() {
                    if started >= launched {
                        running.insert((parent.clone(), *launched));
                    }
                }
            }
        }
        running
    }

    fn discover(&mut self, store: &Path, day: NaiveDate, oldest: SystemTime) {
        let path = store.join(day.format("%Y/%m/%d").to_string());
        let stamp = std::fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok());
        if self.directories.get(&path) == Some(&stamp) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&path) else {
            return;
        };
        self.directories.insert(path, stamp);
        for entry in entries.flatten() {
            self.discover_path(entry.path(), oldest);
        }
    }

    fn discover_path(&mut self, path: PathBuf, oldest: SystemTime) {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return;
        };
        let Some(stem) = name.strip_suffix(".jsonl") else {
            return;
        };
        let Some(id) = stem.get(stem.len().saturating_sub(36)..) else {
            return;
        };
        let Some(created) = super::codex_sessions::creation_time(id).map(SystemTime::from) else {
            return;
        };
        if created < oldest {
            return;
        }
        let id = id.to_string();
        self.children.entry(path).or_insert_with(|| Child {
            id,
            created,
            ..Child::default()
        });
    }
}

struct NewRollouts {
    _watcher: notify::RecommendedWatcher,
    changes: mpsc::Receiver<Vec<PathBuf>>,
    lost: Arc<AtomicBool>,
}

impl NewRollouts {
    fn watch(root: &Path) -> Option<Self> {
        let (send, changes) = mpsc::sync_channel(256);
        let lost = Arc::new(AtomicBool::new(false));
        let on_error = lost.clone();
        let mut watcher = notify::recommended_watcher(
            move |result: notify::Result<notify::Event>| match result {
                Ok(event)
                    if event.kind.is_create()
                        || matches!(
                            event.kind,
                            notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                        ) =>
                {
                    if send.try_send(event.paths).is_err() {
                        on_error.store(true, Ordering::Relaxed);
                    }
                }
                Err(_) => on_error.store(true, Ordering::Relaxed),
                _ => {}
            },
        )
        .ok()?;
        watcher.watch(root, RecursiveMode::Recursive).ok()?;
        Some(Self {
            _watcher: watcher,
            changes,
            lost,
        })
    }
}

struct Child {
    id: String,
    created: SystemTime,
    stamp: Option<(u64, Option<SystemTime>)>,
    offset: u64,
    pending: Vec<u8>,
    skipping: bool,
    ordinal: u64,
    history_start: Option<u64>,
    parent: Option<String>,
    active: Option<(String, SystemTime)>,
}

impl Default for Child {
    fn default() -> Self {
        Self {
            id: String::new(),
            created: SystemTime::UNIX_EPOCH,
            stamp: None,
            offset: 0,
            pending: Vec::new(),
            skipping: false,
            ordinal: 0,
            history_start: None,
            parent: None,
            active: None,
        }
    }
}

impl Child {
    fn refresh(&mut self, path: &Path) {
        let Ok(meta) = std::fs::metadata(path) else {
            self.active = None;
            self.stamp = None;
            return;
        };
        let stamp = (meta.len(), meta.modified().ok());
        if self.stamp == Some(stamp) && self.offset == meta.len() {
            return;
        }
        if meta.len() < self.offset {
            *self = Self {
                id: self.id.clone(),
                created: self.created,
                ..Self::default()
            };
        }
        let Ok(mut file) = std::fs::File::open(path) else {
            self.active = None;
            self.stamp = None;
            return;
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut bytes = Vec::new();
        if file.take(READ_BYTES).read_to_end(&mut bytes).is_err() {
            return;
        }
        self.offset += bytes.len() as u64;
        self.stamp = Some(stamp);
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            if !self.skipping {
                if self.pending.len() + part.len() > RECORD_BYTES {
                    self.pending.clear();
                    self.skipping = true;
                } else {
                    self.pending.extend_from_slice(part);
                }
            }
            if part.last() == Some(&b'\n') {
                if !self.skipping {
                    let pending = std::mem::take(&mut self.pending);
                    self.record(&pending);
                }
                self.skipping = false;
                self.ordinal += 1;
            }
        }
    }

    fn record(&mut self, bytes: &[u8]) {
        let Ok(record) = serde_json::from_slice::<Record>(bytes) else {
            return;
        };
        let payload = record.payload;
        if self.ordinal == 0 {
            if record.kind == "session_meta" && payload.id.as_deref() == Some(&self.id) {
                self.parent = payload.parent_thread_id;
                self.history_start = payload.subagent_history_start_ordinal;
            }
            return;
        }
        if record.kind == "session_meta" && self.history_start.is_none() {
            // Copied history without an explicit boundary cannot establish a
            // child's own lifecycle. Older, non-forked child files have no copy.
            self.parent = None;
            self.active = None;
            return;
        }
        if self.parent.is_none()
            || self.history_start.is_some_and(|start| self.ordinal < start)
            || record.kind != "event_msg"
        {
            return;
        }
        let Some(turn) = payload.turn_id else {
            return;
        };
        match payload.kind.as_deref() {
            Some("task_started") => {
                self.active = record
                    .timestamp
                    .as_deref()
                    .and_then(|stamp| DateTime::parse_from_rfc3339(stamp).ok())
                    .map(|stamp| (turn, SystemTime::from(stamp)));
            }
            Some("task_complete" | "turn_aborted")
                if self.active.as_ref().is_some_and(|(id, _)| *id == turn) =>
            {
                self.active = None;
            }
            _ => {}
        }
    }
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: String,
    timestamp: Option<String>,
    payload: Payload,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    parent_thread_id: Option<String>,
    subagent_history_start_ordinal: Option<u64>,
    turn_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::CodexSubagents;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};
    use uuid::Uuid;

    const PARENT: &str = "01a1bba7-6f16-7000-8000-000000000001";
    const CHILD: &str = "01a1bbaa-121c-7000-8000-000000000002";
    const ROWS: &str =
        include_str!("../../../muxel-terminal/src/fixtures/codex-background-child.jsonl");

    struct Fixture {
        home: PathBuf,
        child: PathBuf,
        now: SystemTime,
    }

    impl Fixture {
        fn new() -> Self {
            let home =
                std::env::temp_dir().join(format!("muxel-codex-background-{}", Uuid::new_v4()));
            let day = home.join("2026/11/08");
            fs::create_dir_all(&day).unwrap();
            let child = day.join(format!("rollout-2026-11-08T07-57-27-{CHILD}.jsonl"));
            let now = chrono::DateTime::parse_from_rfc3339("2026-11-08T14:58:00Z")
                .unwrap()
                .into();
            Self { home, child, now }
        }

        fn parents(&self) -> Vec<(String, SystemTime)> {
            vec![(PARENT.into(), self.now - Duration::from_secs(240))]
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.home).unwrap();
        }
    }

    #[test]
    fn background_codex_child_fixture_tracks_its_own_turn_and_complete_rows() {
        let fixture = Fixture::new();
        let rows: Vec<_> = ROWS.split_inclusive('\n').collect();
        let mut index = CodexSubagents::default();
        // The copied parent's last turn is running. It belongs to the parent.
        fs::write(&fixture.child, rows[..4].concat()).unwrap();
        assert!(
            index
                .refresh(&fixture.home, &fixture.parents(), fixture.now)
                .is_empty()
        );
        let mut file = OpenOptions::new()
            .append(true)
            .open(&fixture.child)
            .unwrap();
        file.write_all(rows[4..6].concat().as_bytes()).unwrap();
        assert_eq!(
            index.refresh(&fixture.home, &fixture.parents(), fixture.now),
            [fixture.parents()[0].clone()].into()
        );
        // An assistant's final text is not a completed turn.
        file.write_all(rows[6].as_bytes()).unwrap();
        assert!(
            index
                .refresh(&fixture.home, &fixture.parents(), fixture.now)
                .contains(&fixture.parents()[0])
        );
        let complete = rows[7];
        let split = complete.len() / 2;
        file.write_all(&complete.as_bytes()[..split]).unwrap();
        assert!(
            index
                .refresh(&fixture.home, &fixture.parents(), fixture.now)
                .contains(&fixture.parents()[0])
        );
        file.write_all(&complete.as_bytes()[split..]).unwrap();
        assert!(
            index
                .refresh(&fixture.home, &fixture.parents(), fixture.now)
                .is_empty()
        );
    }

    #[test]
    fn background_codex_children_belong_to_one_parent_launch_and_clear_on_abort_or_removal() {
        let fixture = Fixture::new();
        let rows: Vec<_> = ROWS.split_inclusive('\n').collect();
        fs::write(&fixture.child, rows[..6].concat()).unwrap();
        let mut index = CodexSubagents::default();
        assert!(
            index
                .refresh(
                    &fixture.home,
                    &[(CHILD.into(), fixture.parents()[0].1)],
                    fixture.now
                )
                .is_empty()
        );
        assert!(
            index
                .refresh(&fixture.home, &fixture.parents(), fixture.now)
                .contains(&fixture.parents()[0])
        );
        assert!(
            index
                .refresh(&fixture.home, &[(PARENT.into(), fixture.now)], fixture.now)
                .is_empty()
        );
        assert!(
            index
                .refresh(&fixture.home, &fixture.parents(), fixture.now)
                .contains(&fixture.parents()[0])
        );
        let mut file = OpenOptions::new()
            .append(true)
            .open(&fixture.child)
            .unwrap();
        file.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_aborted\",\"turn_id\":\"child-turn\"}}\n").unwrap();
        assert!(
            index
                .refresh(&fixture.home, &fixture.parents(), fixture.now)
                .is_empty()
        );
        file.write_all(rows[5].as_bytes()).unwrap();
        assert!(
            index
                .refresh(&fixture.home, &fixture.parents(), fixture.now)
                .contains(&fixture.parents()[0])
        );
        drop(file);
        fs::remove_file(&fixture.child).unwrap();
        assert!(
            index
                .refresh(&fixture.home, &fixture.parents(), fixture.now)
                .is_empty()
        );
    }
}
