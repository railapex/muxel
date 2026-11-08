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
//! remote project, a repository the files can't vouch for) polls, the project
//! every 1.5 s while shown and a worktree every 10 s.

use super::watching::WatchTarget;
use super::{GitDiffTab, MuxelApp, WORKTREE_CHANGES_EVERY, hotbar};
use crate::file_watch::{self, status_sources};
use crate::integrations::{self, GitChange, RepoLoc, StatusPlan};
use crate::ui_profile;
use gpui::{Context, Task, WeakEntity};
use muxel_core::diff_schedule::DiffSchedule;
use muxel_core::file_watch::{WatchId, WatchSpec};
use std::collections::HashMap;
use std::path::PathBuf;
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
    /// What the last sync saw: the active project, whether the Files tab
    /// showed, and the worktrees.
    seen: Option<(Option<Uuid>, bool, Vec<Uuid>)>,
    armed: Option<Instant>,
    timer: Option<Task<()>>,
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

/// What one target's refreshes read.
pub(super) struct StatusSource {
    kind: StatusKind,
    /// The plan its watch was built from; `None` before the first refresh, or
    /// where it isn't in a work tree, or remote.
    plan: Option<Arc<StatusPlan>>,
    /// The untracked directories its last `git status` showed collapsed.
    untracked_dirs: Vec<PathBuf>,
    /// Resolve the plan again at the next refresh: none yet, HEAD, the index
    /// or what's ignored changed, a directory came or went, or the folder
    /// was gone.
    pub(super) replan: bool,
    pub(super) watch: Option<WatchId>,
}

/// One refresh, run off the UI thread.
struct StatusJob {
    key: Uuid,
    loc: RepoLoc,
    plan: Option<Arc<StatusPlan>>,
    untracked_dirs: Vec<PathBuf>,
    replan: bool,
}

struct StatusResult {
    key: Uuid,
    files: Vec<GitChange>,
    untracked_dirs: Vec<PathBuf>,
    /// The plan this refresh used and the watch built from it, when either
    /// changed; `None` when it kept the target's.
    planned: Option<(Option<Arc<StatusPlan>>, Option<WatchSpec>)>,
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

impl StatusJob {
    fn run(self) -> StatusResult {
        let RepoLoc::Local(dir) = &self.loc else {
            // Remote: read over ssh, never watched.
            return StatusResult {
                key: self.key,
                files: integrations::git_status_files(&self.loc),
                untracked_dirs: Vec::new(),
                planned: self.replan.then_some((None, None)),
                gone: false,
            };
        };
        // A folder that's gone runs no git while it stays gone.
        if !dir.is_dir() {
            return StatusResult {
                key: self.key,
                files: Vec::new(),
                untracked_dirs: Vec::new(),
                planned: Some((None, None)),
                gone: true,
            };
        }
        let plan = if self.replan {
            integrations::status_plan(dir).map(Arc::new)
        } else {
            self.plan
        };
        let files = integrations::git_status_files(&self.loc);
        let dirs = untracked_dirs(&files);
        // Where each directory is its own watch, a directory may have come
        // or gone: the watch is rebuilt from the plan (a walk, no git), and
        // only the OS watches that differ change.
        let rewalk = file_watch::PER_DIR && plan.is_some();
        let planned = (self.replan || dirs != self.untracked_dirs || rewalk).then(|| {
            let spec = plan.as_deref().and_then(|plan| status_sources(plan, &dirs));
            (plan, spec)
        });
        StatusResult {
            key: self.key,
            files,
            untracked_dirs: dirs,
            planned,
            gone: false,
        }
    }
}

impl MuxelApp {
    /// Run each frame: keep a target for the active project once the Files
    /// tab has shown it (on screen while it shows), and one for every
    /// worktree (always on screen: its count is in the sidebar).
    pub(super) fn sync_status_targets(&mut self, cx: &mut Context<Self>) {
        let active = self.workspace.active_project;
        let files_shown = (self.show_git_diff || self.main_view == hotbar::MainView::Changes)
            && self.git_diff_tab == GitDiffTab::Files;
        let worktrees: Vec<Uuid> = self.workspace.worktrees.iter().map(|w| w.id).collect();
        let seen = Some((active, files_shown, worktrees));
        if self.status_targets.seen == seen {
            return;
        }
        let (_, _, worktrees) = seen.clone().unwrap_or_default();
        self.status_targets.seen = seen;

        if let Some(pid) = self.status_targets.project
            && Some(pid) != active
        {
            self.forget_status(pid);
            self.status_targets.project = None;
            self.git_diff_files = None;
        }
        if files_shown
            && self.status_targets.project.is_none()
            && let Some(pid) = active
        {
            self.add_status(pid, StatusKind::Project);
            self.status_targets.project = Some(pid);
        }
        if let Some(pid) = self.status_targets.project {
            self.status_targets.schedule.set_visible(pid, files_shown);
        }

        let gone: Vec<Uuid> = self
            .status_targets
            .sources
            .iter()
            .filter(|(id, source)| source.kind == StatusKind::Worktree && !worktrees.contains(id))
            .map(|(id, _)| *id)
            .collect();
        for wid in gone {
            self.forget_status(wid);
            self.git_diff_worktree_files.remove(&wid);
            self.worktree_changes.remove(&wid);
        }
        for wid in worktrees {
            if !self.status_targets.sources.contains_key(&wid) {
                self.add_status(wid, StatusKind::Worktree);
                self.status_targets
                    .schedule
                    .set_poll(wid, WORKTREE_CHANGES_EVERY);
                self.status_targets.schedule.set_visible(wid, true);
            }
        }
        self.pump_statuses(cx);
    }

