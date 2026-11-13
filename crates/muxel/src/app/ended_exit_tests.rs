//! A program that ends abnormally is reported once: a session as "exited
//! unexpectedly", a command job as its own notice with the exit code.
//! Its exit also leaves the pane Done, but that isn't a finished turn: the feed
//! gets no "finished" entry for it, while a clean exit still gets one. Its
//! sidebar row and dashboard card read "ended" where a clean exit reads "done".

use super::popout_lifecycle_tests::with_app;
use super::recoverable_close_tests::{project, terminal, wait_for_view};
use super::{MuxelApp, NotifKind};
use gpui::{TestAppContext, WindowHandle};
use muxel_terminal::{AgentStatus, TerminalView};
use uuid::Uuid;

/// Two waiting cmd panes, seen alive by a tick and then ended with exit code 2
/// and exit code 0, with nobody watching. Returns (crashed, clean) once a tick
/// has seen both exits.
fn end_two_panes(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> (Uuid, Uuid) {
    let pid = project(cx, window);
    let (crashed, crashed_view, crashed_child) = terminal(cx, window, pid);
    let (clean, clean_view, clean_child) = terminal(cx, window, pid);
    window
        .update(cx, |app, window, cx| {
            // Keep the clean pane, and nobody is watching: each finish would
            // be announced. No desktop popups from a test; the feed still
            // gets its entries.
            app.settings.close_on_exit = false;
            app.notifications_enabled = false;
            app.window_active = false;
            for (iid, name) in [(crashed, "crashed fixture"), (clean, "clean fixture")] {
                app.workspace.instance_mut(iid).unwrap().custom_name = Some(name.into());
            }
            app.tick(window, cx);
        })
        .unwrap();
    crashed_child.terminate(2);
    clean_child.terminate(0);
    wait_for_view(cx, window, &crashed_view, TerminalView::exited);
    wait_for_view(cx, window, &clean_view, TerminalView::exited);
    window
        .update(cx, |app, window, cx| {
            app.window_active = false;
            app.tick(window, cx);
            assert!(app.exit_logged.contains(&crashed) && app.exit_logged.contains(&clean));
        })
        .unwrap();
    (crashed, clean)
}

#[test]
fn an_abnormal_exit_is_reported_once_and_a_clean_exit_still_finishes() {
    with_app(|cx, window| {
        let (crashed, clean) = end_two_panes(cx, window);
        window
            .update(cx, |app, _, _| {
                let entries = |pane| {
                    app.notifications
                        .iter()
                        .filter(move |n| n.instance == Some(pane))
                        .map(|n| n.kind)
                        .collect::<Vec<_>>()
                };
                let crash_reports = app
                    .notifications
                    .iter()
                    .filter(|n| {
                        n.kind == NotifKind::Error
                            && (n.instance == Some(crashed) || n.title.contains("crashed fixture"))
                    })
                    .count();
                assert_eq!(crash_reports, 1, "the abnormal exit is reported once");
                assert!(
                    !entries(crashed).contains(&NotifKind::Done),
                    "an abnormal exit is no finished turn"
                );
                assert_eq!(
                    entries(clean),
                    vec![NotifKind::Done],
                    "a clean exit still finishes"
                );
            })
            .unwrap();
    });
}

#[test]
fn an_abnormal_exit_reads_ended_and_a_clean_exit_done() {
    with_app(|cx, window| {
        let (crashed, clean) = end_two_panes(cx, window);
        window
            .update(cx, |app, _, cx| {
                // Both panes sit at Done. The sidebar row and the dashboard
                // card both take their words from pane_status_label.
                let label = |pane| {
                    let activity = &app.workspace.instance(pane).unwrap().activity;
                    app.pane_status_label(pane, AgentStatus::Done, activity, cx)
                };
                let crashed_label = label(crashed);
                assert!(
                    crashed_label.starts_with("ended"),
                    "an abnormal exit reads ended, got {crashed_label:?}"
                );
                let clean_label = label(clean);
                assert!(
                    clean_label.starts_with("done"),
                    "a clean exit still reads done, got {clean_label:?}"
                );
                // A tmux pane muxel is reconnecting keeps its dead client's view
                // between retries, and its bar says it is reconnecting.
                app.reconnecting.insert(crashed, 1);
                assert!(
                    !app.pane_ended(crashed, cx),
                    "a reconnecting pane isn't ended"
                );
            })
            .unwrap();
    });
}
