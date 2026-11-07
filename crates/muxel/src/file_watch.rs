//! The app's one file watcher: a notify backend whose events
//! [`WatchRouter`] routes to registrations, so every watched target shares one
//! backend (one inotify instance on Linux, one stream on macOS).
//!
//! Claude's session hook records are registered today ([`claude_records`]).
//! Without the watch each Claude pane opens its record path every two seconds,
//! and almost always finds nothing: the hook writes only after `/clear`,
//! `/resume` or `/fork`. Discovery still polls slowly while watched
//! ([`muxel_core::session_discovery::WATCHED_HOOK_FALLBACK`]), because a
//! watcher can lose events, and at the usual cadence when it can't watch.
//!
//! On Windows so is Grok's session registry (`grok_registry`), which each
//! Grok pane otherwise reads every one to two seconds.

use muxel_core::file_watch::{
    DirNames, Drained, EventKind, OsWatch, Recursion, WatchEvent, WatchFilter, WatchId,
    WatchRouter, WatchSpec,
};
use notify::event::ModifyKind;
use notify::{RecursiveMode, Watcher};
use std::path::Path;
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

/// Grok's session registry, `active_sessions.json` in Grok's folder `dir`,
/// which also holds credentials, logs and locks that change on their own.
/// Every Grok pane reads it, so a write wakes them all. The parent is watched
/// as for [`claude_records`], since only it reports `dir` going away.
#[cfg(windows)]
pub(crate) fn grok_registry(dir: &Path) -> WatchSpec {
    let mut paths = vec![(dir.to_path_buf(), Recursion::Dir)];
    let mut lose_on = vec![dir_names(dir)];
    if let Some(parent) = dir.parent() {
        paths.push((parent.to_path_buf(), Recursion::Dir));
        lose_on.push(dir_names(parent));
    }
    WatchSpec {
        paths,
        filter: WatchFilter::FileName {
            dir: dir_names(dir),
            name: crate::app::GROK_REGISTRY.into(),
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

        /// The backend reports `event` while alive. Only the app's watcher
        /// tests play one, and they run on Windows alone.
        #[cfg(windows)]
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
    use super::{FileWatch, claude_records};
    use muxel_core::file_watch::{Change, WatchId};
    use std::collections::HashSet;
    use std::path::Path;
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
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Change::default();
        while !done(&seen) && Instant::now() < deadline {
            if signal.try_recv().is_err() {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            for (from, change) in watch.take().changes {
                assert_eq!(from, id);
                seen.hits.extend(change.hits);
                seen.rescan |= change.rescan;
                seen.lost |= change.lost;
            }
        }
        seen
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
}
