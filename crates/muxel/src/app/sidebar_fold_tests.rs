//! Recent work's bands fold in each project's sidebar, Older starting folded,
//! and a project keeps its folds across restarts. A jump from the workspace
//! line opens whatever hides the row it lands on.

use super::popout_lifecycle_tests::with_app;
use super::{Instance, Project, RowJump, SidebarEntry, SidebarSort, with_pending};
use muxel_core::sidebar::{ActivityBand, BandLabel, RowCounts, instances_by_id};
use muxel_core::{AgentActivity, AgentActivityState, PaneNode, add_tab};
use uuid::Uuid;

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
fn the_unread_jump_opens_the_project_and_band_hiding_its_row() {
    with_app(|cx, window| {
        window
            .update(cx, |app, window, cx| {
                app.settings.sidebar_sort = SidebarSort::RecentWork;
                app.current_workspace = Some(Uuid::new_v4());
                let root = muxel_store::data_dir()
                    .unwrap()
                    .join(Uuid::new_v4().to_string());
                std::fs::create_dir_all(&root).unwrap();
                let pid = app.workspace.add_project(Project::new("jump", &root));
                let elsewhere = app.workspace.add_project(Project::new("elsewhere", &root));
                let now = chrono::Utc::now().timestamp_millis();
                let mut add = |project, activity| {
                    let mut instance = Instance::shell(project);
                    instance.activity = activity;
                    let id = instance.id;
                    app.workspace.add_instance(instance);
                    id
                };
                let seen = || {
                    let mut seen = finished(now - 60_000);
                    seen.attend(now);
                    seen
                };
                let today = add(pid, seen());
                let older = add(pid, finished(now - 30 * 86_400_000));
                // The focused pane is in another project.
                let there = add(elsewhere, seen());
                let project = app.workspace.project_mut(pid).unwrap();
                project.layout = Some(PaneNode::leaf(today));
                assert!(add_tab(&mut project.layout, today, older));
                project.collapsed = true;
                app.workspace.project_mut(elsewhere).unwrap().layout = Some(PaneNode::leaf(there));
                app.workspace.active_project = Some(elsewhere);
                app.active_instance = Some(there);

                let rows = app.sidebar_rows_each().remove(&pid).unwrap();
                let instances = instances_by_id(&app.workspace);
                let counts = app.sidebar_counts(&rows, &with_pending(&instances, &[]), cx);
                assert_eq!(
                    counts,
                    RowCounts {
                        asking: 0,
                        working: 0,
                        unread: 1,
                    },
                    "the header counts the unread row its collapsed project and folded band hide"
                );

                app.jump_to_next(RowJump::Unread, window, cx);
                assert_eq!(app.active_instance, Some(older));
                assert_eq!(
                    app.workspace.active_project,
                    Some(pid),
                    "the jump shows the row's project"
                );
                use ActivityBand::{Older, Today};
                use SidebarEntry::{Band, Pane};
                assert_eq!(
                    app.sidebar_entries_now(pid, cx),
                    vec![Band(Today), Pane(today), Band(Older), Pane(older)],
                    "the band holding the row opens"
                );
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                )
                .unwrap();
                let saved = saved.project(pid).unwrap();
                assert!(!saved.collapsed, "the project opens, and stays open");
                assert!(!saved.band_folded(Older, true), "so does the band");
            })
            .unwrap();
    });
}

#[test]
fn older_starts_folded_and_a_project_saves_its_folds() {
    with_app(|cx, window| {
        window
            .update(cx, |app, _, cx| {
                app.settings.sidebar_sort = SidebarSort::RecentWork;
                app.current_workspace = Some(Uuid::new_v4());
                let root = muxel_store::data_dir()
                    .unwrap()
                    .join(Uuid::new_v4().to_string());
                std::fs::create_dir_all(&root).unwrap();
                let pid = app.workspace.add_project(Project::new("folds", root));
                let now = chrono::Utc::now().timestamp_millis();
                let mut add = |activity| {
                    let mut instance = Instance::shell(pid);
                    instance.activity = activity;
                    let id = instance.id;
                    app.workspace.add_instance(instance);
                    id
                };
                let today = add(finished(now));
                let older = add(finished(now - 30 * 86_400_000));
                let project = app.workspace.project_mut(pid).unwrap();
                project.layout = Some(PaneNode::leaf(today));
                assert!(add_tab(&mut project.layout, today, older));

                use ActivityBand::{Older, Today};
                use SidebarEntry::{Band, FoldedBand, Pane};
                assert_eq!(
                    app.sidebar_entries_now(pid, cx),
                    vec![
                        Band(Today),
                        Pane(today),
                        FoldedBand(BandLabel {
                            band: Older,
                            rows: 1,
                            unread: 1,
                        }),
                    ],
                    "Older starts folded, saying what it hides"
                );

                app.toggle_band(pid, Older, cx);
                assert_eq!(
                    app.sidebar_entries_now(pid, cx),
                    vec![Band(Today), Pane(today), Band(Older), Pane(older)]
                );
                app.toggle_collapse(pid, cx);
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                )
                .unwrap();
                let saved = saved.project(pid).unwrap();
                assert!(!saved.band_folded(Older, true), "the opened band is saved");
                assert!(saved.collapsed, "the project fold is saved");
            })
            .unwrap();
    });
}
