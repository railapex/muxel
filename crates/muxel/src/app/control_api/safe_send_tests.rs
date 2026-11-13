//! `send --once`, `send --when-idle` and the draft check, on a real pane.

use super::{Answer, SendAdmission, check_send_state};
use crate::app::MuxelApp;
use gpui::{AppContext as _, Context, Entity, TestAppContext, WindowHandle};
use muxel_core::control::{self as ctl, Command, ErrorCode};
use muxel_core::{AgentActivity, AgentActivityState, Instance, PaneNode, Project};
use muxel_terminal::{
    AgentStatus, CommandSpec, SendRefusal, TerminalLaunch, TerminalSession, TerminalView,
};
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
    tmux_pane_reading(cx, window, name, None)
}

/// [`tmux_pane`], with its terminal reading status and screens as
/// `status_program`'s would.
fn tmux_pane_reading(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    name: &str,
    status_program: Option<&str>,
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
    let (pane, _view, session) = pane_with(cx, window, name, |spec| {
        let spec = spec.with_tmux_input(route);
        match status_program {
            Some(program) => spec.with_status_program(Some(program.to_owned())),
            None => spec,
        }
    });
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
        detach: false,
        no_wake: false,
        timeout_secs: None,
    }
}

/// A person presses Enter on the pane's draft. (On `next`, which gates
/// submission writes, this is `write_user_submission(b"\r", Submission::Turn)`.)
fn submit_draft(session: &TerminalSession) {
    session.mark_turn_submitted();
    session.write_input(b"\r");
}

/// `command`, a send, detached (`--when-idle --detach`).
fn detached(mut command: Command) -> Command {
    if let Command::Send {
        when_idle, detach, ..
    } = &mut command
    {
        (*when_idle, *detach) = (true, true);
    }
    command
}

/// `once-status KEY` for `pane`.
fn once_status(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pane: Uuid,
    key: &str,
) -> serde_json::Value {
    let answer = window
        .update(cx, |app, _, cx| {
            app.control_command(
                Command::OnceStatus {
                    agent: pane.to_string(),
                    key: key.into(),
                },
                None,
                cx,
            )
        })
        .unwrap();
    let Answer::Now(Ok(status)) = answer else {
        panic!("once-status answers at once");
    };
    status
}

/// `once-cancel KEY` for `pane`. A withdrawal is answered once it is on
/// disk, by a job off the UI thread, which runs here.
fn once_cancel(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pane: Uuid,
    key: &str,
) -> serde_json::Value {
    let answer = window
        .update(cx, |app, _, cx| {
            app.control_command(
                Command::OnceCancel {
                    agent: pane.to_string(),
                    key: key.into(),
                },
                None,
                cx,
            )
        })
        .unwrap();
    match answer {
        Answer::Now(Ok(answer)) => answer,
        Answer::Later(write) => write().expect("the withdrawal is on disk"),
        _ => panic!("once-cancel answers without an error"),
    }
}

/// Where the open workspace's detached sends are kept.
fn journal_path(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> std::path::PathBuf {
    window
        .update(cx, |app, _, _| {
            muxel_store::send_journal_path(app.current_workspace.unwrap()).unwrap()
        })
        .unwrap()
}

/// What a muxel restart leaves: no sends or keys in memory, only the open
/// workspace's record of its detached sends, read back as it opens. (The
/// pane keeps its terminal here; a real restart relaunches it.)
fn restart(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) {
    cx.run_until_parked();
    window
        .update(cx, |app, _, cx| {
            app.control_queue.clear();
            app.control_once.clear();
            let workspace = app.current_workspace.unwrap();
            app.restore_control_sends(workspace, cx);
        })
        .unwrap();
    cx.run_until_parked();
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
            app.note_adopted_conversation(pane, None, cx);
        })
        .unwrap();
}

/// Codex names thread `sid` in its title: the terminal's binding moves to it
/// at once, before muxel has validated it, let alone stored it.
fn codex_titles(session: &TerminalSession, sid: &str) {
    session.process_probe_output(format!("\x1b]0;{sid} | Ready\x07").as_bytes());
}

/// A local Claude pane as muxel sees it: its program is Claude's, so muxel
/// reads its conversation ends, while its terminal runs cmd.exe.
fn claude_pane(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    name: &str,
) -> (Uuid, Arc<TerminalSession>) {
    let (pane, _view, session) = shell_pane(cx, window, name);
    window
        .update(cx, |app, _, _| {
            app.workspace.instance_mut(pane).unwrap().program = Some("claude".into());
        })
        .unwrap();
    (pane, session)
}

/// Claude's SessionEnd hook for conversation `sid` in `pane`, which `/clear`,
/// `/resume` and exiting run, and wait for, before the next conversation
/// starts (Claude Code 2.1.322).
fn claude_ends(pane: Uuid, sid: &str, reason: &str) {
    let input = serde_json::json!({
        "session_id": sid,
        "transcript_path": format!("C:/claude/projects/control/{sid}.jsonl"),
        "cwd": "C:/control",
        "hook_event_name": "SessionEnd",
        "reason": reason,
    });
    crate::session_binding::write_claude_binding_from_reader(
        &muxel_store::data_dir().unwrap(),
        pane,
        std::io::Cursor::new(input.to_string()),
    )
    .unwrap();
}

/// Muxel adopts conversation `sid` from a start record Claude wrote just now:
/// the conversation a `/clear` or `/resume` opened.
fn adopt_started(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, pane: Uuid, sid: &str) {
    window
        .update(cx, |app, _, cx| {
            app.workspace.instance_mut(pane).unwrap().session_id = Some(sid.into());
            app.note_adopted_conversation(pane, Some(std::time::SystemTime::now()), cx);
        })
        .unwrap();
}

