//! When `git status` runs for the Changes view and the worktrees' change
//! counts, and how those learn their files changed.
//!
//! Each target is one work tree, keyed by what shows it: the active project,
//! whose changed files the Changes view's Files tab lists while it's on
//! screen, and every worktree, whose change count sits beside it in the
//! sidebar and whose files the Worktrees tab lists from the same `git
//! status`. A [`DiffSchedule`] decides when each is due, as for diff panes: a
//! watched target runs `git status` when something it lists may have changed
//! ([`status_sources`]) and every 30 s anyway; one the watcher can't cover (a
//! remote project, a repository the files can't vouch for) polls: the
//! project every 1.5 s while shown, a worktree every 10 s, or every 1.5 s
//! while the Worktrees tab lists it. Opening the Worktrees tab refreshes the
//! worktrees it lists.
//!
//! A path coming or going that the plan neither tracks nor knows git ignores
//! (a fresh `target/`, an object file beside its source) is asked about
//! first: one `git check-ignore` for the burst, and `git status` only when
//! git doesn't ignore something in it. What git said it ignores is kept
//! until the plan is read again, which a change to any ignore rule causes.

use super::watching::WatchTarget;
use super::{GitDiffTab, MuxelApp, WORKTREE_CHANGES_EVERY, hotbar};
use crate::file_watch::{self, status_sources};
use crate::integrations::{self, GitChange, RepoLoc, StatusPlan};
use crate::ui_profile;
use gpui::{Context, Task, WeakEntity};
use muxel_core::diff_schedule::{BACKSTOP, DiffSchedule, POLL};
use muxel_core::file_watch::{Change, WatchId, WatchSpec};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use uuid::Uuid;

/// Refresh state for every `git status` target.
#[derive(Default)]
pub(super) struct StatusTargets {
    pub(super) schedule: DiffSchedule,
    pub(super) sources: HashMap<Uuid, StatusSource>,
    /// The project whose target the Files tab reads, once it has been shown.
    project: Option<Uuid>,
    /// What the last sync saw.
    seen: Option<Seen>,
    /// The worktrees the Worktrees tab lists, polled at its pace.
    listed: HashSet<Uuid>,
    armed: Option<Instant>,
    timer: Option<Task<()>>,
}

/// What [`MuxelApp::sync_status_targets`] reconciles against.
#[derive(Clone, PartialEq)]
struct Seen {
    active: Option<Uuid>,
    /// The active project's repository: Settings can point it elsewhere.
    root: Option<StatusRoot>,
    files_shown: bool,
    worktrees_shown: bool,
    worktrees: Vec<(Uuid, StatusRoot)>,
}

/// What a target's refresh shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StatusKind {
    /// The Changes view's Files tab, for the active project.
    Project,
    /// A worktree's change count and its Worktrees-tab row.
    Worktree,
}

impl StatusKind {
    fn label(self) -> &'static str {
        match self {
            StatusKind::Project => "project",
            StatusKind::Worktree => "worktree",
        }
    }
}

/// The repository a target reads: a folder here, or one on a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StatusRoot {
    Local(PathBuf),
    Remote(Uuid, String),
}

/// What one target's refreshes read.
pub(super) struct StatusSource {
    kind: StatusKind,
    /// The repository it was made for. A project moved to another folder
    /// gets a new target; a refresh of the old one is dropped when it lands.
    root: StatusRoot,
    /// The plan its watch was built from; `None` before the first refresh, or
    /// where it isn't in a work tree, or remote.
    plan: Option<Arc<StatusPlan>>,
    /// The untracked directories its last `git status` showed collapsed.
    untracked_dirs: Vec<PathBuf>,
    /// Resolve the plan again at the next refresh: none yet, HEAD, the index
    /// or what's ignored changed, a directory came or went, events were lost,
    /// or the folder was gone.
    pub(super) replan: bool,
    /// Something `git status` lists changed, or may have (lost events, an
    /// explicit refresh): the next refresh runs it, whatever git says about
    /// `pending`.
    dirty: bool,
    /// Paths unknown to the plan that came or went since the last refresh;
    /// it asks git which of them it ignores (see [`Change::unknown`]).
    pending: HashSet<PathBuf>,
    /// Paths git said it ignores since the plan was read, and so everything
    /// under them.
    learned: HashSet<PathBuf>,
    /// When `git status` last ran for it.
    status_at: Option<Instant>,
    pub(super) watch: Option<WatchId>,
}

