//! Outside control calls a pane `starting` until its program has drawn
//! something a person can see, as the pane's own spinner does. An agent that
//! has drawn nothing yet is still initialising: a fresh Claude kept text typed
//! then but dropped the Enter, leaving the prompt unsent in its box.

use crate::app::MuxelApp;
use crate::app::popout_lifecycle_tests::with_app_drawing;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::control::{AgentState, ErrorCode};
use muxel_core::{Instance, PaneNode, Project};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// What outside control makes of the pane at one moment.
#[derive(Debug)]
struct Seen {
    drawn: bool,
    text: String,
    state: Option<AgentState>,
    launch_id: Option<Uuid>,
    forced_send: Result<(), Option<ErrorCode>>,
    keys: Result<(), Option<ErrorCode>>,
}

fn see(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, pane: Uuid) -> Option<Seen> {
    window
        .update(cx, |app, _, cx| {
            let view = app.terminals.get(&pane)?.read(cx);
            let drawn = view.has_visible_content();
            let text = view.visible_text().trim().to_string();
            let info = app.control_info_of(pane, 0, cx).unwrap();
            let forced_send = app
                .control_send(pane, "hello".into(), true, None, cx)
                .map(drop)
                .map_err(|error| error.code);
            let keys = app
                .control_keys(pane, &["enter".into()], cx)
                .map(drop)
                .map_err(|error| error.code);
            Some(Seen {
                drawn,
                text,
                state: info.status,
                launch_id: info.launch_id,
                forced_send,
                keys,
            })
        })
        .unwrap()
}

#[test]
fn a_pane_is_starting_to_ctl_until_its_program_draws() {
    with_app_drawing(true, |cx, window| {
        let pane = window
            .update(cx, |app, window, cx| {
                let cwd = muxel_store::data_dir().unwrap().join("project");
                std::fs::create_dir_all(&cwd).unwrap();
                // cmd.exe prints a warning (drawing at once) in a \\?\ folder.
                let cwd = std::path::PathBuf::from(
                    cwd.to_string_lossy()
                        .trim_start_matches(r"\\?\")
                        .to_string(),
                );
                let pid = app.workspace.add_project(Project::new("starting", cwd));
                app.workspace.active_project = Some(pid);
                app.current_workspace.get_or_insert_with(Uuid::new_v4);
                app.show_terms = false;
                app.show_workspace_selector = false;
                app.use_tmux = false;
                app.use_worktree = false;
                app.settings.control_enabled = true;
                app.settings.control_allow_shells = true;
                // Silent for about two seconds, then a line, then it waits.
                let mut instance = Instance::shell(pid);
                instance.preset = String::new();
                instance.preset_id = None;
                instance.program = Some("cmd.exe".into());
                instance.args = [
                    "/d",
                    "/q",
                    "/c",
                    "ping -n 3 127.0.0.1 >nul & echo ready & more",
                ]
                .into_iter()
                .map(str::to_string)
                .collect();
                let id = instance.id;
                app.workspace.project_mut(pid).unwrap().layout = Some(PaneNode::leaf(id));
                app.workspace.add_instance(instance);
                app.active_instance = Some(id);
                app.ensure_project_terminals_deferred(pid, window, cx);
                id
            })
            .unwrap();

        let started = Instant::now();
        let mut before: Option<Seen> = None;
        let mut first_drawn: Option<(Duration, Seen)> = None;
        let after = loop {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the pane never drew its line; first drawn {first_drawn:?}, last undrawn {before:?}"
            );
            cx.executor().advance_clock(Duration::from_millis(250));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(50));
            let Some(seen) = see(cx, window, pane) else {
                continue;
            };
            if seen.text.contains("ready") {
                break seen;
            }
            if !seen.drawn {
                before = Some(seen);
            } else if first_drawn.is_none() {
                first_drawn = Some((started.elapsed(), seen));
            }
        };

        // Running, nothing visible yet: starting, no launch to pin, and nothing
        // is typed into it, forced or not.
        let before = before.expect("the pane was never seen before it drew");
        assert_eq!(before.state, Some(AgentState::Starting), "{before:?}");
        assert_eq!(before.launch_id, None, "{before:?}");
        assert_eq!(
            before.forced_send,
            Err(Some(ErrorCode::Starting)),
            "{before:?}"
        );
        assert_eq!(before.keys, Err(Some(ErrorCode::Starting)), "{before:?}");
        // The first thing drawn is the program's own line, after its silence.
        assert!(
            first_drawn.is_none(),
            "drew before its line: {first_drawn:?}"
        );

        // Once it has drawn, it reads as its program reports (a shell running a
        // command is working), with its launch, and takes input again.
        assert!(after.drawn, "{after:?}");
        assert_ne!(after.state, Some(AgentState::Starting), "{after:?}");
        assert!(after.launch_id.is_some(), "{after:?}");
        assert_eq!(after.forced_send, Ok(()), "{after:?}");
        assert_eq!(after.keys, Ok(()), "{after:?}");
    });
}