/// `status` for `pane`.
fn status_of(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pane: Uuid,
) -> serde_json::Value {
    let answer = window
        .update(cx, |app, _, cx| {
            app.control_command(
                Command::Status {
                    agent: Some(pane.to_string()),
                },
                None,
                cx,
            )
        })
        .unwrap();
    let Answer::Now(Ok(status)) = answer else {
        panic!("status answers at once");
    };
    status
}

/// Run the app until `key`'s waiting send has typed and finished, or was
/// dropped.
fn until_settled(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pane: Uuid,
    key: &str,
) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = once_status(cx, window, pane, key);
        if !matches!(status["state"].as_str(), Some("queued" | "typing")) {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "the send never settled: {status}"
        );
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The wall clock in milliseconds, as pane activity records it.
fn wall_ms() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    i64::try_from(since.as_millis()).unwrap()
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
        let admission = |cx: &mut TestAppContext| {
            window
                .update(cx, |app, _, cx| app.control_send_admission(pane, false, cx))
                .unwrap()
        };
        // Wait for the shell's prompt: nothing refuses a send there.
        let deadline = Instant::now() + Duration::from_secs(20);
        while admission(cx).refusal.is_some() {
            assert!(
                Instant::now() < deadline,
                "the shell never took a send: {:?}",
                admission(cx).refusal
            );
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        session.write_input(b"\x1b[A");
        assert_eq!(admission(cx).refusal, None, "an arrow key isn't a draft");
        session.write_input(b"half typed");
        let drafted = admission(cx);
        assert_eq!(drafted.refusal, Some(SendRefusal::Draft));
        let refused = check_send_state(drafted).unwrap_err();
        assert_eq!(refused.code, Some(ErrorCode::DraftPresent));
        assert_eq!(refused.pasted, None);
        assert!(
            check_send_state(SendAdmission {
                force: true,
                ..drafted
            })
            .is_ok()
        );
        session.kill();
    });
}

/// Claude's screen as a send finds it: its idle title, and its input box
/// between two rules with the cursor in it, after whatever `draft` holds.
fn claude_box(session: &TerminalSession, draft: &str) {
    let rule = "─".repeat(40);
    let column = 3 + draft.chars().count();
    session.process_probe_output(
        format!(
            "\x1b]0;✳ Claude Code\x07\x1b[?2004h\x1b[?25h\x1b[2J\x1b[H\
             {rule}\r\n❯ {draft}\r\n{rule}\r\n  ? for shortcuts\x1b[2;{column}H"
        )
        .as_bytes(),
    );
}

/// The reply to a send into a Claude pane behind a fake tmux that takes the
/// prompt and its Enter, while Claude's box shows `after` once anything is
/// typed and never reports a turn.
fn claude_send_reply(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    name: &str,
    prompt: &str,
    after: &str,
) -> serde_json::Value {
    let (pane, session, typed) = tmux_pane_reading(cx, window, name, Some("claude.exe"));
    // Drawn again on every pass, so the console's own startup output can't
    // land on top of it.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        claude_box(&session, "");
        let refusal = window
            .update(cx, |app, _, cx| app.control_send_admission(pane, false, cx))
            .unwrap()
            .refusal;
        if refusal.is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Claude never took a send: {refusal:?}"
        );
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    let (answer, _) = window
        .update(cx, |app, _, cx| {
            claude_box(&session, "");
            ask(app, send(pane, prompt, None, false, false), cx)
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let response = loop {
        if typed.load(Ordering::Acquire) {
            claude_box(&session, after);
        }
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        if let Ok(response) = answer.try_recv() {
            break response;
        }
        assert!(Instant::now() < deadline, "no reply");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(typed.load(Ordering::Acquire), "nothing was typed");
    session.kill();
    response.into_result().unwrap()
}

/// A Claude that drops a send's Enter, as a fresh one does, keeps the prompt
/// in its box. The send can't confirm a turn, and says the prompt is waiting
/// there unsent rather than only that it's unconfirmed. A box that empties
/// proves nothing either way, so that send is only unconfirmed.
#[test]
fn a_claude_send_whose_prompt_stays_in_its_box_is_pending_in_the_composer() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let prompt = "reply with the word violet";
        let kept = claude_send_reply(cx, window, "control-claude-kept", prompt, prompt);
        assert_eq!(kept["outcome"], "pending_in_composer", "{kept}");
        assert_eq!(kept["submission_confirmed"], false, "{kept}");
        let emptied = claude_send_reply(cx, window, "control-claude-emptied", prompt, "");
        assert_eq!(emptied["outcome"], "unconfirmed", "{emptied}");
        assert_eq!(emptied["submission_confirmed"], false, "{emptied}");
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

/// `status` and `panes` report what the sidebar shows a person: a finish
/// nobody has looked at is unread until someone looks at the pane, the tab
/// pin, and when the pane last worked, under its own name and its older one.
#[test]
fn status_reports_the_unread_finish_the_pin_and_the_last_work() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, view, session) = shell_pane(cx, window, "control-status-facts");
        // A look counts once the pane shows something, and the shell's
        // startup output reads as work until it goes quiet.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !view.read_with(cx, |view, _| {
            view.has_visible_content() && view.runtime_snapshot().status == AgentStatus::Idle
        }) {
            assert!(Instant::now() < deadline, "the shell never settled");
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = |app: &mut MuxelApp, cx: &mut Context<MuxelApp>| {
            let command = Command::Status {
                agent: Some(pane.to_string()),
            };
            let Answer::Now(Ok(status)) = app.control_command(command, None, cx) else {
                panic!("a read answers at once");
            };
            status
        };
        let done = wall_ms() - 60_000;
        window
            .update(cx, |app, window, cx| {
                // No work recorded in the pane yet.
                app.workspace.instance_mut(pane).unwrap().activity = AgentActivity::default();
                let fresh = status(app, cx);
                assert_eq!(fresh["unread"], false, "{fresh}");
                assert_eq!(fresh["pinned"], false, "{fresh}");
                assert!(fresh.get("last_work_at").is_none(), "{fresh}");

                // A turn finished a minute ago and nobody has looked. The
                // program reports idle now, which is `status`; the finish it
                // recorded is what the sidebar marks, which is `unread`.
                app.workspace.instance_mut(pane).unwrap().activity = AgentActivity {
                    work_started_at: Some(done - 1_000),
                    completed_at: Some(done),
                    last_state: Some(AgentActivityState::Done),
                    ..Default::default()
                };
                let finished = status(app, cx);
                assert_eq!(finished["status"], "idle", "{finished}");
                assert_eq!(finished["unread"], true, "{finished}");
                assert_eq!(finished["last_work_at"], done, "{finished}");
                assert_eq!(finished["status_since"], done, "{finished}");
                let listed = listed_in_panes(app, pane, cx);
                assert_eq!(listed["unread"], true, "{listed}");
                assert_eq!(listed["last_work_at"], done, "{listed}");

                // The user looks at it: read, and the finish keeps its time.
                app.select_instance(pane, window, cx);
                let seen = status(app, cx);
                assert_eq!(seen["unread"], false, "{seen}");
                assert_eq!(seen["last_work_at"], done, "{seen}");

                // Pinning the tab shows; unpinning clears it.
                app.toggle_pin(pane, window, cx);
                assert_eq!(status(app, cx)["pinned"], true);
                assert_eq!(listed_in_panes(app, pane, cx)["pinned"], true);
                app.toggle_pin(pane, window, cx);
                assert_eq!(status(app, cx)["pinned"], false);
            })
            .unwrap();
        session.kill();
    });
}

