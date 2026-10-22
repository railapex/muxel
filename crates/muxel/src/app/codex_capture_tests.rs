use super::MuxelApp;
use super::session_discovery::{
    inject_codex_launch_lookup, inject_codex_restore_scan, inject_codex_resume_lookup,
};
use crate::theme;
use gpui::{TestAppContext, WindowHandle};
use muxel_core::{AgentPreset, CodexResumePath, CodexSession, Instance, Project, RemoteRef};
use muxel_terminal::{CommandSpec, TerminalLaunch};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use std::time::Instant;
use uuid::Uuid;

struct TempRoot(PathBuf);
static CODEX_CAPTURE_FIXTURE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn pending_codex_reconnect_keeps_one_launch_token() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        window
            .update(cx, |app, window, cx| {
                let instance = app.workspace.instance_mut(pane).unwrap();
                instance.session_id = Some("01a1543c-9700-7000-8000-000000000001".into());
                instance.use_tmux = true;
                instance.tmux_session = Some("muxel_review_deadbeef".into());
                instance.tmux_socket = Some(muxel_core::tmux::Socket::Private);
                let command = CommandSpec::program("cmd.exe", vec!["/C".into(), "exit 37".into()]);
                let launch = TerminalLaunch::spawn_for_instance(command, (80, 24), pane).unwrap();
                app.install_terminal_spawn(
                    super::TerminalSpawnMeta {
                        instance_id: pane,
                        token: 0,
                        program: "cmd.exe".into(),
                        cwd: String::new(),
                        was_resume: true,
                        started: Instant::now(),
                    },
                    Ok(launch),
                    window,
                    cx,
                );
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            if window
                .update(cx, |app, _, cx| {
                    app.terminals.get(&pane).unwrap().read(cx).exited()
                })
                .unwrap()
            {
                break;
            }
            assert!(Instant::now() < deadline, "test child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        window
            .update(cx, |app, window, cx| {
                app.terminal_launches.get_mut(&pane).unwrap().0 =
                    Instant::now() - Duration::from_secs(60);
                // Skip the first-outage tmux probe; this test has no tmux server.
                app.reconnecting.insert(pane, 1);
                app.tick(window, cx);
                let first = app.terminal_launching.get(&pane).unwrap().token;
                // The worker cannot finish while this UI update is running.
                app.tick(window, cx);
                let second = app.terminal_launching.remove(&pane).unwrap().token;
                assert_eq!(first, second);
            })
            .unwrap();
    });
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

fn with_app(run: impl FnOnce(&mut TestAppContext, WindowHandle<MuxelApp>)) {
    let _fixture = CODEX_CAPTURE_FIXTURE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = std::env::temp_dir().join(format!("muxel-codex-capture-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let root = root.canonicalize().unwrap();
    let _temp = TempRoot(root.clone());
    let _storage = muxel_store::test_support::set_root(root.clone());
    assert_eq!(muxel_store::data_dir(), Some(root.join("data")));
    assert_eq!(muxel_store::config_dir(), Some(root.join("config")));
    let mut cx = TestAppContext::single();
    cx.skip_drawing();
    cx.update(|cx| {
        gpui_component::init(cx);
        theme::register_bundled_themes(cx);
        cx.set_global(theme::UiScale(1.0));
        cx.set_global(theme::UiFontSize(14.0));
        theme::apply_initial_theme("", cx);
    });
    let window = cx.add_window(MuxelApp::new);
    run(&mut cx, window);
}

fn codex_pane(cx: &mut TestAppContext, window: WindowHandle<MuxelApp>) -> Uuid {
    window
        .update(cx, |app, _, _| {
            let cwd = muxel_store::data_dir().unwrap().join("project");
            std::fs::create_dir_all(&cwd).unwrap();
            let project = app.workspace.add_project(Project::new("capture", cwd));
            let preset = AgentPreset::codex();
            let mut instance = Instance::from_preset(project, &preset);
            instance.session_started = true;
            let pane = instance.id;
            app.presets.push(preset);
            app.workspace.add_instance(instance);
            app.current_workspace.get_or_insert_with(Uuid::new_v4);
            app.use_tmux = false;
            app.use_worktree = false;
            pane
        })
        .unwrap()
}

#[test]
fn codex_launch_preparation_queues_slow_recovery_without_waiting() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let _lookup = inject_codex_restore_scan(Arc::new(move |_, _, id| {
            assert!(id.is_none());
            std::thread::sleep(Duration::from_millis(350));
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(None)
        }));
        window
            .update(cx, |app, window, cx| {
                let token = app.reserve_terminal_spawn(pane, window, cx).unwrap();
                app.launch_reserved_terminal_async(pane, token, window, cx);
                app.terminal_launching.remove(&pane); // prevent the test from spawning Codex
            })
            .unwrap();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "lookup ran during UI preparation"
        );
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(1));
        let deadline = Instant::now() + Duration::from_secs(3);
        while calls.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        cx.run_until_parked();
    });
}

