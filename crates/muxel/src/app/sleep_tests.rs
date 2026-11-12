#![cfg(windows)]
//! Sleep and wake against real PTY children. Each fixture is `cmd.exe` running
//! a script that prints a real Codex OSC title, so the pane reaches a provider
//! status the way an agent's does; its saved program names a Claude binary that
//! doesn't exist, so nothing here can start a real agent.

use super::conpty_test_support::ChildHandle;
use super::popout_lifecycle_tests::{with_app, with_app_drawing};
use super::sleep::{QUIT_POLL, SleepCause, sleep_refusal_label};
use super::{Instance, MuxelApp, Project};
use gpui::{
    AppContext, Bounds, Context, Entity, Pixels, TestAppContext, VisualTestContext, Window,
    WindowHandle, px,
};
use muxel_core::sleep::{ResumeFile, SleepRefusal, SleepRequest};
use muxel_core::{AgentActivity, AgentActivityState, AgentPreset, EnvVar, PaneNode, add_tab};
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
/// waits on `more`. `agent` saves it as a resumable Claude conversation, with a
/// transcript file at its saved path.
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
                let id = Uuid::new_v4().to_string();
                let transcript = root.join("transcripts").join(format!("{id}.jsonl"));
                std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
                std::fs::write(&transcript, "{}\n").unwrap();
                instance.session_id = Some(id);
                instance.session_path = Some(transcript);
                instance.session_started = true;
            }
            let iid = instance.id;
            app.workspace.add_instance(instance);
            let project = app.workspace.project_mut(pid).unwrap();
            if let Some(anchor) = project.first_instance() {
                assert!(add_tab(&mut project.layout, anchor, iid));
            } else {
                project.layout = Some(PaneNode::leaf(iid));
            }
            let (view, child) = launch(app, window, cx, iid, name);
            (iid, view, child)
        })
        .unwrap()
}

/// Run pane `iid`'s fixture program and install it as the pane's terminal.
fn launch(
    app: &mut MuxelApp,
    window: &mut Window,
    cx: &mut Context<MuxelApp>,
    iid: Uuid,
    name: &str,
) -> (Entity<TerminalView>, ChildHandle) {
    let pid = app.workspace.instance(iid).unwrap().project_id;
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
    let launch = TerminalLaunch::spawn(
        CommandSpec::program("cmd.exe", args)
            .with_cwd(directory)
            .with_status_program(Some("codex".into())),
        (80, 24),
    )
    .unwrap();
    let view = cx.new(|cx| TerminalView::new(launch, iid, Instant::now(), window, cx));
    let child = ChildHandle::new(view.read(cx).session());
    app.terminals.insert(iid, view.clone());
    (view, child)
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
                    app.sleep_instance(agent, SleepRequest::Pane, SleepCause::Pane, cx)
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
        assert!(log.contains(&format!(
            "sleep: \"sleepy agent\" [{agent}] (Claude) by pane menu"
        )));
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

/// Claude deletes transcripts older than `cleanupPeriodDays`, and a saved id
/// whose file is gone resumes nothing. Every path refuses to sleep such a pane,
/// and outside control says whether an asleep pane's wake brings it back.
#[test]
fn an_agent_whose_conversation_file_is_gone_stays_awake_and_ctl_says_so() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (shell, _, _shell_child) = pane(cx, window, pid, "a shell", false);
        let (agent, agent_view, _agent_child) = pane(cx, window, pid, "forgotten agent", true);
        wait_ready(cx, window, &agent_view);
        window
            .update(cx, |app, _, cx| {
                let now = chrono::Utc::now().timestamp_millis();
                app.workspace.instance_mut(agent).unwrap().activity = AgentActivity {
                    completed_at: Some(now - 5 * HOUR_MS),
                    ..Default::default()
                };
                app.active_instance = Some(shell);
                let paths = [
                    SleepRequest::Pane,
                    SleepRequest::Project,
                    SleepRequest::Auto {
                        after_hours: 4,
                        now,
                    },
                ];
                let transcript = app
                    .workspace
                    .instance(agent)
                    .unwrap()
                    .session_path
                    .clone()
                    .unwrap();
                for request in paths {
                    assert_eq!(
                        app.sleep_refusal_for(agent, request, cx),
                        None,
                        "{request:?}"
                    );
                }
                // A running pane has no wake to report on.
                assert_eq!(app.control_info_of(agent, 0, cx).unwrap().resumes, None);
                std::fs::remove_file(&transcript).unwrap();
                for request in paths {
                    assert_eq!(
                        app.sleep_refusal_for(agent, request, cx),
                        Some(SleepRefusal::NoResume),
                        "{request:?} slept a pane whose transcript is gone"
                    );
                }
                assert_eq!(
                    app.sleep_menu_state(agent, cx),
                    Some(Err(sleep_refusal_label(SleepRefusal::NoResume)))
                );
                app.settings.sleep_after_hours = 4;
                app.auto_sleep_checked = None;
                app.auto_sleep_pass(cx);
                assert!(app.terminals.contains_key(&agent), "the timer slept it");
                // With its file back it sleeps, and asleep, `resumes` follows
                // the file each time it is asked.
                std::fs::write(&transcript, "{}\n").unwrap();
                assert!(
                    app.sleep_instance(agent, SleepRequest::Pane, SleepCause::Pane, cx)
                        .is_ok()
                );
                assert_eq!(
                    app.control_info_of(agent, 0, cx).unwrap().resumes,
                    Some(true)
                );
                std::fs::remove_file(&transcript).unwrap();
                assert_eq!(
                    app.control_info_of(agent, 0, cx).unwrap().resumes,
                    Some(false)
                );
            })
            .unwrap();
        wait_until(
            cx,
            window,
            QUIT_POLL,
            "the quit to finish",
            move |app, _| !app.sleep_exits.contains_key(&agent),
        );
    });
}

