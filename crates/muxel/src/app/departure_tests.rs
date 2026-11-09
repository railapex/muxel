//! A confirmed Restart: what it interrupts stays interrupted, anything it
//! can't settle makes it give up with the app left running and taking input,
//! and only a new version that has started ends this one.

use super::MuxelApp;
use super::UpdateState;
use super::control_api::SUBMIT_DELAY_MS;
use super::control_api::pending_input_tests::{
    add_shell, dying_tmux_route, press_send, tmux_route,
};
use super::departure::{ExitKind, Stage};
use super::pending_input::PendingKind;
use super::popout_lifecycle_tests::with_app;
use super::recoverable_close::CloseDeparture;
use crate::update::RelaunchPlan;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::control::ErrorCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn ready(app: &mut MuxelApp) {
    app.update_state = UpdateState::Ready(RelaunchPlan {
        program: "muxel-next".into(),
        args: vec![],
    });
}

fn failing(_: &RelaunchPlan) -> std::io::Result<()> {
    Err(std::io::Error::other("no such program"))
}

/// A relaunch the test fails if it's ever reached.
fn must_not_start(_: &RelaunchPlan) -> std::io::Result<()> {
    panic!("the Restart started the new version");
}

static STARTED: AtomicUsize = AtomicUsize::new(0);

