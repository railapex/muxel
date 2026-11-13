//! The sidebar's filter box narrows every project's rows as drawn, Enter opens
//! the first row it leaves, and a row's menu fills it with that row's folder
//! in its own project. The PROJECTS line's asleep count hides sleeping rows.

use super::popout_lifecycle_tests::with_app_drawing;
use super::{Instance, MuxelApp, Project, SidebarSort};
use gpui::{Modifiers, TestAppContext, VisualTestContext, WindowHandle};
use gpui_component::input::InputEvent;
use muxel_core::{AgentActivity, AgentActivityState, PaneNode, add_tab};
use std::path::PathBuf;
use uuid::Uuid;

/// The app past its first-run screens, sidebar shown, sorted by `sort`.
fn ready(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, sort: SidebarSort) {
    window
        .update(cx, |app, _, _| {
            app.show_terms = false;
            app.show_workspace_selector = false;
            app.sidebar_collapsed = false;
            app.settings.sidebar_sort = sort;
            app.settings.older_starts_folded = false;
            app.use_tmux = false;
            app.use_worktree = false;
        })
        .unwrap();
}

fn project(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, name: &str) -> (Uuid, PathBuf) {
    window
        .update(cx, |app, _, _| {
            let root = muxel_store::data_dir().unwrap().join(name);
            std::fs::create_dir_all(&root).unwrap();
            let pid = app.workspace.add_project(Project::new(name, &root));
            app.workspace.active_project.get_or_insert(pid);
            (pid, root)
        })
        .unwrap()
}

/// A session named `name` launched in `folder`, asleep, so neither drawing
/// nor opening it starts a process.
fn session(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pid: Uuid,
    name: &str,
    folder: PathBuf,
    asleep: bool,
    activity: AgentActivity,
) -> Uuid {
    window
        .update(cx, |app, _, _| {
            let mut instance = Instance::shell(pid);
            instance.custom_name = Some(name.into());
            instance.launch_dir = Some(folder);
            instance.activity = activity;
            let iid = instance.id;
            app.workspace.add_instance(instance);
            let project = app.workspace.project_mut(pid).unwrap();
            if let Some(anchor) = project.first_instance() {
                assert!(add_tab(&mut project.layout, anchor, iid));
            } else {
                project.layout = Some(PaneNode::leaf(iid));
            }
            if asleep {
                app.dormant_instances.insert(iid);
            }
            iid
        })
        .unwrap()
}

/// Which of `rows` the sidebar draws, after letting it draw.
fn drawn(
    cx: &mut TestAppContext,
    vcx: &mut VisualTestContext,
    rows: &[(&str, Uuid)],
) -> Vec<String> {
    cx.run_until_parked();
    rows.iter()
        .filter(|(_, iid)| {
            vcx.debug_bounds(format!("inst-row-{}", iid.simple()).leak())
                .is_some()
        })
        .map(|(name, _)| name.to_string())
        .collect()
}

fn click(cx: &mut TestAppContext, vcx: &mut VisualTestContext, selector: &'static str) {
    cx.run_until_parked();
    let bounds = vcx
        .debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} not drawn"));
    vcx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
}