#[test]
fn restored_codex_preflight_remains_owned_by_its_wave() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let _lookup = inject_codex_restore_scan(Arc::new(|_, _, _| {
            panic!("restore preparation detached the Codex scan")
        }));
        window
            .update(cx, |app, window, cx| {
                let prepared = app
                    .prepare_restored_terminal_spawn(pane, window, cx)
                    .unwrap();
                let super::RestoredTerminalPreparation::CodexPreflight(token, _) = prepared else {
                    panic!("started Codex pane did not enter restore preflight")
                };
                assert!(super::launch_token_current(
                    &app.terminal_launching,
                    pane,
                    token
                ));
                app.terminal_launching.remove(&pane);
            })
            .unwrap();
        cx.run_until_parked();
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
    });
}

#[test]
fn missing_legacy_codex_rollout_starts_fresh_inside_tmux() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let id = "01a1543c-9700-7000-8000-000000000001";
        let _dated = inject_codex_resume_lookup(Arc::new(|_, _, _| Ok(CodexResumePath::Unknown)));
        let _scan = inject_codex_restore_scan(Arc::new(|_, _, wanted| {
            assert_eq!(wanted, Some("01a1543c-9700-7000-8000-000000000001"));
            Ok(None)
        }));
        window
            .update(cx, |app, window, cx| {
                let instance = app.workspace.instance_mut(pane).unwrap();
                instance.session_id = Some(id.into());
                instance.use_tmux = true;
                instance.tmux_session = Some("muxel_capture_deadbeef".into());
                instance.tmux_socket = Some(muxel_core::tmux::Socket::Private);
                let token = app.reserve_terminal_spawn(pane, window, cx).unwrap();
                let request = app.codex_launch_preflight_request(pane).unwrap();
                let finding = super::session_discovery::codex_launch_preflight(&request);
                assert!(app.apply_codex_launch_preflight(pane, token, &request, finding));
                let (meta, spec, _) = app
                    .prepare_reserved_terminal_spawn(pane, token, cx)
                    .unwrap();
                assert!(!meta.was_resume);
                assert!(!spec.args.join(" ").contains(&format!("resume {id}")));
                assert!(app.workspace.instance(pane).unwrap().session_id.is_none());
            })
            .unwrap();
    });
}

#[test]
fn incomplete_legacy_codex_scan_keeps_the_saved_id() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let id = "01a1543c-9700-7000-8000-000000000001";
        let _dated = inject_codex_resume_lookup(Arc::new(|_, _, _| Ok(CodexResumePath::Unknown)));
        let _scan = inject_codex_restore_scan(Arc::new(|_, _, _| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }));
        window
            .update(cx, |app, window, cx| {
                app.workspace.instance_mut(pane).unwrap().session_id = Some(id.into());
                let token = app.reserve_terminal_spawn(pane, window, cx).unwrap();
                let request = app.codex_launch_preflight_request(pane).unwrap();
                let finding = super::session_discovery::codex_launch_preflight(&request);
                assert!(app.apply_codex_launch_preflight(pane, token, &request, finding));
                let instance = app.workspace.instance(pane).unwrap();
                assert_eq!(instance.session_id.as_deref(), Some(id));
                assert!(instance.session_started);
            })
            .unwrap();
    });
}

#[test]
fn unique_legacy_codex_rollout_recovers_idless_pane() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let id = "01a1543c-9700-7000-8000-000000000001";
        let path = muxel_store::data_dir()
            .unwrap()
            .join(format!("rollout-{id}.jsonl"));
        let found = path.clone();
        let _scan = inject_codex_restore_scan(Arc::new(move |_, _, wanted| {
            assert!(wanted.is_none());
            Ok(Some(CodexSession {
                id: id.into(),
                path: found.clone(),
            }))
        }));
        window
            .update(cx, |app, window, cx| {
                let token = app.reserve_terminal_spawn(pane, window, cx).unwrap();
                let request = app.codex_launch_preflight_request(pane).unwrap();
                let finding = super::session_discovery::codex_launch_preflight(&request);
                assert!(app.apply_codex_launch_preflight(pane, token, &request, finding));
                let instance = app.workspace.instance(pane).unwrap();
                assert_eq!(instance.session_id.as_deref(), Some(id));
                assert_eq!(instance.session_path.as_ref(), Some(&path));
                assert!(instance.session_started);
            })
            .unwrap();
    });
}

