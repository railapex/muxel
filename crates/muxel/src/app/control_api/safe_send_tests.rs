//! `send --once`, `send --when-idle` and the draft check, on a real pane.

use super::{Answer, SendAdmission, check_send_state};
use crate::app::MuxelApp;
use gpui::{AppContext as _, Context, Entity, TestAppContext, WindowHandle};
use muxel_core::control::{self as ctl, AgentState, Command, ErrorCode};
use muxel_core::{Instance, PaneNode, Project};
use muxel_terminal::{CommandSpec, TerminalLaunch, TerminalSession, TerminalView};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// A cmd.exe pane that `ctl` may type into.
fn shell_pane(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    name: &str,
) -> (Uuid, Entity<TerminalView>, Arc<TerminalSession>) {
    window
        .update(cx, |app, window, cx| {
            let root = muxel_store::data_dir().unwrap().join(name);
            std::fs::create_dir_all(&root).unwrap();
            let project = app.workspace.add_project(Project::new("control", root));
            let mut instance = Instance::shell(project);
            instance.program = Some("cmd.exe".into());
            instance.args = vec!["/D".into(), "/Q".into(), "/K".into()];
            let pane = instance.id;
            let spec = CommandSpec::program("cmd.exe", instance.args.clone());
            app.workspace.add_instance(instance);
            // ctl answers nothing until a workspace is open, and sees only
            // the panes in its layouts.
            app.workspace.project_mut(project).unwrap().layout = Some(PaneNode::leaf(pane));
            app.workspace.active_project = Some(project);
            app.current_workspace = Some(Uuid::new_v4());
            app.settings.control_enabled = true;
            app.settings.control_allow_shells = true;
            let launch = TerminalLaunch::spawn(spec, (80, 24)).unwrap();
            let view = cx.new(|cx| TerminalView::new(launch, pane, Instant::now(), window, cx));
            let session = view.read(cx).session().clone();
            app.terminals.insert(pane, view.clone());
            (pane, view, session)
        })
        .unwrap()
}

fn send(agent: Uuid, text: &str, once: Option<&str>, when_idle: bool, force: bool) -> Command {
    Command::Send {
        agent: agent.to_string(),
        text: text.into(),
        force,
        launch_id: None,
        once: once.map(str::to_owned),
        when_idle,
        timeout_secs: None,
    }
}

/// Run `command` as the control server would, and hand back its reply.
fn ask(
    app: &mut MuxelApp,
    command: Command,
    cx: &mut Context<MuxelApp>,
) -> (std::sync::mpsc::Receiver<ctl::Response>, Arc<AtomicBool>) {
    let (reply, answer) = std::sync::mpsc::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let typed = app.control_command(command, None, cx);
    app.deliver_control_answer(typed, reply, cancelled.clone(), cx);
    (answer, cancelled)
}

/// Run the app until `answer` arrives, moving the fake clock past the
/// paste-to-Enter delay and the confirmation polls.
fn reply(
    cx: &mut TestAppContext,
    answer: &std::sync::mpsc::Receiver<ctl::Response>,
) -> ctl::Response {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        if let Ok(response) = answer.try_recv() {
            return response;
        }
        assert!(Instant::now() < deadline, "no reply");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A prompt typed into a half-written message would merge with it, so a send
/// refuses while the composer holds text; `--force` still types.
#[test]
fn a_send_refuses_a_composer_that_holds_a_draft() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-draft");
        let draft = |cx: &mut TestAppContext| {
            window
                .update(cx, |app, _, cx| app.control_send_admission(pane, false, cx))
                .unwrap()
        };
        assert!(!draft(cx).draft_present);
        session.write_input(b"\x1b[A");
        assert!(!draft(cx).draft_present, "an arrow key isn't a draft");
        session.write_input(b"half typed");
        let admission = draft(cx);
        assert!(admission.draft_present);
        let idle = SendAdmission {
            state: Some(AgentState::Idle),
            ..admission
        };
        let refused = check_send_state(idle).unwrap_err();
        assert_eq!(refused.code, Some(ErrorCode::DraftPresent));
        assert_eq!(refused.pasted, None);
        assert!(
            check_send_state(SendAdmission {
                force: true,
                ..idle
            })
            .is_ok()
        );
        session.kill();
    });
}

