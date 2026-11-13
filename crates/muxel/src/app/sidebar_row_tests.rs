//! A Recent work row keeps enough of its name to tell it by. At every sidebar
//! width a family's tags on its top row give way first, then the row's own
//! pill, inside the sidebar, and the name never drops under its floor.

use super::popout_lifecycle_tests::with_app_drawing;
use super::{Instance, MuxelApp, Project, ROW_NAME_FLOOR_REMS, SidebarSort};
use gpui::{AppContext, TestAppContext, VisualTestContext, WindowHandle};
use muxel_core::muse::{MuseMetadata, MuseState};
use muxel_core::{PaneNode, add_tab};
use muxel_terminal::{CommandSpec, TerminalLaunch, TerminalView};
use std::time::Instant;
use uuid::Uuid;

/// A live session in `pid` named `name` that asks, as Muse's permission hook
/// reports it. The app's test teardown kills it.
fn asking_session(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pid: Uuid,
    name: &str,
    opened_by: Option<Uuid>,
) -> Uuid {
    window
        .update(cx, |app, window, cx| {
            let mut instance = Instance::shell(pid);
            instance.custom_name = Some(name.into());
            instance.opened_by = opened_by;
            instance.program = Some("cmd.exe".into());
            instance.args = ["/d", "/q", "/c", "more"].map(String::from).to_vec();
            let iid = instance.id;
            let spec = CommandSpec::program("cmd.exe", instance.args.clone())
                .with_status_program(Some("muse".into()));
            let launch = TerminalLaunch::spawn(spec, (80, 24)).expect("spawn fixture");
            let view = cx.new(|cx| TerminalView::new(launch, iid, Instant::now(), window, cx));
            view.update(cx, |view, cx| {
                view.set_muse_hook_state(
                    &MuseMetadata {
                        state: MuseState::Blocked,
                        ..Default::default()
                    },
                    cx,
                )
            });
            app.workspace.add_instance(instance);
            let project = app.workspace.project_mut(pid).unwrap();
            if let Some(anchor) = project.first_instance() {
                assert!(add_tab(&mut project.layout, anchor, iid));
            } else {
                project.layout = Some(PaneNode::leaf(iid));
            }
            app.terminals.insert(iid, view);
            iid
        })
        .unwrap()
}

#[test]
fn a_family_top_row_keeps_its_name_while_its_tags_give_way() {
    with_app_drawing(true, |cx, window| {
        let pid = window
            .update(cx, |app, _, _| {
                // Past the first-run screens, with the sidebar shown.
                app.show_terms = false;
                app.show_workspace_selector = false;
                app.sidebar_collapsed = false;
                app.settings.sidebar_sort = SidebarSort::RecentWork;
                app.use_tmux = false;
                app.use_worktree = false;
                let root = muxel_store::data_dir().unwrap().join("rows");
                std::fs::create_dir_all(&root).unwrap();
                let pid = app.workspace.add_project(Project::new("rows", root));
                app.workspace.active_project = Some(pid);
                pid
            })
            .unwrap();
        // The top row asks, so its own pill is wide, and a child asks under it.
        let top = asking_session(cx, window, pid, "Orchestrator", None);
        asking_session(cx, window, pid, "Worker", Some(top));
        let selector = |what: &str| -> &'static str { format!("{what}-{}", top.simple()).leak() };
        let (name, tag, pill) = (
            selector("inst-name"),
            selector("inst-asking"),
            selector("inst-badge"),
        );
        let mut vcx = VisualTestContext::from_window(window.into(), cx);

        // (sidebar width, name width, tag width, pill width), widest first.
        let mut drawn = Vec::new();
        for width in (160..=320).rev().step_by(16) {
            window
                .update(cx, |app, _, cx| {
                    // A new workspace key seeds the sidebar's resize state
                    // with this width.
                    app.current_workspace = Some(Uuid::new_v4());
                    app.workspace.sidebar_width = Some(width as f32);
                    cx.notify();
                })
                .unwrap();
            cx.run_until_parked();
            let mut bounds = |selector: &'static str| {
                vcx.debug_bounds(selector)
                    .unwrap_or_else(|| panic!("{selector} not drawn at {width}px"))
            };
            let sidebar = bounds("sidebar-scroll");
            assert!(
                (sidebar.size.width - gpui::px(width as f32)).abs() < gpui::px(1.0),
                "the sidebar is drawn {width}px wide, not {:?}",
                sidebar.size.width
            );
            let drawn_pill = bounds(pill);
            assert!(
                drawn_pill.right() <= sidebar.right() + gpui::px(0.5),
                "at {width}px the row's own pill ends at {:?}, past the sidebar's edge {:?}",
                drawn_pill.right(),
                sidebar.right()
            );
            drawn.push((
                width,
                bounds(name).size.width,
                bounds(tag).size.width,
                drawn_pill.size.width,
            ));
        }
        let floor = vcx.update(|window, _| window.rem_size()) * ROW_NAME_FLOOR_REMS;
        let at_floor = |name: gpui::Pixels| name < floor + gpui::px(0.5);
        let (_, widest_name, full_tag, full_pill) = drawn[0];
        let (_, _, narrowest_tag, _) = drawn[drawn.len() - 1];
        assert!(
            !at_floor(widest_name),
            "the name takes the room it has: {drawn:?}"
        );
        assert!(
            narrowest_tag < full_tag,
            "the tag gives way at the narrowest width: {drawn:?}"
        );
        for &(width, name, tag, pill) in &drawn {
            assert!(
                name >= floor - gpui::px(0.5),
                "at {width}px the name is {name:?} wide, under its floor {floor:?}: {drawn:?}"
            );
            assert!(
                tag >= full_tag - gpui::px(0.5) || at_floor(name),
                "at {width}px the tag gave way while the name was over its floor: {drawn:?}"
            );
            assert!(
                pill >= full_pill - gpui::px(0.5) || tag < gpui::px(0.5),
                "at {width}px the row's own pill gave way before the tag: {drawn:?}"
            );
        }
    });
}