/// `pane`'s entry in a `panes` reply.
fn listed_in_panes(
    app: &mut MuxelApp,
    pane: Uuid,
    cx: &mut Context<MuxelApp>,
) -> serde_json::Value {
    let Answer::Now(Ok(panes)) = app.control_command(Command::Panes { project: None }, None, cx)
    else {
        panic!("a read answers at once");
    };
    panes["projects"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|project| project["agents"].as_array().unwrap())
        .find(|agent| agent["uuid"] == pane.to_string())
        .expect("panes lists the pane")
        .clone()
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
                app.note_adopted_conversation(pane, None, cx);
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

/// Wire rings a doorbell and goes. muxel answers `queued` at once, keeps the
/// send although nobody waits on the connection, holds it behind a draft,
/// and types it once the draft is submitted and the pane is idle again. Its
/// key reports each step.
#[test]
fn a_detached_send_answers_at_once_and_types_after_the_draft() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-detach");
        session.write_input(b"half typed");
        let asked_at = super::now_ms();
        let doorbell = || detached(send(pane, "echo rung", Some("d-20"), true, false));
        let (answer, cancelled) = window
            .update(cx, |app, _, cx| ask(app, doorbell(), cx))
            .unwrap();
        cx.run_until_parked();
        let queued = answer
            .try_recv()
            .expect("a detached send answers before it types")
            .into_result()
            .unwrap();
        assert_eq!(queued["queued"], true);
        assert_eq!(queued["once"], "d-20");
        assert_eq!(queued["uuid"], pane.to_string());
        // A day from now, by default.
        let expires_at = queued["expires_at"].as_i64().unwrap();
        let day = i64::try_from(ctl::DETACHED_TIMEOUT_SECS * 1000).unwrap();
        assert!((asked_at + day..asked_at + day + 60_000).contains(&expires_at));
        // The caller hangs up: the connection marks that, and the reply
        // channel goes with it.
        cancelled.store(true, Ordering::Release);
        drop(answer);
        let status = once_status(cx, window, pane, "d-20");
        assert_eq!(status["state"], "queued", "{status}");
        // A fresh shell can still read as working; the draft holds it after.
        let waiting_on = status["waiting_on"].as_str().unwrap_or_default();
        assert!(
            ["working", "draft_present"].contains(&waiting_on),
            "{status}"
        );
        assert_eq!(status["expires_at"], expires_at);
        // A retry of a lost reply gets the same one.
        let (retry, _) = window
            .update(cx, |app, _, cx| ask(app, doorbell(), cx))
            .unwrap();
        let retry = reply(cx, &retry).into_result().unwrap();
        assert_eq!(retry["queued"], true);
        assert_eq!(retry["duplicate"], true);
        // The person submits the draft; once the pane is idle the send types.
        submit_draft(&session);
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            let status = once_status(cx, window, pane, "d-20");
            if status["state"] != "queued" {
                break status;
            }
            assert!(Instant::now() < deadline, "the detached send never typed");
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        };
        let status = if status["state"] == "typing" {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                let status = once_status(cx, window, pane, "d-20");
                if status["state"] != "typing" {
                    break status;
                }
                assert!(
                    Instant::now() < deadline,
                    "the detached send never finished"
                );
                cx.executor().advance_clock(Duration::from_millis(100));
                cx.run_until_parked();
                std::thread::sleep(Duration::from_millis(10));
            }
        } else {
            status
        };
        assert_eq!(status["state"], "done", "{status}");
        assert_eq!(status["result"]["ok"], true);
        assert_eq!(status["result"]["result"]["sent"], true);
        session.kill();
    });
}