#[test]
fn ambiguous_legacy_codex_rollouts_start_fresh_with_one_import_notice() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let _scan = inject_codex_restore_scan(Arc::new(|_, _, wanted| {
            assert!(wanted.is_none());
            Ok(None)
        }));
        window
            .update(cx, |app, window, cx| {
                let token = app.reserve_terminal_spawn(pane, window, cx).unwrap();
                let request = app.codex_launch_preflight_request(pane).unwrap();
                let finding = super::session_discovery::codex_launch_preflight(&request);
                let notices_before = app.notifications.len();
                assert!(app.apply_codex_launch_preflight(pane, token, &request, finding));
                let instance = app.workspace.instance(pane).unwrap();
                assert!(!instance.session_started);
                assert!(instance.session_id.is_none());
                assert_eq!(app.notifications.len(), notices_before + 1);
                assert!(
                    app.notifications
                        .last()
                        .unwrap()
                        .subtitle
                        .contains("Import")
                );
                assert!(!app.apply_codex_launch_preflight(
                    pane,
                    token,
                    &request,
                    super::session_discovery::CodexLaunchResult::Missing
                ));
                assert_eq!(app.notifications.len(), notices_before + 1);
            })
            .unwrap();
    });
}

#[test]
fn remote_codex_resume_does_not_touch_the_local_session_store() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let _lookup = inject_codex_launch_lookup(Arc::new(|_, _| panic!("remote history scan")));
        let _stat =
            inject_codex_resume_lookup(Arc::new(|_, _, _| panic!("remote local-store stat")));
        let id = "01a1543c-9700-7000-8000-000000000001";
        window
            .update(cx, |app, _, _| {
                let instance = app.workspace.instance_mut(pane).unwrap();
                instance.session_id = Some(id.into());
                let project = instance.project_id;
                app.workspace.project_mut(project).unwrap().remote = Some(RemoteRef {
                    host_id: Uuid::new_v4(),
                    remote_root: "/remote/project".into(),
                });
                let (args, _) = app.session_resume_for(pane).unwrap().unwrap();
                assert_eq!(args, vec!["resume", id]);
                assert_eq!(
                    app.workspace.instance(pane).unwrap().session_id.as_deref(),
                    Some(id)
                );
            })
            .unwrap();
    });
}

#[test]
fn exact_codex_identity_is_resumed_and_its_path_is_persisted() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let id = "01a1543c-9700-7000-8000-000000000001";
        let path = muxel_store::data_dir()
            .unwrap()
            .join(format!("rollout-{id}.jsonl"));
        let expected = path.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let _lookup =
            inject_codex_launch_lookup(Arc::new(|_, _| panic!("exact identity used latest")));
        let _stat = inject_codex_resume_lookup(Arc::new(move |_, saved, _| {
            assert_eq!(saved, id);
            observed.fetch_add(1, Ordering::Relaxed);
            Ok(CodexResumePath::Found(expected.clone()))
        }));
        window
            .update(cx, |app, _, _| {
                app.workspace.instance_mut(pane).unwrap().session_id = Some(id.into());
                let (args, _) = app.session_resume_for(pane).unwrap().unwrap();
                assert_eq!(args, vec!["resume", id]);
                assert_eq!(
                    app.workspace.instance(pane).unwrap().session_path.as_ref(),
                    Some(&path)
                );
            })
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let document = window
            .update(cx, |app, _, _| {
                muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap()
            })
            .unwrap();
        let saved = muxel_store::load_workspace_from(&document).unwrap();
        assert_eq!(
            saved.instance(pane).unwrap().session_path.as_ref(),
            Some(&path)
        );
    });
}