/// The look behind that rule and `resumes`, against where each provider writes
/// its conversation: missing, then written. Grok's isn't located.
#[test]
fn the_resume_file_look_finds_each_providers_conversation_where_it_writes_it() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        window
            .update(cx, |app, _, _| {
                let root = app.workspace.project(pid).unwrap().root_path.clone();
                let home = root.join("home");
                let _home = super::claude_test_home(Some(home.clone()));
                let mut started = |preset: AgentPreset, id: &str| {
                    let mut instance = Instance::from_preset(pid, &preset);
                    instance.session_id = Some(id.into());
                    instance.session_started = true;
                    app.presets.push(preset);
                    instance
                };
                // No saved path: the transcript for the pane's folder.
                let claude_id = Uuid::new_v4().to_string();
                let claude = started(AgentPreset::claude(), &claude_id);
                let claude_file = muxel_core::claude_session_path(&home, &root, &claude_id);
                let codex_id = "01a1543c-9700-7000-8000-000000000001";
                let mut codex = started(AgentPreset::codex(), codex_id);
                let codex_file = home
                    .join(".codex/sessions/2026/11/12")
                    .join(format!("rollout-2026-11-12T00-00-00-{codex_id}.jsonl"));
                codex.session_path = Some(codex_file.clone());
                let muse_id = Uuid::new_v4().to_string();
                let mut muse = started(AgentPreset::muse(), &muse_id);
                let data = root.join("xdg");
                muse.env = vec![EnvVar {
                    key: "XDG_DATA_HOME".into(),
                    value: data.display().to_string(),
                }];
                let muse_file = data
                    .join("muse/sessions/2026/11/12")
                    .join(&muse_id)
                    .join("session.jsonl");
                let grok = started(AgentPreset::grok(), &Uuid::new_v4().to_string());
                for (instance, file) in [
                    (&claude, &claude_file),
                    (&codex, &codex_file),
                    (&muse, &muse_file),
                ] {
                    assert_eq!(
                        app.resume_file(instance),
                        ResumeFile::Gone,
                        "{}",
                        instance.preset
                    );
                    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                    std::fs::write(file, "{}\n").unwrap();
                    assert_eq!(
                        app.resume_file(instance),
                        ResumeFile::Present,
                        "{}",
                        instance.preset
                    );
                }
                assert_eq!(app.resume_file(&grok), ResumeFile::Unnamed);
            })
            .unwrap();
    });
}

