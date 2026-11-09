//! A loop whose agent needs Claude's launch preflight starts its terminal
//! through a deferred launch, so for a moment its pane has no terminal. The
//! run must outlast that gap, own exactly the terminal that launch installs,
//! and close its pane after the turn like any other loop, even when the whole
//! turn falls between two loop heartbeats.

use super::codex_capture_tests::with_app;
use super::{MuxelApp, claude_test_home, unix_now};
use gpui::{Task, TestAppContext, WindowHandle};
use muxel_core::{AgentPreset, InjectionMode, Loop, PostRunAction, Project};
use muxel_terminal::{AgentStatus, TerminalSession};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Kills the stand-in child however the test ends.
struct Kill(Arc<TerminalSession>);

impl Drop for Kill {
    fn drop(&mut self) {
        self.0.kill();
    }
}

/// A project with an Exit-policy loop on `preset`; returns the project and
/// the loop's index. Heartbeats run only when a test calls them.
fn exit_loop(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    preset: AgentPreset,
) -> (Uuid, usize) {
    window
        .update(cx, |app, _, _| {
            app._loop_timer = Task::ready(());
            app.notifications_enabled = false;
            let cwd = muxel_store::data_dir().unwrap().join("loop-project");
            std::fs::create_dir_all(&cwd).unwrap();
            let pid = app.workspace.add_project(Project::new("Loops", cwd));
            app.workspace.active_project = Some(pid);
            app.current_workspace = Some(Uuid::new_v4());
            let mut lp = Loop::new("Nightly", pid);
            lp.preset_id = Some(preset.id);
            lp.post_run = PostRunAction::Exit;
            app.presets.push(preset);
            app.loops.push(lp);
            (pid, app.loops.len() - 1)
        })
        .unwrap()
}

/// An Exit loop on the stock Claude preset: a Claude program with a resume
/// flag, so its launch goes through the preflight, and a session flag, so
/// muxel knows the conversation the loop's turn goes to. Claude's home stays
/// a test directory while the returned guard lives.
fn claude_loop(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
) -> (Uuid, usize, impl Drop + use<>) {
    let home = muxel_store::data_dir().unwrap().join("home");
    std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
    let guard = claude_test_home(Some(home));
    let (pid, idx) = exit_loop(cx, window, AgentPreset::claude());
    (pid, idx, guard)
}

/// Fire loop `idx` and return its new pane, whose launch the preflight has
/// deferred. `program` and `args` stand in for Claude.
fn fire(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pid: Uuid,
    idx: usize,
    program: &str,
    args: &[&str],
) -> Uuid {
    window
        .update(cx, |app, window, cx| {
            app.fire_loop(idx, unix_now(), window, cx);
            let pane = app
                .workspace
                .instances
                .iter()
                .find(|i| i.project_id == pid)
                .expect("the loop opened a pane")
                .id;
            assert!(
                !app.terminals.contains_key(&pane) && app.terminal_launching.contains_key(&pane),
                "Claude's preflight should defer this launch"
            );
            let row = app.workspace.instance_mut(pane).unwrap();
            row.program = Some(program.into());
            row.args = args.iter().map(|a| a.to_string()).collect();
            row.injection = InjectionMode::None;
            row.system_prompt = None;
            pane
        })
        .unwrap()
}