#[test]
fn codex_store_error_keeps_the_saved_resume_identity() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let _stat = inject_codex_resume_lookup(Arc::new(|_, _, _| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        }));
        let id = "01a1543c-9700-7000-8000-000000000001";
        window
            .update(cx, |app, _, _| {
                app.workspace.instance_mut(pane).unwrap().session_id = Some(id.into());
                let (args, _) = app.session_resume_for(pane).unwrap().unwrap();
                assert_eq!(args, vec!["resume", id]);
                assert!(app.workspace.instance(pane).unwrap().session_started);
            })
            .unwrap();
    });
}

#[test]
fn inconclusive_codex_resume_survives_unrelated_early_exit_recovery() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let id = "01a1543c-9700-7000-8000-000000000001";
        let _lookup = inject_codex_resume_lookup(Arc::new(|_, _, _| Ok(CodexResumePath::Unknown)));
        let document = window
            .update(cx, |app, _, _| {
                app.workspace.instance_mut(pane).unwrap().session_id = Some(id.into());
                let (args, _) = app.session_resume_for(pane).unwrap().unwrap();
                assert_eq!(args, vec!["resume", id]);
                // This is the decision used by tick after a direct resume exits
                // nonzero within ten seconds for an unrelated reason.
                assert!(!app.should_recover_expired_resume(pane, false, true));
                assert!(app.should_recover_expired_resume(pane, true, false));
                app.persist();
                muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap()
            })
            .unwrap();
        let saved = muxel_store::load_workspace_from(&document).unwrap();
        assert_eq!(
            saved.instance(pane).unwrap().session_id.as_deref(),
            Some(id)
        );
    });
}

#[test]
fn exited_codex_terminal_keeps_inconclusive_resume_through_tick() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let id = "01a1543c-9700-7000-8000-000000000001";
        let _lookup = inject_codex_resume_lookup(Arc::new(|_, _, _| Ok(CodexResumePath::Unknown)));
        let document = window
            .update(cx, |app, window, cx| {
                app.workspace.instance_mut(pane).unwrap().session_id = Some(id.into());
                app.workspace.instance_mut(pane).unwrap().session_path = Some(
                    muxel_store::data_dir()
                        .unwrap()
                        .join(format!("rollout-{id}.jsonl")),
                );
                let token = app.reserve_terminal_spawn(pane, window, cx).unwrap();
                let (meta, spec, size) = app
                    .prepare_reserved_terminal_spawn(pane, token, cx)
                    .unwrap();
                assert!(meta.was_resume);
                assert!(spec.args.iter().any(|arg| arg == "resume"));
                #[cfg(windows)]
                let command = CommandSpec::program("cmd.exe", vec!["/C".into(), "exit 37".into()]);
                #[cfg(not(windows))]
                let command = CommandSpec::program("/bin/sh", vec!["-c".into(), "exit 37".into()]);
                let launch = TerminalLaunch::spawn_for_instance(command, size, pane).unwrap();
                app.finish_terminal_spawn(meta, Ok(launch), window, cx);
                muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap()
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            cx.run_until_parked();
            // The unfocused terminal drain coalesces output on GPUI's virtual
            // clock; an `exit 37` can produce output before its exit event.
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            let exited = window
                .update(cx, |app, _, cx| {
                    app.terminals.get(&pane).unwrap().read(cx).exited()
                })
                .unwrap();
            if exited {
                break;
            }
            assert!(Instant::now() < deadline, "test child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        window
            .update(cx, |app, window, cx| app.tick(window, cx))
            .unwrap();
        let saved = muxel_store::load_workspace_from(&document).unwrap();
        assert_eq!(
            saved.instance(pane).unwrap().session_id.as_deref(),
            Some(id)
        );
        window
            .update(cx, |app, _, cx| {
                let view = app.terminals.get(&pane).unwrap().read(cx);
                assert!(view.exited());
                assert_eq!(view.exit_code(), Some(37));
                assert_eq!(
                    app.workspace.instance(pane).unwrap().session_id.as_deref(),
                    Some(id)
                );
            })
            .unwrap();
    });
}

