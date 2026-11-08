//! Typing through `muxel ctl` is the app's until it ends: its pending-input
//! guard outlives whoever asked for it, and quitting or restarting interrupts
//! it at its next write.

use super::super::MuxelApp;
use super::super::popout_lifecycle_tests::with_app;
use super::SUBMIT_DELAY_MS;
use gpui::{AppContext as _, Context, Window};
use muxel_core::control::ErrorCode;
use muxel_core::{Instance, Project};
use muxel_terminal::{CommandSpec, TerminalLaunch, TerminalSession, TerminalView, TmuxInputRoute};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use uuid::Uuid;

fn output(stdout: &str) -> std::process::Output {
    std::process::Output {
        status: <std::process::ExitStatus as std::os::windows::process::ExitStatusExt>::from_raw(0),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

/// A fake tmux whose agent pane is `%7`. Every command answers at once except
/// `send-keys`, which waits until the test lets it through.
fn tmux_route() -> (TmuxInputRoute, Arc<Mutex<Vec<String>>>, mpsc::Sender<()>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let (release, released) = mpsc::channel::<()>();
    let released = Mutex::new(released);
    let route = TmuxInputRoute::new("muxel_p_1", true, move |args: &[String], _| {
        let args: Vec<String> = if args[0] == "if-shell" {
            args.last()
                .unwrap()
                .split_whitespace()
                .map(String::from)
                .collect()
        } else {
            args.to_vec()
        };
        seen.lock().unwrap().push(args[0].clone());
        Ok(match args[0].as_str() {
            "list-panes" => output("muxel_p_1:%7:1::3.4\n"),
            "display-message" => output("%7\n"),
            "send-keys" => {
                let _ = released.lock().unwrap().recv();
                output("")
            }
            _ => output(""),
        })
    });
    (route, calls, release)
}

/// A cmd pane outside control may type into, its input through `tmux` when
/// given.
fn add_shell(
    app: &mut MuxelApp,
    tmux: Option<TmuxInputRoute>,
    name: &str,
    window: &mut Window,
    cx: &mut Context<MuxelApp>,
) -> (Uuid, Arc<TerminalSession>) {
    let root = muxel_store::data_dir().unwrap().join(name);
    std::fs::create_dir_all(&root).unwrap();
    let project = app.workspace.add_project(Project::new("control", root));
    let mut instance = Instance::shell(project);
    instance.program = Some("cmd.exe".into());
    instance.args = vec!["/D".into(), "/Q".into(), "/K".into()];
    let pane = instance.id;
    let mut spec = CommandSpec::program("cmd.exe", instance.args.clone());
    if let Some(route) = tmux {
        spec = spec.with_tmux_input(route);
    }
    app.workspace.add_instance(instance);
    app.settings.control_enabled = true;
    app.settings.control_allow_shells = true;
    let launch = TerminalLaunch::spawn(spec, (80, 24)).unwrap();
    let view = cx.new(|cx| TerminalView::new(launch, pane, Instant::now(), window, cx));
    let session = view.read(cx).session().clone();
    app.terminals.insert(pane, view);
    (pane, session)
}

/// A send through tmux whose requester goes away before tmux takes its Enter
/// still finishes. It stays pending until the Enter is in, recorded in the
/// terminal's facts and as the pane's control turn.
#[test]
fn a_send_outlives_its_requester_and_stays_pending_until_its_enter_is_recorded() {
    let (route, calls, release) = tmux_route();
    with_app(|cx, window| {
        let (pane, session) = window
            .update(cx, |app, window, cx| {
                let (pane, session) = add_shell(app, Some(route), "pending-unawaited", window, cx);
                // A fresh shell can read as working, which only `--force` passes.
                let typing = app
                    .control_send(pane, "first".into(), true, None, cx)
                    .unwrap();
                // Whoever asked stops waiting at once.
                drop(app.control_press(typing, None, None, cx));
                (pane, session)
            })
            .unwrap();
        // The paste goes through tmux; after its delay the Enter waits there.
        let deadline = Instant::now() + Duration::from_secs(15);
        while !calls.lock().unwrap().iter().any(|call| call == "send-keys") {
            assert!(Instant::now() < deadline, "the Enter never reached tmux");
            cx.run_until_parked();
            cx.executor()
                .advance_clock(Duration::from_millis(SUBMIT_DELAY_MS));
            std::thread::sleep(Duration::from_millis(5));
        }
        window
            .update(cx, |app, _, _| {
                assert_eq!(
                    app.pending_inputs.pending().len(),
                    1,
                    "the send stopped pending while tmux still had its Enter"
                );
                assert!(!app.control_turns.contains_key(&pane));
            })
            .unwrap();
        assert_eq!(session.pending_input_ops(), 1);
        assert!(!session.has_submitted_turn());

        release.send(()).unwrap();
        loop {
            cx.run_until_parked();
            let settled = window
                .update(cx, |app, _, _| app.pending_inputs.pending().is_empty())
                .unwrap();
            if settled {
                break;
            }
            assert!(Instant::now() < deadline, "the send never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
        window
            .update(cx, |app, _, _| {
                assert!(
                    app.control_turns.contains_key(&pane),
                    "the send stopped pending before it noted its turn"
                );
            })
            .unwrap();
        assert!(session.has_submitted_turn());
        assert!(session.first_submission_at().is_some());
        assert_eq!(session.pending_input_ops(), 0);
        session.kill();
    });
}

/// Quitting or restarting interrupts a send waiting for its Enter: the Enter
/// isn't pressed, the send says why, and it stops pending. The pane's input
/// isn't closed by it, so a later send goes through.
#[test]
fn an_interrupted_send_never_presses_its_enter_and_later_input_still_works() {
    with_app(|cx, window| {
        let (pane, session, first) = window
            .update(cx, |app, window, cx| {
                let (pane, session) = add_shell(app, None, "pending-interrupted", window, cx);
                let typing = app
                    .control_send(pane, "first".into(), true, None, cx)
                    .unwrap();
                let pressed = app.control_press(typing, None, None, cx);
                let (tx, rx) = mpsc::channel();
                cx.spawn(async move |_, _| {
                    tx.send(pressed.await).unwrap();
                })
                .detach();
                (pane, session, rx)
            })
            .unwrap();
        // Pasted, and waiting out the delay before its Enter.
        cx.run_until_parked();
        let interrupted = window
            .update(cx, |app, _, _| app.pending_inputs.interrupt_all())
            .unwrap();
        assert_eq!(interrupted.len(), 1);
        cx.executor()
            .advance_clock(Duration::from_millis(SUBMIT_DELAY_MS));
        cx.run_until_parked();
        let error = first
            .try_recv()
            .expect("the send answered")
            .expect_err("an interrupted send pressed its Enter");
        assert_eq!(error.code, Some(ErrorCode::InputChanged));
        assert_eq!(error.pasted, Some(true));
        assert!(
            error.message.contains("quitting or restarting"),
            "{error:?}"
        );
        assert!(!session.has_submitted_turn());
        window
            .update(cx, |app, _, _| {
                assert!(app.pending_inputs.pending().is_empty())
            })
            .unwrap();

        let second = window
            .update(cx, |app, _, cx| {
                let typing = app
                    .control_send(pane, "second".into(), true, None, cx)
                    .unwrap();
                let pressed = app.control_press(typing, None, None, cx);
                let (tx, rx) = mpsc::channel();
                cx.spawn(async move |_, _| {
                    tx.send(pressed.await).unwrap();
                })
                .detach();
                rx
            })
            .unwrap();
        cx.run_until_parked();
        cx.executor()
            .advance_clock(Duration::from_millis(SUBMIT_DELAY_MS));
        cx.run_until_parked();
        let sent = second.try_recv().expect("the second send answered");
        assert!(sent.is_ok(), "{sent:?}");
        assert!(session.has_submitted_turn());
        session.kill();
    });
}