/// A detached doorbell rings one conversation. When the pane moves to
/// another, the doorbell is dropped having typed nothing, its key says why,
/// and the key can ring the new conversation.
#[test]
fn a_detached_send_is_dropped_when_its_conversation_changes() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-detach-conversation");
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        session.write_input(b"half typed");
        let before = session.input_rev();
        let doorbell = |sid| {
            pinned(
                detached(send(pane, "echo rung", Some("d-21"), true, false)),
                sid,
            )
        };
        let (queued, _) = window
            .update(cx, |app, _, cx| ask(app, doorbell(CONVERSATION_A), cx))
            .unwrap();
        assert_eq!(reply(cx, &queued).into_result().unwrap()["queued"], true);
        adopt_conversation(cx, window, pane, CONVERSATION_B);
        window
            .update(cx, |app, _, cx| app.pump_control_queue(cx))
            .unwrap();
        let status = once_status(cx, window, pane, "d-21");
        assert_eq!(status["state"], "dropped", "{status}");
        assert_eq!(status["result"]["code"], "launch_mismatch");
        assert_eq!(status["result"]["reason"], "conversation_changed");
        assert_eq!(session.input_rev(), before, "A's doorbell typed into B");
        // Nothing typed, so the key rings again.
        let (again, _) = window
            .update(cx, |app, _, cx| ask(app, doorbell(CONVERSATION_B), cx))
            .unwrap();
        let again = reply(cx, &again).into_result().unwrap();
        assert_eq!(again["queued"], true);
        assert!(again.get("duplicate").is_none(), "{again}");
        session.kill();
    });
}

/// The draft a doorbell waits behind is `/clear`, and the person submits it.
/// The pane reads idle with no draft at once, and `/clear` starts no turn,
/// but muxel hears of the new conversation only a moment later. The
/// doorbell waits for a turn to show, so muxel's next reading drops it
/// instead of letting it type into the new conversation.
#[test]
fn a_waiting_send_behind_a_submitted_clear_types_nothing_into_the_new_conversation() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-detach-clear");
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        session.write_input(b"/clear");
        let doorbell = pinned(
            detached(send(pane, "echo rung", Some("d-24"), true, false)),
            CONVERSATION_A,
        );
        let (queued, _) = window
            .update(cx, |app, _, cx| ask(app, doorbell, cx))
            .unwrap();
        assert_eq!(reply(cx, &queued).into_result().unwrap()["queued"], true);
        // One update: the Enter, then the queue, before any status tick.
        let (submitted, waiting) = window
            .update(cx, |app, _, cx| {
                submit_draft(&session);
                let submitted = session.input_rev();
                app.pump_control_queue(cx);
                (submitted, app.once_status(pane, "d-24"))
            })
            .unwrap();
        assert_eq!(waiting["state"], "queued", "{waiting}");
        assert_eq!(waiting["waiting_on"], "just_submitted");
        assert_eq!(session.input_rev(), submitted, "the doorbell typed");
        // muxel adopts the conversation /clear started.
        adopt_conversation(cx, window, pane, CONVERSATION_B);
        window
            .update(cx, |app, _, cx| app.pump_control_queue(cx))
            .unwrap();
        let status = once_status(cx, window, pane, "d-24");
        assert_eq!(status["state"], "dropped", "{status}");
        assert_eq!(status["result"]["reason"], "conversation_changed");
        assert_eq!(session.input_rev(), submitted, "A's doorbell typed into B");
        session.kill();
    });
}