#[test]
fn a_woken_agent_gets_the_whole_threshold_before_the_timer_sleeps_it_again() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (shell, _, _shell_child) = pane(cx, window, pid, "a shell", false);
        let (agent, agent_view, _agent_child) = pane(cx, window, pid, "woken agent", true);
        wait_ready(cx, window, &agent_view);
        window
            .update(cx, |app, _, cx| {
                let now = chrono::Utc::now().timestamp_millis();
                app.workspace.instance_mut(agent).unwrap().activity = AgentActivity {
                    completed_at: Some(now - 5 * HOUR_MS),
                    ..Default::default()
                };
                app.settings.sleep_after_hours = 4;
                app.active_instance = Some(shell);
                app.auto_sleep_checked = None;
                app.auto_sleep_pass(cx);
                assert!(app.is_dormant(agent), "idle 5 h past a 4 h threshold");
            })
            .unwrap();
        wait_until(
            cx,
            window,
            QUIT_POLL,
            "the quit to finish",
            move |app, _| !app.sleep_exits.contains_key(&agent),
        );
        let before_wake = chrono::Utc::now().timestamp_millis();
        let woken_view = window
            .update(cx, |app, window, cx| {
                // Opening it wakes it through the resume launch.
                app.focus_instance(agent, window, cx);
                assert!(app.terminal_launching.contains_key(&agent));
                // Stand in for that launch landing: the real one would run the
                // fixture's missing Claude binary.
                app.terminal_launching.remove(&agent);
                // What a landing does: clear the sleep, and guard the restored
                // activity so the program's start isn't counted as new work.
                app.workspace.instance_mut(agent).unwrap().asleep_since = None;
                app.restoring_activity.insert(agent);
                launch(app, window, cx, agent, "woken agent")
            })
            .unwrap();
        wait_ready(cx, window, &woken_view.0);
        window
            .update(cx, |app, _, cx| {
                // Only the wake is new. Under load the restore guard can lapse
                // and the status tick can stamp the program's start as work;
                // drop that, so the wake alone decides what happens next.
                let now = chrono::Utc::now().timestamp_millis();
                let activity = &mut app.workspace.instance_mut(agent).unwrap().activity;
                let woken_at = activity.woken_at;
                *activity = AgentActivity {
                    completed_at: Some(now - 5 * HOUR_MS),
                    woken_at,
                    ..Default::default()
                };
                // The user looks away without typing; the next pass leaves it up.
                app.active_instance = Some(shell);
                assert_eq!(
                    app.sleep_refusal_for(agent, SleepRequest::Project, cx),
                    None,
                    "idle and unfocused: only the age gate can hold it"
                );
                app.auto_sleep_checked = None;
                app.auto_sleep_pass(cx);
                assert!(
                    app.terminals.contains_key(&agent),
                    "woken just now, though it last worked 5 h ago"
                );
                // Due again N hours after the wake, not before.
                let woke = woken_at.expect("the wake is recorded");
                assert!(woke >= before_wake);
                let at = |now| SleepRequest::Auto {
                    after_hours: 4,
                    now,
                };
                assert_eq!(
                    app.sleep_refusal_for(agent, at(woke + 4 * HOUR_MS - 1), cx),
                    Some(SleepRefusal::RecentlyActive)
                );
                assert_eq!(
                    app.sleep_refusal_for(agent, at(woke + 4 * HOUR_MS), cx),
                    None
                );
            })
            .unwrap();
    });
}