impl StatusSource {
    /// Take a watch's news. Whether it may change what `git status` lists,
    /// so the target is due once the writes settle.
    pub(super) fn note(&mut self, change: &Change) -> bool {
        eprintln!("PROBE[{}] note kind={:?} touched={} relist={} rescan={} unknown={:?} learned={:?}", std::thread::current().name().unwrap_or("?"), self.kind, change.touched, change.relist, change.rescan, change.unknown, self.learned);
        self.dirty |= change.touched || change.rescan;
        // Lost events may include HEAD's, the index's or an ignore file's.
        self.replan |= change.relist || change.rescan;
        let mut fresh = false;
        for path in &change.unknown {
            if !hidden(&self.learned, path) {
                fresh |= self.pending.insert(path.clone());
            }
        }
        change.touched || fresh
    }
}

/// Whether `path` is, or is under, one of `ignored`.
fn hidden(ignored: &HashSet<PathBuf>, path: &Path) -> bool {
    path.ancestors().any(|path| ignored.contains(path))
}

/// `paths` and the directories above each, below the top level: git
/// ignoring a fresh `target` covers whatever a build writes under it.
fn with_ancestors(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut all = BTreeSet::new();
    for path in paths {
        for path in path.ancestors() {
            if path.as_os_str().is_empty() || !all.insert(path.to_path_buf()) {
                break;
            }
        }
    }
    all.into_iter().collect()
}

/// One refresh, run off the UI thread.
struct StatusJob {
    key: Uuid,
    root: StatusRoot,
    loc: RepoLoc,
    plan: Option<Arc<StatusPlan>>,
    untracked_dirs: Vec<PathBuf>,
    replan: bool,
    /// Run `git status` whatever git says about `pending`.
    must: bool,
    watched: bool,
    pending: Vec<PathBuf>,
    learned: HashSet<PathBuf>,
}

struct StatusResult {
    key: Uuid,
    root: StatusRoot,
    /// `None` when `git status` didn't run: git ignores everything that came
    /// or went.
    files: Option<Vec<GitChange>>,
    untracked_dirs: Vec<PathBuf>,
    /// The plan this refresh used and the watch built from it, when either
    /// changed; `None` when it kept the target's.
    planned: Option<(Option<Arc<StatusPlan>>, Option<WatchSpec>)>,
    /// It read the plan again, so what git said it ignores may not hold.
    replanned: bool,
    /// What git said it ignores, of the paths it was asked about.
    ignored: Vec<PathBuf>,
    /// The folder was gone: resolve the plan once it's back.
    gone: bool,
}

/// The untracked directories `git status` showed collapsed (`?? notes/`),
/// relative to the work tree's top level.
fn untracked_dirs(files: &[GitChange]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = files
        .iter()
        .filter(|change| change.status == "??" && change.path.ends_with('/'))
        .map(|change| PathBuf::from(&change.path).components().collect())
        .collect();
    dirs.sort();
    dirs
}

impl StatusResult {
    /// A result that changes nothing; each refresh fills in what it found.
    fn of(job: &StatusJob) -> Self {
        Self {
            key: job.key,
            root: job.root.clone(),
            files: None,
            untracked_dirs: Vec::new(),
            planned: None,
            replanned: false,
            ignored: Vec::new(),
            gone: false,
        }
    }
}