/// Claude records the end of a pane's conversation before its `/clear`
/// finishes, and muxel hears of the next conversation a moment later. A send
/// pinned to the old one types nothing in that moment, and `status` says the
/// pane's conversation ended, why and when, rather than naming one. Once muxel
/// adopts the next one, a send pinned to it types.
#[test]
fn a_pinned_send_types_nothing_once_claude_has_ended_its_conversation() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, session) = claude_pane(cx, window, "control-claude-clear");
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        assert_eq!(status_of(cx, window, pane)["session_id"], CONVERSATION_A);
        // An idle `/clear`: A's end is on disk; muxel hasn't adopted B.
        claude_ends(pane, CONVERSATION_A, "clear");
        let before = session.input_rev();
        let (refused, _) = window
            .update(cx, |app, _, cx| {
                ask(
                    app,
                    pinned(
                        send(pane, "echo pinned", Some("c-1"), false, true),
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
        assert!(
            refused.message.contains("ended conversation"),
            "{}",
            refused.message
        );
        assert_eq!(
            session.input_rev(),
            before,
            "A's prompt typed into the conversation /clear started"
        );
        let key = window
            .update(cx, |app, _, _| app.control_once.status(pane, "c-1"))
            .unwrap();
        assert_eq!(key["state"], "unused");
        let status = status_of(cx, window, pane);
        assert!(status.get("session_id").is_none(), "{status}");
        assert_eq!(status["conversation_ended"]["session_id"], CONVERSATION_A);
        assert_eq!(status["conversation_ended"]["reason"], "clear");
        assert!(
            status["conversation_ended"]["at"].as_i64().unwrap() > wall_ms() - 60_000,
            "{status}"
        );
        // Muxel adopts the conversation `/clear` started.
        adopt_started(cx, window, pane, CONVERSATION_B);
        let status = status_of(cx, window, pane);
        assert_eq!(status["session_id"], CONVERSATION_B);
        assert!(status.get("conversation_ended").is_none(), "{status}");
        let (sent, _) = window
            .update(cx, |app, _, cx| {
                ask(
                    app,
                    pinned(
                        send(pane, "echo pinned", Some("c-2"), false, true),
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

/// A `/clear` queued behind a busy turn, with a prompt queued ahead of it.
/// The prompt's turn shows after the `/clear`'s Enter, so a turn having shown
/// says nothing about whether the conversation survived. On Claude Code
/// 2.1.322 the pane reads working through that turn and the gap before the
/// `/clear` runs, which holds a waiting send; this test just doesn't run the
/// queue then. Claude records A's end before the `/clear` finishes, so when
/// the pane is next idle, before muxel adopts B, the send reads it and waits,
/// and muxel adopting B drops it.
#[test]
fn a_waiting_send_types_nothing_into_the_conversation_a_queued_clear_starts() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, session) = claude_pane(cx, window, "control-claude-queued-clear");
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        session.write_input(b"fix the tests");
        let doorbell = pinned(
            detached(send(pane, "echo rung", Some("c-3"), true, false)),
            CONVERSATION_A,
        );
        let (queued, _) = window
            .update(cx, |app, _, cx| ask(app, doorbell, cx))
            .unwrap();
        assert_eq!(reply(cx, &queued).into_result().unwrap()["queued"], true);
        let typed = window
            .update(cx, |app, _, cx| {
                // The prompt, then `/clear`, both queued behind the busy turn.
                submit_draft(&session);
                session.write_input(b"/clear");
                submit_draft(&session);
                // The prompt's turn starts after the `/clear`'s Enter.
                app.workspace
                    .instance_mut(pane)
                    .unwrap()
                    .activity
                    .work_started_at = Some(wall_ms() + 1);
                // The `/clear` runs: Claude ends A and the pane is idle.
                claude_ends(pane, CONVERSATION_A, "clear");
                let typed = session.input_rev();
                app.pump_control_queue(cx);
                typed
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let status = once_status(cx, window, pane, "c-3");
            assert_eq!(status["state"], "queued", "{status}");
            assert_eq!(
                session.input_rev(),
                typed,
                "A's doorbell typed into the conversation /clear started"
            );
            if status["waiting_on"] == "conversation_unconfirmed" {
                break;
            }
            assert!(Instant::now() < deadline, "{status}");
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        adopt_started(cx, window, pane, CONVERSATION_B);
        window
            .update(cx, |app, _, cx| app.pump_control_queue(cx))
            .unwrap();
        let status = once_status(cx, window, pane, "c-3");
        assert_eq!(status["state"], "dropped", "{status}");
        assert_eq!(status["result"]["reason"], "conversation_changed");
        assert_eq!(session.input_rev(), typed, "A's doorbell typed into B");
        session.kill();
    });
}

/// `/model` keeps the conversation and starts no turn, so nothing shows
/// after its Enter, and Claude writes no end record. A send pinned to the
/// conversation waits a moment and types; an ordinary prompt shows its turn,
/// and the send types once the pane is idle.
#[test]
fn a_waiting_send_pinned_to_a_claude_conversation_goes_after_model_and_a_prompt() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, session) = claude_pane(cx, window, "control-claude-model");
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        for (key, draft, turn) in [("c-5", "/model", false), ("c-6", "hello", true)] {
            session.write_input(draft.as_bytes());
            let doorbell = pinned(
                detached(send(pane, "echo rung", Some(key), true, false)),
                CONVERSATION_A,
            );
            let (queued, _) = window
                .update(cx, |app, _, cx| ask(app, doorbell, cx))
                .unwrap();
            assert_eq!(reply(cx, &queued).into_result().unwrap()["queued"], true);
            window
                .update(cx, |app, _, _| {
                    submit_draft(&session);
                    if turn {
                        app.workspace
                            .instance_mut(pane)
                            .unwrap()
                            .activity
                            .work_started_at = Some(wall_ms() + 1);
                    }
                })
                .unwrap();
            let status = until_settled(cx, window, pane, key);
            assert_eq!(status["state"], "done", "{draft}: {status}");
            assert_eq!(status["result"]["result"]["sent"], true, "{draft}");
        }
        session.kill();
    });
}

/// `/resume` ends the pane's conversation before it opens the chosen one, as
/// `/clear` does. A send pinned to the one it left types nothing; once muxel
/// adopts the resumed one, a send pinned to that types. Resuming back into
/// the first conversation, whose end record is still on disk, binds it after
/// that end, so a send pinned to it types again.
#[test]
fn a_pinned_send_follows_resume_away_from_its_conversation_and_back() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, session) = claude_pane(cx, window, "control-claude-resume");
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        let send_to = |cx: &mut TestAppContext, sid: &str| {
            let (answer, _) = window
                .update(cx, |app, _, cx| {
                    ask(
                        app,
                        pinned(send(pane, "echo pinned", None, false, true), sid),
                        cx,
                    )
                })
                .unwrap();
            reply(cx, &answer).into_result()
        };
        claude_ends(pane, CONVERSATION_A, "resume");
        let before = session.input_rev();
        let refused = send_to(cx, CONVERSATION_A).unwrap_err();
        assert_eq!(refused.reason.as_deref(), Some("conversation_unconfirmed"));
        assert_eq!(
            session.input_rev(),
            before,
            "A's prompt typed after /resume"
        );
        adopt_started(cx, window, pane, CONVERSATION_B);
        assert_eq!(send_to(cx, CONVERSATION_B).unwrap()["sent"], true);
        // And back into A.
        claude_ends(pane, CONVERSATION_B, "resume");
        let refused = send_to(cx, CONVERSATION_B).unwrap_err();
        assert_eq!(refused.reason.as_deref(), Some("conversation_unconfirmed"));
        adopt_started(cx, window, pane, CONVERSATION_A);
        assert_eq!(send_to(cx, CONVERSATION_A).unwrap()["sent"], true);
        session.kill();
    });
}