#[test]
fn the_filter_box_narrows_every_project_and_a_row_menu_fills_it() {
    with_app_drawing(true, |cx, window| {
        ready(cx, window, SidebarSort::Layout);
        let (alpha, alpha_root) = project(cx, window, "alpha");
        let (beta, beta_root) = project(cx, window, "beta");
        let idle = AgentActivity::default;
        let dates = session(
            cx,
            window,
            alpha,
            "Dates fix",
            alpha_root.join("ember"),
            true,
            idle(),
        );
        let sidebar = session(
            cx,
            window,
            alpha,
            "Sidebar work",
            alpha_root.clone(),
            true,
            idle(),
        );
        let brownout = session(
            cx,
            window,
            alpha,
            "Brownout review",
            alpha_root,
            true,
            idle(),
        );
        let beta_row = session(cx, window, beta, "Brownout beta", beta_root, true, idle());
        let rows = [
            ("Dates fix", dates),
            ("Sidebar work", sidebar),
            ("Brownout review", brownout),
            ("Brownout beta", beta_row),
        ];
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        let all = rows.map(|(name, _)| name.to_string()).to_vec();
        assert_eq!(
            drawn(cx, &mut vcx, &rows),
            all,
            "an empty box hides nothing"
        );

        // Typed, in both projects.
        assert!(
            vcx.debug_bounds("sidebar-filter").is_some(),
            "the box is drawn"
        );
        // Typed: the box's own events, as its Input sends them. (Focusing the
        // Input itself needs gpui-component's Root, which the test window
        // doesn't have.)
        let input = |cx: &mut TestAppContext, event: InputEvent, text: Option<&str>| {
            window
                .update(cx, |app, window, cx| {
                    app.sidebar_filter.update(cx, |input, cx| {
                        if let Some(text) = text {
                            input.set_value(text.to_string(), window, cx);
                        }
                        cx.emit(event);
                    })
                })
                .unwrap()
        };
        input(cx, InputEvent::Change, Some("brownout"));
        assert_eq!(
            drawn(cx, &mut vcx, &rows),
            ["Brownout review", "Brownout beta"],
            "typing narrows every project to the rows that match"
        );
        let enter = InputEvent::PressEnter {
            secondary: false,
            shift: false,
        };
        input(cx, enter, None);
        cx.run_until_parked();
        let active = window.update(cx, |app, _, _| app.active_instance).unwrap();
        assert_eq!(
            active,
            Some(brownout),
            "Enter opens the first row the box leaves, not the project's first row"
        );

        // The row menu's "Show only this folder" on Dates fix.
        let clear_alpha: &'static str = format!("clear-filter-{}", alpha.simple()).leak();
        let clear_beta: &'static str = format!("clear-filter-{}", beta.simple()).leak();
        let menu = |cx: &mut TestAppContext| {
            window
                .update(cx, |app, _, cx| {
                    let project = app.workspace.project(alpha).unwrap();
                    let instance = app.workspace.instance(dates).unwrap();
                    let (label, text) =
                        app.folder_filter_menu_entry(&app.row_filter(cx), project, instance);
                    (label.to_string(), text)
                })
                .unwrap()
        };
        let (label, text) = menu(cx);
        assert_eq!(label, "Show only this folder");
        window
            .update(cx, |app, window, cx| {
                app.set_sidebar_filter(text, window, cx)
            })
            .unwrap();
        assert_eq!(
            drawn(cx, &mut vcx, &rows),
            ["Dates fix"],
            "the menu's filter leaves that folder's rows, in that project"
        );
        assert!(
            vcx.debug_bounds(clear_alpha).is_some(),
            "the project the box names offers Clear"
        );
        assert!(
            vcx.debug_bounds(clear_beta).is_none(),
            "a project the box doesn't name has no Clear"
        );
        let (label, text) = menu(cx);
        assert_eq!(
            (label.as_str(), text),
            ("Show all sessions", None),
            "the row whose folder the box names offers to clear it"
        );
        click(cx, &mut vcx, clear_alpha);
        assert_eq!(drawn(cx, &mut vcx, &rows), all, "Clear empties the box");
        assert!(vcx.debug_bounds(clear_alpha).is_none());
    });
}

/// A turn that finished at `at`, unseen.
fn finished(at: i64) -> AgentActivity {
    let mut activity = AgentActivity::default();
    activity.observe(None, AgentActivityState::Working, at - 1_000);
    activity.observe(
        Some(AgentActivityState::Working),
        AgentActivityState::Done,
        at,
    );
    activity
}

#[test]
fn the_asleep_count_hides_sleeping_rows_but_not_one_that_needs_you() {
    with_app_drawing(true, |cx, window| {
        ready(cx, window, SidebarSort::RecentWork);
        let (pid, root) = project(cx, window, "sleep");
        let now = chrono::Utc::now().timestamp_millis();
        let seen = || {
            let mut seen = finished(now - 60_000);
            seen.attend(now);
            seen
        };
        let awake = session(cx, window, pid, "Awake", root.clone(), false, seen());
        let asleep = session(cx, window, pid, "Asleep", root.clone(), true, seen());
        let unread = session(cx, window, pid, "Asleep unread", root, true, finished(now));
        let rows = [
            ("Awake", awake),
            ("Asleep", asleep),
            ("Asleep unread", unread),
        ];
        let mut vcx = VisualTestContext::from_window(window.into(), cx);
        let all = rows.map(|(name, _)| name.to_string()).to_vec();
        assert_eq!(drawn(cx, &mut vcx, &rows), all);

        click(cx, &mut vcx, "workspace-asleep");
        assert_eq!(
            drawn(cx, &mut vcx, &rows),
            ["Awake", "Asleep unread"],
            "awake only hides a sleeping row, but not one with something unread"
        );
        assert!(
            window
                .update(cx, |app, _, _| app.settings.sidebar_awake_only)
                .unwrap(),
            "the toggle is a setting"
        );
        click(cx, &mut vcx, "workspace-asleep");
        assert_eq!(
            drawn(cx, &mut vcx, &rows),
            all,
            "and clicked again shows them"
        );
    });
}
