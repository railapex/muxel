//! The app's one file watcher: a notify backend whose events
//! [`WatchRouter`] routes to registrations, so every watched target shares one
//! backend (one inotify instance on Linux, one stream on macOS).
//!
//! Claude's and Muse's session hook records are registered
//! ([`claude_records`], [`muse_panes`]). Without the watch each Claude pane
//! opens its record path every two seconds, and almost always finds nothing:
//! the hook writes only after `/clear`, `/resume` or `/fork`. Each bound Muse
//! pane re-reads its record every second for a state change its hook writes.
//! Discovery still polls slowly while watched
//! ([`muxel_core::session_discovery::WATCHED_HOOK_FALLBACK`]), because a
//! watcher can lose events, and at the usual cadence when it can't watch.
//!
//! So are diff panes' sources ([`diff_sources`]): a visible diff pane runs
//! `git diff` when a tracked file or HEAD's branch changes, instead of every
//! 1.5 s (see [`muxel_core::diff_schedule`]).

use crate::integrations::DiffPlan;
use muxel_core::file_watch::{
    DirNames, Drained, EventKind, OsWatch, Recursion, TrackedFiles, WatchEvent, WatchFilter,
    WatchId, WatchRouter, WatchSpec,
};
use notify::event::ModifyKind;
use notify::{RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

/// `dir` under every name a backend may report it by.
fn dir_names(dir: &Path) -> DirNames {
    DirNames::new(dir.to_path_buf(), dir.canonicalize().ok())
}

/// Claude's session hook records: `<pane>.json` in the hook directory `dir`.
/// Its temporary file is renamed onto that name, and the `.claimed` name is
/// muxel's own, so neither counts. The parent is watched too, without
/// recursion, because only it reports `dir` being removed or renamed on every
/// platform. Losing either detaches the watch from where the hook writes.
pub(crate) fn claude_records(dir: &Path) -> WatchSpec {
    let mut paths = vec![(dir.to_path_buf(), Recursion::Dir)];
    let mut lose_on = vec![dir_names(dir)];
    if let Some(parent) = dir.parent() {
        paths.push((parent.to_path_buf(), Recursion::Dir));
        lose_on.push(dir_names(parent));
    }
    WatchSpec {
        paths,
        filter: WatchFilter::RecordFiles {
            dir: dir_names(dir),
        },
        lose_on,
    }
}

/// The most directories one diff pane watches where each is its own watch
/// (inotify, whose recursive mode walks and watches every directory, ignored
/// ones like `target/` too). A larger checkout's pane polls while visible.
pub(crate) const DIFF_WATCH_DIRS: usize = 4096;

/// What a diff pane over `dir` watches, by `plan`: the tracked files under
/// `dir`, HEAD and the index, and the branch HEAD names. `None` where the
/// pane polls instead: an unsettled repository (see [`DiffPlan::settled`]),
/// a `dir` that can't be resolved, or more directories than
/// [`DIFF_WATCH_DIRS`]. It reads the disk, so it runs off the UI thread.
pub(crate) fn diff_sources(dir: &Path, plan: &DiffPlan) -> Option<WatchSpec> {
    // Windows and macOS watch a tree with one handle or stream.
    let per_dir = !cfg!(any(windows, target_os = "macos"));
    diff_sources_with(dir, plan, per_dir, DIFF_WATCH_DIRS)
}

/// [`diff_sources`], watching each directory on its own when `per_dir`, and
/// at most `budget` paths.
fn diff_sources_with(
    dir: &Path,
    plan: &DiffPlan,
    per_dir: bool,
    budget: usize,
) -> Option<WatchSpec> {
    if !plan.settled {
        return None;
    }
    let real_root = dir.canonicalize().ok()?;
    let root = DirNames::new(dir.to_path_buf(), Some(real_root.clone()));
    let mut paths: Vec<(PathBuf, Recursion)> = Vec::new();
    let mut watch = |path: PathBuf, recursion: Recursion| {
        if !paths.iter().any(|(watched, _)| *watched == path) {
            paths.push((path, recursion));
        }
    };
    watch(
        dir.to_path_buf(),
        if per_dir {
            Recursion::Dir
        } else {
            Recursion::Tree
        },
    );
    // A git file under every name its event may carry: as git printed it,
    // canonical, and under `dir`'s name when the tree watch reports it.
    // Its folder is watched unless that tree covers it already; a folder
    // that doesn't exist yet (`reftable/`, a packed branch's) isn't, and
    // the index or HEAD changing when it appears sends the pane to re-plan.
    let mut git_file = |file: &Path| -> Option<DirNames> {
        let name = file.file_name()?;
        let parent = file.parent()?;
        let real_parent = parent.canonicalize().ok();
        let mut names = DirNames::new(
            file.to_path_buf(),
            real_parent.as_ref().map(|real| real.join(name)),
        );
        let under_root = real_parent
            .as_ref()
            .and_then(|real| real.strip_prefix(&real_root).ok())
            .map(|rel| dir.join(rel));
        if let Some(rel) = &under_root {
            names = names.also(rel.join(name));
        }
        if real_parent.is_some() && (per_dir || under_root.is_none()) {
            watch(parent.to_path_buf(), Recursion::Dir);
        }
        Some(names)
    };
    let relist = plan
        .state
        .iter()
        .filter_map(|file| git_file(file))
        .collect();
    let refs = plan.refs.iter().filter_map(|file| git_file(file)).collect();
    let tracked = TrackedFiles::new(root.clone(), plan.tracked.iter().cloned(), refs, relist);
    if per_dir {
        // A tracked directory that's gone gets its watch when it comes
        // back: its parent's watch reports that, and the pane re-plans.
        for rel in &tracked.dirs {
            let path = dir.join(rel);
            if path.is_dir() {
                watch(path, Recursion::Dir);
            }
        }
    }
    (paths.len() <= budget).then(|| WatchSpec {
        paths,
        filter: WatchFilter::Tracked(Arc::new(tracked)),
        // Gone with its directory; a recreated one isn't watched.
        lose_on: vec![root],
    })
}

/// Muse's session hook records: `<pane>/launch.json` and `<pane>/<nonce>.json`
/// in the hook directory `dir`, so the whole tree is watched. The parent is
/// watched as for [`claude_records`] (the two share it), and losing either
/// detaches the watch likewise.
pub(crate) fn muse_panes(dir: &Path) -> WatchSpec {
    let mut paths = vec![(dir.to_path_buf(), Recursion::Tree)];
    let mut lose_on = vec![dir_names(dir)];
    if let Some(parent) = dir.parent() {
        paths.push((parent.to_path_buf(), Recursion::Dir));
        lose_on.push(dir_names(parent));
    }
    WatchSpec {
        paths,
        filter: WatchFilter::PaneDirs {
            dir: dir_names(dir),
        },
        lose_on,
    }
}

fn watch_event(event: &notify::Event) -> WatchEvent {
    use notify::EventKind as Kind;
    WatchEvent {
        kind: match event.kind {
            Kind::Access(_) => EventKind::Access,
            Kind::Create(_) => EventKind::Create,
            Kind::Modify(ModifyKind::Name(_)) => EventKind::Rename,
            Kind::Modify(_) => EventKind::Modify,
            Kind::Remove(_) => EventKind::Remove,
            Kind::Any | Kind::Other => EventKind::Other,
        },
        paths: event.paths.clone(),
        rescan: event.need_rescan(),
    }
}

fn lock(router: &Mutex<WatchRouter>) -> MutexGuard<'_, WatchRouter> {
    router.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// The handler a backend calls with each event: it routes the event and
/// signals the app. The backend owns it until it stops, so the signal closes,
/// and `alive` drops, when the backend's worker goes away.
pub(crate) struct RouteEvents {
    router: Arc<Mutex<WatchRouter>>,
    signal: async_channel::Sender<()>,
    _alive: Arc<()>,
}

impl notify::EventHandler for RouteEvents {
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        let mut router = lock(&self.router);
        match event {
            Ok(event) => router.event(&watch_event(&event)),
            Err(error) => {
                log::warn!("file watcher failed: {error}");
                router.error(&error.paths);
            }
        }
        if router.has_news() {
            // Full means a signal is already waiting for this news.
            let _ = self.signal.try_send(());
        }
    }
}