/// A person's `/clear` can land between a send's paste and its Enter. muxel
/// reads the end record again before the Enter and withholds it, saying the
/// text may still be in the prompt.
#[test]
fn an_end_between_the_paste_and_the_enter_withholds_the_enter() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, session) = claude_pane(cx, window, "control-claude-enter");
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        let before = session.input_rev();
        let (answer, _) = window
            .update(cx, |app, _, cx| {
                ask(
                    app,
                    pinned(send(pane, "echo pinned", None, false, true), CONVERSATION_A),
                    cx,
                )
            })
            .unwrap();
        // The paste goes in; the Enter waits out its delay on the fake clock.
        let deadline = Instant::now() + Duration::from_secs(20);
        while session.input_rev() == before {
            assert!(Instant::now() < deadline, "the send never pasted");
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        claude_ends(pane, CONVERSATION_A, "clear");
        let refused = reply(cx, &answer).into_result().unwrap_err();
        assert_eq!(refused.code, Some(ErrorCode::LaunchMismatch));
        assert_eq!(refused.reason.as_deref(), Some("conversation_unconfirmed"));
        assert_eq!(refused.pasted, Some(true));
        assert!(
            refused.message.contains("Enter was withheld"),
            "{}",
            refused.message
        );
        session.kill();
    });
}

/// Wire's hook withdraws a doorbell whose mail it has already delivered. A
/// withdrawn send never types, and a cancel with nothing to withdraw is an
/// answer, not an error, so the hook can fire it without asking first.
#[test]
fn once_cancel_withdraws_a_waiting_detached_send() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-detach-cancel");
        session.write_input(b"half typed");
        let (queued, _) = window
            .update(cx, |app, _, cx| {
                ask(
                    app,
                    detached(send(pane, "echo rung", Some("d-22"), true, false)),
                    cx,
                )
            })
            .unwrap();
        assert_eq!(reply(cx, &queued).into_result().unwrap()["queued"], true);
        let cancel = |cx: &mut TestAppContext, key: &str| once_cancel(cx, window, pane, key);
        let withdrawn = cancel(cx, "d-22");
        assert_eq!(withdrawn["cancelled"], true, "{withdrawn}");
        assert_eq!(withdrawn["state"], "dropped");
        assert_eq!(withdrawn["result"]["code"], "cancelled");
        window
            .update(cx, |app, _, _| assert!(app.control_queue.is_empty()))
            .unwrap();
        let again = cancel(cx, "d-22");
        assert_eq!(again["cancelled"], false);
        assert_eq!(again["state"], "dropped");
        let unused = cancel(cx, "d-none");
        assert_eq!(unused["cancelled"], false);
        assert_eq!(unused["state"], "unused");
        // The pane frees up, and nothing types.
        submit_draft(&session);
        let submitted = session.input_rev();
        for _ in 0..30 {
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
        }
        assert_eq!(session.input_rev(), submitted, "a withdrawn send typed");
        session.kill();
    });
}

/// The eight-per-muxel cap keeps connections for interactive calls; a
/// detached send holds none, so a full queue of attached sends doesn't
/// refuse it.
#[test]
fn detached_sends_dont_count_toward_the_waiting_cap() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-detach-cap");
        session.write_input(b"half typed");
        let answers = window
            .update(cx, |app, _, cx| {
                for _ in 0..super::safe_send::MAX_QUEUED_SENDS {
                    app.queue_attached_send_for_test(Uuid::new_v4());
                }
                let attached = ask(app, send(pane, "echo attached", None, true, false), cx);
                let detached = ask(
                    app,
                    detached(send(pane, "echo rung", Some("d-23"), true, false)),
                    cx,
                );
                (attached.0, detached.0)
            })
            .unwrap();
        let full = reply(cx, &answers.0).into_result().unwrap_err();
        assert_eq!(full.reason.as_deref(), Some("queue_full"));
        let queued = reply(cx, &answers.1).into_result().unwrap();
        assert_eq!(queued["queued"], true, "{queued}");
        session.kill();
    });
}

/// A pinned doorbell waits behind a draft when muxel restarts. It is on disk
/// before its caller hears `queued`, comes back still waiting with its key,
/// answers a repeat without typing, and types once the draft clears. Its
/// finish is on disk too: the next restart finds it done.
#[test]
fn a_restart_carries_a_pinned_detached_send_on_and_it_types_once() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-detach-restart");
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        session.write_input(b"half typed");
        let doorbell = || {
            pinned(
                detached(send(pane, "echo rung", Some("d-30"), true, false)),
                CONVERSATION_A,
            )
        };
        let (queued, _) = window
            .update(cx, |app, _, cx| ask(app, doorbell(), cx))
            .unwrap();
        let queued = reply(cx, &queued).into_result().unwrap();
        assert!(queued["boot_id"].is_string(), "{queued}");
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(journal_path(cx, window)).unwrap()).unwrap();
        let kept = &on_disk["keys"][0];
        assert_eq!(kept["key"], "d-30", "{on_disk}");
        assert_eq!(kept["state"], "queued");
        assert_eq!(kept["send"]["text"], "echo rung");
        assert_eq!(kept["send"]["session_id"], CONVERSATION_A);
        restart(cx, window);
        let status = once_status(cx, window, pane, "d-30");
        assert_eq!(status["state"], "queued", "{status}");
        assert_eq!(status["restored"], true);
        assert_eq!(status["expires_at"], queued["expires_at"]);
        assert_eq!(status["boot_id"], queued["boot_id"]);
        let (retry, _) = window
            .update(cx, |app, _, cx| ask(app, doorbell(), cx))
            .unwrap();
        let retry = reply(cx, &retry).into_result().unwrap();
        assert_eq!(retry["queued"], true);
        assert_eq!(retry["duplicate"], true);
        submit_draft(&session);
        let status = until_settled(cx, window, pane, "d-30");
        assert_eq!(status["state"], "done", "{status}");
        assert_eq!(status["result"]["result"]["sent"], true);
        let typed = status["result"].clone();
        // Found as it finished, not as it was at its paste (`interrupted`).
        restart(cx, window);
        let status = once_status(cx, window, pane, "d-30");
        assert_eq!(status["state"], "done", "{status}");
        assert_eq!(status["result"], typed);
        session.kill();
    });
}

