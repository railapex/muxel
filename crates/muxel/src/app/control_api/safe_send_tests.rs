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
    pane_with(cx, window, name, std::convert::identity)
}

/// A cmd.exe pane whose ctl input goes through tmux, to a fake tmux server
/// that takes everything. The flag says whether anything was typed.
fn tmux_pane(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    name: &str,
) -> (Uuid, Arc<TerminalSession>, Arc<AtomicBool>) {
    let typed = Arc::new(AtomicBool::new(false));
    let saw = typed.clone();
    let route = muxel_terminal::TmuxInputRoute::new(
        "muxel_p_1",
        true,
        move |args: &[String], _: Option<&[u8]>| {
            let command = if args[0] == "if-shell" {
                args.last()
                    .and_then(|script| script.split_whitespace().next())
                    .unwrap_or_default()
                    .to_owned()
            } else {
                args[0].clone()
            };
            if matches!(command.as_str(), "load-buffer" | "send-keys") {
                saw.store(true, Ordering::Release);
            }
            let stdout = match command.as_str() {
                "list-panes" => "muxel_p_1:%7:1::3.4\n",
                "display-message" => "%7\n",
                _ => "",
            };
            Ok(std::process::Output {
                status:
                    <std::process::ExitStatus as std::os::windows::process::ExitStatusExt>::from_raw(
                        0,
                    ),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        },
    );
    let (pane, _view, session) = pane_with(cx, window, name, |spec| spec.with_tmux_input(route));
    assert!(session.delivers_through_tmux());
    (pane, session, typed)
}

fn pane_with(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    name: &str,
    wrap: impl FnOnce(CommandSpec) -> CommandSpec,
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
            let spec = wrap(CommandSpec::program("cmd.exe", instance.args.clone()));
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
        session_id: None,
        once: once.map(str::to_owned),
        when_idle,
        timeout_secs: None,
    }
}

/// `command`, a send, pinned to conversation `sid` (`--session-id`).
fn pinned(mut command: Command, sid: &str) -> Command {
    if let Command::Send { session_id, .. } = &mut command {
        *session_id = Some(sid.into());
    }
    command
}

/// Two provider conversations, as Codex names them in its title.
const CONVERSATION_A: &str = "01a1543c-9700-7000-8000-00000000000a";
const CONVERSATION_B: &str = "01a1543c-9700-7000-8000-00000000000b";

/// Adopt conversation `sid` for the pane, as session discovery does once it
/// has validated a provider's new one (a `/clear`, `/resume`, Codex thread).
fn adopt_conversation(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pane: Uuid,
    sid: &str,
) {
    window
        .update(cx, |app, _, cx| {
            app.workspace.instance_mut(pane).unwrap().session_id = Some(sid.into());
            app.note_adopted_conversation(pane, cx);
        })
        .unwrap();
}

/// Codex names thread `sid` in its title: the terminal's binding moves to it
/// at once, before muxel has validated it, let alone stored it.
fn codex_titles(session: &TerminalSession, sid: &str) {
    session.process_probe_output(format!("\x1b]0;{sid} | Ready\x07").as_bytes());
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

/// `status` names the conversation muxel can vouch for, and every snapshot
/// says which protocol this muxel takes, so a client learns both before it
/// sends.
#[test]
fn status_and_panes_name_the_conversation_and_the_protocol() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-session-status");
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        let answer = |cx: &mut TestAppContext, command: Command| {
            let answer = window
                .update(cx, |app, _, cx| app.control_command(command, None, cx))
                .unwrap();
            let Answer::Now(Ok(value)) = answer else {
                panic!("a read answers at once");
            };
            value
        };
        let one = |cx: &mut TestAppContext| {
            answer(
                cx,
                Command::Status {
                    agent: Some(pane.to_string()),
                },
            )
        };
        let status = one(cx);
        assert_eq!(status["session_id"], CONVERSATION_A);
        assert_eq!(status["protocol"], ctl::NEWEST_VERSION);
        let all = answer(cx, Command::Status { agent: None });
        assert_eq!(all["protocol"], ctl::NEWEST_VERSION);
        assert_eq!(all["agents"][0]["session_id"], CONVERSATION_A);
        let panes = answer(cx, Command::Panes { project: None });
        assert_eq!(panes["protocol"], ctl::NEWEST_VERSION);
        // Codex names a thread muxel hasn't confirmed: it names none.
        codex_titles(&session, CONVERSATION_B);
        let status = one(cx);
        assert!(status.get("session_id").is_none(), "{status}");
        // Once adopted, B.
        adopt_conversation(cx, window, pane, CONVERSATION_B);
        assert_eq!(one(cx)["session_id"], CONVERSATION_B);
        session.kill();
    });
}