/// The running watcher. Dropping it stops it.
pub(crate) struct FileWatch {
    watcher: Option<Box<dyn Watcher>>,
    router: Arc<Mutex<WatchRouter>>,
    /// Upgradable while the backend still holds its handler.
    backend: Weak<()>,
}

impl FileWatch {
    /// Start the platform's watcher, watching nothing yet. The receiver gets
    /// a signal whenever news is waiting in [`Self::take`]; signals coalesce,
    /// so one may stand for many events. The receiver closes if the backend
    /// stops.
    pub fn start() -> anyhow::Result<(Self, async_channel::Receiver<()>)> {
        Self::start_with(notify::recommended_watcher)
    }

    /// As [`Self::start`], with the backend `backend` builds around the handler.
    pub fn start_with<W: Watcher + 'static>(
        backend: impl FnOnce(RouteEvents) -> notify::Result<W>,
    ) -> anyhow::Result<(Self, async_channel::Receiver<()>)> {
        let router = Arc::new(Mutex::new(WatchRouter::new(cfg!(target_os = "macos"))));
        let (signal, receiver) = async_channel::bounded(1);
        let alive = Arc::new(());
        let backend_alive = Arc::downgrade(&alive);
        let watcher = backend(RouteEvents {
            router: Arc::clone(&router),
            signal,
            _alive: alive,
        })?;
        Ok((
            Self {
                watcher: Some(Box::new(watcher)),
                router,
                backend: backend_alive,
            },
            receiver,
        ))
    }

    /// Add a registration and the OS watches it needs. If one can't be added
    /// the registration is undone and the error returned, so its owner polls.
    pub fn register(&mut self, spec: WatchSpec) -> anyhow::Result<WatchId> {
        let (id, ops) = lock(&self.router).register(spec);
        if let Err(error) = self.apply(&ops) {
            self.unregister(id);
            return Err(error);
        }
        Ok(id)
    }

    /// Give a registration a new spec, adding and dropping only the OS
    /// watches that differ. If one can't be added the registration is
    /// dropped and the error returned, so its owner polls.
    pub fn respec(&mut self, id: WatchId, spec: WatchSpec) -> anyhow::Result<()> {
        let ops = lock(&self.router).respec(id, spec);
        if let Err(error) = self.apply(&ops) {
            self.unregister(id);
            return Err(error);
        }
        Ok(())
    }

    /// Drop a registration, and the OS watches nothing else needs.
    pub fn unregister(&mut self, id: WatchId) {
        let ops = lock(&self.router).unregister(id);
        if let Err(error) = self.apply(&ops) {
            log::warn!("file watcher could not narrow its watches: {error}");
        }
    }

    /// Apply the router's OS watch calls. An unwatch that fails (its
    /// directory is already gone) changes nothing that matters.
    fn apply(&mut self, ops: &[OsWatch]) -> anyhow::Result<()> {
        let Some(watcher) = self.watcher.as_mut() else {
            return Ok(());
        };
        for op in ops {
            match op {
                OsWatch::Watch(path, recursion) => {
                    let mode = match recursion {
                        Recursion::Dir => RecursiveMode::NonRecursive,
                        Recursion::Tree => RecursiveMode::Recursive,
                    };
                    watcher.watch(path, mode)?;
                }
                OsWatch::Unwatch(path) => {
                    let _ = watcher.unwatch(path);
                }
            }
        }
        Ok(())
    }

    pub fn take(&self) -> Drained {
        lock(&self.router).take()
    }
}