/// A detached send types only once it is on disk. One muxel can't record is
/// refused with nothing typed: at its enqueue, its caller is told and its key
/// stays unused; before its paste, it is dropped untyped.
#[test]
fn a_detached_send_muxel_cant_record_types_nothing() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-detach-unrecorded");
        session.write_input(b"half typed");
        let path = journal_path(cx, window);
        // A folder where the record goes: no write can replace it.
        std::fs::create_dir_all(&path).unwrap();
        let doorbell = |key: &str| detached(send(pane, "echo rung", Some(key), true, false));
        let (refused, _) = window
            .update(cx, |app, _, cx| ask(app, doorbell("d-31"), cx))
            .unwrap();
        let refused = reply(cx, &refused).into_result().unwrap_err();
        assert!(refused.message.contains("couldn't record"), "{refused:?}");
        assert_eq!(once_status(cx, window, pane, "d-31")["state"], "unused");
        window
            .update(cx, |app, _, _| assert!(app.control_queue.is_empty()))
            .unwrap();
        // Recorded as it queues, then not before its paste.
        std::fs::remove_dir(&path).unwrap();
        let (queued, _) = window
            .update(cx, |app, _, cx| ask(app, doorbell("d-32"), cx))
            .unwrap();
        assert_eq!(reply(cx, &queued).into_result().unwrap()["queued"], true);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        submit_draft(&session);
        let submitted = session.input_rev();
        let status = until_settled(cx, window, pane, "d-32");
        assert_eq!(status["state"], "dropped", "{status}");
        assert!(
            status["result"]["error"]
                .as_str()
                .is_some_and(|error| error.contains("before typing it")),
            "{status}"
        );
        assert_eq!(
            session.input_rev(),
            submitted,
            "it typed without being recorded"
        );
        std::fs::remove_dir(&path).unwrap();
        session.kill();
    });
}

/// A pinned doorbell to a fresh pane, queued behind a draft, and muxel
/// restarted under it.
fn restored_doorbell(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    name: &str,
    key: &str,
) -> (Uuid, Arc<TerminalSession>) {
    let (pane, _view, session) = shell_pane(cx, window, name);
    adopt_conversation(cx, window, pane, CONVERSATION_A);
    session.write_input(b"half typed");
    let doorbell = pinned(
        detached(send(pane, "echo rung", Some(key), true, false)),
        CONVERSATION_A,
    );
    let (queued, _) = window
        .update(cx, |app, _, cx| ask(app, doorbell, cx))
        .unwrap();
    assert_eq!(reply(cx, &queued).into_result().unwrap()["queued"], true);
    restart(cx, window);
    assert_eq!(once_status(cx, window, pane, key)["restored"], true);
    (pane, session)
}

/// The draft clears, and the pane is given time to take a send: nothing types.
fn nothing_types(cx: &mut TestAppContext, session: &TerminalSession) {
    submit_draft(session);
    let submitted = session.input_rev();
    for _ in 0..30 {
        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
    }
    assert_eq!(session.input_rev(), submitted, "the send typed");
}

/// `once-cancel` withdraws a send muxel restored as it withdraws any other,
/// and the next restart doesn't bring it back.
#[test]
fn once_cancel_withdraws_a_restored_send() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, session) = restored_doorbell(cx, window, "control-restore-cancel", "d-33");
        let withdrawn = once_cancel(cx, window, pane, "d-33");
        assert_eq!(withdrawn["cancelled"], true, "{withdrawn}");
        assert_eq!(withdrawn["result"]["code"], "cancelled");
        // On disk by the time the caller hears it, before anything else runs.
        let on_disk: serde_json::Value =
            serde_json::from_slice(&std::fs::read(journal_path(cx, window)).unwrap()).unwrap();
        assert_eq!(on_disk["keys"][0]["state"], "dropped", "{on_disk}");
        restart(cx, window);
        assert_eq!(once_status(cx, window, pane, "d-33")["state"], "dropped");
        nothing_types(cx, &session);
        session.kill();
    });
}

/// A pane that comes back from a restart in another conversation drops the
/// send pinned to the old one, untyped.
#[test]
fn a_restored_send_is_dropped_when_its_pane_comes_back_in_another_conversation() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, session) = restored_doorbell(cx, window, "control-restore-moved", "d-34");
        adopt_conversation(cx, window, pane, CONVERSATION_B);
        window
            .update(cx, |app, _, cx| app.pump_control_queue(cx))
            .unwrap();
        let moved = once_status(cx, window, pane, "d-34");
        assert_eq!(moved["state"], "dropped", "{moved}");
        assert_eq!(moved["result"]["reason"], "conversation_changed");
        nothing_types(cx, &session);
        session.kill();
    });
}

/// Put `pane` to sleep as sleep leaves it: its program ended, its terminal
/// gone, the pane asleep. Waking it starts its saved program (cmd.exe here)
/// through the restore path.
fn put_to_sleep(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pane: Uuid,
    session: &TerminalSession,
) {
    session.kill();
    window
        .update(cx, |app, _, _| {
            app.terminals.remove(&pane);
            app.workspace.instance_mut(pane).unwrap().asleep_since = Some(wall_ms());
            app.dormant_instances.insert(pane);
            assert_eq!(app.dormant_reason(pane), Some("sleeping"));
        })
        .unwrap();
}

/// The terminal a woken pane runs now.
fn woken_session(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pane: Uuid,
) -> Arc<TerminalSession> {
    window
        .read_with(cx, |app, cx| {
            app.terminal_view(pane)
                .expect("the pane runs again")
                .read(cx)
                .session()
                .clone()
        })
        .unwrap()
}