/// Run the app until `done` returns something.
fn wait_for<T>(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    what: &str,
    mut done: impl FnMut(&mut MuxelApp, &mut gpui::Context<MuxelApp>) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        cx.executor().advance_clock(Duration::from_millis(250));
        cx.run_until_parked();
        if let Some(found) = window.update(cx, |app, _, cx| done(app, cx)).unwrap() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait for the deferred launch to install pane `pane`'s terminal.
fn installed(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pane: Uuid,
) -> Arc<TerminalSession> {
    wait_for(cx, window, "the deferred launch to install", |app, cx| {
        app.terminals
            .get(&pane)
            .map(|view| view.read(cx).session().clone())
    })
}

/// The stand-in's turn: submitted, Working (Claude's marker on screen),
/// then finished (the marker scrolled away).
fn run_turn(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pane: Uuid,
    session: &TerminalSession,
    between_working_and_idle: impl FnOnce(&mut TestAppContext),
) {
    session.mark_turn_submitted();
    session.write_input(b"esc to interrupt\r");
    wait_for(cx, window, "the pane to work", |app, cx| {
        (app.terminals[&pane].read(cx).status() == AgentStatus::Working).then_some(())
    });
    between_working_and_idle(cx);
    session.write_input(&b"\r".repeat(80));
    wait_for(cx, window, "the turn to finish", |app, cx| {
        matches!(
            app.terminals[&pane].read(cx).status(),
            AgentStatus::Idle | AgentStatus::Done
        )
        .then_some(())
    });
}

fn heartbeat(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) {
    window
        .update(cx, |app, _, cx| app.process_running_loops(cx))
        .unwrap();
}

fn tracked(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>, pane: Uuid) -> bool {
    window
        .update(cx, |app, _, _| app.running_loops.contains_key(&pane))
        .unwrap()
}

#[test]
fn a_claude_loop_keeps_its_run_through_its_deferred_launch_and_closes_after_its_turn() {
    with_app(|cx, window| {
        let (pid, idx, _home) = claude_loop(cx, window);
        // `more` echoes what it's typed and stays up. cmd ignores the session
        // flag muxel puts ahead of these arguments.
        let pane = fire(cx, window, pid, idx, "cmd.exe", &["/d", "/q", "/c", "more"]);

        heartbeat(cx, window);
        assert!(
            tracked(cx, window, pane),
            "the run was dropped while its terminal was still launching"
        );
        window
            .update(cx, |app, window, cx| {
                app.run_loop_now(idx, window, cx);
                let panes = app
                    .workspace
                    .instances
                    .iter()
                    .filter(|i| i.project_id == pid)
                    .count();
                assert_eq!(panes, 1, "a second fire started while the first runs");
            })
            .unwrap();

        let session = installed(cx, window, pane);
        let _kill = Kill(session.clone());
        heartbeat(cx, window);
        assert!(
            tracked(cx, window, pane),
            "the run doesn't own the terminal its deferred launch installed"
        );

        run_turn(cx, window, pane, &session, |cx| {
            heartbeat(cx, window);
            assert!(tracked(cx, window, pane), "the run ended mid-turn");
        });
        heartbeat(cx, window);
        window
            .update(cx, |app, _, _| {
                assert!(
                    app.workspace.instance(pane).is_none(),
                    "an Exit loop left its pane open after its turn"
                );
                assert!(!app.running_loops.contains_key(&pane));
            })
            .unwrap();
    });
}

#[test]
fn a_turn_between_two_heartbeats_closes_its_exit_pane_and_frees_the_next_fire() {
    with_app(|cx, window| {
        let (pid, idx, _home) = claude_loop(cx, window);
        let pane = fire(cx, window, pid, idx, "cmd.exe", &["/d", "/q", "/c", "more"]);
        let session = installed(cx, window, pane);
        let _kill = Kill(session.clone());
        heartbeat(cx, window);
        assert!(tracked(cx, window, pane));

        // The whole turn runs before the next heartbeat, which never sees it
        // Working. The app's status tick runs in between, as it does every
        // second.
        run_turn(cx, window, pane, &session, |_| {});
        window
            .update(cx, |app, window, cx| app.tick(window, cx))
            .unwrap();

        // The next loop tick: its heartbeat, then whatever loop is due.
        window
            .update(cx, |app, window, cx| {
                app.loops[idx].last_run = Some(1);
                app.tick_loops(window, cx);
                assert!(
                    app.workspace.instance(pane).is_none(),
                    "an Exit loop whose turn fell between two heartbeats left its pane open"
                );
                let next = app
                    .workspace
                    .instances
                    .iter()
                    .find(|i| i.project_id == pid)
                    .map(|i| i.id)
                    .expect("the next scheduled fire was refused as still running");
                assert!(app.running_loops.contains_key(&next));
                // Stop the next fire's launch before it starts a real agent.
                app.close_instance_inner(next, "test", cx);
            })
            .unwrap();
    });
}

#[test]
fn a_loop_whose_launch_fails_at_once_does_not_hold_the_loop() {
    with_app(|cx, window| {
        // No preflight: the launch runs at once, fails, and a fallback shell
        // shows the error in the pane.
        let missing = AgentPreset {
            name: "Missing".into(),
            program: Some("muxel-no-such-program.exe".into()),
            ..AgentPreset::shell()
        };
        let (pid, idx) = exit_loop(cx, window, missing);
        let pane = window
            .update(cx, |app, window, cx| {
                app.fire_loop(idx, unix_now(), window, cx);
                let pane = app
                    .workspace
                    .instances
                    .iter()
                    .find(|i| i.project_id == pid)
                    .expect("the loop opened a pane")
                    .id;
                let view = app.terminals.get(&pane).expect("the fallback shell");
                assert!(view.read(cx).launch_error().is_some());
                pane
            })
            .unwrap();
        let _kill = window
            .update(cx, |app, _, cx| {
                Kill(app.terminals[&pane].read(cx).session().clone())
            })
            .unwrap();
        heartbeat(cx, window);
        assert!(
            !tracked(cx, window, pane),
            "a failed launch's fallback shell holds the loop"
        );
    });
}

#[test]
fn a_claude_loop_run_never_adopts_a_terminal_its_own_launch_did_not_install() {
    for case in ["superseded", "failed"] {
        with_app(|cx, window| {
            let (pid, idx, _home) = claude_loop(cx, window);
            let program = match case {
                "failed" => "muxel-no-such-program.exe",
                _ => "cmd.exe",
            };
            let pane = fire(cx, window, pid, idx, program, &["/d", "/q", "/c", "more"]);
            if case == "superseded" {
                // Relaunching the pane replaces the loop's launch with a new one.
                window
                    .update(cx, |app, window, cx| app.spawn_terminal(pane, window, cx))
                    .unwrap();
            }
            wait_for(cx, window, "the launch to finish", |app, _| {
                (app.terminals.contains_key(&pane) || app.failed_launches.contains_key(&pane))
                    .then_some(())
            });
            let _kill = window
                .update(cx, |app, _, cx| {
                    app.terminals
                        .get(&pane)
                        .map(|view| Kill(view.read(cx).session().clone()))
                })
                .unwrap();
            heartbeat(cx, window);
            assert!(
                !tracked(cx, window, pane),
                "{case}: the run outlived its launch or took another one's terminal"
            );
        });
    }
}
