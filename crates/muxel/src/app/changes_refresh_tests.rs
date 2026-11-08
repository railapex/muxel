//! The Changes view and the worktrees' change counts run `git status` only
//! while shown, and a watched one only when what it lists may have changed.
//! Draws real frames with no PTY, so it runs on every desktop test host.
use super::hotbar::MainView;
use super::{GitDiffTab, MuxelApp, Project, WORKTREE_CHANGES_EVERY, Worktree};
use crate::file_watch::FileWatch;
use crate::file_watch::test_backend::TestBackend;
use crate::test_support::{TestRepo, git_out, short_temp_path};
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::diff_schedule::{BACKSTOP, MIN_GAP, POLL};
use notify::event::{CreateKind, DataChange, Flag, ModifyKind, RemoveKind};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

/// Past a watched target's quiet period and its gap after the last refresh,
/// short of the poll: only a watch's wake refreshes it within it.
const SOON: Duration = MIN_GAP.saturating_add(Duration::from_millis(100));
const _: () = assert!(SOON.as_millis() < POLL.as_millis());

struct Fixture {
    repo: TestRepo,
    /// The repository's top level as git prints it, which status targets
    /// watch, so a watcher's events name paths under it.
    top: PathBuf,
    /// A linked worktree of `repo`, registered as muxel's, and its top level.
    worktree: PathBuf,
    worktree_top: PathBuf,
    wid: Uuid,
}

fn toplevel(dir: &Path) -> PathBuf {
    PathBuf::from(git_out(dir, &["rev-parse", "--show-toplevel"]))
}