/// A waiting send wakes an asleep pane and types once it is up, so its caller
/// needn't wake it first. The wake is a machine's, from `ctl send`.
#[test]
fn a_waiting_send_wakes_an_asleep_pane_and_types_once_it_is_up() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-wake-send");
        put_to_sleep(cx, window, pane, &session);
        let (queued, _) = window
            .update(cx, |app, _, cx| {
                ask(
                    app,
                    detached(send(pane, "echo rung", Some("d-40"), true, false)),
                    cx,
                )
            })
            .unwrap();
        assert_eq!(reply(cx, &queued).into_result().unwrap()["queued"], true);
        let status = until_settled(cx, window, pane, "d-40");
        assert_eq!(status["state"], "done", "{status}");
        assert_eq!(status["result"]["result"]["sent"], true);
        let woken = woken_session(cx, window, pane);
        window
            .read_with(cx, |app, _| {
                assert!(!app.is_dormant(pane));
                let activity = &app.workspace.instance(pane).unwrap().activity;
                assert_eq!(activity.woken_by, Some(muxel_core::Waker::Machine));
            })
            .unwrap();
        let log = std::fs::read_to_string(muxel_store::event_log_path().unwrap()).unwrap();
        assert!(
            log.contains(&format!("[{pane}] by ctl send (machine)")),
            "{log}"
        );
        woken.kill();
    });
}

/// At most two waiting sends wake panes at once. A third waits, `waking`,
/// until one of theirs is up, then wakes its own, and all three type.
#[test]
fn at_most_two_waiting_sends_wake_panes_at_once() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let panes: Vec<Uuid> = (0..3)
            .map(|i| {
                let (pane, _view, session) = shell_pane(cx, window, &format!("control-wake-{i}"));
                put_to_sleep(cx, window, pane, &session);
                pane
            })
            .collect();
        let answers = window
            .update(cx, |app, _, cx| {
                let answers: Vec<_> = panes
                    .iter()
                    .map(|&pane| ask(app, send(pane, "echo rung", None, true, false), cx).0)
                    .collect();
                // Nothing has started yet: the wakes run after this update.
                assert_eq!(app.control_wakes.len(), ctl::MAX_SEND_WAKES);
                let held: Vec<&Uuid> = panes
                    .iter()
                    .filter(|pane| !app.control_wakes.contains(pane))
                    .collect();
                assert_eq!(held.len(), 1);
                assert_eq!(app.queued_waiting_on_for_test(*held[0]), Some("waking"));
                answers
            })
            .unwrap();
        for answer in &answers {
            let sent = reply(cx, answer).into_result().unwrap();
            assert_eq!(sent["sent"], true, "{sent}");
        }
        for &pane in &panes {
            woken_session(cx, window, pane).kill();
        }
    });
}

/// `--no-wake` leaves an asleep pane asleep and refuses as a plain send does,
/// and so does a send naming a launch, which no launch after a wake matches.
/// Either key stays unused.
#[test]
fn a_send_that_mustnt_wake_an_asleep_pane_is_refused() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, _view, session) = shell_pane(cx, window, "control-no-wake");
        let launch = session.id;
        put_to_sleep(cx, window, pane, &session);
        let mut no_wake = detached(send(pane, "echo rung", Some("d-41"), true, false));
        if let Command::Send { no_wake: flag, .. } = &mut no_wake {
            *flag = true;
        }
        let mut launch_pinned = detached(send(pane, "echo rung", Some("d-42"), true, false));
        if let Command::Send { launch_id, .. } = &mut launch_pinned {
            *launch_id = Some(launch);
        }
        for (command, key) in [(no_wake, "d-41"), (launch_pinned, "d-42")] {
            let (refused, _) = window
                .update(cx, |app, _, cx| ask(app, command, cx))
                .unwrap();
            let refused = reply(cx, &refused).into_result().unwrap_err();
            assert_eq!(refused.code, Some(ErrorCode::NotRunning), "{refused:?}");
            assert_eq!(refused.reason.as_deref(), Some("sleeping"));
            assert_eq!(once_status(cx, window, pane, key)["state"], "unused");
        }
        window
            .update(cx, |app, _, _| {
                assert!(app.is_dormant(pane));
                assert!(app.control_queue.is_empty());
                assert!(app.control_wakes.is_empty());
            })
            .unwrap();
    });
}

/// A send muxel restored after a restart wakes the pane that came back
/// asleep, and types into it once the conversation it was pinned to checks
/// out.
#[test]
fn a_restored_send_wakes_the_pane_that_came_back_asleep() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let (pane, session) = restored_doorbell(cx, window, "control-restore-asleep", "d-43");
        put_to_sleep(cx, window, pane, &session);
        let deadline = Instant::now() + Duration::from_secs(20);
        while window
            .update(cx, |app, _, cx| {
                app.pump_control_queue(cx);
                app.terminal_view(pane).is_none()
            })
            .unwrap()
        {
            assert!(Instant::now() < deadline, "the pane never woke");
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        // The resumed conversation, as session discovery adopts it.
        adopt_conversation(cx, window, pane, CONVERSATION_A);
        let status = until_settled(cx, window, pane, "d-43");
        assert_eq!(status["state"], "done", "{status}");
        assert_eq!(status["restored"], true);
        woken_session(cx, window, pane).kill();
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
            .update(cx, |app, _, cx| app.refuse_control_queue(cx))
            .unwrap();
        let refused = reply(cx, &waiting).into_result().unwrap_err();
        assert_eq!(refused.code, Some(ErrorCode::Exited));
        assert_eq!(refused.pasted, None);
        session.kill();
    });
}
