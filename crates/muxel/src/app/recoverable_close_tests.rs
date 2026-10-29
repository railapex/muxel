#![cfg(windows)]

use super::popout_lifecycle_tests::with_app;
use super::{
    ConfirmAction, EditorSnapshot, Instance, MuxelApp, NotifKind, Project, SaveTarget, Workspace,
    WorkspaceMeta,
};
use gpui::{AppContext, Entity, Focusable, TestAppContext, VisualTestContext, WindowHandle};
use muxel_core::control::ErrorCode;
use muxel_core::{PaneNode, Worktree, add_tab};
use muxel_terminal::{
    AgentStatus, CommandSpec, TerminalLaunch, TerminalSession, TerminalStatusReason, TerminalView,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    TerminateProcess, WaitForSingleObject,
};

/// Keep the original process handle: an unrelated process reusing its PID must
/// never count as either survival or successful teardown of this fixture.
struct OwnedChild {
    session: Arc<TerminalSession>,
    process: HANDLE,
}

impl OwnedChild {
    fn new(session: Arc<TerminalSession>) -> Self {
        // SAFETY: the session owns the process just spawned by this fixture.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                false,
                session.child_pid().expect("owned fixture child PID"),
            )
        }
        .expect("retain owned fixture process handle");
        Self { session, process }
    }

    fn exited(&self) -> bool {
        // SAFETY: this handle stays open until the guard is dropped.
        let status = unsafe { WaitForSingleObject(self.process, 0) };
        if status == WAIT_OBJECT_0 {
            true
        } else {
            assert_eq!(status, WAIT_TIMEOUT, "query owned fixture process");
            false
        }
    }

    fn terminate(&self, exit_code: u32) {
        // SAFETY: this is the retained handle of this fixture's own child.
        // Selecting its actual OS exit code exercises both clean and abnormal
        // exit paths without changing TerminalView's runtime fields.
        unsafe { TerminateProcess(self.process, exit_code) }.expect("terminate owned fixture");
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.session.kill();
        // SAFETY: this guard exclusively owns this retained process handle.
        let _ = unsafe { CloseHandle(self.process) };
    }
}

fn project(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> Uuid {
    window
        .update(cx, |app, _, _| {
            let root = muxel_store::data_dir()
                .unwrap()
                .join(Uuid::new_v4().to_string());
            std::fs::create_dir_all(&root).unwrap();
            let pid = app
                .workspace
                .add_project(Project::new("close lifecycle", root));
            app.workspace.active_project = Some(pid);
            app.current_workspace = Some(Uuid::new_v4());
            app.use_tmux = false;
            app.use_worktree = false;
            app.settings.control_enabled = true;
            app.settings.control_allow_shells = true;
            app.settings.confirm_close_terminal = true;
            pid
        })
        .unwrap()
}

fn terminal(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pid: Uuid,
) -> (Uuid, Entity<TerminalView>, OwnedChild) {
    window
        .update(cx, |app, window, cx| {
            let mut instance = Instance::shell(pid);
            instance.program = Some("cmd.exe".into());
            instance.args = ["/d", "/q", "/c", "more"]
                .into_iter()
                .map(str::to_string)
                .collect();
            let iid = instance.id;
            let launch = TerminalLaunch::spawn(
                CommandSpec::program("cmd.exe", instance.args.clone()),
                (80, 24),
            )
            .expect("spawn owned waiting command");
            let view = cx.new(|cx| TerminalView::new(launch, iid, Instant::now(), window, cx));
            let child = OwnedChild::new(view.read(cx).session().clone());
            app.workspace.add_instance(instance);
            let project = app.workspace.project_mut(pid).unwrap();
            if let Some(anchor) = project.first_instance() {
                assert!(add_tab(&mut project.layout, anchor, iid));
            } else {
                project.layout = Some(PaneNode::leaf(iid));
            }
            app.terminals.insert(iid, view.clone());
            app.active_instance = Some(iid);
            assert!(!child.exited(), "fixture must start alive");
            (iid, view, child)
        })
        .unwrap()
}

fn wait_for_exit(cx: &mut TestAppContext, child: &OwnedChild) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if child.exited() {
            return;
        }
        assert!(Instant::now() < deadline, "held process was not finalized");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn idle_agent_close_holds_process_and_undo_restores_tab_order() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (a, _, first) = terminal(cx, window, pid);
        let (b, _, second) = terminal(cx, window, pid);
        let (c, _, survivor) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                // Admission is covered separately. Exercise the actual hold
                // operation with a selection ordered differently from its tabs.
                let group = app
                    .hold_closes(vec![(b, false), (a, false)], None, cx)
                    .unwrap();
                assert_eq!(app.workspace.project(pid).unwrap().instances(), vec![c]);
                for (iid, child) in [(a, &first), (b, &second)] {
                    assert!(app.is_closing(iid));
                    assert!(app.closing_until(iid).is_some());
                    let session = app.terminal_view(iid).unwrap().read(cx).session();
                    assert!(Arc::ptr_eq(session, &child.session));
                    assert!(!child.exited(), "hold killed the process");
                }
                app.undo_close(group, window, cx);
                assert_eq!(
                    app.workspace.project(pid).unwrap().instances(),
                    vec![a, b, c]
                );
                for (iid, child) in [(a, &first), (b, &second), (c, &survivor)] {
                    assert!(!app.is_closing(iid));
                    assert!(app.closing_until(iid).is_none());
                    assert!(Arc::ptr_eq(
                        app.terminal_view(iid).unwrap().read(cx).session(),
                        &child.session,
                    ));
                    assert!(!child.exited());
                }
            })
            .unwrap();
    });
}