impl StatusJob {
    fn run(mut self) -> StatusResult {
        eprintln!("PROBE[{}] run key={} loc={:?} must={} replan={} watched={} pending={:?}", std::thread::current().name().unwrap_or("?"), self.key, match &self.loc { RepoLoc::Local(d) => d.display().to_string(), _ => "remote".into() }, self.must, self.replan, self.watched, self.pending);
        let RepoLoc::Local(dir) = &self.loc else {
            // Remote: read over ssh, never watched.
            return StatusResult {
                files: Some(integrations::git_status_files(&self.loc)),
                planned: self.replan.then_some((None, None)),
                replanned: self.replan,
                ..StatusResult::of(&self)
            };
        };
        // A folder that's gone runs no git while it stays gone.
        if !dir.is_dir() {
            return StatusResult {
                files: Some(Vec::new()),
                planned: Some((None, None)),
                replanned: true,
                gone: true,
                ..StatusResult::of(&self)
            };
        }
        // An event the watcher lost may have left the plan behind HEAD, the
        // index or an ignore rule.
        self.replan |= self.watched && self.plan.as_ref().is_some_and(|plan| !plan.current());
        eprintln!("PROBE[{}] after-stamp replan={} current={:?}", std::thread::current().name().unwrap_or("?"), self.replan, self.plan.as_ref().map(|p| p.current()));
        let plan = if self.replan {
            integrations::status_plan(dir).map(Arc::new)
        } else {
            self.plan.clone()
        };
        let mut ignored = Vec::new();
        if let Some(p) = &plan { eprintln!("PROBE[{}] plan root={} ignored={:?} git_dir={:?} settled={} state={:?}", std::thread::current().name().unwrap_or("?"), p.root.display(), p.ignored, p.git_dir, p.diff.settled, p.diff.state); }
        if !(self.must || self.replan || self.pending.is_empty())
            && let Some(plan) = &plan
            && let Some(found) =
                integrations::check_ignored(&plan.root, &with_ancestors(&self.pending))
        {
            eprintln!("PROBE[{}] check_ignored asked={:?} found={:?}", std::thread::current().name().unwrap_or("?"), with_ancestors(&self.pending), found);
            let all = self.pending.iter().all(|path| hidden(&found, path));
            ignored = found.into_iter().collect();
            if all {
                return StatusResult {
                    ignored,
                    ..StatusResult::of(&self)
                };
            }
        }
        eprintln!("PROBE[{}] status runs key={}", std::thread::current().name().unwrap_or("?"), self.key);
        let files = integrations::git_status_files(&self.loc);
        let dirs = untracked_dirs(&files);
        // Where each directory is its own watch, a directory may have come
        // or gone: the watch is rebuilt from the plan (a walk, no git), and
        // only the OS watches that differ change.
        let rewalk = file_watch::PER_DIR && plan.is_some();
        let planned = (self.replan || dirs != self.untracked_dirs || rewalk).then(|| {
            let mut learned = if self.replan {
                HashSet::new()
            } else {
                std::mem::take(&mut self.learned)
            };
            learned.extend(ignored.iter().cloned());
            let spec = plan
                .as_deref()
                .and_then(|plan| status_sources(plan, &dirs, &learned));
            (plan, spec)
        });
        StatusResult {
            files: Some(files),
            untracked_dirs: dirs,
            planned,
            replanned: self.replan,
            ignored,
            ..StatusResult::of(&self)
        }
    }
}

