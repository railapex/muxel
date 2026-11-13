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
use std::time::{Duration, Instant, SystemTime};

const READ_BYTES: u64 = 256 * 1024;
const RECORD_BYTES: usize = 1024 * 1024;

/// Shared by all stores in one app refresh, including repeated catch-up reads.
pub struct CodexReadBudget {
    remaining: u64,
    deadline: Instant,
}

impl Default for CodexReadBudget {
    fn default() -> Self {
        Self {
            remaining: 2 * 1024 * 1024,
            deadline: Instant::now() + Duration::from_millis(50),
        }
    }
}

impl CodexReadBudget {
    fn exhausted(&self) -> bool {
        self.remaining == 0 || Instant::now() >= self.deadline
    }
}

#[derive(Default, Debug)]
pub struct CodexSubagentSnapshot {
    pub running: HashSet<(String, SystemTime)>,
    pub catching_up: HashSet<(String, SystemTime)>,
}

#[derive(Default)]
pub struct CodexSubagents {
    store: Option<PathBuf>,
    oldest_launch: Option<SystemTime>,
    scanned_through: Option<NaiveDate>,
    directories: HashMap<PathBuf, Option<SystemTime>>,
    children: HashMap<PathBuf, Child>,
    new_rollouts: Option<NewRollouts>,
    watch_attempted: bool,
    last_backstop: Option<SystemTime>,
    next_child: usize,
}

impl CodexSubagents {
    /// Return running children and parents whose lifecycle evidence is not current.
    /// Call on a worker; remote panes must not query the local rollout store.
    pub fn refresh(
        &mut self,
        store: &Path,
        parents: &[(String, SystemTime)],
        now: SystemTime,
    ) -> CodexSubagentSnapshot {
        self.refresh_with_budget(store, parents, now, &mut CodexReadBudget::default())
    }

    pub fn refresh_with_budget(
        &mut self,
        store: &Path,
        parents: &[(String, SystemTime)],
        now: SystemTime,
        budget: &mut CodexReadBudget,
    ) -> CodexSubagentSnapshot {
        let Some(oldest) = parents.iter().map(|(_, launched)| *launched).min() else {
            *self = Self::default();
            return CodexSubagentSnapshot::default();
        };
        if self.store.as_deref() != Some(store) {
            *self = Self {
                store: Some(store.to_path_buf()),
                ..Self::default()
            };
        }
        if self.oldest_launch.is_none_or(|seen| oldest < seen) {
            // An older pane can resolve after a newer one. Revisit files skipped
            // by the previous cutoff without discarding parsed child history.
            self.oldest_launch = Some(oldest);
            self.scanned_through = None;
            self.directories.clear();
            self.last_backstop = None;
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
        let mut discovery_current = true;
        for _ in 0..32 {
            if next > last_day {
                break;
            }
            if !self.discover(store, next, oldest) {
                discovery_current = false;
                break;
            }
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
                discovery_current &= self.discover(store, day, oldest);
            }
            self.last_backstop = Some(now);
        }
        let mut requested: HashMap<&str, Vec<SystemTime>> = HashMap::new();
        for (id, launched) in parents {
            requested.entry(id).or_default().push(*launched);
        }
        let mut result = CodexSubagentSnapshot::default();
        if !discovery_current || self.scanned_through.is_none_or(|day| day < last_day) {
            result.catching_up.extend(parents.iter().cloned());
        }
        let mut children: Vec<_> = self.children.iter_mut().collect();
        let count = children.len();
        let start = self.next_child.checked_rem(count).unwrap_or(0);
        children.rotate_left(start);
        let mut read_through = 0;
        for (position, (path, child)) in children.into_iter().enumerate() {
            // Read a new file's metadata once, then follow only requested parents.
            if (child.ordinal > 0 && child.parent.is_none())
                || child
                    .parent
                    .as_deref()
                    .is_some_and(|id| !requested.contains_key(id))
            {
                continue;
            }
            let current = if budget.exhausted() {
                false
            } else {
                // Rotation includes excluded paths, not just eligible reads.
                read_through = position + 1;
                child.refresh(path, budget, &requested)
            };
            if !current {
                if let Some(parent) = &child.parent {
                    for launched in requested.get(parent.as_str()).into_iter().flatten() {
                        result.catching_up.insert((parent.clone(), *launched));
                    }
                } else {
                    // Until metadata is read, any requested parent could own it.
                    result.catching_up.extend(parents.iter().cloned());
                }
            }
            if let (Some(parent), Some((_, started))) = (&child.parent, &child.active) {
                for launched in requested.get(parent.as_str()).into_iter().flatten() {
                    if started >= launched {
                        result.running.insert((parent.clone(), *launched));
                    }
                }
            }
        }
        self.next_child = (start + read_through).checked_rem(count).unwrap_or(0);
        result
    }