#[test]
fn held_panes_stay_out_of_ctl_listings_and_refuse_closing() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (held, _, child) = terminal(cx, window, pid);
        let (visible, _, _survivor) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                assert!(app.control_writable(held, cx).is_ok());
                let group = app.hold_closes(vec![(held, false)], None, cx).unwrap();
                let deadline = app.closing_until(held).unwrap();
                let listed: Vec<_> = app.control_instances().iter().map(|i| i.id).collect();
                assert!(!listed.contains(&held));
                assert!(listed.contains(&visible));
                let error = app
                    .control_writable(held, cx)
                    .err()
                    .expect("held write refused");
                assert_eq!(error.code, Some(ErrorCode::Closing));
                assert_eq!(error.undo_until, Some(deadline));
                let error = app.control_resolve(&held.to_string()).unwrap_err();
                assert_eq!(error.code, Some(ErrorCode::Closing));
                assert_eq!(error.undo_until, Some(deadline));
                assert!(!child.exited());
                app.undo_close(group, window, cx);
                assert!(app.control_writable(held, cx).is_ok());
                assert_eq!(app.control_resolve(&held.to_string()).unwrap(), held);
            })
            .unwrap();
    });
}

#[test]
fn save_during_countdown_restores_held_panes_to_layout() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (a, _, child) = terminal(cx, window, pid);
        let (b, _, _survivor) = terminal(cx, window, pid);
        window
            .update(cx, |app, _, cx| {
                app.hold_closes(vec![(a, false)], None, cx).unwrap();
                let restored = app.workspace_with_closes_restored();
                let bytes = serde_json::to_vec(&restored).unwrap();
                let durable: Workspace = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(durable.project(pid).unwrap().instances(), vec![a, b]);
                assert!(durable.instance(a).is_some());
                assert!(app.try_persist());
                let path = muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                let saved =
                    muxel_store::load_workspace_from(&path).expect("saved countdown workspace");
                assert_eq!(saved.project(pid).unwrap().instances(), vec![a, b]);
                assert_eq!(app.workspace.project(pid).unwrap().instances(), vec![b]);
                assert!(app.is_closing(a), "saving ended the countdown");
                assert!(!child.exited());
            })
            .unwrap();
    });
}