fn with_app(run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>, &Fixture)) {
    let root = std::env::temp_dir().join(format!("muxel-changes-refresh-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let _storage = muxel_store::test_support::set_root(root.clone());
    let repo = TestRepo::init();
    repo.commit(
        &[
            ("notes.txt", "first\n"),
            (".gitignore", "target/\n*.o\n*.log\n"),
        ],
        "seed",
    );
    std::fs::create_dir_all(repo.path().join("target")).unwrap();
    let worktree = short_temp_path();
    repo.git(&[
        "worktree",
        "add",
        "-q",
        "-b",
        "side",
        &worktree.to_string_lossy(),
    ]);
    let wid = Uuid::new_v4();
    let mut cx = TestAppContext::single();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    let window = cx.add_window(MuxelApp::new);
    window
        .update(&mut cx, |app, _, _| {
            // The status targets' timer stays live; the others would add
            // unrelated work.
            app._status_timer = gpui::Task::ready(());
            app._update_timer = gpui::Task::ready(());
            // No watcher, so targets poll, unless a test hands the app one
            // (`watch_with`) before the first frame.
            app.file_watch = Some(None);
            let pid = app
                .workspace
                .add_project(Project::new("changes fixture", repo.path().to_path_buf()));
            app.workspace.active_project = Some(pid);
            app.workspace.add_worktree(Worktree {
                id: wid,
                project_id: pid,
                name: "fixture".into(),
                path: worktree.clone(),
                branch: "side".into(),
                color: 0,
                detached: true,
            });
            app.current_workspace = Some(Uuid::new_v4());
        })
        .unwrap();
    let fixture = Fixture {
        top: toplevel(repo.path()),
        worktree_top: toplevel(&worktree),
        repo,
        worktree,
        wid,
    };
    run(&mut cx, window, &fixture);
    drop(cx);
    fixture.repo.git(&[
        "worktree",
        "remove",
        "--force",
        &fixture.worktree.to_string_lossy(),
    ]);
    let _ = std::fs::remove_dir_all(&fixture.worktree);
    std::fs::remove_dir_all(root).unwrap();
}

fn wait(cx: &mut TestAppContext, time: Duration) {
    cx.executor().advance_clock(time);
    cx.run_until_parked();
}

/// Give the app a watcher whose events the test plays.
fn watch_with(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> TestBackend {
    let backend = TestBackend::default();
    let (watch, signal) = FileWatch::start_with(|handler| backend.build(handler)).unwrap();
    window
        .update(cx, |app, _, cx| app.follow_file_watch(watch, signal, cx))
        .unwrap();
    backend
}

fn set_view(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, view: MainView) {
    window
        .update(cx, |app, window, cx| app.set_main_view(view, window, cx))
        .unwrap();
    cx.run_until_parked();
}

/// The Changes view's Files tab, past its first refresh and the check after
/// its watch began, so nothing is due but the backstop.
fn show_changes(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) {
    set_view(cx, window, MainView::Changes);
    wait(cx, 2 * MIN_GAP);
}

fn files(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> Vec<String> {
    window
        .update(cx, |app, _, _| {
            app.git_diff_files
                .iter()
                .flatten()
                .map(|change| change.path.clone())
                .collect()
        })
        .unwrap()
}

fn count(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, wid: Uuid) -> Option<usize> {
    window
        .update(cx, |app, _, _| app.worktree_changes.get(&wid).copied())
        .unwrap()
}

fn send(backend: &TestBackend, kind: notify::EventKind, path: &Path) {
    backend.send(Ok(notify::Event::new(kind).add_path(path.to_path_buf())));
}

fn modified(backend: &TestBackend, path: &Path) {
    send(
        backend,
        notify::EventKind::Modify(ModifyKind::Data(DataChange::Content)),
        path,
    );
}

fn created(backend: &TestBackend, path: &Path) {
    send(backend, notify::EventKind::Create(CreateKind::File), path);
}

fn removed(backend: &TestBackend, path: &Path) {
    send(backend, notify::EventKind::Remove(RemoveKind::File), path);
}

fn add_line(dir: &Path) {
    std::fs::write(dir.join("notes.txt"), "first\nsecond\n").unwrap();
}

/// Each listed file's two-letter status and path.
fn statuses(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> Vec<(String, String)> {
    window
        .update(cx, |app, _, _| {
            app.git_diff_files
                .iter()
                .flatten()
                .map(|change| (change.status.clone(), change.path.clone()))
                .collect()
        })
        .unwrap()
}

fn status(code: &str, path: &str) -> (String, String) {
    (code.to_string(), path.to_string())
}

/// The Changes view's Worktrees tab.
fn show_worktrees(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) {
    window
        .update(cx, |app, window, cx| {
            app.set_git_diff_tab(GitDiffTab::Worktrees, cx);
            app.set_main_view(MainView::Changes, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
}

/// `added.txt`, tracked from now on, with no event for the index's write.
fn add_unreported(fx: &Fixture) {
    std::fs::write(fx.top.join("added.txt"), "one\n").unwrap();
    fx.repo.git(&["add", "added.txt"]);
}

#[test]
fn the_changes_view_shows_an_edit_without_a_tick() {
    with_app(|cx, window, fx| {
        let backend = watch_with(cx, window);
        show_changes(cx, window);
        assert!(files(cx, window).is_empty(), "a clean tree");
        add_line(fx.repo.path());
        modified(&backend, &fx.top.join("notes.txt"));
        wait(cx, SOON);
        assert_eq!(files(cx, window), ["notes.txt"]);
    });
}

#[test]
fn a_new_untracked_file_shows_but_build_output_starts_no_git() {
    with_app(|cx, window, fx| {
        let backend = watch_with(cx, window);
        show_changes(cx, window);
        let root = &fx.top;
        // The new file goes unreported, so only a refresh started by the
        // build's writes, or a poll, would list it.
        std::fs::write(root.join("scratch.txt"), "new\n").unwrap();
        std::fs::write(root.join("target").join("out.o"), "built\n").unwrap();
        created(&backend, &root.join("target").join("out.o"));
        modified(&backend, &root.join("target"));
        wait(cx, 3 * POLL);
        assert!(files(cx, window).is_empty(), "no git ran");
        created(&backend, &root.join("scratch.txt"));
        wait(cx, SOON);
        assert_eq!(files(cx, window), ["scratch.txt"]);
    });
}

#[test]
fn a_hidden_changes_view_runs_no_git_and_catches_up_when_shown() {
    with_app(|cx, window, fx| {
        let backend = watch_with(cx, window);
        show_changes(cx, window);
        set_view(cx, window, MainView::Panes);
        add_line(fx.repo.path());
        modified(&backend, &fx.top.join("notes.txt"));
        wait(cx, 3 * POLL);
        assert!(files(cx, window).is_empty(), "hidden, it ran no git");
        // Frames only: no timer is due.
        set_view(cx, window, MainView::Changes);
        assert_eq!(files(cx, window), ["notes.txt"]);
    });
}

#[test]
fn a_worktree_edit_updates_its_count_without_a_tick() {
    with_app(|cx, window, fx| {
        let backend = watch_with(cx, window);
        // The sidebar shows the count: no view needs opening.
        wait(cx, 2 * MIN_GAP);
        assert_eq!(count(cx, window, fx.wid), Some(0));
        add_line(&fx.worktree);
        modified(&backend, &fx.worktree_top.join("notes.txt"));
        wait(cx, SOON);
        assert_eq!(count(cx, window, fx.wid), Some(1));
        // The Worktrees tab lists the same files.
        let listed = window
            .update(cx, |app, _, _| {
                app.git_diff_worktree_files[&fx.wid]
                    .iter()
                    .map(|change| change.path.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap();
        assert_eq!(listed, ["notes.txt"]);
    });
}

#[test]
fn an_unwatched_worktree_recounts_on_its_own_poll() {
    with_app(|cx, window, fx| {
        // No watcher: the count polls, as slowly as before.
        cx.run_until_parked();
        assert_eq!(count(cx, window, fx.wid), Some(0));
        add_line(&fx.worktree);
        wait(cx, 3 * POLL);
        assert_eq!(
            count(cx, window, fx.wid),
            Some(0),
            "not on a diff pane's poll"
        );
        wait(cx, WORKTREE_CHANGES_EVERY);
        assert_eq!(count(cx, window, fx.wid), Some(1));
    });
}

#[test]
fn a_project_moved_to_another_folder_watches_that_one() {
    with_app(|cx, window, fx| {
        let backend = watch_with(cx, window);
        show_changes(cx, window);
        add_line(fx.repo.path());
        modified(&backend, &fx.top.join("notes.txt"));
        wait(cx, SOON);
        assert_eq!(files(cx, window), ["notes.txt"]);
        let other = TestRepo::init();
        other.commit(&[("other.txt", "one\n")], "seed");
        let other_top = toplevel(other.path());
        // As the project's folder setting does.
        window
            .update(cx, |app, _, cx| {
                let pid = app.workspace.active_project.unwrap();
                app.workspace.project_mut(pid).unwrap().root_path = other.path().to_path_buf();
                cx.notify();
            })
            .unwrap();
        wait(cx, 2 * MIN_GAP);
        assert!(
            files(cx, window).is_empty(),
            "the other folder's clean tree"
        );
        std::fs::write(other.path().join("other.txt"), "one\ntwo\n").unwrap();
        modified(&backend, &other_top.join("other.txt"));
        wait(cx, SOON);
        assert_eq!(files(cx, window), ["other.txt"]);
    });
}

#[test]
fn build_output_appearing_after_the_plan_runs_no_status() {
    with_app(|cx, window, fx| {
        let backend = watch_with(cx, window);
        let root = &fx.top;
        // No target/ when the plan is read, so the plan doesn't list it.
        std::fs::remove_dir_all(root.join("target")).unwrap();
        show_changes(cx, window);
        // Unreported: only a `git status` would list it.
        std::fs::write(root.join("scratch.txt"), "new\n").unwrap();
        let debug = root.join("target").join("debug");
        std::fs::create_dir_all(&debug).unwrap();
        std::fs::write(debug.join("out.o"), "built\n").unwrap();
        for path in [root.join("target"), debug.clone(), debug.join("out.o")] {
            created(&backend, &path);
        }
        wait(cx, 3 * POLL);
        assert!(files(cx, window).is_empty(), "git ignores all of it");
        // An object file beside the tracked ones, ignored by `*.o`.
        std::fs::write(root.join("main.o"), "built\n").unwrap();
        created(&backend, &root.join("main.o"));
        wait(cx, 3 * POLL);
        assert!(files(cx, window).is_empty(), "git ignores that too");
        created(&backend, &root.join("scratch.txt"));
        wait(cx, SOON);
        assert_eq!(files(cx, window), ["scratch.txt"]);
    });
}

#[test]
fn a_file_git_stops_ignoring_is_followed_once_the_rule_goes() {
    with_app(|cx, window, fx| {
        let backend = watch_with(cx, window);
        show_changes(cx, window);
        let root = &fx.top;
        std::fs::write(root.join("a.log"), "log\n").unwrap();
        created(&backend, &root.join("a.log"));
        wait(cx, 3 * POLL);
        assert!(files(cx, window).is_empty(), "ignored by *.log");
        std::fs::write(root.join(".gitignore"), "target/\n*.o\n").unwrap();
        modified(&backend, &root.join(".gitignore"));
        wait(cx, SOON);
        assert_eq!(files(cx, window), [".gitignore", "a.log"]);
        std::fs::remove_file(root.join("a.log")).unwrap();
        removed(&backend, &root.join("a.log"));
        wait(cx, SOON);
        assert_eq!(files(cx, window), [".gitignore"]);
    });
}

#[test]
fn the_last_file_leaving_a_collapsed_directory_clears_its_row() {
    with_app(|cx, window, fx| {
        let backend = watch_with(cx, window);
        let sub = fx.top.join("drafts").join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("last.txt"), "x\n").unwrap();
        show_changes(cx, window);
        assert_eq!(files(cx, window), ["drafts/"]);
        std::fs::remove_file(sub.join("last.txt")).unwrap();
        removed(&backend, &sub.join("last.txt"));
        wait(cx, SOON);
        assert!(files(cx, window).is_empty(), "git lists no empty directory");
    });
}

#[test]
fn a_watcher_overflow_re_plans_the_status() {
    with_app(|cx, window, fx| {
        let backend = watch_with(cx, window);
        show_changes(cx, window);
        add_unreported(fx);
        backend.send(Ok(
            notify::Event::new(notify::EventKind::Other).set_flag(Flag::Rescan)
        ));
        wait(cx, SOON);
        assert_eq!(statuses(cx, window), [status("A ", "added.txt")]);
        std::fs::write(fx.top.join("added.txt"), "one\nmore\n").unwrap();
        modified(&backend, &fx.top.join("added.txt"));
        wait(cx, SOON);
        assert_eq!(statuses(cx, window), [status("AM", "added.txt")]);
    });
}

#[test]
fn the_backstop_re_plans_the_status_after_a_lost_index_event() {
    with_app(|cx, window, fx| {
        let backend = watch_with(cx, window);
        show_changes(cx, window);
        add_unreported(fx);
        wait(cx, BACKSTOP);
        assert_eq!(statuses(cx, window), [status("A ", "added.txt")]);
        std::fs::write(fx.top.join("added.txt"), "one\nmore\n").unwrap();
        modified(&backend, &fx.top.join("added.txt"));
        wait(cx, SOON);
        assert_eq!(statuses(cx, window), [status("AM", "added.txt")]);
    });
}

#[test]
fn the_worktrees_tab_refreshes_as_it_opens_and_polls_an_unwatched_worktree() {
    with_app(|cx, window, fx| {
        // No watcher: every target polls.
        cx.run_until_parked();
        assert_eq!(count(cx, window, fx.wid), Some(0));
        add_line(&fx.worktree);
        show_worktrees(cx, window);
        assert_eq!(count(cx, window, fx.wid), Some(1), "refreshed as it opened");
        std::fs::write(fx.worktree.join("notes.txt"), "first\n").unwrap();
        wait(cx, POLL);
        assert_eq!(
            count(cx, window, fx.wid),
            Some(0),
            "as often as the Files tab"
        );
        // Closed again: the sidebar's pace.
        set_view(cx, window, MainView::Panes);
        add_line(&fx.worktree);
        wait(cx, 3 * POLL);
        assert_eq!(count(cx, window, fx.wid), Some(0));
        wait(cx, WORKTREE_CHANGES_EVERY);
        assert_eq!(count(cx, window, fx.wid), Some(1));
    });
}