impl MuxelApp {
    /// Run each frame: keep a target for the active project once the Files
    /// tab has shown it (on screen while it shows), and one for every
    /// worktree (always on screen: its count is in the sidebar).
    pub(super) fn sync_status_targets(&mut self, cx: &mut Context<Self>) {
        let active = self.workspace.active_project;
        let changes_shown = self.show_git_diff || self.main_view == hotbar::MainView::Changes;
        let seen = Seen {
            active,
            root: active.and_then(|pid| self.status_root(pid, StatusKind::Project)),
            files_shown: changes_shown && self.git_diff_tab == GitDiffTab::Files,
            worktrees_shown: changes_shown && self.git_diff_tab == GitDiffTab::Worktrees,
            worktrees: self
                .workspace
                .worktrees
                .iter()
                .map(|w| (w.id, StatusRoot::Local(w.path.clone())))
                .collect(),
        };
        if self.status_targets.seen.as_ref() == Some(&seen) {
            return;
        }
        self.status_targets.seen = Some(seen.clone());

        // Another project, or this one moved to another folder.
        if let Some(pid) = self.status_targets.project
            && (Some(pid) != active || self.status_source_root(pid) != seen.root.as_ref())
        {
            self.forget_status(pid);
            self.status_targets.project = None;
            self.git_diff_files = None;
        }
        if seen.files_shown
            && self.status_targets.project.is_none()
            && let Some(pid) = active
            && let Some(root) = seen.root.clone()
        {
            self.add_status(pid, StatusKind::Project, root);
            self.status_targets.project = Some(pid);
        }
        if let Some(pid) = self.status_targets.project {
            self.status_targets
                .schedule
                .set_visible(pid, seen.files_shown);
        }

        let gone: Vec<Uuid> = self
            .status_targets
            .sources
            .iter()
            .filter(|(id, source)| {
                source.kind == StatusKind::Worktree
                    && !seen
                        .worktrees
                        .iter()
                        .any(|(wid, root)| wid == *id && *root == source.root)
            })
            .map(|(id, _)| *id)
            .collect();
        for wid in gone {
            self.forget_status(wid);
            self.status_targets.listed.remove(&wid);
            self.git_diff_worktree_files.remove(&wid);
            self.worktree_changes.remove(&wid);
        }
        for (wid, root) in &seen.worktrees {
            if !self.status_targets.sources.contains_key(wid) {
                self.add_status(*wid, StatusKind::Worktree, root.clone());
                self.status_targets
                    .schedule
                    .set_poll(*wid, WORKTREE_CHANGES_EVERY);
                self.status_targets.schedule.set_visible(*wid, true);
            }
        }

        // The Worktrees tab lists the active project's worktrees: each
        // refreshes as it opens, and an unwatched one polls as the Files tab
        // does while listed.
        let listed: HashSet<Uuid> = if seen.worktrees_shown {
            self.active_worktrees().into_iter().collect()
        } else {
            HashSet::new()
        };
        let opened: Vec<Uuid> = listed
            .difference(&self.status_targets.listed)
            .copied()
            .collect();
        let closed: Vec<Uuid> = self
            .status_targets
            .listed
            .difference(&listed)
            .copied()
            .collect();
        for wid in closed {
            self.status_targets
                .schedule
                .set_poll(wid, WORKTREE_CHANGES_EVERY);
        }
        for wid in opened {
            self.status_targets.schedule.set_poll(wid, POLL);
            self.request_status(wid);
        }
        self.status_targets.listed = listed;
        self.pump_statuses(cx);
    }

    /// The repository `key`'s refreshes read now.
    fn status_root(&self, key: Uuid, kind: StatusKind) -> Option<StatusRoot> {
        match kind {
            StatusKind::Project => {
                let project = self.workspace.project(key)?;
                Some(match &project.remote {
                    Some(remote) => StatusRoot::Remote(remote.host_id, remote.remote_root.clone()),
                    None => StatusRoot::Local(project.root_path.clone()),
                })
            }
            StatusKind::Worktree => self
                .workspace
                .worktree(key)
                .map(|w| StatusRoot::Local(w.path.clone())),
        }
    }

    fn status_source_root(&self, key: Uuid) -> Option<&StatusRoot> {
        self.status_targets
            .sources
            .get(&key)
            .map(|source| &source.root)
    }

    /// The active project's worktrees that have a target.
    fn active_worktrees(&self) -> Vec<Uuid> {
        let active = self.workspace.active_project;
        self.workspace
            .worktrees
            .iter()
            .filter(|w| Some(w.project_id) == active)
            .map(|w| w.id)
            .filter(|id| self.status_targets.sources.contains_key(id))
            .collect()
    }

    fn add_status(&mut self, key: Uuid, kind: StatusKind, root: StatusRoot) {
        self.status_targets.sources.insert(
            key,
            StatusSource {
                kind,
                root,
                plan: None,
                untracked_dirs: Vec::new(),
                replan: true,
                dirty: true,
                pending: HashSet::new(),
                learned: HashSet::new(),
                status_at: None,
                watch: None,
            },
        );
    }