/// Wire rings a doorbell with one key per notice. However often it retries,
/// the pane gets the prompt once, and every retry learns how the first went.
#[test]
fn a_repeated_once_key_types_once_and_returns_the_first_receipt() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-once");
        // A fresh shell can read as Working, which only --force passes.
        let (first, _) = window
            .update(cx, |app, _, cx| {
                ask(
                    app,
                    send(pane, "echo rung", Some("d-9f2a"), false, true),
                    cx,
                )
            })
            .unwrap();
        let first = reply(cx, &first).into_result().unwrap();
        assert_eq!(first["sent"], true);
        let typed = session.input_rev();
        let (repeat, _) = window
            .update(cx, |app, _, cx| {
                ask(
                    app,
                    send(pane, "echo rung", Some("d-9f2a"), false, true),
                    cx,
                )
            })
            .unwrap();
        let repeat = reply(cx, &repeat).into_result().unwrap();
        assert_eq!(repeat["duplicate"], true);
        assert_eq!(repeat["outcome"], first["outcome"]);
        assert_eq!(session.input_rev(), typed, "the repeat typed");
        let status = window
            .update(cx, |app, _, cx| {
                app.control_command(
                    Command::OnceStatus {
                        agent: pane.to_string(),
                        key: "d-9f2a".into(),
                    },
                    None,
                    cx,
                )
            })
            .unwrap();
        let Answer::Now(Ok(status)) = status else {
            panic!("once-status answers at once");
        };
        assert_eq!(status["state"], "done");
        assert_eq!(status["result"]["ok"], true);
        session.kill();
    });
}

/// A send refused before it typed leaves its key for the retry.
#[test]
fn a_once_key_refused_before_typing_can_be_sent_again() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-once-refused");
        session.write_input(b"half typed");
        let before = session.input_rev();
        let refused = window
            .update(cx, |app, _, cx| {
                // Programs other than shells are refused when shells aren't
                // allowed; that refusal comes before any typing.
                app.settings.control_allow_shells = false;
                let answer =
                    app.control_command(send(pane, "hi", Some("d-1"), false, true), None, cx);
                app.settings.control_allow_shells = true;
                answer
            })
            .unwrap();
        let Answer::Now(Err(refused)) = refused else {
            panic!("a refused send answers at once");
        };
        assert_eq!(refused.pasted, None);
        assert_eq!(session.input_rev(), before);
        let status = window
            .update(cx, |app, _, _| app.control_once.status(pane, "d-1"))
            .unwrap();
        assert_eq!(status["state"], "unused");
        session.kill();
    });
}

/// A waiting send holds its pane's one queue place, and one whose caller
/// has gone types nothing and gives its key back.
#[test]
fn a_waiting_send_whose_caller_left_types_nothing() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-queue");
        // A draft keeps the send waiting whatever the shell's state reads.
        session.write_input(b"half typed");
        let before = session.input_rev();
        let (waiting, cancelled) = window
            .update(cx, |app, _, cx| {
                ask(
                    app,
                    send(pane, "[wire v1 d-2] mail", Some("d-2"), true, false),
                    cx,
                )
            })
            .unwrap();
        cx.run_until_parked();
        assert!(waiting.try_recv().is_err(), "the send didn't wait");
        let (second, _) = window
            .update(cx, |app, _, cx| {
                assert!(app.control_queue.contains_key(&pane));
                assert_eq!(app.control_once.status(pane, "d-2")["state"], "pending");
                ask(app, send(pane, "another", None, true, false), cx)
            })
            .unwrap();
        let second = reply(cx, &second).into_result().unwrap_err();
        assert_eq!(second.code, Some(ErrorCode::Busy));
        assert_eq!(second.reason.as_deref(), Some("queued"));
        cancelled.store(true, Ordering::Release);
        window
            .update(cx, |app, _, cx| app.pump_control_queue(cx))
            .unwrap();
        let gone = reply(cx, &waiting).into_result().unwrap_err();
        assert_eq!(gone.pasted, None);
        assert_eq!(session.input_rev(), before, "a cancelled send typed");
        window
            .update(cx, |app, _, _| {
                assert!(app.control_queue.is_empty());
                assert_eq!(app.control_once.status(pane, "d-2")["state"], "unused");
            })
            .unwrap();
        session.kill();
    });
}

/// A workspace switch takes the waiting send's pane away: it is refused,
/// having typed nothing.
#[test]
fn a_workspace_switch_refuses_waiting_sends() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-queue-switch");
        session.write_input(b"half typed");
        let (waiting, _) = window
            .update(cx, |app, _, cx| {
                ask(app, send(pane, "hi", Some("d-3"), true, false), cx)
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |app, _, _| app.refuse_control_queue())
            .unwrap();
        let refused = reply(cx, &waiting).into_result().unwrap_err();
        assert_eq!(refused.code, Some(ErrorCode::Exited));
        assert_eq!(refused.pasted, None);
        session.kill();
    });
}
