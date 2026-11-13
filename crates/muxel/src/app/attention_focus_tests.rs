//! Focus next agent needing attention (Ctrl+Shift+A) shows the pane it picks,
//! in whichever project that pane is.

use super::popout_lifecycle_tests::with_app;
use super::{Instance, Project};
use muxel_core::{AgentActivityState, PaneNode};
use uuid::Uuid;

#[test]
fn focus_attention_switches_to_the_project_of_the_pane_it_picks() {
    with_app(|cx, window| {
        window
            .update(cx, |app, window, cx| {
                app.current_workspace = Some(Uuid::new_v4());
                let root = muxel_store::data_dir()
                    .unwrap()
                    .join(Uuid::new_v4().to_string());
                std::fs::create_dir_all(&root).unwrap();
                let here = app.workspace.add_project(Project::new("here", &root));
                let there = app.workspace.add_project(Project::new("there", &root));
                let now = chrono::Utc::now().timestamp_millis();
                let focused = Instance::shell(here);
                // A finish nobody has seen, in the project not on screen.
                let mut finished = Instance::shell(there);
                finished
                    .activity
                    .observe(None, AgentActivityState::Working, now - 2_000);
                finished.activity.observe(
                    Some(AgentActivityState::Working),
                    AgentActivityState::Done,
                    now - 1_000,
                );
                let (focused_id, finished_id) = (focused.id, finished.id);
                app.workspace.add_instance(focused);
                app.workspace.add_instance(finished);
                app.workspace.project_mut(here).unwrap().layout = Some(PaneNode::leaf(focused_id));
                app.workspace.project_mut(there).unwrap().layout =
                    Some(PaneNode::leaf(finished_id));
                app.workspace.active_project = Some(here);
                app.active_instance = Some(focused_id);

                app.focus_attention(window, cx);

                assert_eq!(app.active_instance, Some(finished_id));
                assert_eq!(
                    app.workspace.active_project,
                    Some(there),
                    "the picked pane's project is the one on screen"
                );
            })
            .unwrap();
    });
}