fn starting(_: &RelaunchPlan) -> std::io::Result<()> {
    STARTED.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// A launch that never finishes, so a Restart's settling wait can't end.
fn stuck_launch(app: &mut MuxelApp) {
    app.terminal_launching.insert(
        Uuid::new_v4(),
        super::LaunchTicket {
            token: 1,
            reason: muxel_core::TerminalLaunchReason::Restore,
            since: Instant::now(),
            startup: None,
        },
    );
}

/// Run the app and its fake clock, a tick at a time, until `done` holds.
fn run_until(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    what: &str,
    done: impl Fn(&MuxelApp) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cx.run_until_parked();
        if window.update(cx, |app, _, _| done(app)).unwrap() {
            return;
        }
        assert!(Instant::now() < deadline, "{what}");
        cx.executor().advance_clock(Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The newest entry in the event feed, as "title: subtitle".
fn last_event(app: &MuxelApp) -> String {
    app.notifications
        .last()
        .map(|event| format!("{}: {}", event.title, event.subtitle))
        .unwrap_or_default()
}

/// Paste a ctl send into `pane` and leave it waiting out the delay before its
/// Enter. Its answer arrives on the returned channel.
fn send_waiting_for_enter(
    app: &mut MuxelApp,
    pane: Uuid,
    text: &str,
    cx: &mut gpui::Context<MuxelApp>,
) -> mpsc::Receiver<Result<serde_json::Value, muxel_core::control::Error>> {
    press_send(app, pane, text, cx).unwrap()
}

/// v8's first case. A Restart whose new version fails to start gives up: every
/// hold goes and the app takes input again. The send it interrupted while it
/// waited for its Enter wakes after that, and still never presses Enter.
#[test]
fn a_restart_that_fails_to_start_leaves_its_interrupted_send_cancelled() {
    with_app(|cx, window| {
        let (pane, session, first) = window
            .update(cx, |app, window, cx| {
                ready(app);
                app.relaunch = failing;
                let (pane, session) = add_shell(app, None, "restart-spawn-fails", window, cx);
                let first = send_waiting_for_enter(app, pane, "first", cx);
                (pane, session, first)
            })
            .unwrap();
        // Pasted, and waiting out the delay before its Enter.
        cx.run_until_parked();
        window
            .update(cx, |app, _, cx| app.confirm_exit(ExitKind::Restart, cx))
            .unwrap();
        run_until(cx, window, "the Restart never gave up", |app| {
            app.departing.is_none()
        });
        let event = window.update(cx, |app, _, _| last_event(app)).unwrap();
        assert!(event.contains("Restart failed"), "{event}");
        assert!(event.contains("no such program"), "{event}");
        assert!(event.contains("won't resume"), "{event}");
        assert!(
            first.try_recv().is_err(),
            "the send answered before the Restart gave up"
        );
        // The holds are gone.
        assert!(session.accepts_input_epoch(session.input_epoch()));

        cx.executor()
            .advance_clock(Duration::from_millis(SUBMIT_DELAY_MS));
        cx.run_until_parked();
        let error = first
            .try_recv()
            .expect("the send answered")
            .expect_err("an interrupted send pressed its Enter");
        assert_eq!(error.code, Some(ErrorCode::InputChanged));
        assert_eq!(error.pasted, Some(true));
        assert!(!session.has_submitted_turn());

        // The app takes new input again.
        let second = window
            .update(cx, |app, _, cx| {
                assert!(app.pending_inputs.pending().is_empty());
                send_waiting_for_enter(app, pane, "second", cx)
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

/// v8's second case: a launch still under way at 5 s makes the Restart give
/// up before it holds anything, and the send it interrupted stays cancelled.
/// While it waits, new launches, ctl input, ctl new and the quit prompt are
/// all refused.
#[test]
fn a_restart_that_cant_settle_gives_up_and_refuses_new_work_meanwhile() {
    with_app(|cx, window| {
        let (pane, session, first) = window
            .update(cx, |app, window, cx| {
                ready(app);
                app._status_timer = gpui::Task::ready(());
                let (pane, session) = add_shell(app, None, "restart-settle", window, cx);
                let first = send_waiting_for_enter(app, pane, "first", cx);
                stuck_launch(app);
                (pane, session, first)
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |app, window, cx| {
                app.confirm_exit(ExitKind::Restart, cx);
                assert_eq!(app.departing, Some(Stage::Preparing));
                let refused = press_send(app, pane, "later", cx)
                    .expect_err("a send was admitted while restarting");
                assert_eq!(refused.code, Some(ErrorCode::Closing));
                let new_pane = muxel_core::Instance::shell(app.workspace.projects[0].id);
                let new_id = new_pane.id;
                app.workspace.add_instance(new_pane);
                assert_eq!(
                    app.reserve_terminal_spawn(
                        new_id,
                        muxel_core::TerminalLaunchReason::NewPane,
                        window,
                        cx
                    ),
                    None
                );
                assert!(
                    app.pending_inputs
                        .begin(PendingKind::Dictation, None)
                        .is_err()
                );
                app.ask_exit(ExitKind::Quit, cx);
                assert!(!app.show_quit_confirm, "asked to quit while restarting");
            })
            .unwrap();
        run_until(cx, window, "the Restart never gave up", |app| {
            app.departing.is_none()
        });
        let event = window.update(cx, |app, _, _| last_event(app)).unwrap();
        assert!(event.contains("still starting"), "{event}");
        assert!(event.contains("won't resume"), "{event}");
        // The send woke during the wait, and its Enter was never pressed.
        let error = first
            .try_recv()
            .expect("the send answered")
            .expect_err("an interrupted send pressed its Enter");
        assert_eq!(error.code, Some(ErrorCode::InputChanged));
        assert!(!session.has_submitted_turn());
        window
            .update(cx, |app, _, _| {
                assert!(
                    app.pending_inputs
                        .begin(PendingKind::Dictation, None)
                        .is_ok()
                );
            })
            .unwrap();
        session.kill();
    });
}

/// The fence has one deadline for all of it. A tmux Enter that tmux hasn't
/// taken by 2 s makes the Restart give up; the Enter, queued before the hold,
/// is still delivered after.
#[test]
fn a_restart_whose_fence_doesnt_answer_in_time_gives_up() {
    let (route, calls, release) = tmux_route();
    with_app(|cx, window| {
        let (session, first) = window
            .update(cx, |app, window, cx| {
                ready(app);
                let (pane, session) = add_shell(app, Some(route), "restart-fence", window, cx);
                let first = send_waiting_for_enter(app, pane, "first", cx);
                (session, first)
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !calls.lock().unwrap().iter().any(|call| call == "send-keys") {
            assert!(Instant::now() < deadline, "the Enter never reached tmux");
            cx.run_until_parked();
            cx.executor()
                .advance_clock(Duration::from_millis(SUBMIT_DELAY_MS));
            std::thread::sleep(Duration::from_millis(5));
        }
        window
            .update(cx, |app, _, cx| app.confirm_exit(ExitKind::Restart, cx))
            .unwrap();
        run_until(cx, window, "the Restart never gave up", |app| {
            app.departing.is_none()
        });
        let event = window.update(cx, |app, _, _| last_event(app)).unwrap();
        assert!(event.contains("within 2 seconds"), "{event}");
        assert!(session.accepts_input_epoch(session.input_epoch()));

        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let sent = loop {
            cx.run_until_parked();
            if let Ok(sent) = first.try_recv() {
                break sent;
            }
            assert!(Instant::now() < deadline, "the send never finished");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(sent.is_ok(), "{sent:?}");
        assert!(session.has_submitted_turn());
        session.kill();
    });
}

/// A fence that can't say whether the writes before it finished counts as a
/// failure, not as settled: tmux's input worker dies holding an Enter, and
/// the Restart gives up rather than start the new version.
#[test]
fn a_restart_whose_fence_cant_confirm_its_writes_gives_up() {
    let (route, calls, release) = dying_tmux_route();
    with_app(|cx, window| {
        let (session, first) = window
            .update(cx, |app, window, cx| {
                ready(app);
                app.relaunch = must_not_start;
                let (pane, session) = add_shell(app, Some(route), "restart-fence-lost", window, cx);
                let first = send_waiting_for_enter(app, pane, "first", cx);
                (session, first)
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !calls.lock().unwrap().iter().any(|call| call == "send-keys") {
            assert!(Instant::now() < deadline, "the Enter never reached tmux");
            cx.run_until_parked();
            cx.executor()
                .advance_clock(Duration::from_millis(SUBMIT_DELAY_MS));
            std::thread::sleep(Duration::from_millis(5));
        }
        window
            .update(cx, |app, _, cx| app.confirm_exit(ExitKind::Restart, cx))
            .unwrap();
        cx.run_until_parked();
        // The worker dies, and the send learns it, before the fence's
        // deadline can pass.
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let sent = loop {
            cx.run_until_parked();
            if let Ok(sent) = first.try_recv() {
                break sent;
            }
            assert!(Instant::now() < deadline, "the send never answered");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(sent.is_err(), "{sent:?}");
        run_until(cx, window, "the Restart never gave up", |app| {
            app.departing.is_none()
        });
        let event = window.update(cx, |app, _, _| last_event(app)).unwrap();
        assert!(event.contains("couldn't be confirmed"), "{event}");
        assert!(session.accepts_input_epoch(session.input_epoch()));
        session.kill();
    });
}

/// An open file dialog refuses a Restart before it touches anything. Once the
/// dialog is gone, the Restart starts.
#[test]
fn an_open_file_dialog_refuses_a_restart_untouched() {
    with_app(|cx, window| {
        window
            .update(cx, |app, _, cx| {
                ready(app);
                let dialog = app.native_dialog(std::future::pending::<()>());
                let sweep = app
                    .pending_inputs
                    .begin(PendingKind::WakeSweep, None)
                    .unwrap();
                app.confirm_exit(ExitKind::Restart, cx);
                assert_eq!(app.departing, None);
                assert!(!sweep.interrupted(), "a refused Restart interrupted input");
                let event = last_event(app);
                assert!(event.contains("Close the file dialog first"), "{event}");

                drop(dialog);
                app.confirm_exit(ExitKind::Restart, cx);
                assert_eq!(app.departing, Some(Stage::Preparing));
                assert!(sweep.interrupted());
            })
            .unwrap();
        // Tests can't start muxel, so it gives up.
        run_until(cx, window, "the Restart never gave up", |app| {
            app.departing.is_none()
        });
    });
}

/// A Quit confirmed while a Restart prepares waits, and runs once the
/// Restart gives up.
#[test]
fn a_quit_confirmed_during_a_restart_runs_when_the_restart_gives_up() {
    with_app(|cx, window| {
        let (_kill, killed) = async_channel::bounded::<()>(1);
        window
            .update(cx, |app, _, cx| {
                ready(app);
                app._status_timer = gpui::Task::ready(());
                stuck_launch(app);
                // A tmux kill still running keeps the Quit from ending the
                // test process.
                app.close_tmux_kills
                    .push(cx.background_executor().spawn(async move {
                        let _ = killed.recv().await;
                    }));
                app.confirm_exit(ExitKind::Restart, cx);
                app.confirm_exit(ExitKind::Quit, cx);
                assert!(!app.confirm_quit, "the Quit ran during the Restart");
            })
            .unwrap();
        run_until(cx, window, "the Restart never gave up", |app| {
            app.departing.is_none()
        });
        window
            .update(cx, |app, _, _| {
                let quitting =
                    app.confirm_quit && matches!(app.close_departure, Some(CloseDeparture::Quit));
                // Clear the queued Quit before any assertion: even a failing
                // test must never quit the real app.
                app.close_departure = None;
                app.close_departure_task = None;
                assert!(quitting, "the Quit didn't run after the Restart gave up");
            })
            .unwrap();
    });
}

/// A confirmed Quit interrupts pending input. A Restart takes over a Quit
/// still waiting for tmux sessions to die, waits for them itself, then starts
/// the new version once and ends.
#[test]
fn a_restart_takes_over_a_waiting_quit_and_starts_the_new_version_once() {
    with_app(|cx, window| {
        let (kill, killed) = async_channel::bounded::<()>(1);
        window
            .update(cx, |app, _, cx| {
                ready(app);
                app.relaunch = starting;
                app.close_tmux_kills
                    .push(cx.background_executor().spawn(async move {
                        let _ = killed.recv().await;
                    }));
                let sweep = app
                    .pending_inputs
                    .begin(PendingKind::WakeSweep, None)
                    .unwrap();
                app.confirm_exit(ExitKind::Quit, cx);
                assert!(sweep.interrupted(), "a confirmed Quit left input running");
                assert!(matches!(app.close_departure, Some(CloseDeparture::Quit)));
                app.confirm_exit(ExitKind::Restart, cx);
                assert!(app.close_departure.is_none() && app.close_departure_task.is_none());
                assert!(!app.confirm_quit);
                assert_eq!(app.departing, Some(Stage::Preparing));
            })
            .unwrap();
        // It saves and then waits for the kill, as the Quit would have.
        for _ in 0..50 {
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(20));
        }
        window
            .update(cx, |app, _, _| {
                assert_eq!(app.departing, Some(Stage::Preparing));
                assert!(!app.restart_ended);
            })
            .unwrap();
        assert_eq!(STARTED.load(Ordering::SeqCst), 0);

        kill.try_send(()).unwrap();
        run_until(cx, window, "the Restart never ended", |app| {
            app.restart_ended
        });
        window
            .update(cx, |app, _, cx| {
                assert_eq!(app.departing, Some(Stage::Committed));
                // Nothing restarts it twice.
                app.confirm_exit(ExitKind::Restart, cx);
                app.confirm_exit(ExitKind::Quit, cx);
                assert!(!app.confirm_quit);
            })
            .unwrap();
        cx.run_until_parked();
        assert_eq!(STARTED.load(Ordering::SeqCst), 1);
    });
}
