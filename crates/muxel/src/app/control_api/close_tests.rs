//! `ctl close` on real panes: all or none, through the countdown with Undo.

use crate::app::MuxelApp;
use crate::app::recoverable_close_tests::{
    project, terminal, title_terminal, wait_for_exit, wait_for_view,
};
use gpui::{TestAppContext, WindowHandle};
use muxel_core::control::{self as ctl, Command};
use muxel_terminal::{AgentStatus, TerminalStatusReason};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// The reply to `close`, asked as the control server would, from `caller`.
fn close(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    agents: &[String],
    force: bool,
    caller: Option<Uuid>,
) -> ctl::Response {
    let (reply, answer) = std::sync::mpsc::channel();
    window
        .update(cx, |app, _, cx| {
            let command = Command::Close {
                agents: agents.to_vec(),
                force,
            };
            let answer = app.control_command(command, caller, cx);
            app.deliver_control_answer(answer, reply, Arc::new(AtomicBool::new(false)), cx);
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        cx.run_until_parked();
        if let Ok(response) = answer.try_recv() {
            return response;
        }
        assert!(Instant::now() < deadline, "no reply");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// An orchestrator retiring builders closes the ones it names, all or none:
/// a busy pane refuses the whole close and nothing is held. `--force` holds
/// every pane in one countdown with Undo, named as the caller's, checking a
/// quiet pane again at its end and a busy one not, and Notifications keep a
/// line for the close once the countdown is gone.
#[test]
fn ctl_close_holds_all_or_none_and_force_takes_the_busy_ones() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let pid = project(cx, window);
        let (quiet, quiet_view, quiet_child) =
            title_terminal(cx, window, pid, "quiet fixture", "Ready");
        let (busy, busy_view, busy_child) =
            title_terminal(cx, window, pid, "busy fixture", "Working");
        wait_for_view(cx, window, &quiet_view, |view| {
            view.runtime_snapshot().reason == TerminalStatusReason::ProviderTitle
                && view.status() == AgentStatus::Idle
        });
        wait_for_view(cx, window, &busy_view, |view| {
            view.runtime_snapshot().reason == TerminalStatusReason::ProviderTitle
                && view.status() == AgentStatus::Working
        });
        let both = [quiet.to_string(), busy.to_string()];

        let refused = close(cx, window, &both, false, None)
            .into_result()
            .unwrap_err();
        assert_eq!(refused.code, Some(ctl::ErrorCode::Working), "{refused:?}");
        assert_eq!(refused.refused.len(), 1, "{refused:?}");
        assert_eq!(refused.refused[0].uuid, Some(busy));
        assert_eq!(refused.refused[0].agent, busy.to_string());
        assert!(
            refused.message.contains("busy fixture"),
            "{}",
            refused.message
        );
        // A name that is no pane refuses too, and says which it was.
        let unknown = close(
            cx,
            window,
            &[quiet.to_string(), "no such pane".into()],
            false,
            None,
        )
        .into_result()
        .unwrap_err();
        assert_eq!(unknown.refused.len(), 1, "{unknown:?}");
        assert_eq!(unknown.refused[0].agent, "no such pane");
        assert_eq!(unknown.refused[0].uuid, None);
        window
            .update(cx, |app, _, _| {
                assert!(!app.is_closing(quiet));
                assert!(!app.is_closing(busy));
                assert!(app.close_groups.is_empty());
            })
            .unwrap();

        let closed = close(cx, window, &both, true, None).into_result().unwrap();
        assert_eq!(closed["closed"], true, "{closed}");
        window
            .update(cx, |app, _, cx| {
                assert!(app.is_closing(quiet));
                assert!(app.is_closing(busy));
                assert!(!app.closing[&quiet].forced);
                assert!(app.closing[&busy].forced);
                assert_eq!(app.close_groups.len(), 1);
                assert_eq!(app.close_groups[0].members, vec![quiet, busy]);
                assert_eq!(app.close_groups[0].caller.as_deref(), Some("muxel ctl"));
                let deadline = app.closing_until(quiet).unwrap();
                assert_eq!(closed["undo_until"], deadline);
                assert_eq!(closed["panes"][0]["uuid"], quiet.to_string());
                assert_eq!(closed["panes"][0]["undo_until"], deadline);
                assert_eq!(closed["panes"][1]["uuid"], busy.to_string());
                // A held pane still reads as what its program is doing.
                let status = app.control_info_of(quiet, 0, cx).unwrap();
                assert_eq!(status.status, Some(ctl::AgentState::Idle));
                let closing = status.closing.expect("a held pane says so");
                assert_eq!(closing.reason, "counting_down");
                assert_eq!(closing.undo_until, Some(deadline));
                let line = app.notifications.last().expect("the close is recorded");
                assert!(
                    line.title
                        .contains("muxel ctl closed quiet fixture, busy fixture"),
                    "{}",
                    line.title
                );
                assert!(
                    line.subtitle.contains("forced: busy fixture"),
                    "{}",
                    line.subtitle
                );
            })
            .unwrap();
        assert!(!quiet_child.exited(), "a held pane keeps its process");
        assert!(!busy_child.exited(), "a held pane keeps its process");

        // Held panes refuse a second close.
        let again = close(cx, window, &both, true, None)
            .into_result()
            .unwrap_err();
        assert_eq!(again.code, Some(ctl::ErrorCode::Closing), "{again:?}");
        assert_eq!(again.refused.len(), 2, "{again:?}");
    });
}

/// An exited pane has nothing left to lose, so it closes at once with no
/// countdown, and its caller is named in the record.
#[test]
fn ctl_close_of_an_exited_pane_closes_it_at_once() {
    super::super::popout_lifecycle_tests::with_app(|cx, window| {
        let pid = project(cx, window);
        let (caller, _, _caller_child) = terminal(cx, window, pid);
        let (gone, view, child) = terminal(cx, window, pid);
        child.terminate(0);
        wait_for_exit(cx, &child);
        wait_for_view(cx, window, &view, |view| view.exited());
        let closed = close(cx, window, &[gone.to_string()], false, Some(caller))
            .into_result()
            .unwrap();
        assert_eq!(closed["undo_until"], serde_json::Value::Null, "{closed}");
        assert_eq!(closed["panes"][0]["undo_until"], serde_json::Value::Null);
        window
            .update(cx, |app, _, _| {
                assert!(app.workspace.instance(gone).is_none());
                assert!(!app.is_closing(gone));
                assert!(app.close_groups.is_empty());
                let line = app.notifications.last().expect("the close is recorded");
                let caller = app.workspace.instance(caller).unwrap().display_name();
                assert!(line.title.starts_with(caller), "{}", line.title);
            })
            .unwrap();
    });
}