#[test]
fn its_own_sleep_takes_the_focused_agent_and_sleep_idle_agents_leaves_it() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (shell, _, _shell_child) = pane(cx, window, pid, "a shell", false);
        let (focused, focused_view, _focused_child) = pane(cx, window, pid, "focused agent", true);
        let (other, other_view, _other_child) = pane(cx, window, pid, "other agent", true);
        for view in [&focused_view, &other_view] {
            wait_ready(cx, window, view);
        }
        window
            .update(cx, |app, window, cx| {
                app.active_instance = Some(focused);
                // The project's bulk sleep names no pane: it leaves this one.
                app.sleep_idle_agents(pid, cx);
                assert!(app.is_dormant(other));
                assert!(
                    app.terminals.contains_key(&focused),
                    "Sleep idle agents took the focused pane"
                );
                // Its own Sleep names it, so it sleeps while focused.
                assert_eq!(app.sleep_menu_state(focused, cx), Some(Ok(())));
                app.sleep_from_menu(focused, window, cx);
                assert!(app.is_dormant(focused));
                assert!(
                    app.workspace
                        .instance(focused)
                        .unwrap()
                        .asleep_since
                        .is_some()
                );
                // Focus moves on as after a close, past the dormant tab to the
                // running shell, so no focus path wakes the pane straight back.
                assert_eq!(app.active_instance, Some(shell));
                assert!(app.is_dormant(other));
            })
            .unwrap();
        wait_until(
            cx,
            window,
            QUIT_POLL,
            "the quits to finish",
            move |app, _| app.sleep_exits.is_empty(),
        );
    });
}

fn bounds(cx: &mut VisualTestContext, what: &str, iid: Uuid) -> Bounds<Pixels> {
    let selector: &'static str = Box::leak(format!("{what}-{}", iid.simple()).into_boxed_str());
    cx.debug_bounds(selector)
        .unwrap_or_else(|| panic!("{selector} wasn't drawn"))
}

#[test]
fn a_dormant_sidebar_row_stays_one_line_at_the_narrowest_sidebar() {
    with_app_drawing(true, |cx, window| {
        let pid = project(cx, window);
        let (running, running_view, _child) = pane(cx, window, pid, "running agent", true);
        wait_ready(cx, window, &running_view);
        let asleep = window
            .update(cx, |app, _, cx| {
                app.show_terms = false;
                app.show_workspace_selector = false;
                app.workspace.sidebar_width = Some(160.0);
                let now = chrono::Utc::now().timestamp_millis();
                let mut instance = app.workspace.instance(running).unwrap().clone();
                instance.id = Uuid::new_v4();
                instance.session_id = Some(Uuid::new_v4().to_string());
                instance.custom_name = Some("sleeping agent".into());
                // The longest label a finished turn gets: "done · 23h".
                instance.activity = AgentActivity {
                    completed_at: Some(now - 23 * HOUR_MS),
                    last_state: Some(AgentActivityState::Done),
                    ..Default::default()
                };
                instance.asleep_since = Some(now - HOUR_MS);
                let iid = instance.id;
                app.workspace.add_instance(instance);
                let layout = &mut app.workspace.project_mut(pid).unwrap().layout;
                assert!(add_tab(layout, running, iid));
                assert!(muxel_core::set_active_tab(layout, running));
                app.dormant_instances.insert(iid);
                app.active_instance = Some(running);
                cx.notify();
                iid
            })
            .unwrap();
        cx.executor().advance_clock(Duration::from_millis(500));
        cx.run_until_parked();
        assert!(
            window
                .read_with(cx, |app, _| app.is_dormant(asleep))
                .unwrap()
        );
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        let row = bounds(&mut visual, "inst-row", running);
        let dormant = bounds(&mut visual, "inst-row", asleep);
        let badge = bounds(&mut visual, "inst-badge", asleep);
        assert!(row.size.width < px(160.0), "the sidebar is at its minimum");
        assert_eq!(
            dormant.size.height, row.size.height,
            "the dormant row wrapped"
        );
        assert!(
            badge.right() <= dormant.right(),
            "the dormant badge ran past its row"
        );
    });
}