#[test]
fn project_remove_finalizes_its_held_panes_first() {
    with_app(|cx, window| {
        let removed = project(cx, window);
        let (held, _, child) = terminal(cx, window, removed);
        let kept = project(cx, window);
        let (visible, _, survivor) = terminal(cx, window, kept);
        window
            .update(cx, |app, _, cx| {
                app.hold_closes(vec![(held, false)], None, cx).unwrap();
                app.delete_project(removed, cx);
                assert!(app.workspace.project(removed).is_none());
                assert!(app.workspace.instance(held).is_none());
                assert!(!app.is_closing(held));
                assert!(app.terminal_view(held).is_none());
                assert!(app.workspace.project(kept).is_some());
                assert!(app.workspace.instance(visible).is_some());
                assert!(!survivor.exited());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn workspace_switch_finalizes_before_drain() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (held, _, child) = terminal(cx, window, pid);
        let next = Uuid::new_v4();
        let target_project = window
            .update(cx, |app, window, cx| {
                let previous = app.current_workspace.unwrap();
                app.workspaces.workspaces.extend([
                    WorkspaceMeta {
                        id: previous,
                        name: "previous".into(),
                    },
                    WorkspaceMeta {
                        id: next,
                        name: "target".into(),
                    },
                ]);
                let root = muxel_store::data_dir().unwrap().join("target-project");
                std::fs::create_dir_all(&root).unwrap();
                let mut target = Workspace::default();
                let target_project = target.add_project(Project::new("target", root));
                muxel_store::save_workspace_to(
                    &muxel_store::workspace_doc_path(next).unwrap(),
                    &target,
                )
                .unwrap();
                app.hold_closes(vec![(held, false)], None, cx).unwrap();
                app.enter_workspace(next, window, cx);
                assert_eq!(app.current_workspace, Some(next));
                assert!(!app.is_closing(held));
                assert!(app.terminal_view(held).is_none());
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(previous).unwrap(),
                )
                .expect("old workspace persisted before switch");
                assert!(
                    saved.instance(held).is_none(),
                    "switch persisted a held pane as live"
                );
                target_project
            })
            .unwrap();
        cx.run_until_parked();
        window
            .read_with(cx, |app, _| {
                assert!(app.workspace.project(target_project).is_some());
                assert!(app.workspace.instance(held).is_none());
                assert!(!app.is_closing(held));
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn popout_os_close_follows_the_close_rule() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        let popup = window
            .update(cx, |app, window, cx| {
                app.pop_out_instance(pane, window, cx);
                app.popouts[&pane].window
            })
            .unwrap();
        let mut visual = VisualTestContext::from_window(popup.into(), cx);
        // This fixture is a running Windows command with no OSC 133 prompt
        // evidence. Native close must ask, rather than treating output quiet as idle.
        assert!(!visual.simulate_close(), "busy native close must be vetoed");
        cx.run_until_parked();
        window
            .update(cx, |app, _, cx| {
                assert!(
                    app.confirm.is_some(),
                    "OS close did not reach central confirmation"
                );
                assert!(!app.is_closing(pane));
                assert!(!child.exited());
                app.cancel_confirm(cx);
            })
            .unwrap();
        assert!(popup.read_with(cx, |_, _| ()).is_ok());
        assert!(!visual.simulate_close());
        cx.run_until_parked();
        window
            .update(cx, |app, window, cx| {
                assert!(app.confirm.is_some());
                app.run_confirm(window, cx);
                assert!(app.is_closing(pane));
                assert!(
                    !child.exited(),
                    "confirmed busy close must still count down"
                );
            })
            .unwrap();
        cx.run_until_parked();
        assert!(popup.read_with(cx, |_, _| ()).is_err());
        window
            .update(cx, |app, window, cx| {
                assert!(!app.popouts.contains_key(&pane));
                assert!(Arc::ptr_eq(
                    app.terminal_view(pane).unwrap().read(cx).session(),
                    &child.session,
                ));
                let group = app
                    .close_groups
                    .last()
                    .expect("one confirmed close group")
                    .id;
                app.undo_close(group, window, cx);
                assert_eq!(app.workspace.project(pid).unwrap().instances(), vec![pane]);
                assert!(app.terminals.contains_key(&pane));
                assert!(!app.is_closing(pane));
                assert!(Arc::ptr_eq(
                    app.terminal_view(pane).unwrap().read(cx).session(),
                    &child.session,
                ));
                assert!(!child.exited());
            })
            .unwrap();
    });
}

#[test]
fn forced_close_skips_expiry_recheck() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        window
            .update(cx, |app, _, cx| {
                app.hold_closes(vec![(pane, true)], Some("lifecycle fixture".into()), cx)
                    .unwrap();
                assert!(!child.exited());
                app.expire_closes(Instant::now() + Duration::from_secs(16), cx);
                assert!(app.workspace.instance(pane).is_none());
                assert!(app.terminal_view(pane).is_none());
                assert!(!app.is_closing(pane));
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn finalizing_all_held_closes_removes_processes_before_persist() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        window
            .update(cx, |app, _, cx| {
                app.hold_closes(vec![(pane, false)], None, cx).unwrap();
                assert!(app.finalize_all_closes(cx));
                assert!(app.try_persist());
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                )
                .unwrap();
                assert!(saved.instance(pane).is_none());
                assert!(!app.is_closing(pane));
                assert!(app.terminal_view(pane).is_none());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

fn wait_for_view(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    view: &Entity<TerminalView>,
    condition: impl Fn(&TerminalView) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        // The real PTY reader and GPUI's virtual output-coalescing timers must
        // both advance before an actual child exit/title reaches its view.
        cx.executor().advance_clock(Duration::from_millis(10));
        cx.run_until_parked();
        if window
            .read_with(cx, |_, cx| condition(view.read(cx)))
            .unwrap()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "owned terminal did not reach expected state"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn held_panes_skip_tick_attention_and_restart() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (held_clean, held_clean_view, held_clean_child) = terminal(cx, window, pid);
        let (held_bad, held_bad_view, held_bad_child) = terminal(cx, window, pid);
        let (live_clean, live_clean_view, live_clean_child) = terminal(cx, window, pid);
        let (live_bad, live_bad_view, live_bad_child) = terminal(cx, window, pid);
        let activities = window
            .update(cx, |app, window, cx| {
                app.settings.close_on_exit = true;
                app.notifications_enabled = false;
                for (iid, name) in [
                    (held_clean, "held clean fixture"),
                    (held_bad, "held abnormal fixture"),
                    (live_clean, "live clean fixture"),
                    (live_bad, "live abnormal fixture"),
                ] {
                    app.workspace.instance_mut(iid).unwrap().custom_name = Some(name.into());
                }
                app.tick(window, cx);
                let restart = app
                    .terminal_restarts
                    .begin(app.workspace.instance(held_clean).unwrap())
                    .unwrap();
                app.hold_closes(vec![(held_clean, false), (held_bad, false)], None, cx)
                    .unwrap();
                assert!(!app.terminal_restarts.current(held_clean, restart));
                app.restart_agent(held_clean, window, cx);
                assert!(!app.terminal_restarts.contains(held_clean));
                assert!(Arc::ptr_eq(
                    app.terminal_view(held_clean).unwrap().read(cx).session(),
                    &held_clean_child.session
                ));
                app.notifications.clear();
                [held_clean, held_bad]
                    .map(|iid| app.workspace.instance(iid).unwrap().activity.clone())
            })
            .unwrap();
        held_clean_child.terminate(0);
        held_bad_child.terminate(7);
        live_clean_child.terminate(0);
        live_bad_child.terminate(7);
        for view in [
            &held_clean_view,
            &held_bad_view,
            &live_clean_view,
            &live_bad_view,
        ] {
            wait_for_view(cx, window, view, TerminalView::exited);
        }
        window
            .update(cx, |app, window, cx| {
                assert_eq!(held_clean_view.read(cx).exit_code(), Some(0));
                assert_eq!(live_clean_view.read(cx).exit_code(), Some(0));
                app.tick(window, cx);
                for (iid, activity) in [held_clean, held_bad].into_iter().zip(activities) {
                    assert!(app.is_closing(iid));
                    assert_eq!(app.workspace.instance(iid).unwrap().activity, activity);
                    assert!(app.terminal_view(iid).is_some());
                    assert!(!app.exit_logged.contains(&iid));
                    assert!(!app.terminal_restarts.contains(iid));
                    assert!(!app.terminal_launching.contains_key(&iid));
                    assert!(
                        !app.notifications
                            .iter()
                            .any(|notice| notice.instance == Some(iid))
                    );
                    assert!(!app.readings.queue.contains(&iid));
                }
                assert!(
                    app.notifications
                        .iter()
                        .all(|notice| !notice.title.contains("held clean fixture")
                            && !notice.title.contains("held abnormal fixture"))
                );
                assert!(
                    app.workspace.instance(live_clean).is_none(),
                    "live clean control did not auto-close"
                );
                assert!(
                    app.workspace.instance(live_bad).is_some(),
                    "abnormal control should remain a tombstone"
                );
                assert!(
                    app.notifications
                        .iter()
                        .any(|notice| notice.title.contains("live clean fixture"))
                );
                assert!(
                    app.notifications
                        .iter()
                        .any(|notice| notice.title.contains("live abnormal fixture"))
                );
                assert!(app.exit_logged.contains(&live_bad));
            })
            .unwrap();
    });
}

/// The owned command emits real Codex OSC-title protocol bytes through its PTY.
/// No provider binary, injected runtime status or fake title-source callback is used.
fn title_terminal(
    cx: &mut TestAppContext,
    window: WindowHandle<MuxelApp>,
    pid: Uuid,
    name: &str,
    state: &str,
) -> (Uuid, Entity<TerminalView>, OwnedChild) {
    window
        .update(cx, |app, window, cx| {
            let root = app.workspace.project(pid).unwrap().root_path.clone();
            let filename = format!("title-{}.cmd", Uuid::new_v4());
            let script = root.join(&filename);
            std::fs::write(
                &script,
                format!("@echo \x1b]0;{name} ^| {state}\x07\r\n@more\r\n"),
            )
            .unwrap();
            // cmd rejects canonical verbatim paths. Pass a local filename and
            // the same normal cwd used by the verified B2 title fixture.
            let directory = root.to_string_lossy();
            let directory = directory
                .strip_prefix(r"\\?\")
                .unwrap_or(&directory)
                .to_string();
            let args = vec!["/d".into(), "/q".into(), "/c".into(), filename];
            let mut instance = Instance::shell(pid);
            instance.program = Some("codex".into());
            instance.custom_name = Some(name.into());
            let iid = instance.id;
            let launch = TerminalLaunch::spawn(
                CommandSpec::program("cmd.exe", args)
                    .with_cwd(directory)
                    .with_status_program(Some("codex".into())),
                (80, 24),
            )
            .unwrap();
            let view = cx.new(|cx| TerminalView::new(launch, iid, Instant::now(), window, cx));
            let child = OwnedChild::new(view.read(cx).session().clone());
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

#[test]
fn batch_close_splits_quiet_and_active_members() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (anchor, _, _anchor_child) = terminal(cx, window, pid);
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
        window.update(cx, |app, window, cx| {
            app.close_other_tabs(anchor, window, cx);
            let pending = app.confirm.as_ref().expect("mixed batch prompts once");
            assert!(matches!(&pending.action, ConfirmAction::CloseBatch(ids) if ids.as_slice() == [quiet, busy]));
            assert!(pending.message.contains("busy fixture"));
            assert!(!pending.message.contains("quiet fixture"));
            assert!(!app.is_closing(quiet));
            assert!(!app.is_closing(busy));
        }).unwrap();
        let (new_tab, _, new_child) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                app.run_close_alternative(cx);
                assert!(app.is_closing(quiet));
                assert!(!app.closing[&quiet].forced);
                assert!(!app.is_closing(busy));
                assert!(!app.is_closing(new_tab));
                assert_eq!(app.close_groups.len(), 1);
                assert_eq!(app.close_groups[0].members, vec![quiet]);
                app.undo_latest_close(window, cx);
                // Repeat the original captured selection and confirm all. The tab
                // created while the first prompt was open remains excluded.
                app.request_recoverable_close(vec![quiet, busy], cx);
                app.run_confirm(window, cx);
                assert!(app.is_closing(quiet));
                assert!(app.is_closing(busy));
                assert!(!app.closing[&quiet].forced);
                assert!(app.closing[&busy].forced);
                assert_eq!(app.close_groups.len(), 1);
                assert_eq!(app.close_groups[0].members, vec![quiet, busy]);
                assert_eq!(
                    app.workspace.project(pid).unwrap().instances(),
                    vec![anchor, new_tab]
                );
                assert!(!quiet_child.exited());
                assert!(!busy_child.exited());
                assert!(!new_child.exited());
            })
            .unwrap();
    });
}

// Git's worktree argv rejects the verbatim prefix returned by canonicalizing
// the isolated test root. Keep actual fixture metadata on a normal drive path.
fn git_fixture_path(path: PathBuf) -> PathBuf {
    let value = path.to_string_lossy();
    PathBuf::from(value.strip_prefix(r"\\?\").unwrap_or(&value))
}

fn git_fixture(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "core.hooksPath=NUL",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=Muxel test",
            "-c",
            "user.email=muxel-test@example.invalid",
        ])
        .arg("-C")
        .arg(root)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("run git inside disposable fixture");
    assert!(
        output.status.success(),
        "fixture git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn worktree_last_pane_dispose_answer_is_final() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        window
            .update(cx, |app, _, cx| {
                let root = app.workspace.project(pid).unwrap().root_path.clone();
                git_fixture(&root, &["init", "--initial-branch=main"]);
                git_fixture(&root, &["commit", "--allow-empty", "-m", "fixture"]);
                let worktree_path =
                    git_fixture_path(muxel_store::data_dir().unwrap().join("owned-worktree"));
                git_fixture(
                    &root,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        "muxel/close-fixture",
                        worktree_path.to_str().unwrap(),
                    ],
                );
                let draft = worktree_path.join("draft.txt");
                std::fs::write(&draft, "keep this work\n").unwrap();
                let wid = Uuid::new_v4();
                app.workspace.add_worktree(Worktree {
                    id: wid,
                    project_id: pid,
                    name: "close fixture".into(),
                    path: worktree_path.clone(),
                    branch: "muxel/close-fixture".into(),
                    color: 0,
                    detached: false,
                });
                let instance = app.workspace.instance_mut(pane).unwrap();
                instance.worktree_id = Some(wid);
                instance.worktree_path = Some(worktree_path.clone());
                instance.worktree_branch = Some("muxel/close-fixture".into());
                app.request_close_instance(pane, cx);
                let pending = app
                    .pending_worktree_dispose
                    .front()
                    .expect("worktree decision must precede close");
                assert_eq!(pending.wid, wid);
                assert_eq!(pending.close_after, vec![pane]);
                assert!(pending.changed > 0);
                assert!(app.confirm.is_none(), "worktree must be the sole prompt");
                assert!(!app.is_closing(pane));
                assert!(
                    !child.exited(),
                    "preflight killed the terminal before the choice"
                );
                app.dispose_worktree_keep(cx);
                assert!(app.workspace.instance(pane).is_none());
                assert!(app.terminal_view(pane).is_none());
                assert!(!app.is_closing(pane));
                assert!(
                    app.close_groups.is_empty(),
                    "worktree choice must not count down"
                );
                assert!(app.pending_worktree_dispose.is_empty());
                assert!(app.workspace.worktree(wid).unwrap().detached);
                assert_eq!(std::fs::read_to_string(&draft).unwrap(), "keep this work\n");
                app.expire_closes(Instant::now() + Duration::from_secs(60), cx);
                assert!(
                    app.pending_worktree_dispose.is_empty(),
                    "a second dispose question appeared at expiry"
                );
                assert!(worktree_path.is_dir());
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn dirty_editor_cancel_and_save_failure_preserve_the_draft_until_discarded() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (iid, editor) = window.update(cx, |app, window, cx| {
            // A directory is a deterministic unwritable file target on Windows;
            // this does not rely on ACLs or the account's privileges.
            let path = app.workspace.project(pid).unwrap().root_path.clone();
            let instance = Instance::editor(pid, Some(path.clone()));
            let iid = instance.id;
            let snapshot = EditorSnapshot { text: "unsaved draft".into(), path: Some(path), language: "text".into(), cursor: None, dirty: true, external_change: None, disk_stamp: None, show_rendered: false, diff_dir: None };
            let editor = snapshot.build(app.editor_config(), window, cx);
            app.workspace.add_instance(instance);
            app.workspace.project_mut(pid).unwrap().layout = Some(PaneNode::leaf(iid));
            app.editors.insert(iid, editor.clone());
            app.request_close_instance(iid, cx);
            assert!(matches!(app.confirm.as_ref().map(|p| &p.action), Some(ConfirmAction::SaveEditorBeforeClose { iid: pending, .. }) if *pending == iid));
            app.cancel_confirm(cx);
            assert!(app.workspace.instance(iid).is_some());
            assert!(editor.read(cx).is_dirty());
            assert_eq!(editor.read(cx).text(cx), "unsaved draft");
            app.request_close_instance(iid, cx);
            app.run_confirm(window, cx);
            (iid, editor)
        }).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let failed = window
                .read_with(cx, |app, _| {
                    app.notifications.iter().any(|notice| {
                        matches!(notice.kind, NotifKind::Error)
                            && notice.title == super::t("Save failed").as_ref()
                    })
                })
                .unwrap();
            if failed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "actual editor save failure was not reported"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        window
            .update(cx, |app, _, cx| {
                assert!(app.workspace.instance(iid).is_some());
                assert_eq!(app.editors.get(&iid), Some(&editor));
                assert!(editor.read(cx).is_dirty());
                assert_eq!(editor.read(cx).text(cx), "unsaved draft");
                assert!(!app.is_closing(iid));
                app.request_close_instance(iid, cx);
                app.run_close_alternative(cx);
                assert!(app.workspace.instance(iid).is_none());
                assert!(!app.editors.contains_key(&iid));
                assert!(app.close_groups.is_empty());
            })
            .unwrap();
    });
}

#[test]
fn workspace_switch_retries_a_failed_close_flush_before_leaving() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                let previous = app.current_workspace.unwrap();
                let next = Uuid::new_v4();
                app.workspaces.workspaces.extend([
                    WorkspaceMeta {
                        id: previous,
                        name: "previous".into(),
                    },
                    WorkspaceMeta {
                        id: next,
                        name: "target".into(),
                    },
                ]);
                let target_root = muxel_store::data_dir().unwrap().join("retry-target");
                std::fs::create_dir_all(&target_root).unwrap();
                let mut target = Workspace::default();
                let target_project = target.add_project(Project::new("target", target_root));
                muxel_store::save_workspace_to(
                    &muxel_store::workspace_doc_path(next).unwrap(),
                    &target,
                )
                .unwrap();
                app.hold_closes(vec![(pane, false)], None, cx).unwrap();
                let old_path = muxel_store::workspace_doc_path(previous).unwrap();
                let saved_countdown = old_path.with_extension("before-close.json");
                let saved = muxel_store::load_workspace_from(&old_path).unwrap();
                assert!(
                    saved.instance(pane).is_some(),
                    "fixture must retain a recoverable countdown document"
                );
                // Replace only this isolated fixture file with an empty directory.
                // The real store's atomic rename must fail regardless of privileges.
                std::fs::rename(&old_path, &saved_countdown).unwrap();
                std::fs::create_dir(&old_path).unwrap();
                app.enter_workspace(next, window, cx);
                assert_eq!(
                    app.current_workspace,
                    Some(previous),
                    "failed flush must veto switch"
                );
                assert!(app.save_errors.contains_key(&SaveTarget::Workspace));
                assert!(
                    app.workspace.instance(pane).is_none(),
                    "first attempt should finalize the close"
                );
                assert!(!app.is_closing(pane));
                std::fs::remove_dir(&old_path).unwrap();
                std::fs::rename(&saved_countdown, &old_path).unwrap();
                assert!(
                    muxel_store::load_workspace_from(&old_path)
                        .unwrap()
                        .instance(pane)
                        .is_some()
                );
                // No held group remains to trigger the original finalize-only save
                // condition. A second attempt must still flush the pending removal.
                app.enter_workspace(next, window, cx);
                assert_eq!(app.current_workspace, Some(next));
                assert!(app.workspace.project(target_project).is_some());
                let saved = muxel_store::load_workspace_from(&old_path).unwrap();
                assert!(
                    saved.instance(pane).is_none(),
                    "retry left a finalized pane in the old durable document"
                );
                assert!(!app.save_errors.contains_key(&SaveTarget::Workspace));
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn project_finalization_preserves_other_projects_in_a_shared_close_group() {
    with_app(|cx, window| {
        let first_project = project(cx, window);
        let (first, _, first_child) = terminal(cx, window, first_project);
        let second_project = project(cx, window);
        let (second, _, second_child) = terminal(cx, window, second_project);
        window
            .update(cx, |app, window, cx| {
                let group = app
                    .hold_closes(vec![(first, false), (second, false)], None, cx)
                    .unwrap();
                let deadline = app.closing_until(second).unwrap();
                app.finalize_project_closes(first_project, cx);
                assert!(app.workspace.instance(first).is_none());
                assert!(app.terminal_view(first).is_none());
                assert!(!app.is_closing(first));
                assert!(
                    app.workspace.instance(second).is_some(),
                    "project finalization closed another project's pane"
                );
                assert!(app.is_closing(second));
                assert_eq!(app.closing_until(second), Some(deadline));
                let remaining = app
                    .close_groups
                    .iter()
                    .find(|remaining| remaining.id == group)
                    .expect("other project keeps its close group");
                assert_eq!(remaining.members, vec![second]);
                assert!(Arc::ptr_eq(
                    app.terminal_view(second).unwrap().read(cx).session(),
                    &second_child.session
                ));
                assert!(!second_child.exited());
                assert!(app.try_persist());
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                )
                .unwrap();
                assert!(saved.instance(first).is_none());
                assert_eq!(
                    saved.project(second_project).unwrap().instances(),
                    vec![second]
                );
                assert!(
                    app.workspace
                        .project(second_project)
                        .unwrap()
                        .instances()
                        .is_empty()
                );
                app.undo_close(group, window, cx);
                assert_eq!(
                    app.workspace.project(second_project).unwrap().instances(),
                    vec![second]
                );
                assert!(!app.is_closing(second));
                assert!(Arc::ptr_eq(
                    app.terminal_view(second).unwrap().read(cx).session(),
                    &second_child.session
                ));
                assert!(!second_child.exited());
            })
            .unwrap();
        wait_for_exit(cx, &first_child);
    });
}

#[test]
fn clean_fully_landed_last_worktree_close_has_no_dispose_modal_or_countdown() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, _, child) = terminal(cx, window, pid);
        window
            .update(cx, |app, _, cx| {
                let root = app.workspace.project(pid).unwrap().root_path.clone();
                git_fixture(&root, &["init", "--initial-branch=main"]);
                git_fixture(&root, &["commit", "--allow-empty", "-m", "fixture"]);
                let path = git_fixture_path(
                    muxel_store::data_dir()
                        .unwrap()
                        .join("clean-owned-worktree"),
                );
                git_fixture(
                    &root,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        "muxel/clean-close-fixture",
                        path.to_str().unwrap(),
                    ],
                );
                let wid = Uuid::new_v4();
                app.workspace.add_worktree(Worktree {
                    id: wid,
                    project_id: pid,
                    name: "clean close fixture".into(),
                    path: path.clone(),
                    branch: "muxel/clean-close-fixture".into(),
                    color: 0,
                    detached: false,
                });
                let instance = app.workspace.instance_mut(pane).unwrap();
                instance.worktree_id = Some(wid);
                instance.worktree_path = Some(path.clone());
                instance.worktree_branch = Some("muxel/clean-close-fixture".into());
                assert_eq!(super::integrations::worktree_change_count(&path), 0);
                let base = super::integrations::repo_head(&root).expect("fixture base HEAD");
                assert_eq!(
                    super::integrations::worktree_unmerged_count(&path, &base),
                    0
                );
                app.request_close_instance(pane, cx);
                assert!(
                    app.pending_worktree_dispose.is_empty(),
                    "fully landed worktree showed an empty dispose question"
                );
                assert!(app.confirm.is_none());
                assert!(!app.is_closing(pane));
                assert!(app.close_groups.is_empty());
                assert!(
                    app.workspace.instance(pane).is_none(),
                    "fully landed last pane must close immediately"
                );
                assert!(app.terminal_view(pane).is_none());
                assert!(app.workspace.worktree(wid).is_none());
                assert!(!path.exists(), "fully landed worktree should be removed");
            })
            .unwrap();
        wait_for_exit(cx, &child);
    });
}

