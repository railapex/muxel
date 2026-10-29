#![cfg(windows)]

use super::popout_lifecycle_tests::with_app;
use super::{Instance, MuxelApp, Project, activity_state};
use gpui::{AppContext, Entity, Task, TestAppContext, WindowHandle};
use muxel_core::{PaneNode, agent_activity_label};
use muxel_terminal::{
    AgentStatus, CommandSpec, TerminalLaunch, TerminalSession, TerminalStatusReason, TerminalView,
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, WaitForSingleObject,
};

struct OwnedChild {
    session: Arc<TerminalSession>,
    process: HANDLE,
}

impl OwnedChild {
    fn new(session: Arc<TerminalSession>) -> Self {
        // SAFETY: retain the exact PTY fixture process, preventing PID reuse
        // from being mistaken for a surviving terminal during this test.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                session.child_pid().expect("owned fixture PID"),
            )
        }
        .expect("retain owned fixture process");
        Self { session, process }
    }

    fn assert_running(&self) {
        // SAFETY: this guard retains the handle until Drop.
        assert_eq!(
            unsafe { WaitForSingleObject(self.process, 0) },
            WAIT_TIMEOUT
        );
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.session.kill();
        // SAFETY: this guard exclusively owns this process handle.
        let _ = unsafe { CloseHandle(self.process) };
    }
}

