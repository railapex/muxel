//! Diff panes run `git diff` only while on screen, and a watched one only
//! when its files change. Draws real frames with no PTY, so it runs on every
//! desktop test host.
use super::{MuxelApp, Project};
use crate::file_watch::FileWatch;
use crate::file_watch::test_backend::TestBackend;
use crate::test_support::TestRepo;
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::diff_schedule::{MIN_GAP, POLL};
use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode};
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

/// Longer than the poll of a pane without a watch.
const DIFF_TICK: Duration = Duration::from_millis(1600);
/// Past a watched pane's quiet period and its gap after the last refresh,
/// short of the poll: only a watch's wake refreshes the pane within it.
const SOON: Duration = MIN_GAP.saturating_add(Duration::from_millis(100));
const _: () = assert!(SOON.as_millis() < POLL.as_millis());

struct Fixture {
    pid: Uuid,
    a: TestRepo,
    b: TestRepo,
}

fn committed_repo() -> TestRepo {
    let repo = TestRepo::init();
    repo.commit(&[("notes.txt", "first\n")], "seed");
    repo
}

fn add_line(repo: &TestRepo) {
    std::fs::write(repo.path().join("notes.txt"), "first\nsecond\n").unwrap();
}

fn with_app(run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>, &Fixture)) {
    let root = std::env::temp_dir().join(format!("muxel-diff-refresh-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let _storage = muxel_store::test_support::set_root(root.clone());
    let (a, b) = (committed_repo(), committed_repo());
    let mut cx = TestAppContext::single();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    let window = cx.add_window(MuxelApp::new);
    let pid = window
        .update(&mut cx, |app, _, _| {
            // The diff timer stays live; the others would add unrelated work.
            app._status_timer = gpui::Task::ready(());
            app._update_timer = gpui::Task::ready(());
            // No watcher, so panes poll while visible, unless a test hands
            // the app one (`watch_with`).
            app.file_watch = Some(None);
            let pid = app
                .workspace
                .add_project(Project::new("diff fixture", a.path().to_path_buf()));
            app.workspace.active_project = Some(pid);
            app.current_workspace = Some(Uuid::new_v4());
            pid
        })
        .unwrap();
    run(&mut cx, window, &Fixture { pid, a, b });
    drop(cx);
    std::fs::remove_dir_all(root).unwrap();
}

fn open_diff(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    fx: &Fixture,
    repo: &TestRepo,
) -> Uuid {
    let iid = window
        .update(cx, |app, window, cx| {
            let anchor = app.active_instance;
            app.open_diff_for_dir(fx.pid, repo.path().to_path_buf(), anchor, window, cx);
            app.active_instance.expect("the new diff pane is focused")
        })
        .unwrap();
    cx.run_until_parked();
    iid
}

/// Two diff panes as tabs of one group, the second shown: A is hidden.
fn hidden_and_shown(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    fx: &Fixture,
) -> (Uuid, Uuid) {
    let a = open_diff(cx, window, fx, &fx.a);
    let b = open_diff(cx, window, fx, &fx.b);
    window
        .update(cx, |app, _, _| {
            let layout = app.workspace.project(fx.pid).unwrap().layout.as_ref();
            assert_eq!(
                layout.map(|l| l.shown_tabs(app.active_instance, app.maximized)),
                Some(vec![b]),
                "A and B share one tab group with B in front"
            );
        })
        .unwrap();
    (a, b)
}

fn diff_text(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, iid: Uuid) -> String {
    window
        .update(cx, |app, _, cx| {
            let (_, editor) = app
                .open_editor_resources()
                .into_iter()
                .find(|(id, _)| *id == iid)
                .expect("the diff pane is open");
            editor.read(cx).text(cx)
        })
        .unwrap()
}

fn tick(cx: &mut TestAppContext) {
    wait(cx, DIFF_TICK);
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

fn watched(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, iid: Uuid) -> bool {
    window
        .update(cx, |app, _, _| app.diff_panes.sources[&iid].watch.is_some())
        .unwrap()
}

/// A's diff pane, watched through a test backend, past its first refresh
/// and the check after the watch began, so nothing is due but the backstop.
fn watched_diff(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    fx: &Fixture,
) -> (TestBackend, Uuid) {
    let backend = watch_with(cx, window);
    let iid = open_diff(cx, window, fx, &fx.a);
    assert!(watched(cx, window, iid), "the pane watches its repository");
    wait(cx, 2 * MIN_GAP);
    (backend, iid)
}

fn send(backend: &TestBackend, kind: notify::EventKind, paths: &[PathBuf]) {
    let mut event = notify::Event::new(kind);
    for path in paths {
        event = event.add_path(path.clone());
    }
    backend.send(Ok(event));
}

fn modified(backend: &TestBackend, path: PathBuf) {
    send(
        backend,
        notify::EventKind::Modify(ModifyKind::Data(DataChange::Content)),
        &[path],
    );
}

#[test]
fn a_tracked_edit_refreshes_a_visible_diff_without_a_tick() {
    with_app(|cx, window, fx| {
        let (backend, iid) = watched_diff(cx, window, fx);
        add_line(&fx.a);
        modified(&backend, fx.a.path().join("notes.txt"));
        wait(cx, SOON);
        assert!(diff_text(cx, window, iid).contains("+second"));
    });
}

#[test]
fn writes_under_an_untracked_directory_start_no_git() {
    with_app(|cx, window, fx| {
        let (backend, iid) = watched_diff(cx, window, fx);
        // The tracked edit goes unreported, so only a refresh started by
        // the other writes, or a poll, would show it.
        add_line(&fx.a);
        let root = fx.a.path();
        send(
            &backend,
            notify::EventKind::Create(CreateKind::File),
            &[root.join("target/debug/out.o"), root.join("scratch.txt")],
        );
        modified(&backend, root.join("target"));
        modified(&backend, root.to_path_buf());
        wait(cx, 3 * POLL);
        assert!(
            !diff_text(cx, window, iid).contains("+second"),
            "no git ran"
        );
        // The pane is live: its tracked file's event refreshes it.
        modified(&backend, root.join("notes.txt"));
        wait(cx, SOON);
        assert!(diff_text(cx, window, iid).contains("+second"));
    });
}

#[test]
fn a_commit_on_the_branch_refreshes_the_diff() {
    with_app(|cx, window, fx| {
        let (backend, iid) = watched_diff(cx, window, fx);
        add_line(&fx.a);
        modified(&backend, fx.a.path().join("notes.txt"));
        wait(cx, SOON);
        assert!(diff_text(cx, window, iid).contains("+second"));
        fx.a.commit(&[], "second");
        // Only the branch's ref is reported moving, as git renames it into
        // place.
        let heads = fx.a.path().join(".git").join("refs").join("heads");
        send(
            &backend,
            notify::EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            &[heads.join("main.lock"), heads.join("main")],
        );
        wait(cx, SOON);
        assert!(!diff_text(cx, window, iid).contains("+second"));
    });
}

#[test]
fn a_lost_diff_watch_puts_the_pane_back_on_its_poll() {
    with_app(|cx, window, fx| {
        let (backend, iid) = watched_diff(cx, window, fx);
        send(
            &backend,
            notify::EventKind::Remove(RemoveKind::Folder),
            &[fx.a.path().to_path_buf()],
        );
        cx.run_until_parked();
        assert!(!watched(cx, window, iid));
        add_line(&fx.a);
        tick(cx);
        assert!(diff_text(cx, window, iid).contains("+second"));
    });
}

#[test]
fn a_hidden_diff_pane_starts_no_git_until_it_is_shown() {
    with_app(|cx, window, fx| {
        let (a, b) = hidden_and_shown(cx, window, fx);
        add_line(&fx.a);
        add_line(&fx.b);
        tick(cx);
        assert!(
            diff_text(cx, window, b).contains("+second"),
            "the shown pane refreshes on the timer"
        );
        assert!(
            !diff_text(cx, window, a).contains("+second"),
            "the hidden pane ran no git"
        );
    });
}

#[test]
fn a_diff_pane_shown_after_a_change_refreshes_at_once() {
    with_app(|cx, window, fx| {
        let (a, _) = hidden_and_shown(cx, window, fx);
        add_line(&fx.a);
        window
            .update(cx, |app, window, cx| app.focus_instance(a, window, cx))
            .unwrap();
        // Frames only: the timer doesn't tick.
        cx.run_until_parked();
        assert!(diff_text(cx, window, a).contains("+second"));
    });
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "a pop-out's gpui_component::Root asks a test window for its NSView, which gpui's test platform can't give"
)]
fn a_popped_out_diff_pane_keeps_refreshing() {
    with_app(|cx, window, fx| {
        let a = open_diff(cx, window, fx, &fx.a);
        window
            .update(cx, |app, window, cx| app.pop_out_instance(a, window, cx))
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |app, _, _| assert!(app.popouts.contains_key(&a)))
            .unwrap();
        add_line(&fx.a);
        tick(cx);
        assert!(diff_text(cx, window, a).contains("+second"));
    });
}