    fn add_status(&mut self, key: Uuid, kind: StatusKind) {
        self.status_targets.sources.insert(
            key,
            StatusSource {
                kind,
                plan: None,
                untracked_dirs: Vec::new(),
                replan: true,
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

    /// Refresh what the Changes view shows at the first chance: the refresh
    /// button, or a git action from the view. A refresh already running
    /// finishes first.
    pub(super) fn request_changes(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<Uuid> = match self.git_diff_tab {
            GitDiffTab::Files => self.status_targets.project.into_iter().collect(),
            GitDiffTab::Worktrees => {
                let active = self.workspace.active_project;
                self.workspace
                    .worktrees
                    .iter()
                    .filter(|w| Some(w.project_id) == active)
                    .map(|w| w.id)
                    .collect()
            }
        };
        for key in keys {
            self.status_targets.schedule.request(key);
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
            self.status_targets.schedule.request(key);
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
            let Some(kind) = self.status_targets.sources.get(&key).map(|s| s.kind) else {
                self.status_targets.schedule.remove(key);
                continue;
            };
            let Some(loc) = self.status_loc(key, kind) else {
                self.status_targets.schedule.landed(key, now);
                continue;
            };
            let Some(source) = self.status_targets.sources.get_mut(&key) else {
                continue;
            };
            jobs.push(StatusJob {
                key,
                loc,
                plan: source.plan.clone(),
                untracked_dirs: source.untracked_dirs.clone(),
                replan: std::mem::take(&mut source.replan),
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
        self.status_targets.schedule.landed(key, now);
        let Some(source) = self.status_targets.sources.get_mut(&key) else {
            return; // gone meanwhile
        };
        let kind = source.kind;
        source.replan |= result.gone;
        source.untracked_dirs = result.untracked_dirs;
        if let Some((plan, spec)) = result.planned {
            source.plan = plan;
            self.watch_status(key, spec, cx);
        }
        match kind {
            StatusKind::Project => {
                if self.status_targets.project == Some(key) {
                    self.git_diff_files = Some(result.files);
                }
            }
            StatusKind::Worktree => {
                self.worktree_changes.insert(key, result.files.len());
                self.git_diff_worktree_files.insert(key, result.files);
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