#[test]
fn workspace_departure_waits_for_local_tmux_close_tasks_before_draining_processes() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (held, _, held_child) = terminal(cx, window, pid);
        let (survivor, _, survivor_child) = terminal(cx, window, pid);
        let (release, waiting) = async_channel::bounded::<()>(1);
        let next = Uuid::new_v4();
        let (previous, target_project) = window
            .update(cx, |app, window, cx| {
                app._status_timer = gpui::Task::ready(());
                let previous = app.current_workspace.unwrap();
                app.workspaces.workspaces.extend([
                    WorkspaceMeta {
                        id: previous,
                        name: "previous".into(),
                    },
                    WorkspaceMeta {
                        id: next,
                        name: "target".into(),
                    },
                ]);
                let root = muxel_store::data_dir().unwrap().join("departure-target");
                std::fs::create_dir_all(&root).unwrap();
                let mut target = Workspace::default();
                let target_project = target.add_project(Project::new("target", root));
                muxel_store::save_workspace_to(
                    &muxel_store::workspace_doc_path(next).unwrap(),
                    &target,
                )
                .unwrap();
                app.hold_closes(vec![(held, false)], None, cx).unwrap();
                // Use the same real GPUI task collection as local tmux teardown,
                // with a controlled completion instead of launching tmux.
                app.close_tmux_kills
                    .push(cx.background_executor().spawn(async move {
                        waiting.recv().await.expect("release owned departure task");
                    }));
                app.enter_workspace(next, window, cx);
                assert_eq!(app.current_workspace, Some(previous));
                assert!(app.close_departure_task.is_some());
                assert!(matches!(
                    app.close_departure,
                    Some(super::recoverable_close::CloseDeparture::Workspace(id)) if id == next
                ));
                assert!(app.workspace.instance(held).is_none());
                assert!(app.workspace.instance(survivor).is_some());
                assert!(Arc::ptr_eq(
                    app.terminal_view(survivor).unwrap().read(cx).session(),
                    &survivor_child.session,
                ));
                assert!(
                    !survivor_child.exited(),
                    "departure drained the old process early"
                );
                let saved = muxel_store::load_workspace_from(
                    &muxel_store::workspace_doc_path(previous).unwrap(),
                )
                .unwrap();
                assert!(saved.instance(held).is_none());
                assert!(saved.instance(survivor).is_some());
                (previous, target_project)
            })
            .unwrap();
        // Allow several production departure polls while the task is blocked.
        for _ in 0..3 {
            cx.executor().advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
        }
        window
            .read_with(cx, |app, _| {
                assert_eq!(app.current_workspace, Some(previous));
                assert!(app.close_departure_task.is_some());
                assert!(!app.close_tmux_kills.is_empty());
                assert!(app.workspace.instance(survivor).is_some());
                assert!(!survivor_child.exited());
            })
            .unwrap();
        release.try_send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.executor().advance_clock(Duration::from_millis(20));
            cx.run_until_parked();
            let entered = window
                .read_with(cx, |app, _| app.current_workspace == Some(next))
                .unwrap();
            if entered {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "departure did not resume after teardown completed"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        window
            .read_with(cx, |app, _| {
                assert!(app.close_departure_task.is_none());
                assert!(app.close_departure.is_none());
                assert!(app.close_tmux_kills.is_empty());
                assert!(app.workspace.project(target_project).is_some());
                assert!(app.workspace.instance(survivor).is_none());
                assert!(app.terminal_view(survivor).is_none());
            })
            .unwrap();
        wait_for_exit(cx, &held_child);
        wait_for_exit(cx, &survivor_child);
    });
}