impl Drop for FileWatch {
    /// A live backend is stopped as usual: on app exit, or after it reported
    /// an error. One that already dropped its handler has lost its worker,
    /// and notify's inotify watcher unwraps a send to that worker when
    /// dropped, which would panic on the UI thread. Its remains (a few file
    /// descriptors) are leaked instead; that happens at most once a run.
    fn drop(&mut self) {
        let Some(watcher) = self.watcher.take() else {
            return;
        };
        if self.backend.strong_count() > 0 {
            drop(watcher);
        } else {
            std::mem::forget(watcher);
        }
    }
}

/// A stand-in for notify's backend, for tests that need to play its failures.
#[cfg(test)]
pub(crate) mod test_backend {
    use super::RouteEvents;
    use notify::{Config, EventHandler, RecursiveMode, Watcher, WatcherKind};
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    /// The test's side of a [`TestWatcher`]: it holds the event handler the
    /// watcher was built around, so the test can play an event, or the
    /// backend's worker dying by dropping that handler.
    #[derive(Clone, Default)]
    pub(crate) struct TestBackend {
        handler: Arc<Mutex<Option<RouteEvents>>>,
        dropped: Arc<AtomicBool>,
    }

    impl TestBackend {
        /// What `FileWatch::start_with` takes.
        pub fn build(&self, handler: RouteEvents) -> notify::Result<TestWatcher> {
            *self.handler.lock().unwrap() = Some(handler);
            Ok(TestWatcher {
                handler: Arc::clone(&self.handler),
                dropped: Arc::clone(&self.dropped),
            })
        }