    fn forget_status(&mut self, key: Uuid) {
        if let Some(id) = self
            .status_targets
            .sources
            .remove(&key)
            .and_then(|source| source.watch)
        {
            self.drop_watch(id);
        }
        self.status_targets.schedule.remove(key);
    }

    /// Run `key`'s `git status` at the first chance.
    fn request_status(&mut self, key: Uuid) {
        if let Some(source) = self.status_targets.sources.get_mut(&key) {
            source.dirty = true;
            self.status_targets.schedule.request(key);
        }
    }

    /// Refresh what the Changes view shows at the first chance: the refresh
    /// button, or a git action from the view. A refresh already running
    /// finishes first.
    pub(super) fn request_changes(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<Uuid> = match self.git_diff_tab {
            GitDiffTab::Files => self.status_targets.project.into_iter().collect(),
            GitDiffTab::Worktrees => self.active_worktrees(),
        };
        for key in keys {
            self.request_status(key);
        }
        self.pump_statuses(cx);
    }

    /// Recount every worktree at the first chance: a git action from muxel
    /// (commit, stash, pull…) may have changed any of them.
    pub(super) fn request_worktree_counts(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<Uuid> = self
            .status_targets
            .sources
            .iter()
            .filter(|(_, source)| source.kind == StatusKind::Worktree)
            .map(|(id, _)| *id)
            .collect();
        for key in keys {
            self.request_status(key);
        }
        self.pump_statuses(cx);
    }

