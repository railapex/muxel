//! Recent work's bands fold in each project's sidebar, Older starting folded,
//! and a project keeps its folds across restarts.

use super::popout_lifecycle_tests::with_app;
use super::{Instance, Project, SidebarEntry, SidebarSort};
use muxel_core::sidebar::{ActivityBand, BandLabel};
use muxel_core::{AgentActivity, AgentActivityState, PaneNode, add_tab};
use uuid::Uuid;

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
                // A turn that finished at `at`, unseen.
                let finished = |at: i64| {
                    let mut activity = AgentActivity::default();
                    activity.observe(None, AgentActivityState::Working, at - 1_000);
                    activity.observe(
                        Some(AgentActivityState::Working),
                        AgentActivityState::Done,
                        at,
                    );
                    activity
                };
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