fn wait_for_activity(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    view: &Entity<TerminalView>,
    expected: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        if window
            .read_with(cx, |_, cx| {
                view.read(cx).runtime_activity() == Some(expected)
            })
            .unwrap()
        {
            return;
        }
        if Instant::now() >= deadline {
            let diagnostics = window
                .read_with(cx, |_, cx| {
                    let view = view.read(cx);
                    let session = view.session();
                    let bounded = |text: &str, limit: usize| {
                        text.chars().take(limit).collect::<String>()
                    };
                    format!(
                        "view_title={:?}\nraw_title={:?}\nruntime={:?}\nactivity={:?}\nlaunch_error={:?}\nbinding_revision={}\nsession_hint={:?}\nview_session_hint={:?}\nexited={} exit_code={:?}\nvisible_grid={:?}",
                        view.title().as_deref().map(|text| bounded(text, 1024)),
                        session.title().as_deref().map(|text| bounded(text, 1024)),
                        view.runtime_snapshot(),
                        view.runtime_activity().map(|text| bounded(text, 1024)),
                        view.launch_error().map(|text| bounded(text, 1024)),
                        session.binding_revision(),
                        session.session_id_hint().as_deref().map(|text| bounded(text, 256)),
                        view.session_id_hint().as_deref().map(|text| bounded(text, 256)),
                        view.exited(),
                        view.exit_code(),
                        bounded(&view.visible_text(), 4096),
                    )
                })
                .unwrap_or_else(|error| format!("could not inspect fixture: {error}"));
            panic!("PTY activity never became {expected:?}\n{diagnostics}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn popout_activity_prose_changes_do_not_redraw_the_root_with_a_stable_lifecycle_label() {
    with_app(|cx, window| {
        let (pane, view, child) = window
            .update(cx, |app, window, cx| {
                // Drive the production sampling explicitly: an automatic tick
                // must not consume the second frame before our assertion.
                app._status_timer = Task::ready(());
                let root = muxel_store::data_dir().unwrap().join("activity-fixture");
                std::fs::create_dir_all(&root).unwrap();
                let script = root.join("activity.cmd");
                // These are real OSC bytes emitted by an owned cmd process.
                // UTF-8 mode preserves Codex's middle-dot activity separator.
                // set /p supplies a handshake between the two title frames.
                std::fs::write(
                    &script,
                    "@echo off\r\nchcp 65001 >nul\r\necho \x1b]0;Activity fixture ^| working · Read source 1\x07\r\nset /p fixture_next=\r\necho \x1b]0;Activity fixture ^| working · Read source 2\x07\r\nmore\r\n",
                )
                .unwrap();
                // cmd rejects the canonical verbatim prefix as an UNC path.
                let directory = root.to_string_lossy();
                let directory = directory.strip_prefix(r"\\?\").unwrap_or(&directory).to_string();
                let pid = app.workspace.add_project(Project::new("activity fixture", root));
                let mut instance = Instance::shell(pid);
                instance.program = Some("codex".into());
                let pane = instance.id;
                // An explicit path: cmd skips the current directory when
                // NoDefaultCurrentDirectoryInExePath is set.
                let args = vec!["/d".into(), "/q".into(), "/c".into(), r".\activity.cmd".into()];
                let spec = CommandSpec::program("cmd.exe", args)
                    .with_cwd(directory)
                    .with_status_program(Some("codex".into()));
                let launch = TerminalLaunch::spawn(spec, (80, 24)).expect("spawn owned title fixture");
                let view = cx.new(|cx| TerminalView::new(launch, pane, Instant::now(), window, cx));
                let child = OwnedChild::new(view.read(cx).session().clone());
                app.workspace.add_instance(instance);
                app.workspace.project_mut(pid).unwrap().layout = Some(PaneNode::leaf(pane));
                app.workspace.active_project = Some(pid);
                app.terminals.insert(pane, view.clone());
                app.use_tmux = false;
                app.use_worktree = false;
                app.pop_out_instance(pane, window, cx);
                assert!(!app.terminals.contains_key(&pane));
                assert!(app.popouts.contains_key(&pane));
                (pane, view, child)
            })
            .unwrap();
        wait_for_activity(cx, window, &view, "Read source 1");
        let now = 1_800_000_000_000;
        let (before, durable, label) = window
            .update(cx, |app, _, cx| {
                app.sync_popout_activity(now, cx);
                let settled = app.sync_popout_activity(now, cx);
                assert!(!settled.persist);
                assert!(
                    !settled.redraw,
                    "the unchanged control sample must be quiet"
                );
                let before = view.read(cx).runtime_snapshot();
                assert_eq!(before.status, AgentStatus::Working);
                assert_eq!(before.reason, TerminalStatusReason::ProviderTitle);
                assert!(before.is_attributable_to(child.session.binding_revision()));
                let durable = app.workspace.instance(pane).unwrap().activity.clone();
                let label = agent_activity_label(activity_state(before.status), &durable, now);
                child.assert_running();
                child.session.write_input(b"\r");
                (before, durable, label)
            })
            .unwrap();
        wait_for_activity(cx, window, &view, "Read source 2");
        let updates = window
            .update(cx, |app, _, cx| {
                let after = view.read(cx).runtime_snapshot();
                assert!(after.snapshot_seq > before.snapshot_seq);
                assert_eq!(after.status, before.status);
                assert_eq!(after.reason, before.reason);
                assert_eq!(after.attributable, before.attributable);
                assert_eq!(after.launch_id, before.launch_id);
                assert_eq!(after.binding_rev, before.binding_rev);
                assert_eq!(after.completion_revision, before.completion_revision);
                assert_eq!(after.completion_ready, before.completion_ready);
                let updates = app.sync_popout_activity(now, cx);
                let activity = &app.workspace.instance(pane).unwrap().activity;
                assert_eq!(activity, &durable);
                assert_eq!(
                    agent_activity_label(activity_state(after.status), activity, now),
                    label
                );
                assert!(
                    !updates.persist,
                    "activity prose must not rewrite lifecycle history"
                );
                child.assert_running();
                assert!(app.popouts.contains_key(&pane));
                (updates.redraw, updates.persist)
            })
            .unwrap();
        // Assert outside the app lease so a red result can still run cleanup.
        assert_eq!(
            updates,
            (false, false),
            "activity-only snapshot_seq change must not notify the root"
        );
    });
}