    /// Start every refresh that's due, and wake again when the next one is.
    pub(super) fn pump_statuses(&mut self, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let due = self.status_targets.schedule.take_due(now);
        if !due.is_empty() {
            self.refresh_statuses(due, cx);
        }
        let next = self.status_targets.schedule.next_due(now);
        if next == self.status_targets.armed {
            return;
        }
        self.status_targets.armed = next;
        self.status_targets.timer = next.map(|at| {
            cx.spawn(async move |this: WeakEntity<Self>, cx| {
                cx.background_executor()
                    .timer(at.saturating_duration_since(now))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.status_targets.armed = None;
                    this.pump_statuses(cx);
                });
            })
        });
    }

    /// Where `key`'s `git status` runs: the project's repository (over ssh
    /// for a remote one), or the worktree's folder.
    fn status_loc(&self, key: Uuid, kind: StatusKind) -> Option<RepoLoc> {
        match kind {
            StatusKind::Project => self.repo_loc(key),
            StatusKind::Worktree => self
                .workspace
                .worktree(key)
                .map(|w| RepoLoc::Local(w.path.clone())),
        }
    }

    /// Run `git status` for `keys` on a background thread, then show each
    /// result.
    fn refresh_statuses(&mut self, keys: Vec<Uuid>, cx: &mut Context<Self>) {
        let now = cx.background_executor().now();
        let mut jobs = Vec::with_capacity(keys.len());
        for key in keys {
            let Some(source) = self.status_targets.sources.get(&key) else {
                self.status_targets.schedule.remove(key);
                continue;
            };
            let kind = source.kind;
            // Moved since: the next frame's sync replaces the target.
            let moved = self.status_root(key, kind).as_ref() != Some(&source.root);
            let loc = self.status_loc(key, kind).filter(|_| !moved);
            let Some(loc) = loc else {
                self.status_targets.schedule.landed(key, now);
                continue;
            };
            let Some(source) = self.status_targets.sources.get_mut(&key) else {
                continue;
            };
            let watched = source.watch.is_some();
            let backstop = source.status_at.is_none_or(|at| now >= at + BACKSTOP);
            jobs.push(StatusJob {
                key,
                root: source.root.clone(),
                loc,
                plan: source.plan.clone(),
                untracked_dirs: source.untracked_dirs.clone(),
                replan: std::mem::take(&mut source.replan),
                must: std::mem::take(&mut source.dirty) || !watched || backstop,
                watched,
                pending: source.pending.drain().collect(),
                learned: source.learned.clone(),
            });
        }
        if jobs.is_empty() {
            return;
        }
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let results = cx
                .background_executor()
                .spawn(async move {
                    let _git = ui_profile::git_caller("git-status");
                    jobs.into_iter().map(StatusJob::run).collect::<Vec<_>>()
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                for result in results {
                    this.land_status(result, cx);
                }
                this.pump_statuses(cx);
                cx.notify();
            });
        })
        .detach();
    }

    fn land_status(&mut self, result: StatusResult, cx: &mut Context<Self>) {
        let key = result.key;
        let now = cx.background_executor().now();
        // Gone meanwhile, or made again for the project's new folder: that
        // target's own refresh shows it.
        let Some(source) = self
            .status_targets
            .sources
            .get_mut(&key)
            .filter(|source| source.root == result.root)
        else {
            return;
        };
        self.status_targets.schedule.landed(key, now);
        let kind = source.kind;
        source.replan |= result.gone;
        if result.replanned {
            source.learned.clear();
        }
        source.learned.extend(result.ignored);
        let Some(files) = result.files else {
            return;
        };
        source.status_at = Some(now);
        source.untracked_dirs = result.untracked_dirs;
        if let Some((plan, spec)) = result.planned {
            source.plan = plan;
            self.watch_status(key, spec, cx);
        }
        match kind {
            StatusKind::Project => {
                if self.status_targets.project == Some(key) {
                    self.git_diff_files = Some(files);
                }
            }
            StatusKind::Worktree => {
                self.worktree_changes.insert(key, files.len());
                self.git_diff_worktree_files.insert(key, files);
            }
        }
    }

    /// Point `key`'s watch at `spec`, or stop watching where there's none.
    fn watch_status(&mut self, key: Uuid, spec: Option<WatchSpec>, cx: &mut Context<Self>) {
        let Some(source) = self.status_targets.sources.get_mut(&key) else {
            return;
        };
        let (old, label) = (source.watch.take(), source.kind.label());
        let watch = match (old, spec) {
            (Some(id), Some(spec)) => {
                let respec = match self.file_watch.as_mut() {
                    Some(Some(watch)) => watch.respec(id, spec),
                    _ => Err(anyhow::anyhow!("the file watcher stopped")),
                };
                match respec {
                    Ok(()) => Some(id),
                    Err(error) => {
                        log::warn!("git status for a {label} stopped watching: {error}");
                        ui_profile::status_watch(label, "lost");
                        self.watch_targets.remove(&id);
                        None
                    }
                }
            }
            // The watcher failed earlier in this run, and said so then.
            (None, Some(_)) if matches!(self.file_watch, Some(None)) => None,
            (None, Some(spec)) => match self.watch(spec, WatchTarget::Status(key), cx) {
                Ok(id) => {
                    ui_profile::status_watch(label, "watching");
                    // Writes between running git and this watch went unseen.
                    if let Some(source) = self.status_targets.sources.get_mut(&key) {
                        source.dirty = true;
                    }
                    self.status_targets.schedule.changed(key, None);
                    Some(id)
                }
                Err(error) => {
                    let spent = error.downcast_ref::<notify::Error>().is_some_and(|error| {
                        matches!(error.kind, notify::ErrorKind::MaxFilesWatch)
                    });
                    log::warn!(
                        "git status for a {label} polls: {error}{}",
                        if spent {
                            " (fs.inotify.max_user_watches is spent)"
                        } else {
                            ""
                        }
                    );
                    ui_profile::status_watch(label, "polling");
                    None
                }
            },
            (Some(id), None) => {
                self.drop_watch(id);
                ui_profile::status_watch(label, "polling");
                None
            }
            (None, None) => {
                ui_profile::status_watch(label, "polling");
                None
            }
        };
        if let Some(source) = self.status_targets.sources.get_mut(&key) {
            source.watch = watch;
        }
        self.status_targets
            .schedule
            .set_watched(key, watch.is_some());
    }

    /// `key`'s watch is gone: it polls.
    pub(super) fn status_unwatched(&mut self, key: Uuid) {
        let Some(source) = self.status_targets.sources.get_mut(&key) else {
            return;
        };
        source.watch = None;
        ui_profile::status_watch(source.kind.label(), "lost");
        self.status_targets.schedule.set_watched(key, false);
    }
}
