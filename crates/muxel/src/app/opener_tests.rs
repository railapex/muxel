//! Which session a file or page belongs to, through the app's real open paths.

use super::popout_lifecycle_tests::with_app;
use super::{Instance, MuxelApp, OpenFocus, Project, ResourceOrigin};
use muxel_core::sidebar::{SidebarEntry, SidebarSort, instances_by_id, sidebar_order};
use muxel_core::{PaneNode, add_tab};
use uuid::Uuid;

fn opened_by(app: &MuxelApp, iid: Uuid) -> Option<Uuid> {
    app.workspace.instance(iid).and_then(|i| i.opened_by)
}

#[test]
fn reopening_a_file_moves_it_under_the_session_that_opened_it_last() {
    with_app(|cx, window| {
        window
            .update(cx, |app, window, cx| {
                let root = muxel_store::data_dir()
                    .unwrap()
                    .join(Uuid::new_v4().to_string());
                std::fs::create_dir_all(&root).unwrap();
                let plan_path = root.join("plan.md");
                let notes_path = root.join("notes.md");
                std::fs::write(&plan_path, "# plan\n").unwrap();
                std::fs::write(&notes_path, "# notes\n").unwrap();
                let pid = app.workspace.add_project(Project::new("opener", root));
                app.workspace.active_project = Some(pid);
                app.current_workspace = Some(Uuid::new_v4());
                let (first, second) = (Instance::shell(pid), Instance::shell(pid));
                let (first_id, second_id) = (first.id, second.id);
                app.workspace.add_instance(first);
                app.workspace.add_instance(second);
                let project = app.workspace.project_mut(pid).unwrap();
                project.layout = Some(PaneNode::leaf(first_id));
                assert!(add_tab(&mut project.layout, first_id, second_id));

                let plan = app
                    .open_editor_at(
                        pid,
                        Some(plan_path.clone()),
                        ResourceOrigin::Pane(first_id),
                        OpenFocus::Focus,
                        window,
                        cx,
                    )
                    .unwrap();
                assert_eq!(opened_by(app, plan), Some(first_id));

                // The second session opens the same file: same pane, new owner.
                let again = app
                    .open_editor_at(
                        pid,
                        Some(plan_path.clone()),
                        ResourceOrigin::Pane(second_id),
                        OpenFocus::Focus,
                        window,
                        cx,
                    )
                    .unwrap();
                assert_eq!(again, plan);
                assert_eq!(opened_by(app, plan), Some(second_id));

                // The file browser names no session and leaves the owner.
                app.open_editor_at(
                    pid,
                    Some(plan_path),
                    ResourceOrigin::Chrome {
                        near: Some(first_id),
                    },
                    OpenFocus::Focus,
                    window,
                    cx,
                )
                .unwrap();
                assert_eq!(opened_by(app, plan), Some(second_id));

                // A link followed from the file belongs to the file's session.
                let notes = app
                    .open_editor_at(
                        pid,
                        Some(notes_path),
                        ResourceOrigin::Pane(plan),
                        OpenFocus::Focus,
                        window,
                        cx,
                    )
                    .unwrap();
                assert_eq!(opened_by(app, notes), Some(second_id));

                let project = app.workspace.project(pid).unwrap();
                let rows = sidebar_order(
                    &project.instances(),
                    &instances_by_id(&app.workspace),
                    chrono::Utc::now().timestamp_millis(),
                    SidebarSort::RecentWork,
                );
                let at = |entry| rows.iter().position(|row| *row == entry).unwrap();
                let second_row = at(SidebarEntry::Pane(second_id));
                assert_eq!(at(SidebarEntry::Nested(notes)), second_row + 1, "{rows:?}");
                assert_eq!(at(SidebarEntry::Nested(plan)), second_row + 2, "{rows:?}");
            })
            .unwrap();
    });
}

/// A link clicked in rendered Markdown opens from the Markdown file's pane, so
/// the file it opens belongs to the session that opened the Markdown.
#[test]
fn a_rendered_markdown_links_file_belongs_to_the_markdowns_session() {
    with_app(|cx, window| {
        let (view, session) = window
            .update(cx, |app, window, cx| {
                let root = muxel_store::data_dir()
                    .unwrap()
                    .join(Uuid::new_v4().to_string());
                std::fs::create_dir_all(&root).unwrap();
                let readme_path = root.join("README.md");
                std::fs::write(&readme_path, "[guide](guide.md)\n").unwrap();
                std::fs::write(root.join("guide.md"), "# guide\n").unwrap();
                let pid = app.workspace.add_project(Project::new("opener", root));
                app.workspace.active_project = Some(pid);
                app.current_workspace = Some(Uuid::new_v4());
                app.main_window = Some(window.window_handle());
                let session = Instance::shell(pid);
                let session_id = session.id;
                app.workspace.add_instance(session);
                app.workspace.project_mut(pid).unwrap().layout = Some(PaneNode::leaf(session_id));

                let readme = app
                    .open_editor_at(
                        pid,
                        Some(readme_path),
                        ResourceOrigin::Pane(session_id),
                        OpenFocus::Focus,
                        window,
                        cx,
                    )
                    .unwrap();
                assert_eq!(opened_by(app, readme), Some(session_id));
                (app.editors[&readme].entity_id(), session_id)
            })
            .unwrap();

        cx.update(|cx| MuxelApp::open_rendered_link(view, "guide.md", false, cx));
        cx.run_until_parked();

        window
            .read_with(cx, |app, cx| {
                let guide = app
                    .editors
                    .iter()
                    .find(|(_, editor)| {
                        editor
                            .read(cx)
                            .path()
                            .and_then(|path| path.file_name())
                            .is_some_and(|name| name == "guide.md")
                    })
                    .map(|(iid, _)| *iid)
                    .expect("the link opened nothing");
                assert_eq!(opened_by(app, guide), Some(session));
            })
            .unwrap();
    });
}