        /// The backend reports `event` while alive.
        pub fn send(&self, event: notify::Result<notify::Event>) {
            self.handler
                .lock()
                .unwrap()
                .as_mut()
                .expect("the backend is alive")
                .handle_event(event);
        }

        /// The worker dies, as inotify's does when its poll fails: the
        /// handler it owned, and with it the signal's sender, is dropped.
        pub fn kill(&self) {
            drop(self.handler.lock().unwrap().take());
        }

        /// Whether the watcher's destructor ran.
        pub fn dropped(&self) -> bool {
            self.dropped.load(Ordering::SeqCst)
        }
    }

    pub(crate) struct TestWatcher {
        handler: Arc<Mutex<Option<RouteEvents>>>,
        dropped: Arc<AtomicBool>,
    }

    impl Watcher for TestWatcher {
        fn new<F: EventHandler>(_: F, _: Config) -> notify::Result<Self> {
            Err(notify::Error::generic("built by TestBackend"))
        }

        fn watch(&mut self, _: &Path, _: RecursiveMode) -> notify::Result<()> {
            Ok(())
        }

        fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
            Ok(())
        }

        fn kind() -> WatcherKind {
            WatcherKind::NullWatcher
        }
    }

    impl Drop for TestWatcher {
        /// notify's inotify watcher sends its worker a shutdown and unwraps
        /// the result, so dropping it after the worker died panics.
        fn drop(&mut self) {
            assert!(
                self.handler.lock().unwrap().is_some(),
                "dropped a watcher whose worker died"
            );
            self.dropped.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_backend::TestBackend;
    use super::{FileWatch, claude_records, diff_sources, diff_sources_with};
    use crate::integrations::diff_plan;
    use crate::test_support::{TestRepo, git_out, short_temp_path};
    use muxel_core::file_watch::{Change, Recursion, WatchId};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    /// A backend whose worker died has dropped its handler, which closes the
    /// signal. Dropping its watcher then must not run the destructor that
    /// would talk to the dead worker; a live one is stopped as usual.
    #[test]
    fn a_watch_whose_backend_died_is_released_without_its_destructor() {
        let dead = TestBackend::default();
        let (watch, signal) = FileWatch::start_with(|handler| dead.build(handler)).unwrap();
        dead.kill();
        assert!(signal.try_recv().unwrap_err().is_closed());
        drop(watch);
        assert!(!dead.dropped());

        let live = TestBackend::default();
        let (watch, _signal) = FileWatch::start_with(|handler| live.build(handler)).unwrap();
        drop(watch);
        assert!(live.dropped());
    }

    /// Watch Claude's hook directory under `data` through the platform's
    /// backend, creating it as the app does.
    fn watch_claude(data: &Path) -> (FileWatch, async_channel::Receiver<()>, WatchId) {
        let dir = crate::session_binding::claude_binding_dir(data);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut watch, signal) = FileWatch::start().unwrap();
        let id = watch.register(claude_records(&dir)).unwrap();
        (watch, signal, id)
    }

    /// Gather the registration's news until `done` says so or ten seconds pass.
    fn gather(
        watch: &FileWatch,
        signal: &async_channel::Receiver<()>,
        id: WatchId,
        done: impl Fn(&Change) -> bool,
    ) -> Change {
        gather_for(watch, signal, id, Duration::from_secs(10), done)
    }

    fn gather_for(
        watch: &FileWatch,
        signal: &async_channel::Receiver<()>,
        id: WatchId,
        within: Duration,
        done: impl Fn(&Change) -> bool,
    ) -> Change {
        let deadline = Instant::now() + within;
        let mut seen = Change::default();
        while !done(&seen) && Instant::now() < deadline {
            if signal.try_recv().is_err() {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            for (from, change) in watch.take().changes {
                assert_eq!(from, id);
                seen.hits.extend(change.hits);
                seen.touched |= change.touched;
                seen.relist |= change.relist;
                seen.rescan |= change.rescan;
                seen.lost |= change.lost;
            }
        }
        seen
    }

    /// Each watched path, resolved, and whether it's watched as a tree.
    fn resolved(paths: &[(PathBuf, Recursion)]) -> HashSet<(PathBuf, bool)> {
        paths
            .iter()
            .map(|(path, recursion)| (path.canonicalize().unwrap(), *recursion == Recursion::Tree))
            .collect()
    }

    /// A tree backend watches the diff's folder once, its git files inside
    /// it. A per-directory one watches each folder holding tracked files
    /// (no untracked one) and git's, and past its budget the pane polls.
    #[test]
    fn a_diff_watches_its_tree_once_or_each_tracked_directory() {
        let repo = TestRepo::init();
        repo.commit(&[("top.txt", ""), ("a/x.rs", ""), ("a/b/y.rs", "")], "seed");
        std::fs::create_dir_all(repo.path().join("untracked/deep")).unwrap();
        let root = repo.path();
        let plan = diff_plan(root).expect("a work tree");
        let tree = diff_sources_with(root, &plan, false, 1).expect("within budget");
        assert_eq!(
            resolved(&tree.paths),
            HashSet::from([(root.canonicalize().unwrap(), true)])
        );
        let git = root.join(".git");
        let each: Vec<_> = [
            root.to_path_buf(),
            root.join("a"),
            root.join("a").join("b"),
            git.clone(),
            git.join("refs").join("heads"),
        ]
        .into_iter()
        .map(|dir| (dir, Recursion::Dir))
        .collect();
        let per_dir = diff_sources_with(root, &plan, true, each.len()).expect("within budget");
        assert_eq!(resolved(&per_dir.paths), resolved(&each));
        assert!(diff_sources_with(root, &plan, true, each.len() - 1).is_none());
    }

    /// Real writes reach a diff pane's watch through the platform's backend,
    /// in a main checkout and in a linked worktree, whose git files live in
    /// the main repository: an edit to a tracked file, `git add` (the index,
    /// which re-lists), and the branch moving. An untracked file doesn't.
    #[test]
    fn a_diff_wakes_for_real_tracked_and_git_writes_through_the_platform_watcher() {
        let repo = TestRepo::init();
        repo.commit(&[("notes.txt", "first\n"), ("src/lib.rs", "")], "seed");
        let linked = short_temp_path();
        repo.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            &linked.to_string_lossy(),
        ]);
        for dir in [repo.path().to_path_buf(), linked.clone()] {
            let plan = diff_plan(&dir).expect("a work tree");
            let spec = diff_sources(&dir, &plan).expect("watchable");
            let (mut watch, signal) = FileWatch::start().unwrap();
            let id = watch.register(spec).unwrap();
            let at = dir.display().to_string();

            // Each step first lets the last one's trailing events pass.
            let settle = || gather_for(&watch, &signal, id, Duration::from_millis(500), |_| false);

            std::fs::write(dir.join("scratch.txt"), "new\n").unwrap();
            assert!(!settle().touched, "{at}: an untracked file");

            std::fs::write(dir.join("src").join("lib.rs"), "// edited\n").unwrap();
            let seen = gather(&watch, &signal, id, |seen| seen.touched);
            assert!(seen.touched && !seen.relist, "{at}: a tracked edit");
            settle();

            git_out(&dir, &["add", "scratch.txt"]);
            assert!(
                gather(&watch, &signal, id, |seen| seen.relist).relist,
                "{at}: git add"
            );
            settle();

            // A commit that moves the branch alone, without the index.
            let commit = git_out(
                &dir,
                &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "x"],
            );
            git_out(&dir, &["update-ref", "HEAD", commit.trim()]);
            let seen = gather(&watch, &signal, id, |seen| seen.touched);
            assert!(seen.touched && !seen.relist, "{at}: the branch moved");
            assert!(!seen.lost, "{at}");
            drop(watch);
        }
        repo.git(&["worktree", "remove", "--force", &linked.to_string_lossy()]);
        let _ = std::fs::remove_dir_all(&linked);
    }

    /// Deleting or renaming the hook directory or its parent through the real
    /// backend fails the watch, so discovery goes back to polling the
    /// recreated directory.
    #[test]
    fn losing_the_watched_directory_fails_the_watch_through_the_platform_watcher() {
        for (lost, rename) in [(0, false), (0, true), (1, false), (1, true)] {
            let root = std::env::temp_dir().join(format!("muxel-binding-loss-{}", Uuid::new_v4()));
            let data = root.join("data");
            let dir = crate::session_binding::claude_binding_dir(&data);
            let (watch, signal, id) = watch_claude(&data);
            let gone = dir.ancestors().nth(lost).unwrap().to_path_buf();
            if rename {
                let moved = std::fs::rename(&gone, gone.with_extension("moved"));
                if cfg!(windows) && lost == 1 {
                    // Windows refuses to rename a directory while a watched
                    // directory inside it is open, so the hook's path can't
                    // move out from under the watch there.
                    assert!(moved.is_err());
                    drop(watch);
                    let _ = std::fs::remove_dir_all(&root);
                    continue;
                }
                moved.unwrap();
            } else {
                std::fs::remove_dir_all(&gone).unwrap();
            }
            // As the next hook would.
            std::fs::create_dir_all(&dir).unwrap();
            let failed = gather(&watch, &signal, id, |seen| seen.lost).lost;
            drop(watch);
            let _ = std::fs::remove_dir_all(&root);
            assert!(failed, "{}: rename {rename}", gone.display());
        }
    }

    /// The hook's real write (temp file, then rename onto the record name)
    /// reaches the platform watcher as a wake for exactly that pane.
    #[test]
    fn hook_write_wakes_its_pane_through_the_platform_watcher() {
        let root = std::env::temp_dir().join(format!("muxel-binding-watch-{}", Uuid::new_v4()));
        let data = root.join("data");
        let (watch, signal, id) = watch_claude(&data);
        let pane = Uuid::new_v4();
        let transcript = root.join(format!("projects/p/{}.jsonl", Uuid::nil()));
        let event = serde_json::json!({
            "hook_event_name": "SessionStart",
            "source": "clear",
            "session_id": Uuid::nil().to_string(),
            "transcript_path": transcript,
            "cwd": root,
        });
        crate::session_binding::write_claude_binding_from_reader(
            &data,
            pane,
            event.to_string().as_bytes(),
        )
        .unwrap();
        let seen = gather(&watch, &signal, id, |seen| seen.hits.contains(&pane));
        drop(watch);
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(seen.hits, HashSet::from([pane]));
        assert!(!seen.lost);
    }

    /// A Muse pane launched while its hook directory is watched: the launch
    /// creates the pane's directory, and the hook's record written into it
    /// later wakes exactly that pane. On Linux notify watches the new
    /// directory only once it has seen it created; a record written before
    /// then is missed, which is why discovery keeps polling a Muse pane until
    /// it has found its record. Closing the pane still removes its directory
    /// under the watch.
    #[test]
    fn a_new_pane_directory_then_its_record_wakes_the_pane() {
        use crate::session_binding;
        let root = std::env::temp_dir().join(format!("muxel-muse-watch-{}", Uuid::new_v4()));
        let (data, share) = (root.join("data"), root.join("share"));
        let dir = session_binding::muse_binding_dir(&data);
        std::fs::create_dir_all(&dir).unwrap();
        let (mut watch, signal) = FileWatch::start().unwrap();
        let id = watch.register(super::muse_panes(&dir)).unwrap();
        let env = [
            ("XDG_CONFIG_HOME", root.join("config")),
            ("XDG_DATA_HOME", share),
        ]
        .map(|(key, path)| (key.to_string(), path.display().to_string()));
        let pane = Uuid::new_v4();
        assert!(session_binding::prepare_muse_hooks(&data, pane, &env, &[], None, false).is_some());
        // The hook runs once Muse has started: well after its directory.
        std::thread::sleep(Duration::from_millis(500));
        let _ = watch.take();
        let launch: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.join(pane.to_string()).join("launch.json")).unwrap(),
        )
        .unwrap();
        let nonce: Uuid = serde_json::from_value(launch["launch_id"].clone()).unwrap();
        let session = Uuid::new_v4().to_string();
        let event = serde_json::json!({
            "session_id": session, "cwd": root,
            "hook_event_name": "Notification", "notification_type": "permission_prompt",
        });
        let written = session_binding::write_muse_binding_from_reader(
            &data,
            pane,
            nonce,
            &session,
            event.to_string().as_bytes(),
        );
        let seen = gather(&watch, &signal, id, |seen| seen.hits.contains(&pane));
        session_binding::clear_muse_binding(&data, pane);
        let cleared = !dir.join(pane.to_string()).exists();
        drop(watch);
        let _ = std::fs::remove_dir_all(&root);
        written.unwrap();
        assert_eq!(seen.hits, HashSet::from([pane]));
        assert!(!seen.lost);
        assert!(
            cleared,
            "a closed pane's directory is removed while watched"
        );
    }
}