/// Wire checks who owns a conversation, then rings it. A `/clear` between
/// that check and the paste keeps the terminal and its launch id, so only the
/// conversation pin stops the prompt landing in the new conversation.
#[test]
fn a_send_pinned_to_a_conversation_types_only_into_it() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-session-pin");
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        let before = session.input_rev();
        // Another conversation is refused on arrival, waiting or not.
        for when_idle in [false, true] {
            let force = !when_idle;
            let command = pinned(
                send(pane, "echo pinned", Some("d-7"), when_idle, force),
                CONVERSATION_B,
            );
            let (refused, _) = window
                .update(cx, |app, _, cx| ask(app, command, cx))
                .unwrap();
            let refused = reply(cx, &refused).into_result().unwrap_err();
            assert_eq!(refused.code, Some(ErrorCode::LaunchMismatch));
            assert_eq!(refused.reason.as_deref(), Some("conversation_changed"));
            assert_eq!(refused.pasted, None);
        }
        // Admitted in its conversation, then muxel adopts another before the
        // paste: the paste refuses and types nothing.
        let (answer, cancelled) = window
            .update(cx, |app, _, cx| {
                let typed = app.control_command(
                    pinned(
                        send(pane, "echo pinned", Some("d-7"), false, true),
                        CONVERSATION_A,
                    ),
                    None,
                    cx,
                );
                assert!(matches!(typed, Answer::Type(_)), "the send was admitted");
                app.workspace.instance_mut(pane).unwrap().session_id = Some(CONVERSATION_B.into());
                app.note_adopted_conversation(pane, cx);
                let (reply, answer) = std::sync::mpsc::channel();
                let cancelled = Arc::new(AtomicBool::new(false));
                app.deliver_control_answer(typed, reply, cancelled.clone(), cx);
                (answer, cancelled)
            })
            .unwrap();
        let refused = reply(cx, &answer).into_result().unwrap_err();
        assert!(!cancelled.load(Ordering::Acquire));
        assert_eq!(refused.code, Some(ErrorCode::LaunchMismatch));
        assert_eq!(refused.reason.as_deref(), Some("conversation_changed"));
        assert_eq!(refused.pasted, None);
        assert_eq!(
            session.input_rev(),
            before,
            "the old conversation's prompt typed"
        );
        // Nothing typed, so the key is free for the retry.
        let status = window
            .update(cx, |app, _, _| app.control_once.status(pane, "d-7"))
            .unwrap();
        assert_eq!(status["state"], "unused");
        // In its own conversation it types.
        let (sent, _) = window
            .update(cx, |app, _, cx| {
                ask(
                    app,
                    pinned(
                        send(pane, "echo pinned", Some("d-7"), false, true),
                        CONVERSATION_B,
                    ),
                    cx,
                )
            })
            .unwrap();
        assert_eq!(reply(cx, &sent).into_result().unwrap()["sent"], true);
        session.kill();
    });
}

/// Codex names a new thread in its title, and the terminal is in it at once,
/// before muxel has validated and stored it. A send pinned to the old thread
/// must not type in that window, however idle the new one looks.
#[test]
fn a_pin_to_the_old_thread_types_nothing_while_codex_names_a_new_one() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-session-unconfirmed");
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        codex_titles(&session, CONVERSATION_B);
        let before = session.input_rev();
        let (refused, _) = window
            .update(cx, |app, _, cx| {
                ask(
                    app,
                    pinned(
                        send(pane, "echo old", Some("d-8"), false, true),
                        CONVERSATION_A,
                    ),
                    cx,
                )
            })
            .unwrap();
        let refused = reply(cx, &refused).into_result().unwrap_err();
        assert_eq!(refused.code, Some(ErrorCode::LaunchMismatch));
        assert_eq!(refused.reason.as_deref(), Some("conversation_unconfirmed"));
        assert_eq!(refused.pasted, None);
        // A waiting send waits it out, since the pane may settle back...
        let (waiting, _) = window
            .update(cx, |app, _, cx| {
                ask(
                    app,
                    pinned(
                        send(pane, "echo old", Some("d-8"), true, false),
                        CONVERSATION_A,
                    ),
                    cx,
                )
            })
            .unwrap();
        cx.run_until_parked();
        assert!(waiting.try_recv().is_err(), "the send didn't wait");
        // ...and once muxel adopts B, it is another conversation.
        adopt_conversation(cx, window, pane, CONVERSATION_B);
        window
            .update(cx, |app, _, cx| app.pump_control_queue(cx))
            .unwrap();
        let changed = reply(cx, &waiting).into_result().unwrap_err();
        assert_eq!(changed.reason.as_deref(), Some("conversation_changed"));
        assert_eq!(changed.pasted, None);
        assert_eq!(session.input_rev(), before, "A's prompt typed into B");
        session.kill();
    });
}