#[test]
fn clean_tmux_client_exit_after_codex_resume_keeps_the_pane() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let id = "01a1543c-9700-7000-8000-000000000001";
        let _lookup = inject_codex_resume_lookup(Arc::new(|_, _, _| Ok(CodexResumePath::Unknown)));
        window
            .update(cx, |app, window, cx| {
                let instance = app.workspace.instance_mut(pane).unwrap();
                instance.session_id = Some(id.into());
                instance.session_path = Some(
                    muxel_store::data_dir()
                        .unwrap()
                        .join(format!("rollout-{id}.jsonl")),
                );
                instance.use_tmux = true;
                instance.tmux_session = Some("muxel_capture_deadbeef".into());
                instance.tmux_socket = Some(muxel_core::tmux::Socket::Private);
                app.settings.close_on_exit = true;
                let token = app.reserve_terminal_spawn(pane, window, cx).unwrap();
                let (meta, _, size) = app
                    .prepare_reserved_terminal_spawn(pane, token, cx)
                    .unwrap();
                assert!(meta.was_resume);
                #[cfg(windows)]
                let command = CommandSpec::program("cmd.exe", vec!["/C".into(), "exit 0".into()]);
                #[cfg(not(windows))]
                let command = CommandSpec::program("/bin/sh", vec!["-c".into(), "exit 0".into()]);
                let launch = TerminalLaunch::spawn_for_instance(command, size, pane).unwrap();
                app.finish_terminal_spawn(meta, Ok(launch), window, cx);
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            cx.run_until_parked();
            cx.executor().advance_clock(Duration::from_millis(100));
            cx.run_until_parked();
            if window
                .update(cx, |app, _, cx| {
                    app.terminals.get(&pane).unwrap().read(cx).exited()
                })
                .unwrap()
            {
                break;
            }
            assert!(Instant::now() < deadline, "test child did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        window
            .update(cx, |app, window, cx| {
                app.tick(window, cx);
                assert!(app.workspace.instance(pane).is_some());
                assert_eq!(
                    app.workspace.instance(pane).unwrap().session_id.as_deref(),
                    Some(id)
                );
                app.terminal_launches.get_mut(&pane).unwrap().0 =
                    Instant::now() - Duration::from_secs(super::RECOVER_WITHIN_SECS + 1);
                app.tick(window, cx);
                assert!(app.workspace.instance(pane).is_some());
            })
            .unwrap();
    });
}

#[test]
fn inconclusive_codex_lookups_keep_the_saved_resume_identity() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let root = muxel_store::data_dir().unwrap().join("codex-home");
        let v7 = "01a1543c-9700-7000-8000-000000000001";
        let old = Uuid::new_v4().to_string();
        let wrong_day = root.join(".codex/sessions/2020/01/01");
        std::fs::create_dir_all(&wrong_day).unwrap();
        std::fs::write(wrong_day.join(format!("rollout-{v7}.jsonl")), "{}").unwrap();
        let lookup_root = root.clone();
        let _lookup = inject_codex_resume_lookup(Arc::new(move |_, id, saved| {
            muxel_core::codex_resume_path(&lookup_root, id, saved)
        }));
        let invalid_path = root.join("outside").join(format!("rollout-{v7}.jsonl"));
        for (id, path) in [(old.as_str(), None), (v7, Some(invalid_path)), (v7, None)] {
            window
                .update(cx, |app, _, _| {
                    let instance = app.workspace.instance_mut(pane).unwrap();
                    instance.session_id = Some(id.into());
                    instance.session_path = path.clone();
                    instance.session_started = true;
                    let (args, _) = app.session_resume_for(pane).unwrap().unwrap();
                    assert_eq!(args, vec!["resume", id]);
                    assert_eq!(
                        app.workspace.instance(pane).unwrap().session_id.as_deref(),
                        Some(id)
                    );
                })
                .unwrap();
        }
    });
}

#[test]
fn disappeared_saved_codex_path_clears_the_binding() {
    with_app(|cx, window| {
        let pane = codex_pane(cx, window);
        let root = muxel_store::data_dir().unwrap().join("codex-home");
        let id = "01a1543c-9700-7000-8000-000000000001";
        let path = root
            .join(".codex/sessions/2026/10/19")
            .join(format!("rollout-{id}.jsonl"));
        let lookup_root = root.clone();
        let _lookup = inject_codex_resume_lookup(Arc::new(move |_, id, saved| {
            muxel_core::codex_resume_path(&lookup_root, id, saved)
        }));
        window
            .update(cx, |app, _, _| {
                let instance = app.workspace.instance_mut(pane).unwrap();
                instance.session_id = Some(id.into());
                instance.session_path = Some(path);
                instance.session_started = true;
                assert!(app.session_resume_for(pane).unwrap().is_none());
                let instance = app.workspace.instance(pane).unwrap();
                assert!(instance.session_id.is_none());
                assert!(!instance.session_started);
            })
            .unwrap();
    });
}
