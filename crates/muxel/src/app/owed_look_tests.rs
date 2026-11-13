//! A terminal focused before it draws, as the pane in front is when muxel
//! restores a project at launch, is owed the look focus couldn't record. The
//! status tick pays it once the terminal draws while it is the focused pane of
//! the active window, so a finish saved unread there doesn't stay unread in
//! front of the person looking at it.

use super::restart_completion_tests::with_app;
use super::{Instance, PaneNode, Project};
use muxel_core::{AgentActivity, AgentActivityState};
use std::time::{Duration, Instant};

#[test]
fn a_pane_focused_before_it_draws_is_looked_at_once_it_draws_in_front_of_an_active_window() {
    with_app(|cx, main| {
        let pane = main
            .update(cx, |app, window, cx| {
                let root = muxel_store::data_dir().unwrap().join("project");
                std::fs::create_dir_all(&root).unwrap();
                let pid = app.workspace.add_project(Project::new("owed-look", &root));
                let mut instance = Instance::shell(pid);
                instance.program = Some("cmd.exe".into());
                instance.args = ["/d", "/q", "/k", "echo ready"].map(String::from).into();
                // A turn that finished while nobody was looking, saved unread.
                instance.activity = AgentActivity {
                    work_started_at: Some(1_000),
                    completed_at: Some(2_000),
                    last_state: Some(AgentActivityState::Done),
                    ..Default::default()
                };
                let iid = instance.id;
                app.workspace.add_instance(instance);
                app.workspace.project_mut(pid).unwrap().layout = Some(PaneNode::leaf(iid));
                app.main_window = Some(main.into());
                app.current_workspace.get_or_insert_with(uuid::Uuid::new_v4);
                app.show_terms = false;
                app.show_workspace_selector = false;
                app.use_tmux = false;
                app.use_worktree = false;
                app.window_active = false;
                // As at launch: the project's front pane is focused before its
                // terminal exists.
                app.select_project(pid, window, cx);
                assert_eq!(app.active_instance, Some(iid));
                iid
            })
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "the pane never drew");
            cx.executor().advance_clock(Duration::from_millis(250));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(50));
            let drawn = main
                .update(cx, |app, _, cx| {
                    app.terminals
                        .get(&pane)
                        .is_some_and(|view| view.read(cx).has_visible_content())
                })
                .unwrap();
            if drawn {
                break;
            }
        }

        main.update(cx, |app, window, cx| {
            let unread = |app: &super::MuxelApp| {
                app.workspace
                    .instance(pane)
                    .unwrap()
                    .activity
                    .has_unattended_completion()
            };
            app.tick(window, cx);
            assert!(
                unread(app),
                "drawn in an inactive window: nobody has looked yet"
            );
            // Someone comes back to the window with the pane in front.
            app.window_active = true;
            app.tick(window, cx);
            assert!(!unread(app), "drawn in front of an active window is a look");
        })
        .unwrap();
    });
}
