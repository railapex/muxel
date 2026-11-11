#![cfg(windows)]
//! Sleep and wake against real PTY children. Each fixture is `cmd.exe` running
//! a script that prints a real Codex OSC title, so the pane reaches a provider
//! status the way an agent's does; its saved program names a Claude binary that
//! doesn't exist, so nothing here can start a real agent.

use super::conpty_test_support::ChildHandle;
use super::popout_lifecycle_tests::with_app;
use super::sleep::{QUIT_POLL, SleepCause};
use super::{Instance, MuxelApp, Project};
use gpui::{AppContext, Entity, TestAppContext, WindowHandle};
use muxel_core::sleep::{SleepRefusal, SleepRequest};
use muxel_core::{AgentActivity, PaneNode, add_tab};
use muxel_terminal::{
    AgentStatus, CommandSpec, TerminalLaunch, TerminalStatusReason, TerminalView,
};
use std::time::{Duration, Instant};
use uuid::Uuid;

const HOUR_MS: i64 = 60 * 60 * 1000;

fn project(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> Uuid {
    window
        .update(cx, |app, _, _| {
            let root = muxel_store::data_dir()
                .unwrap()
                .join(Uuid::new_v4().to_string());
            std::fs::create_dir_all(&root).unwrap();
            let pid = app.workspace.add_project(Project::new("sleep", root));
            app.workspace.active_project = Some(pid);
            app.current_workspace = Some(Uuid::new_v4());
            app.use_tmux = false;
            app.use_worktree = false;
            app.settings.control_enabled = true;
            pid
        })
        .unwrap()
}

/// A pane running `cmd.exe` that prints `name | state` as a Codex title, then
/// waits on `more`. `agent` saves it as a resumable Claude conversation.
fn pane(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pid: Uuid,
    name: &str,
    agent: bool,
) -> (Uuid, Entity<TerminalView>, ChildHandle) {
    window
        .update(cx, |app, window, cx| {
            let root = app.workspace.project(pid).unwrap().root_path.clone();
            let filename = format!("sleep-{}.cmd", Uuid::new_v4());
            std::fs::write(
                root.join(&filename),
                format!("@echo \x1b]0;{name} ^| Ready\x07\r\n@more\r\n"),
            )
            .unwrap();
            let directory = root.to_string_lossy();
            let directory = directory
                .strip_prefix(r"\\?\")
                .unwrap_or(&directory)
                .to_string();
            let args = ["/d", "/q", "/c"]
                .into_iter()
                .map(str::to_string)
                .chain([format!(r".\{filename}")])
                .collect();
            let mut instance = Instance::shell(pid);
            instance.custom_name = Some(name.into());
            if agent {
                let claude = app
                    .presets
                    .iter()
                    .find(|preset| preset.name == "Claude")
                    .expect("built-in Claude preset")
                    .clone();
                instance.program = Some(
                    root.join("missing")
                        .join("claude.exe")
                        .display()
                        .to_string(),
                );
                instance.preset = claude.name.clone();
                instance.preset_id = Some(claude.id);
                instance.session_id = Some(Uuid::new_v4().to_string());
                instance.session_started = true;
            }
            let iid = instance.id;
            let launch = TerminalLaunch::spawn(
                CommandSpec::program("cmd.exe", args)
                    .with_cwd(directory)
                    .with_status_program(Some("codex".into())),
                (80, 24),
            )
            .unwrap();
            let view = cx.new(|cx| TerminalView::new(launch, iid, Instant::now(), window, cx));
            let child = ChildHandle::new(view.read(cx).session());
            app.workspace.add_instance(instance);
            let project = app.workspace.project_mut(pid).unwrap();
            if let Some(anchor) = project.first_instance() {
                assert!(add_tab(&mut project.layout, anchor, iid));
            } else {
                project.layout = Some(PaneNode::leaf(iid));
            }
            app.terminals.insert(iid, view.clone());
            (iid, view, child)
        })
        .unwrap()
}

fn wait_until(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    step: Duration,
    what: &str,
    condition: impl Fn(&MuxelApp, &gpui::App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        // The PTY reader is real; output parsing and the quit polls run on
        // GPUI's virtual clock. Both must move.
        cx.executor().advance_clock(step);
        cx.run_until_parked();
        if window.read_with(cx, |app, cx| condition(app, cx)).unwrap() {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_ready(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    view: &Entity<TerminalView>,
) {
    let view = view.clone();
    wait_until(
        cx,
        window,
        Duration::from_millis(10),
        "a Ready title",
        move |_, cx| {
            let view = view.read(cx);
            view.runtime_snapshot().reason == TerminalStatusReason::ProviderTitle
                && view.status() == AgentStatus::Idle
        },
    );
}

fn event_log() -> String {
    std::fs::read_to_string(muxel_store::event_log_path().unwrap()).unwrap_or_default()
}

#[test]
fn sleep_ends_an_idle_agent_and_wake_resumes_it_without_moving_focus() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (shell, _, _shell_child) = pane(cx, window, pid, "a shell", false);
        let (agent, agent_view, agent_child) = pane(cx, window, pid, "sleepy agent", true);
        wait_ready(cx, window, &agent_view);
        window
            .update(cx, |app, _, cx| {
                app.active_instance = Some(shell);
                // A shell never offers Sleep; the idle agent does.
                assert_eq!(app.sleep_menu_state(shell, cx), None);
                assert_eq!(app.sleep_menu_state(agent, cx), Some(Ok(())));
                assert!(
                    app.sleep_instance(agent, SleepRequest::ByHand, SleepCause::Pane, cx)
                        .is_ok()
                );
                // Dormant and asleep at once: the placeholder shows, nothing
                // can type into it, and outside control says why.
                assert!(!app.terminals.contains_key(&agent));
                assert!(app.is_dormant(agent));
                assert!(
                    app.workspace
                        .instance(agent)
                        .unwrap()
                        .asleep_since
                        .is_some()
                );
                let info = app.control_info_of(agent, 0, cx).unwrap();
                assert_eq!(info.not_running_reason.as_deref(), Some("sleeping"));
                let refused = app
                    .control_writable(agent, cx)
                    .err()
                    .expect("typing refused");
                assert_eq!(refused.reason.as_deref(), Some("sleeping"));
                assert_eq!(app.active_instance, Some(shell));
            })
            .unwrap();
        wait_until(
            cx,
            window,
            QUIT_POLL,
            "the quit to finish",
            move |app, _| !app.sleep_exits.contains_key(&agent),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while agent_child.running() {
            assert!(
                Instant::now() < deadline,
                "the sleeping agent's process survived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let log = event_log();
        assert!(log.contains(&format!("sleep: \"sleepy agent\" [{agent}] by pane menu")));
        assert!(
            log.contains(&format!("sleep: [{agent}] quit after"))
                || log.contains(&format!("sleep: [{agent}] killed after")),
            "{log}"
        );
        window
            .update(cx, |app, window, cx| {
                let workspace = app.current_workspace;
                let woke = app.control_wake(agent, workspace, window, cx).unwrap();
                assert_eq!(woke["woken"], true);
                assert_eq!(woke["status"], "starting");
                assert_eq!(woke["uuid"], agent.to_string());
                // Started through restore, in the background.
                assert!(!app.is_dormant(agent));
                assert!(app.terminal_launching.contains_key(&agent));
                assert_eq!(app.active_instance, Some(shell));
                // A retry while it starts is harmless.
                let again = app.control_wake(agent, workspace, window, cx).unwrap();
                assert_eq!(again["woken"], false);
                assert_eq!(again["already_running"], true);
                // Never let the deferred launch run its (missing) program.
                app.close_instance(agent, cx);
            })
            .unwrap();
    });
}

#[test]
fn the_idle_timer_sleeps_only_agents_idle_past_the_threshold_and_not_focused() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (old, old_view, _old_child) = pane(cx, window, pid, "old agent", true);
        let (recent, recent_view, _recent_child) = pane(cx, window, pid, "recent agent", true);
        let (focused, focused_view, _focused_child) = pane(cx, window, pid, "focused agent", true);
        for view in [&old_view, &recent_view, &focused_view] {
            wait_ready(cx, window, view);
        }
        window
            .update(cx, |app, _, cx| {
                let now = chrono::Utc::now().timestamp_millis();
                for (iid, completed) in [
                    (old, now - 5 * HOUR_MS),
                    (recent, now - HOUR_MS),
                    (focused, now - 5 * HOUR_MS),
                ] {
                    app.workspace.instance_mut(iid).unwrap().activity = AgentActivity {
                        completed_at: Some(completed),
                        ..Default::default()
                    };
                }
                app.active_instance = Some(focused);
                app.settings.sleep_after_hours = 4;
                app.auto_sleep_checked = None;
                app.auto_sleep_pass(cx);
                assert!(app.is_dormant(old), "idle 5 h past a 4 h threshold");
                assert!(app.terminals.contains_key(&recent), "idle 1 h");
                assert!(app.terminals.contains_key(&focused), "focused");
                let request = SleepRequest::Auto {
                    after_hours: 4,
                    now,
                };
                assert_eq!(
                    app.sleep_refusal_for(recent, request, cx),
                    Some(SleepRefusal::RecentlyActive)
                );
                assert_eq!(
                    app.sleep_refusal_for(focused, request, cx),
                    Some(SleepRefusal::Focused)
                );
                // Zero hours turns the timer off.
                app.settings.sleep_after_hours = 0;
                app.active_instance = None;
                app.auto_sleep_checked = None;
                app.auto_sleep_pass(cx);
                assert!(app.terminals.contains_key(&focused));
            })
            .unwrap();
        wait_until(
            cx,
            window,
            QUIT_POLL,
            "the quit to finish",
            move |app, _| !app.sleep_exits.contains_key(&old),
        );
    });
}
