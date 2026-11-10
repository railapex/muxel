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
use gpui::{AppContext, TestAppContext, VisualTestContext, WindowHandle};
use muxel_core::control::ErrorCode;
use muxel_terminal::{CommandSpec, TerminalLaunch, TerminalSession, TerminalView};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use uuid::Uuid;

fn ready(app: &mut MuxelApp) {
    app.update_state = UpdateState::Ready(RelaunchPlan {
        program: "muxel-next".into(),
        ..RelaunchPlan::default()
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
/// waited for its Enter never presses Enter, whether it answers before the
/// Restart gives up or after.
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

/// The new version is told which workspace to wait for, and none when no
/// workspace is open.
#[test]
fn a_restart_names_the_workspace_the_new_version_waits_for() {
    with_app(|cx, window| {
        window
            .update(cx, |app, _, _| {
                let plan = RelaunchPlan {
                    program: "muxel-next".into(),
                    ..RelaunchPlan::default()
                };
                assert!(app.restart_plan(&plan).argv().is_empty());
                let id = Uuid::new_v4();
                // Not saved: setting it alone writes nothing.
                app.current_workspace = Some(id);
                let argv = app.restart_plan(&plan).argv();
                app.current_workspace = None;
                assert_eq!(argv, ["--after-restart".to_string(), id.to_string()]);
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
        // It waits for the kill, as the Quit would have.
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

fn started_quietly(_: &RelaunchPlan) -> std::io::Result<()> {
    Ok(())
}

/// A test's own workspace document, removed with it.
struct TestDoc(std::path::PathBuf);

impl TestDoc {
    fn new(name: &str) -> Self {
        Self(std::env::temp_dir().join(format!("muxel-departure-{name}-{}.json", Uuid::new_v4())))
    }

    fn read(&self) -> Option<muxel_core::Workspace> {
        let bytes = std::fs::read(&self.0).ok()?;
        serde_json::from_slice(&bytes).ok()
    }
}

impl Drop for TestDoc {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Change the workspace in a way a save shows: a window placement under `key`.
fn mark(app: &mut MuxelApp, key: Uuid) {
    app.workspace.project_windows.insert(
        key,
        muxel_core::ProjectWindow {
            display: Uuid::nil(),
            geom: None,
        },
    );
}

/// A Restart's save comes after its waits and holds what changed during them;
/// routine saves write nothing meanwhile. Once the new version has started,
/// nothing more is written.
#[test]
fn a_restart_saves_last_and_writes_nothing_once_the_new_version_starts() {
    with_app(|cx, window| {
        let doc = TestDoc::new("saves");
        let (kill, killed) = async_channel::bounded::<()>(1);
        window
            .update(cx, |app, _, cx| {
                ready(app);
                app.relaunch = started_quietly;
                app.test_doc_path = Some(doc.0.clone());
                app.current_workspace = Some(Uuid::new_v4());
                app.close_tmux_kills
                    .push(cx.background_executor().spawn(async move {
                        let _ = killed.recv().await;
                    }));
                app.confirm_exit(ExitKind::Restart, cx);
                assert_eq!(app.departing, Some(Stage::Preparing));
            })
            .unwrap();
        for _ in 0..20 {
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(20));
        }
        let during = Uuid::new_v4();
        window
            .update(cx, |app, _, _| {
                mark(app, during);
                app.persist();
            })
            .unwrap();
        assert!(
            doc.read().is_none(),
            "something saved while the Restart waited"
        );

        kill.try_send(()).unwrap();
        run_until(cx, window, "the Restart never ended", |app| {
            app.restart_ended
        });
        let saved = doc.read().expect("the Restart saved the workspace");
        assert!(
            saved.project_windows.contains_key(&during),
            "the Restart's save missed a change made while it waited"
        );
        let bytes = std::fs::read(&doc.0).unwrap();
        window
            .update(cx, |app, _, _| {
                mark(app, Uuid::new_v4());
                app.persist();
                app.current_workspace = None;
            })
            .unwrap();
        assert_eq!(
            std::fs::read(&doc.0).unwrap(),
            bytes,
            "a save after the new version started"
        );
    });
}

/// A Restart that gives up before its own save, here at its settling wait,
/// writes the saves it held while it prepared.
#[test]
fn a_restart_that_gives_up_writes_the_saves_it_held() {
    with_app(|cx, window| {
        let doc = TestDoc::new("held");
        let during = Uuid::new_v4();
        window
            .update(cx, |app, _, cx| {
                ready(app);
                stuck_launch(app);
                app.relaunch = must_not_start;
                app.test_doc_path = Some(doc.0.clone());
                app.current_workspace = Some(Uuid::new_v4());
                app.confirm_exit(ExitKind::Restart, cx);
                mark(app, during);
                app.persist();
            })
            .unwrap();
        assert!(
            doc.read().is_none(),
            "a save ran while the Restart prepared"
        );
        run_until(cx, window, "the Restart never gave up", |app| {
            app.departing.is_none()
        });
        window
            .update(cx, |app, _, _| app.current_workspace = None)
            .unwrap();
        let saved = doc.read().expect("the held save didn't run");
        assert!(
            saved.project_windows.contains_key(&during),
            "the held save lost a change made while the Restart prepared"
        );
    });
}

/// Past the first-run screens, with nothing else covering the panes.
fn no_overlays(app: &mut MuxelApp, cx: &gpui::App) {
    app.show_terms = false;
    app.show_workspace_selector = false;
    assert!(!app.any_overlay_open(cx), "an overlay is already open");
}

/// Move the fake clock on a second at a time.
fn tick_seconds(cx: &mut TestAppContext, seconds: u64) {
    for _ in 0..seconds {
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
    }
}

/// With every pane idle and no input pending, a Restart counts down instead
/// of asking, its overlay hides the browser panes, and it goes when the
/// countdown ends.
#[test]
fn an_idle_app_counts_down_and_then_goes() {
    with_app(|cx, window| {
        window
            .update(cx, |app, _, cx| {
                ready(app);
                no_overlays(app, cx);
                app.relaunch = started_quietly;
                app.ask_exit(ExitKind::Restart, cx);
                assert!(!app.show_quit_confirm, "an idle app asked");
                assert!(app.exit_countdown.is_some(), "no countdown");
                assert!(app.any_overlay_open(cx), "the countdown isn't an overlay");
                assert_eq!(app.departing, None);
            })
            .unwrap();
        tick_seconds(cx, 4);
        window
            .update(cx, |app, _, _| {
                assert_eq!(app.exit_countdown.as_ref().map(|c| c.left), Some(1));
                assert_eq!(app.departing, None, "it went before the countdown ended");
                // The update check 2 s after launch has run and kept the plan.
                assert!(
                    matches!(app.update_state, UpdateState::Ready(_)),
                    "an update check dropped the downloaded update"
                );
            })
            .unwrap();
        tick_seconds(cx, 1);
        run_until(cx, window, "the Restart never ended", |app| {
            app.restart_ended
        });
        window
            .update(cx, |app, _, _| assert!(app.exit_countdown.is_none()))
            .unwrap();
    });
}

/// Cancel stops the countdown for good.
#[test]
fn a_cancelled_countdown_never_goes() {
    with_app(|cx, window| {
        window
            .update(cx, |app, _, cx| {
                ready(app);
                no_overlays(app, cx);
                app.relaunch = must_not_start;
                app.ask_exit(ExitKind::Restart, cx);
                assert!(app.exit_countdown.is_some(), "no countdown");
                app.cancel_exit_countdown(cx);
            })
            .unwrap();
        tick_seconds(cx, 6);
        window
            .update(cx, |app, _, cx| {
                assert!(app.exit_countdown.is_none());
                assert!(!app.show_quit_confirm);
                assert_eq!(app.departing, None);
                assert!(!app.any_overlay_open(cx));
            })
            .unwrap();
    });
}

/// A pane that would ask when closed, here an unmarked cmd shell, makes the
/// exit ask. So does pending input.
#[test]
fn a_busy_pane_or_pending_input_makes_the_exit_ask() {
    with_app(|cx, window| {
        window
            .update(cx, |app, window, cx| {
                let sweep = app
                    .pending_inputs
                    .begin(PendingKind::WakeSweep, None)
                    .unwrap();
                app.ask_exit(ExitKind::Quit, cx);
                assert!(app.show_quit_confirm && app.exit_countdown.is_none());
                app.show_quit_confirm = false;
                drop(sweep);

                let (_, session) = add_shell(app, None, "exit-asks", window, cx);
                app.ask_exit(ExitKind::Quit, cx);
                assert!(app.show_quit_confirm && app.exit_countdown.is_none());
                app.show_quit_confirm = false;
                session.kill();
            })
            .unwrap();
    });
}

/// Input that turns up during the countdown makes it ask when it ends, as an
/// Undo expiry does, rather than go.
#[test]
fn a_countdown_that_finds_work_at_its_end_asks() {
    with_app(|cx, window| {
        let sweep = window
            .update(cx, |app, _, cx| {
                ready(app);
                app.relaunch = must_not_start;
                app.ask_exit(ExitKind::Restart, cx);
                assert!(app.exit_countdown.is_some(), "no countdown");
                app.pending_inputs
                    .begin(PendingKind::WakeSweep, None)
                    .unwrap()
            })
            .unwrap();
        tick_seconds(cx, 5);
        window
            .update(cx, |app, _, _| {
                assert!(app.exit_countdown.is_none());
                assert!(app.show_quit_confirm, "it didn't ask");
                assert_eq!(app.quit_confirm_kind, ExitKind::Restart);
                assert_eq!(app.departing, None);
                app.show_quit_confirm = false;
            })
            .unwrap();
        drop(sweep);
    });
}

/// A terminal whose preset instructions wait out a long startup delay. It
/// isn't one of the workspace's panes, so only its pending input, not its
/// own close facts, can make an exit ask.
fn waiting_startup(
    app: &mut MuxelApp,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<MuxelApp>,
) -> Arc<TerminalSession> {
    let spec = CommandSpec::program("cmd.exe", vec!["/D".into(), "/Q".into(), "/K".into()])
        .with_startup_input("rem instructions")
        .with_startup_delay(600_000);
    let launch = TerminalLaunch::spawn(spec, (80, 24)).unwrap();
    let pane = Uuid::new_v4();
    let view = cx.new(|cx| TerminalView::new(launch, pane, Instant::now(), window, cx));
    let session = view.read(cx).session().clone();
    app.terminals.insert(pane, view);
    session
}

/// Instructions a preset or runner holds through its startup delay are
/// pending input too: one that turns up during the countdown makes it ask.
#[test]
fn startup_input_still_waiting_makes_the_countdown_ask() {
    with_app(|cx, window| {
        window
            .update(cx, |app, _, cx| {
                ready(app);
                no_overlays(app, cx);
                app.relaunch = must_not_start;
                app.ask_exit(ExitKind::Restart, cx);
                assert!(app.exit_countdown.is_some(), "no countdown");
            })
            .unwrap();
        let session = window.update(cx, waiting_startup).unwrap();
        cx.run_until_parked();
        assert_ne!(session.pending_input_ops(), 0, "nothing is waiting");
        tick_seconds(cx, 5);
        window
            .update(cx, |app, _, _| {
                assert!(app.show_quit_confirm, "it went with startup input waiting");
                assert_eq!(app.departing, None);
                app.show_quit_confirm = false;
            })
            .unwrap();
        session.kill();
    });
}

/// Quit from the workspace selector opened over a loaded workspace, as when
/// switching, leaves the selector and asks for a busy pane. It doesn't quit
/// at once as it does before any workspace is loaded.
#[test]
fn quitting_from_the_selector_over_live_work_asks() {
    with_app(|cx, window| {
        window
            .update(cx, |app, window, cx| {
                let (_, session) = add_shell(app, None, "selector-quit", window, cx);
                // Held, so a Quit that went at once can't end the test app.
                app.close_tmux_kills
                    .push(cx.background_executor().spawn(std::future::pending::<()>()));
                app.current_workspace = Some(Uuid::new_v4());
                app.show_workspace_selector = true;
                app.quit_from_entry_screen(cx);
                assert!(!app.show_workspace_selector, "the selector stayed up");
                assert!(app.show_quit_confirm, "it quit without asking");
                app.show_quit_confirm = false;
                session.kill();
            })
            .unwrap();
    });
}

/// Close the main window as its own close button or Alt+F4 does. A close
/// muxel lets through would end the test app, so it must never be one.
fn close_main_window(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) {
    window
        .update(cx, |app, _, cx| {
            assert!(!app.minimize_to_tray_active());
            // Held, so a Quit that went at once can't end the test app.
            app.close_tmux_kills
                .push(cx.background_executor().spawn(std::future::pending::<()>()));
        })
        .unwrap();
    let closed = VisualTestContext::from_window(window.into(), cx).simulate_close();
    assert!(!closed, "the window closed without asking muxel");
}

/// The window's own close from the selector over live work leaves the
/// selector, which would draw over the question, and asks for a busy pane.
#[test]
fn closing_the_window_from_the_selector_over_live_work_asks() {
    with_app(|cx, window| {
        let session = window
            .update(cx, |app, window, cx| {
                let (_, session) = add_shell(app, None, "selector-close", window, cx);
                app.current_workspace = Some(Uuid::new_v4());
                app.show_workspace_selector = true;
                session
            })
            .unwrap();
        close_main_window(cx, window);
        window
            .update(cx, |app, _, _| {
                assert!(!app.show_workspace_selector, "the selector stayed up");
                assert!(app.show_quit_confirm, "it didn't ask");
                app.show_quit_confirm = false;
                session.kill();
            })
            .unwrap();
    });
}

/// Before any workspace is loaded, the window's own close quits at once, as
/// the selector's Quit does.
#[test]
fn closing_the_window_before_a_workspace_loads_quits_at_once() {
    with_app(|cx, window| {
        window
            .update(cx, |app, _, _| {
                app.current_workspace = None;
                app.show_workspace_selector = true;
            })
            .unwrap();
        close_main_window(cx, window);
        window
            .update(cx, |app, _, _| {
                let quitting =
                    app.confirm_quit && matches!(app.close_departure, Some(CloseDeparture::Quit));
                // Clear the queued Quit before any assertion: even a failing
                // test must never quit the real app.
                app.close_departure = None;
                app.close_departure_task = None;
                assert!(quitting, "it didn't quit at once");
            })
            .unwrap();
    });
}

/// Restart now (what a second Quit does too) goes at once and ends the
/// countdown's timer, so a countdown started after that Restart gave up
/// keeps its full time.
#[test]
fn going_now_ends_the_countdown_and_its_timer() {
    with_app(|cx, window| {
        window
            .update(cx, |app, _, cx| {
                ready(app);
                no_overlays(app, cx);
                app.relaunch = failing;
                app.ask_exit(ExitKind::Restart, cx);
            })
            .unwrap();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        window
            .update(cx, |app, _, cx| {
                app.exit_now(cx);
                assert!(app.exit_countdown.is_none());
                assert_eq!(
                    app.departing,
                    Some(Stage::Preparing),
                    "it didn't go at once"
                );
            })
            .unwrap();
        run_until(cx, window, "the Restart never gave up", |app| {
            app.departing.is_none()
        });
        window
            .update(cx, |app, _, cx| {
                app.ask_exit(ExitKind::Restart, cx);
                assert_eq!(app.exit_countdown.as_ref().map(|c| c.left), Some(5));
            })
            .unwrap();
        // Past the first countdown's next tick, short of the second's.
        cx.executor().advance_clock(Duration::from_millis(600));
        cx.run_until_parked();
        window
            .update(cx, |app, _, cx| {
                assert_eq!(
                    app.exit_countdown.as_ref().map(|c| c.left),
                    Some(5),
                    "the first countdown's timer ticked the second"
                );
                app.cancel_exit_countdown(cx);
            })
            .unwrap();
    });
}

/// A Restart asked for during a Quit's countdown takes it over and keeps its
/// time.
#[test]
fn a_restart_takes_over_a_quits_countdown() {
    with_app(|cx, window| {
        window
            .update(cx, |app, _, cx| {
                ready(app);
                no_overlays(app, cx);
                app.ask_exit(ExitKind::Quit, cx);
                assert_eq!(
                    app.exit_countdown.as_ref().map(|c| c.kind),
                    Some(ExitKind::Quit)
                );
            })
            .unwrap();
        tick_seconds(cx, 2);
        window
            .update(cx, |app, _, cx| {
                app.ask_exit(ExitKind::Restart, cx);
                let countdown = app.exit_countdown.as_ref().expect("the countdown ended");
                assert_eq!(
                    countdown.kind,
                    ExitKind::Restart,
                    "the Restart didn't take over"
                );
                assert_eq!(countdown.left, 3, "taking over reset the countdown");
                // Cancelled before its end: a Quit would end the test app.
                app.cancel_exit_countdown(cx);
            })
            .unwrap();
    });
}
