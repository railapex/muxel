//! A question is modal: no key pressed while one shows reaches the terminal
//! behind it, and Enter and Esc answer it.

use super::popout_lifecycle_tests::with_app_drawing;
use super::{ConfirmAction, Instance, MuxelApp, PaneNode, Project};
use gpui::{AppContext, Context, KeyBinding, Window};
use muxel_terminal::{CommandSpec, TerminalLaunch, TerminalSession, TerminalView};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// A cmd shell, alone in a project of its own, on screen and focused, with
/// nothing else covering the panes.
fn focused_shell(
    app: &mut MuxelApp,
    name: &str,
    window: &mut Window,
    cx: &mut Context<MuxelApp>,
) -> (Uuid, Arc<TerminalSession>) {
    app.show_terms = false;
    app.show_workspace_selector = false;
    assert!(!app.any_overlay_open(cx), "an overlay is already open");
    let root = muxel_store::data_dir().unwrap().join(name);
    std::fs::create_dir_all(&root).unwrap();
    let pid = app.workspace.add_project(Project::new(name, root));
    let mut instance = Instance::shell(pid);
    instance.program = Some("cmd.exe".into());
    instance.args = vec!["/D".into(), "/Q".into(), "/K".into()];
    let pane = instance.id;
    let spec = CommandSpec::program("cmd.exe", instance.args.clone());
    app.workspace.add_instance(instance);
    let launch = TerminalLaunch::spawn(spec, (80, 24)).unwrap();
    let view = cx.new(|cx| TerminalView::new(launch, pane, Instant::now(), window, cx));
    let session = view.read(cx).session().clone();
    app.terminals.insert(pane, view);
    app.workspace.project_mut(pid).unwrap().layout = Some(PaneNode::leaf(pane));
    app.workspace.active_project = Some(pid);
    app.focus_instance(pane, window, cx);
    (pid, session)
}

fn ask_remove(app: &mut MuxelApp, pid: Uuid, cx: &mut Context<MuxelApp>) {
    app.request_confirm(
        "Remove project?",
        "The project and its panes will be removed.",
        "Remove",
        ConfirmAction::DeleteProject(pid),
        cx,
    );
}

/// No key pressed while a question shows reaches the focused terminal behind
/// it: not a letter, not the Enter or Esc that answers it, and not Tab, which
/// is an action rather than a key event.
#[test]
fn a_key_pressed_while_a_question_is_up_never_reaches_a_terminal() {
    with_app_drawing(true, |cx, window| {
        let (pid, session) = window
            .update(cx, |app, window, cx| {
                focused_shell(app, "keys-behind", window, cx)
            })
            .unwrap();
        // Tab reaches a terminal as an action, bound as the app binds it.
        cx.update(|cx| cx.bind_keys([KeyBinding::new("tab", super::SendTab, Some("Terminal"))]));
        cx.run_until_parked();
        // The control: with nothing showing, a key and Tab reach the shell.
        cx.simulate_keystrokes(window.into(), "x");
        let typed = session.last_user_input();
        assert!(typed.is_some(), "the focused shell never got the key");
        std::thread::sleep(Duration::from_millis(2));
        cx.simulate_keystrokes(window.into(), "tab");
        assert_ne!(
            session.last_user_input(),
            typed,
            "the focused shell never got Tab"
        );
        let typed = session.last_user_input();
        let untouched = |what: &str| {
            assert_eq!(session.last_user_input(), typed, "{what} reached the shell");
        };

        // The confirm dialog: Remove project.
        window
            .update(cx, |app, _, cx| ask_remove(app, pid, cx))
            .unwrap();
        for key in ["x", "tab", "escape"] {
            std::thread::sleep(Duration::from_millis(2));
            cx.simulate_keystrokes(window.into(), key);
            untouched(key);
        }
        // The Quit question.
        window
            .update(cx, |app, _, cx| {
                assert!(app.confirm.is_none(), "Esc didn't back out of Remove");
                app.show_quit_confirm = true;
                cx.notify();
            })
            .unwrap();
        for key in ["x", "tab", "escape"] {
            std::thread::sleep(Duration::from_millis(2));
            cx.simulate_keystrokes(window.into(), key);
            untouched(key);
        }
        window
            .update(cx, |app, _, cx| {
                assert!(!app.show_quit_confirm, "Esc didn't back out of Quit");
                ask_remove(app, pid, cx);
            })
            .unwrap();
        std::thread::sleep(Duration::from_millis(2));
        cx.simulate_keystrokes(window.into(), "enter");
        untouched("enter");
        window
            .update(cx, |app, _, _| {
                assert!(app.workspace.project(pid).is_none(), "Enter didn't answer");
            })
            .unwrap();
        session.kill();
    });
}

/// A question answers its keys while focus sits on a pane no longer drawn.
/// A key travels only towards the focused element, so this needs the root to
/// take focus.
#[test]
fn a_question_answers_keys_when_the_focused_pane_is_gone() {
    with_app_drawing(true, |cx, window| {
        let (pid, session) = window
            .update(cx, |app, window, cx| {
                focused_shell(app, "focus-gone", window, cx)
            })
            .unwrap();
        cx.run_until_parked();
        // Off screen, its focus handle alive.
        window
            .update(cx, |app, window, cx| {
                assert!(window.focused(cx).is_some(), "the pane never took focus");
                app.workspace.project_mut(pid).unwrap().layout = None;
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |app, window, cx| {
                assert!(
                    window.focused(cx).is_some(),
                    "the pane's focus went with it"
                );
                app.show_quit_confirm = true;
                cx.notify();
            })
            .unwrap();
        cx.run_until_parked();
        cx.simulate_keystrokes(window.into(), "escape");
        window
            .update(cx, |app, _, _| {
                assert!(!app.show_quit_confirm, "Esc didn't answer the question");
            })
            .unwrap();
        session.kill();
    });
}
