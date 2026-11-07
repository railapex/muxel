//! Diff panes run `git diff` only while on screen. Draws real frames with no
//! PTY, so it runs on every desktop test host.
use super::{MuxelApp, Project};
use crate::test_support::TestRepo;
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use std::time::Duration;
use uuid::Uuid;

/// Longer than one tick of the diff timer.
const DIFF_TICK: Duration = Duration::from_millis(1600);

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
    cx.executor().advance_clock(DIFF_TICK);
    cx.run_until_parked();
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