/// Inside an unfinished synchronized update, Codex can name thread B while
/// the terminal's binding, which VTE updates only when the update ends, still
/// says A. Neither muxel's adoption of A nor A's title vouches for A then.
#[test]
fn a_pin_types_nothing_while_a_synchronized_update_holds_a_new_thread() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-session-sync");
        codex_titles(&session, CONVERSATION_A);
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        let before = session.input_rev();
        // One update, so no PTY drain runs between the frame and the checks.
        let (status, refused) = window
            .update(cx, |app, _, cx| {
                session.process_probe_output(
                    format!("\x1b[?2026h\x1b]0;{CONVERSATION_B} | Ready\x07").as_bytes(),
                );
                let status = app.control_command(
                    Command::Status {
                        agent: Some(pane.to_string()),
                    },
                    None,
                    cx,
                );
                let refused = app.control_command(
                    pinned(
                        send(pane, "echo old", Some("d-9"), false, true),
                        CONVERSATION_A,
                    ),
                    None,
                    cx,
                );
                (status, refused)
            })
            .unwrap();
        let Answer::Now(Ok(status)) = status else {
            panic!("a read answers at once");
        };
        assert!(status.get("session_id").is_none(), "{status}");
        let Answer::Now(Err(refused)) = refused else {
            panic!("A's pin was admitted while B's title waited in the frame");
        };
        assert_eq!(refused.code, Some(ErrorCode::LaunchMismatch));
        assert_eq!(refused.reason.as_deref(), Some("conversation_unconfirmed"));
        assert_eq!(refused.pasted, None);
        assert_eq!(session.input_rev(), before, "A's prompt typed into B");
        session.kill();
    });
}

/// Through tmux the paste waits in tmux's input queue after the last check,
/// where nothing checks the conversation again. A send pinned to one is
/// refused, waiting or not, and types nothing; a waiting one is refused
/// before it waits on anything. A waiting send the caller didn't pin holds
/// its conversation only while it waits, and still types.
#[test]
fn a_pinned_send_to_a_pane_behind_tmux_types_nothing() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, session, typed) = tmux_pane(cx, window, "control-session-tmux");
        codex_titles(&session, CONVERSATION_A);
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        let tmux_refusal = |refused: ctl::Error| {
            assert_eq!(
                refused.code,
                Some(ErrorCode::ParameterUnsupported),
                "{refused:?}"
            );
            assert_eq!(refused.reason.as_deref(), Some("tmux_delivery"));
            assert_eq!(refused.pasted, None);
        };
        let pinned_send = |when_idle: bool| {
            pinned(
                send(pane, "echo pinned", Some("d-10"), when_idle, !when_idle),
                CONVERSATION_A,
            )
        };
        let (refused, _) = window
            .update(cx, |app, _, cx| ask(app, pinned_send(false), cx))
            .unwrap();
        tmux_refusal(reply(cx, &refused).into_result().unwrap_err());
        assert!(
            !typed.load(Ordering::Acquire),
            "a pinned send typed through tmux"
        );
        let status = window
            .update(cx, |app, _, _| app.control_once.status(pane, "d-10"))
            .unwrap();
        assert_eq!(status["state"], "unused");
        let (_unpinned, _) = window
            .update(cx, |app, _, cx| {
                ask(app, send(pane, "echo unpinned", None, true, false), cx)
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !typed.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "the unpinned send never typed");
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        // A pane a waiting send would wait on (here, a draft) still refuses
        // the pin at once, not when the pane frees up.
        session.write_input(b"half typed");
        let (refused, _) = window
            .update(cx, |app, _, cx| ask(app, pinned_send(true), cx))
            .unwrap();
        cx.run_until_parked();
        let refused = refused
            .try_recv()
            .expect("the pinned send waited instead of refusing");
        tmux_refusal(refused.into_result().unwrap_err());
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