#[test]
fn delayed_transcript_refuses_retired_epoch_and_replaced_session_without_changing_focus() {
    with_app(|cx, window| {
        let pid = project(cx, window);
        let (pane, view, child) = terminal(cx, window, pid);
        let (other, other_view, other_child) = terminal(cx, window, pid);
        let (replacement_id, replacement_view, replacement_child) = terminal(cx, window, pid);
        window
            .update(cx, |app, window, cx| {
                app._status_timer = gpui::Task::ready(());
                let retired = app
                    .terminal_input_admission(pane, cx)
                    .expect("initial admission");
                let group = app.hold_closes(vec![(pane, false)], None, cx).unwrap();
                app.undo_close(group, window, cx);
                assert!(Arc::ptr_eq(
                    app.terminal_view(pane).unwrap().read(cx).session(),
                    &child.session,
                ));
                app.focus_instance(other, window, cx);
                assert!(other_view.read(cx).focus_handle(cx).is_focused(window));
                let original_rev = child.session.input_rev();
                let other_rev = other_child.session.input_rev();
                app.insert_transcript(Some(retired), "retired transcript", true, window, cx);
                assert_eq!(child.session.input_rev(), original_rev);
                assert_eq!(other_child.session.input_rev(), other_rev);
                assert_eq!(app.active_instance, Some(other));
                assert!(other_view.read(cx).focus_handle(cx).is_focused(window));
                let fresh = app
                    .terminal_input_admission(pane, cx)
                    .expect("fresh admission after Undo");
                app.insert_transcript(Some(fresh), "fresh transcript", true, window, cx);
                assert!(
                    child.session.input_rev() > original_rev,
                    "fresh production callback did not type"
                );
                assert_eq!(app.active_instance, Some(pane));
                assert!(view.read(cx).focus_handle(cx).is_focused(window));

                // Both sessions have epoch zero: an epoch-only comparison would
                // wrongly admit this callback into a replacement with the same iid.
                assert_eq!(other_child.session.input_epoch(), 0);
                assert_eq!(replacement_child.session.input_epoch(), 0);
                let replaced = app
                    .terminal_input_admission(other, cx)
                    .expect("pre-replacement admission");
                app.terminals.remove(&replacement_id);
                app.terminals.insert(other, replacement_view.clone());
                muxel_core::remove(
                    &mut app.workspace.project_mut(pid).unwrap().layout,
                    replacement_id,
                );
                app.workspace.remove_instance_meta(replacement_id);
                assert!(!Arc::ptr_eq(
                    app.terminal_view(other).unwrap().read(cx).session(),
                    &other_child.session,
                ));
                let current_rev = child.session.input_rev();
                let abandoned_rev = other_child.session.input_rev();
                let replacement_rev = replacement_child.session.input_rev();
                app.insert_transcript(Some(replaced), "wrong launch transcript", true, window, cx);
                assert_eq!(child.session.input_rev(), current_rev);
                assert_eq!(other_child.session.input_rev(), abandoned_rev);
                assert_eq!(replacement_child.session.input_rev(), replacement_rev);
                assert_eq!(app.active_instance, Some(pane));
                assert!(view.read(cx).focus_handle(cx).is_focused(window));
                assert!(!child.exited());
                assert!(!other_child.exited());
                assert!(!replacement_child.exited());
            })
            .unwrap();
    });
}