    fn discover(&mut self, store: &Path, day: NaiveDate, oldest: SystemTime) -> bool {
        let path = store.join(day.format("%Y/%m/%d").to_string());
        let stamp = match std::fs::metadata(&path) {
            Ok(meta) => meta.modified().ok(),
            Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
        };
        if stamp.is_some() && self.directories.get(&path) == Some(&stamp) {
            return true;
        }
        let Ok(entries) = std::fs::read_dir(&path) else {
            return false;
        };
        let mut current = true;
        for entry in entries {
            match entry {
                Ok(entry) => self.discover_path(entry.path(), oldest),
                Err(_) => current = false,
            }
        }
        if current {
            self.directories.insert(path, stamp);
        } else {
            self.directories.remove(&path);
        }
        current
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
    fn refresh(
        &mut self,
        path: &Path,
        budget: &mut CodexReadBudget,
        requested: &HashMap<&str, Vec<SystemTime>>,
    ) -> bool {
        if budget.exhausted() {
            return false;
        }
        let meta = match std::fs::metadata(path) {
            Ok(meta) => meta,
            Err(error) => {
                if error.kind() == std::io::ErrorKind::NotFound {
                    self.active = None;
                    self.stamp = None;
                    return true;
                }
                return false;
            }
        };
        let stamp = (meta.len(), meta.modified().ok());
        if self.stamp == Some(stamp) && self.offset == meta.len() {
            return self.pending.is_empty() && !self.skipping;
        }
        if meta.len() < self.offset {
            *self = Self {
                id: self.id.clone(),
                created: self.created,
                ..Self::default()
            };
        }
        let Ok(mut file) = std::fs::File::open(path) else {
            return false;
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        while self.offset < meta.len() && !budget.exhausted() {
            let mut bytes = Vec::new();
            if (&mut file)
                .take(READ_BYTES.min(budget.remaining))
                .read_to_end(&mut bytes)
                .is_err()
            {
                return false;
            }
            if bytes.is_empty() {
                break;
            }
            budget.remaining -= bytes.len() as u64;
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
            // Newly discovered root files and other parents need only their header.
            // Finish parsing the chunk so a later parent request loses no records.
            if self.ordinal > 0
                && self
                    .parent
                    .as_deref()
                    .is_none_or(|id| !requested.contains_key(id))
            {
                return true;
            }
        }
        self.offset >= meta.len() && self.pending.is_empty() && !self.skipping
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
    use super::{Child, CodexReadBudget, CodexSubagentSnapshot, CodexSubagents};
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

        fn read(
            &self,
            index: &mut CodexSubagents,
            parents: &[(String, SystemTime)],
        ) -> CodexSubagentSnapshot {
            // Lifecycle assertions do not impose the production clock slice.
            // Exhausted byte/time budgets are exercised separately below.
            let mut budget = CodexReadBudget {
                deadline: std::time::Instant::now() + Duration::from_secs(5),
                ..CodexReadBudget::default()
            };
            index.refresh_with_budget(&self.home, parents, self.now, &mut budget)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.home).unwrap();
        }
    }

    #[test]
    fn background_codex_failed_directory_scan_is_pending_and_can_recover() {
        let fixture = Fixture::new();
        let day = fixture.child.parent().unwrap();
        fs::remove_dir(day).unwrap();
        fs::write(day, "not a directory").unwrap();
        let mut index = CodexSubagents::default();
        let parent = fixture.parents()[0].clone();
        let pending = fixture.read(&mut index, &fixture.parents());
        assert!(pending.running.is_empty());
        assert!(pending.catching_up.contains(&parent));
        fs::remove_file(day).unwrap();
        fs::create_dir(day).unwrap();
        let rows: Vec<_> = ROWS.split_inclusive('\n').collect();
        fs::write(&fixture.child, rows[..6].concat()).unwrap();
        let current = fixture.read(&mut index, &fixture.parents());
        assert!(current.running.contains(&parent));
        assert!(current.catching_up.is_empty());
    }

    #[test]
    fn background_codex_shared_budget_withholds_a_partial_completion() {
        let fixture = Fixture::new();
        let rows: Vec<_> = ROWS.split_inclusive('\n').collect();
        let mut history = rows[..6].concat();
        for _ in 0..3 {
            history.push_str(&format!(
                "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"{}\"}}}}\n",
                "x".repeat(200_000)
            ));
        }
        history.push_str(rows[7]);
        fs::write(&fixture.child, history).unwrap();
        let parent = fixture.parents()[0].clone();
        let mut index = CodexSubagents::default();
        let mut budget = CodexReadBudget {
            remaining: 256 * 1024,
            deadline: std::time::Instant::now() + Duration::from_secs(5),
        };
        let partial =
            index.refresh_with_budget(&fixture.home, &fixture.parents(), fixture.now, &mut budget);
        assert!(partial.running.contains(&parent));
        assert!(partial.catching_up.contains(&parent));
        assert_eq!(budget.remaining, 0);
        // A second store in the same worker receives no additional read allowance.
        let mut other = CodexSubagents::default();
        let pending =
            other.refresh_with_budget(&fixture.home, &fixture.parents(), fixture.now, &mut budget);
        assert!(pending.running.is_empty());
        assert!(pending.catching_up.contains(&parent));
        let mut expired = CodexReadBudget {
            deadline: std::time::Instant::now() - Duration::from_secs(1),
            ..CodexReadBudget::default()
        };
        let pending =
            other.refresh_with_budget(&fixture.home, &fixture.parents(), fixture.now, &mut expired);
        assert!(pending.running.is_empty());
        assert!(pending.catching_up.contains(&parent));
        assert_eq!(expired.remaining, 2 * 1024 * 1024);
        let current = fixture.read(&mut index, &fixture.parents());
        assert!(current.catching_up.is_empty());
        assert!(current.running.is_empty());
    }

    #[test]
    fn background_codex_excluded_files_do_not_starve_another_parents_completion() {
        for excluded in [1, 8, 64] {
            let fixture = Fixture::new();
            let launched = fixture.parents()[0].1;
            let parents = vec![(PARENT.into(), launched), ("other-parent".into(), launched)];
            let mut index = CodexSubagents {
                store: Some(fixture.home.clone()),
                watch_attempted: true,
                ..Default::default()
            };
            for slot in 0..excluded + 2 {
                let id = format!("01a1bbaa-121c-7000-8000-{slot:012x}");
                let path = fixture
                    .child
                    .parent()
                    .unwrap()
                    .join(format!("rollout-2026-11-08T07-57-27-{id}.jsonl"));
                index.children.insert(
                    path,
                    Child {
                        id,
                        created: fixture.now,
                        ..Default::default()
                    },
                );
            }
            // Assign roles after insertion to fix the adverse order without
            // assuming a HashMap seed. Existing-path discovery cannot grow it.
            index.children.reserve(1);
            let order: Vec<_> = index.children.keys().cloned().collect();
            let rows: Vec<_> = ROWS.lines().collect();
            for (position, path) in order.iter().enumerate() {
                let parent = if position == excluded {
                    Some(PARENT)
                } else if position == excluded + 1 {
                    Some("other-parent")
                } else if position % 2 == 0 {
                    None // Already-parsed roots are excluded.
                } else {
                    Some("unrequested-parent") // So are other parents' children.
                };
                let child = index.children.get_mut(path).unwrap();
                let metadata = serde_json::json!({
                    "type": "session_meta",
                    "payload": {"id": child.id, "parent_thread_id": parent},
                });
                let mut prefix = format!("{metadata}\n");
                if position >= excluded {
                    prefix.push_str(rows[5]); // A previously observed own turn.
                    prefix.push('\n');
                }
                for record in prefix.as_bytes().split_inclusive(|byte| *byte == b'\n') {
                    child.record(record);
                    child.ordinal += 1;
                }
                child.offset = prefix.len() as u64;
                let tail = if position == excluded {
                    format!(
                        "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"{}\"}}}}\n",
                        "x".repeat(200_000)
                    )
                } else if position == excluded + 1 {
                    format!("{}\n", rows[7]) // This child has already completed.
                } else {
                    String::new()
                };
                fs::write(path, prefix + &tail).unwrap();
            }
            let mut refresh = || {
                let mut budget = CodexReadBudget {
                    remaining: 64 * 1024,
                    deadline: std::time::Instant::now() + Duration::from_secs(5),
                };
                index.refresh_with_budget(&fixture.home, &parents, fixture.now, &mut budget)
            };
            let first = refresh();
            assert!(first.running.contains(&parents[1]));
            assert!(first.catching_up.contains(&parents[1]));
            let second = refresh();
            assert!(second.catching_up.contains(&parents[0]));
            assert!(
                !second.catching_up.contains(&parents[1]),
                "{excluded} excluded files must not put the large child first again"
            );
            assert!(!second.running.contains(&parents[1]));
        }
    }

    #[test]
    fn background_codex_large_history_is_not_published_as_no_child() {
        let fixture = Fixture::new();
        let rows: Vec<_> = ROWS.split_inclusive('\n').collect();
        let mut history = rows[0].to_string();
        for _ in 0..3 {
            history.push_str(&format!(
                "{{\"type\":\"response_item\",\"payload\":{{\"text\":\"{}\"}}}}\n",
                "x".repeat(200_000)
            ));
        }
        history.push_str(rows[5]);
        fs::write(&fixture.child, history).unwrap();
        let mut index = CodexSubagents::default();
        let observed = index.refresh(&fixture.home, &fixture.parents(), fixture.now);
        let parent = fixture.parents()[0].clone();
        assert!(observed.running.contains(&parent) || observed.catching_up.contains(&parent));
        let current = fixture.read(&mut index, &fixture.parents());
        assert!(current.catching_up.is_empty());
        assert!(current.running.contains(&parent));
    }

    #[test]
    fn background_codex_child_fixture_tracks_its_own_turn_and_complete_rows() {
        let fixture = Fixture::new();
        let rows: Vec<_> = ROWS.split_inclusive('\n').collect();
        let mut index = CodexSubagents::default();
        // The copied parent's last turn is running. It belongs to the parent.
        fs::write(&fixture.child, rows[..4].concat()).unwrap();
        assert!(
            fixture
                .read(&mut index, &fixture.parents())
                .running
                .is_empty()
        );
        let mut file = OpenOptions::new()
            .append(true)
            .open(&fixture.child)
            .unwrap();
        file.write_all(rows[4..6].concat().as_bytes()).unwrap();
        assert_eq!(
            fixture.read(&mut index, &fixture.parents()).running,
            [fixture.parents()[0].clone()].into()
        );
        // An assistant's final text is not a completed turn.
        file.write_all(rows[6].as_bytes()).unwrap();
        assert!(
            fixture
                .read(&mut index, &fixture.parents())
                .running
                .contains(&fixture.parents()[0])
        );
        let complete = rows[7];
        let split = complete.len() / 2;
        file.write_all(&complete.as_bytes()[..split]).unwrap();
        assert!(
            fixture
                .read(&mut index, &fixture.parents())
                .running
                .contains(&fixture.parents()[0])
        );
        file.write_all(&complete.as_bytes()[split..]).unwrap();
        assert!(
            fixture
                .read(&mut index, &fixture.parents())
                .running
                .is_empty()
        );
    }

    #[test]
    fn background_codex_discovery_catches_up_when_an_older_pane_resolves_later() {
        let fixture = Fixture::new();
        let rows: Vec<_> = ROWS.split_inclusive('\n').collect();
        fs::write(&fixture.child, rows[..6].concat()).unwrap();
        let mut index = CodexSubagents::default();
        let mut parents = vec![(Uuid::new_v4().to_string(), fixture.now)];
        assert!(fixture.read(&mut index, &parents).running.is_empty());

        // Without file changes or watcher events, the older pane still owns live work.
        index.new_rollouts = None;
        parents.extend(fixture.parents());
        assert!(
            fixture
                .read(&mut index, &parents)
                .running
                .contains(&fixture.parents()[0])
        );
    }

    #[test]
    fn background_codex_children_belong_to_one_parent_launch_and_clear_on_abort_or_removal() {
        let fixture = Fixture::new();
        let rows: Vec<_> = ROWS.split_inclusive('\n').collect();
        fs::write(&fixture.child, rows[..6].concat()).unwrap();
        let mut index = CodexSubagents::default();
        assert!(
            fixture
                .read(&mut index, &[(CHILD.into(), fixture.parents()[0].1)])
                .running
                .is_empty()
        );
        assert!(
            fixture
                .read(&mut index, &fixture.parents())
                .running
                .contains(&fixture.parents()[0])
        );
        assert!(
            fixture
                .read(&mut index, &[(PARENT.into(), fixture.now)])
                .running
                .is_empty()
        );
        assert!(
            fixture
                .read(&mut index, &fixture.parents())
                .running
                .contains(&fixture.parents()[0])
        );
        let mut file = OpenOptions::new()
            .append(true)
            .open(&fixture.child)
            .unwrap();
        file.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"turn_aborted\",\"turn_id\":\"child-turn\"}}\n").unwrap();
        assert!(
            fixture
                .read(&mut index, &fixture.parents())
                .running
                .is_empty()
        );
        file.write_all(rows[5].as_bytes()).unwrap();
        assert!(
            fixture
                .read(&mut index, &fixture.parents())
                .running
                .contains(&fixture.parents()[0])
        );
        drop(file);
        fs::remove_file(&fixture.child).unwrap();
        assert!(
            fixture
                .read(&mut index, &fixture.parents())
                .running
                .is_empty()
        );
    }
}
