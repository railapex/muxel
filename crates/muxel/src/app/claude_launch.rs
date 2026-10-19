//! Reserve on the UI thread, resolve transcripts once per batch on a worker,
//! then build the command only if the reservation and binding still match.

use super::{
    MuxelApp, TerminalSpawnMeta, home_dir, is_claude_program, launch_token_current,
    release_launch_token, session_id_bound_elsewhere,
};
use crate::{session_binding, ui_profile};
use gpui::{AsyncApp, BackgroundExecutor, Context, WeakEntity};
use muxel_core::{AgentPreset, ClaudeSession, ClaudeSessionRequest, InstanceKind};
use muxel_terminal::{CommandSpec, TerminalLaunch};
use std::collections::HashSet;
use std::path::PathBuf;
use uuid::Uuid;

type Ready = (TerminalSpawnMeta, CommandSpec, (u16, u16));

#[derive(Clone, PartialEq, Eq)]
struct LaunchKey {
    workspace: Option<Uuid>,
    pane: Uuid,
    project: Uuid,
    token: u64,
    generation: u64,
    id: Option<String>,
    path: Option<PathBuf>,
    started: bool,
    cwd: PathBuf,
    preset: AgentPreset,
    program: Option<String>,
    args: Vec<String>,
}

#[derive(Clone)]
struct Request {
    key: LaunchKey,
    home: Option<PathBuf>,
    data: Option<PathBuf>,
    siblings: HashSet<String>,
}

enum Probe {
    Found(ClaudeSession),
    Missing,
    Unknown,
    Refused(String),
}

struct Resolution {
    id: Option<String>,
    hook: bool,
    probe: Probe,
}

struct Pending {
    request: Request,
    sender: Option<async_channel::Sender<Resolution>>,
    receiver: async_channel::Receiver<Resolution>,
}

pub(super) struct TerminalPreparation {
    pub meta: TerminalSpawnMeta,
    ready: Option<(CommandSpec, (u16, u16))>,
    pending: Option<Pending>,
}

impl TerminalPreparation {
    pub(super) fn ready((meta, spec, size): Ready) -> Self {
        Self {
            meta,
            ready: Some((spec, size)),
            pending: None,
        }
    }
    #[cfg(test)]
    pub(super) fn ready_parts(self) -> Option<Ready> {
        self.ready.map(|(spec, size)| (self.meta, spec, size))
    }
}

pub(super) fn start_batch(preparations: &mut [TerminalPreparation], executor: &BackgroundExecutor) {
    let jobs: Vec<_> = preparations
        .iter_mut()
        .filter_map(|prepared| {
            let pending = prepared.pending.as_mut()?;
            Some((pending.request.clone(), pending.sender.take()?))
        })
        .collect();
    if !jobs.is_empty() {
        executor
            .spawn(async move {
                resolve_batch(jobs);
            })
            .detach();
    }
}

fn resolve_batch(jobs: Vec<(Request, async_channel::Sender<Resolution>)>) {
    let Some(home) = jobs.first().and_then(|(request, _)| request.home.as_ref()) else {
        for (request, sender) in jobs {
            let _ = sender.try_send(Resolution {
                id: request.key.id,
                hook: false,
                probe: Probe::Unknown,
            });
        }
        return;
    };
    let panes = jobs.iter().map(|(request, _)| request.key.pane).collect();
    let bindings = jobs
        .first()
        .and_then(|(request, _)| request.data.as_deref())
        .map(|data| session_binding::claude_bindings_for_batch(data, &panes, home))
        .transpose();
    let bindings = match bindings {
        Ok(bindings) => bindings.unwrap_or_default(),
        Err(error) => {
            for (request, sender) in jobs {
                let _ = sender.try_send(Resolution {
                    id: request.key.id,
                    hook: false,
                    probe: Probe::Refused(error.to_string()),
                });
            }
            return;
        }
    };
    let mut lookups = Vec::new();
    let mut pending = Vec::new();
    for (request, sender) in &jobs {
        let hook = bindings
            .get(&request.key.pane)
            .filter(|binding| !request.siblings.contains(&binding.id));
        let id = hook
            .map(|binding| binding.id.clone())
            .or_else(|| request.key.id.clone());
        if let Some(binding) = hook.filter(|binding| binding.pending_write) {
            let _ = sender.try_send(Resolution {
                id,
                hook: true,
                probe: Probe::Found(ClaudeSession {
                    path: binding.path.clone(),
                    cwd: binding.cwd.is_dir().then(|| binding.cwd.clone()),
                }),
            });
            continue;
        }
        let Some(session_id) = id.clone().filter(|_| request.key.started || hook.is_some()) else {
            let _ = sender.try_send(Resolution {
                id,
                hook: false,
                probe: Probe::Unknown,
            });
            continue;
        };
        #[cfg(test)]
        if let Some(probe) = super::CLAUDE_LAUNCH_TEST_LOOKUP.lock().unwrap().clone() {
            probe();
            let _ = sender.try_send(Resolution {
                id,
                hook: hook.is_some(),
                probe: Probe::Unknown,
            });
            continue;
        }
        lookups.push(ClaudeSessionRequest {
            id: session_id,
            path: hook
                .map(|binding| binding.path.clone())
                .or_else(|| request.key.path.clone()),
            cwd: Some(request.key.cwd.clone()),
        });
        pending.push((id, hook.is_some(), sender));
    }
    let _phase = ui_profile::phase("background-discovery", "claude-launch-batch", None);
    muxel_core::discover_claude_sessions(home, &lookups, |index, result| {
        let (id, hook, sender) = &pending[index];
        let probe = match result {
            Ok(Some(session)) => Probe::Found(session),
            Ok(None) => Probe::Missing,
            Err(error) => {
                log::warn!("could not check Claude transcript before resume: {error}");
                Probe::Unknown
            }
        };
        let _ = sender.try_send(Resolution {
            id: id.clone(),
            hook: *hook,
            probe,
        });
    });
}

impl MuxelApp {
    fn claude_launch_key(&self, pane: Uuid, token: u64) -> Option<LaunchKey> {
        let instance = self.workspace.instance(pane)?;
        let project = self.workspace.project(instance.project_id)?;
        if project.remote.is_some() || instance.kind != InstanceKind::Terminal {
            return None;
        }
        let preset = instance
            .preset_id
            .and_then(|id| self.presets.iter().find(|preset| preset.id == id))
            .or_else(|| {
                self.presets
                    .iter()
                    .find(|preset| preset.name == instance.preset)
            })?;
        if !is_claude_program(preset.program.as_deref()) || preset.resume_flag.is_none() {
            return None;
        }
        Some(LaunchKey {
            workspace: self.current_workspace,
            pane,
            project: project.id,
            token,
            generation: self.session_discoveries.binding_generation(pane),
            id: instance.session_id.clone(),
            path: instance.session_path.clone(),
            started: instance.session_started,
            cwd: instance
                .worktree_path
                .clone()
                .unwrap_or_else(|| project.root_path.clone()),
            preset: preset.clone(),
            program: instance.program.clone(),
            args: instance.args.clone(),
        })
    }

    pub(super) fn needs_claude_preflight(&self, pane: Uuid) -> bool {
        self.claude_launch_key(pane, 0).is_some()
    }

    pub(super) fn prepare_claude_launch(
        &self,
        pane: Uuid,
        token: u64,
    ) -> Option<TerminalPreparation> {
        let key = self.claude_launch_key(pane, token)?;
        let (sender, receiver) = async_channel::bounded(1);
        let request = Request {
            key,
            home: home_dir(),
            data: muxel_store::data_dir(),
            siblings: self
                .workspace
                .instances
                .iter()
                .filter(|instance| instance.id != pane)
                .filter_map(|instance| instance.session_id.clone())
                .collect(),
        };
        let meta = TerminalSpawnMeta {
            instance_id: pane,
            token,
            program: request.key.preset.program.clone().unwrap_or_default(),
            cwd: request.key.cwd.display().to_string(),
            was_resume: request.key.started,
            started: self.terminal_launching.get(&pane)?.since,
        };
        Some(TerminalPreparation {
            meta,
            ready: None,
            pending: Some(Pending {
                request,
                sender: Some(sender),
                receiver,
            }),
        })
    }

    fn apply_claude_launch(
        &mut self,
        request: Request,
        resolution: Resolution,
        cx: &mut Context<Self>,
    ) -> Option<Ready> {
        let key = request.key;
        if !launch_token_current(&self.terminal_launching, key.pane, key.token)
            || self.claude_launch_key(key.pane, key.token).as_ref() != Some(&key)
        {
            return None;
        }
        if let Probe::Refused(error) = resolution.probe {
            self.failed_launches.insert(
                key.pane,
                crate::i18n::tf(
                    "Could not recover this conversation before launch: {error}",
                    &[("error", &error)],
                ),
            );
            cx.notify();
            return None;
        }
        if resolution.hook
            && resolution.id.as_deref().is_some_and(|id| {
                session_id_bound_elsewhere(&self.workspace.instances, key.pane, id)
            })
        {
            return None;
        }
        let previous = self.workspace.instance(key.pane)?.clone();
        let instance = self.workspace.instance_mut(key.pane)?;
        let mut cwd = None;
        if resolution.hook {
            if instance.session_id != resolution.id {
                instance.auto_name = None;
            }
            instance.session_id = resolution.id;
            instance.session_started = true;
        }
        match resolution.probe {
            Probe::Found(session) => {
                instance.session_path = Some(session.path);
                cwd = session.cwd;
            }
            Probe::Missing => {
                instance.session_id = None;
                instance.session_path = None;
                instance.session_started = false;
            }
            Probe::Unknown => {}
            Probe::Refused(_) => unreachable!(),
        }
        // Persist a fresh host-owned identity before command construction too.
        // There is no provider discovery for a conversation we have yet to start.
        if instance.session_id.is_none() && key.preset.session_id_flag.is_some() {
            instance.session_id = Some(Uuid::new_v4().to_string());
            instance.session_started = false;
            instance.session_path = None;
        }
        if !instance.session_started
            && instance.session_path.is_none()
            && let (Some(home), Some(id)) =
                (request.home.as_deref(), instance.session_id.as_deref())
        {
            instance.session_path = Some(muxel_core::claude_session_path(home, &key.cwd, id));
        }
        if instance.session_id != previous.session_id
            || instance.session_path != previous.session_path
            || instance.session_started != previous.session_started
        {
            self.bump_session_binding_generation(key.pane);
            if !self.try_persist() {
                *self.workspace.instance_mut(key.pane)? = previous;
                self.bump_session_binding_generation(key.pane);
                self.failed_launches.insert(
                    key.pane,
                    crate::i18n::t("Could not save this conversation before launch.").to_string(),
                );
                cx.notify();
                return None;
            }
        }
        self.prepare_resolved_terminal_spawn(key.pane, key.token, cwd, cx)
    }

    pub(super) async fn run_terminal_preparation(
        this: &WeakEntity<Self>,
        prepared: TerminalPreparation,
        cx: &mut AsyncApp,
    ) -> Option<(TerminalSpawnMeta, anyhow::Result<TerminalLaunch>)> {
        let pane = prepared.meta.instance_id;
        let token = prepared.meta.token;
        let ready = if let Some((spec, size)) = prepared.ready {
            Some((prepared.meta, spec, size))
        } else if let Some(pending) = prepared.pending {
            match pending.receiver.recv().await {
                Ok(resolution) => this
                    .update(cx, |app, cx| {
                        app.apply_claude_launch(pending.request, resolution, cx)
                    })
                    .ok()
                    .flatten(),
                Err(_) => None,
            }
        } else {
            None
        };
        let Some((meta, spec, size)) = ready else {
            let _ = this.update(cx, |app, _| {
                release_launch_token(&mut app.terminal_launching, pane, token)
            });
            return None;
        };
        let result = cx
            .background_executor()
            .spawn(async move { TerminalLaunch::spawn_for_instance(spec, size, pane) })
            .await;
        Some((meta, result))
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::{Probe, Resolution, resolve_batch};
    use crate::app::{MuxelApp, codex_capture_tests::with_app};
    use muxel_core::{AgentPreset, ClaudeSession, Instance, Project};
    use uuid::Uuid;

    fn pane(app: &mut MuxelApp) -> Uuid {
        let cwd = muxel_store::data_dir().unwrap().join("claude-project");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = app.workspace.add_project(Project::new("Claude", cwd));
        let preset = AgentPreset::claude();
        let mut instance = Instance::from_preset(project, &preset);
        instance.session_id = Some(Uuid::new_v4().to_string());
        instance.session_started = true;
        let id = instance.id;
        app.workspace.add_instance(instance);
        app.presets.push(preset);
        app.current_workspace = Some(Uuid::new_v4());
        id
    }

    #[test]
    fn claude_preflight_persists_path_and_builds_resume_in_recorded_cwd() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let prepared = app.prepare_terminal_spawn(id, window, cx).unwrap();
                    let started = prepared.meta.started;
                    let request = prepared.pending.unwrap().request;
                    let sid = request.key.id.clone().unwrap();
                    let path = muxel_store::data_dir()
                        .unwrap()
                        .join(format!("{sid}.jsonl"));
                    let moved = muxel_store::data_dir().unwrap().join("moved");
                    std::fs::create_dir_all(&moved).unwrap();
                    let (meta, command, _) = app
                        .apply_claude_launch(
                            request,
                            Resolution {
                                id: Some(sid.clone()),
                                hook: false,
                                probe: Probe::Found(ClaudeSession {
                                    path: path.clone(),
                                    cwd: Some(moved.clone()),
                                }),
                            },
                            cx,
                        )
                        .unwrap();
                    assert_eq!(
                        meta.started, started,
                        "discovery must count toward admission deadline"
                    );
                    assert_eq!(command.cwd.as_deref(), Some(moved.to_str().unwrap()));
                    assert!(
                        command
                            .args
                            .windows(2)
                            .any(|pair| pair == ["--resume", sid.as_str()])
                    );
                    let saved = muxel_store::load_workspace_from(
                        &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        saved.instance(id).unwrap().session_path.as_ref(),
                        Some(&path)
                    );
                    assert_eq!(
                        saved.instance(id).unwrap().session_id.as_deref(),
                        Some(sid.as_str())
                    );
                })
                .unwrap();
        });
    }

    #[test]
    fn claude_preflight_rejects_stale_binding_workspace_and_launch_then_accepts_current() {
        for stale in ["binding", "workspace", "launch", "program"] {
            with_app(|cx, window| {
                window
                    .update(cx, |app, window, cx| {
                        let id = pane(app);
                        let old = app.workspace.instance(id).unwrap().session_id.clone();
                        let pending = app
                            .prepare_terminal_spawn(id, window, cx)
                            .unwrap()
                            .pending
                            .unwrap();
                        match stale {
                            "binding" => app.bump_session_binding_generation(id),
                            "workspace" => app.current_workspace = Some(Uuid::new_v4()),
                            "launch" => {
                                app.terminal_launching.get_mut(&id).unwrap().token += 1;
                            }
                            "program" => {
                                app.workspace.instance_mut(id).unwrap().program =
                                    Some("cmd.exe".into())
                            }
                            _ => unreachable!(),
                        }
                        let candidate = Uuid::new_v4().to_string();
                        assert!(
                            app.apply_claude_launch(
                                pending.request,
                                Resolution {
                                    id: Some(candidate),
                                    hook: true,
                                    probe: Probe::Unknown,
                                },
                                cx
                            )
                            .is_none()
                        );
                        assert_eq!(app.workspace.instance(id).unwrap().session_id, old);
                        app.workspace.instance_mut(id).unwrap().program = Some("claude".into());
                        let token = app.terminal_launching[&id].token;
                        let current = app
                            .prepare_claude_launch(id, token)
                            .unwrap()
                            .pending
                            .unwrap()
                            .request;
                        assert!(
                            app.apply_claude_launch(
                                current,
                                Resolution {
                                    id: old.clone(),
                                    hook: false,
                                    probe: Probe::Unknown,
                                },
                                cx
                            )
                            .is_some(),
                            "current request rejected after {stale}"
                        );
                        assert_eq!(app.workspace.instance(id).unwrap().session_id, old);
                    })
                    .unwrap();
            });
        }
    }

    #[test]
    fn claude_preflight_rechecks_sibling_ownership_at_completion() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let pending = app
                        .prepare_terminal_spawn(id, window, cx)
                        .unwrap()
                        .pending
                        .unwrap();
                    let old = app.workspace.instance(id).unwrap().session_id.clone();
                    let sibling = pane(app);
                    // Keep the same workspace; this test isolates the sibling guard.
                    app.current_workspace = pending.request.key.workspace;
                    let candidate = app.workspace.instance(sibling).unwrap().session_id.clone();
                    assert!(
                        app.apply_claude_launch(
                            pending.request,
                            Resolution {
                                id: candidate,
                                hook: true,
                                probe: Probe::Unknown,
                            },
                            cx
                        )
                        .is_none()
                    );
                    assert_eq!(app.workspace.instance(id).unwrap().session_id, old);
                })
                .unwrap();
        });
    }

    #[test]
    fn claude_worker_completes_slow_probe_without_holding_the_ui_update() {
        with_app(|cx, window| {
            let (id, mut pending) = window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let pending = app
                        .prepare_terminal_spawn(id, window, cx)
                        .unwrap()
                        .pending
                        .unwrap();
                    (id, pending)
                })
                .unwrap();
            pending.request.home = Some(muxel_store::data_dir().unwrap().join("home"));
            let (started, observed) = std::sync::mpsc::channel();
            *crate::app::CLAUDE_LAUNCH_TEST_LOOKUP.lock().unwrap() =
                Some(std::sync::Arc::new(move || {
                    started.send(()).unwrap();
                    std::thread::sleep(std::time::Duration::from_millis(350));
                }));
            struct ClearProbe;
            impl Drop for ClearProbe {
                fn drop(&mut self) {
                    *crate::app::CLAUDE_LAUNCH_TEST_LOOKUP.lock().unwrap() = None;
                }
            }
            let _clear = ClearProbe;
            let request = pending.request.clone();
            let sender = pending.sender.take().unwrap();
            let worker = std::thread::spawn(move || resolve_batch(vec![(request, sender)]));
            observed
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            let clock = std::time::Instant::now();
            window
                .update(cx, |app, _, _| {
                    assert_eq!(
                        app.workspace.instance(id).unwrap().session_id,
                        pending.request.key.id
                    );
                    app.active_instance = Some(id);
                })
                .unwrap();
            assert!(clock.elapsed() < std::time::Duration::from_millis(100));
            worker.join().unwrap();
            let resolution = pending.receiver.recv_blocking().unwrap();
            window
                .update(cx, |app, _, cx| {
                    let (_, command, _) = app
                        .apply_claude_launch(pending.request, resolution, cx)
                        .unwrap();
                    assert!(command.args.iter().any(|arg| arg == "--resume"));
                })
                .unwrap();
        });
    }

    #[test]
    fn claude_preflight_save_failure_rolls_back_and_refuses_the_launch() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let old = app.workspace.instance(id).unwrap().clone();
                    let pending = app
                        .prepare_terminal_spawn(id, window, cx)
                        .unwrap()
                        .pending
                        .unwrap();
                    let document =
                        muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap();
                    std::fs::create_dir_all(&document).unwrap();
                    assert!(
                        app.apply_claude_launch(
                            pending.request,
                            Resolution {
                                id: Some(Uuid::new_v4().to_string()),
                                hook: true,
                                probe: Probe::Found(ClaudeSession {
                                    path: muxel_store::data_dir()
                                        .unwrap()
                                        .join("new-transcript.jsonl"),
                                    cwd: None
                                }),
                            },
                            cx
                        )
                        .is_none()
                    );
                    let current = app.workspace.instance(id).unwrap();
                    assert_eq!(current.session_id, old.session_id);
                    assert_eq!(current.session_path, old.session_path);
                    assert_eq!(current.session_started, old.session_started);
                    assert!(app.failed_launches.contains_key(&id));
                })
                .unwrap();
        });
    }

    #[test]
    fn confirmed_missing_claude_transcript_persists_a_fresh_identity_before_launch() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let old = app.workspace.instance(id).unwrap().session_id.clone();
                    let pending = app
                        .prepare_terminal_spawn(id, window, cx)
                        .unwrap()
                        .pending
                        .unwrap();
                    let (meta, command, _) = app
                        .apply_claude_launch(
                            pending.request,
                            Resolution {
                                id: old.clone(),
                                hook: false,
                                probe: Probe::Missing,
                            },
                            cx,
                        )
                        .unwrap();
                    assert!(!meta.was_resume);
                    assert!(command.args.iter().any(|arg| arg == "--session-id"));
                    assert!(!command.args.iter().any(|arg| arg == "--resume"));
                    let saved = muxel_store::load_workspace_from(
                        &muxel_store::workspace_doc_path(app.current_workspace.unwrap()).unwrap(),
                    )
                    .unwrap();
                    let pane = saved.instance(id).unwrap();
                    assert_ne!(pane.session_id, old);
                    assert!(pane.session_path.is_some());
                    assert!(!pane.session_started);
                })
                .unwrap();
        });
    }

    #[test]
    fn custom_agent_minted_claude_preset_starts_bare_after_a_confirmed_miss() {
        with_app(|cx, window| {
            window
                .update(cx, |app, window, cx| {
                    let id = pane(app);
                    let preset_id = app.workspace.instance(id).unwrap().preset_id.unwrap();
                    app.presets
                        .iter_mut()
                        .find(|preset| preset.id == preset_id)
                        .unwrap()
                        .session_id_flag = None;
                    let pending = app
                        .prepare_terminal_spawn(id, window, cx)
                        .unwrap()
                        .pending
                        .unwrap();
                    let (_, command, _) = app
                        .apply_claude_launch(
                            pending.request,
                            Resolution {
                                id: None,
                                hook: false,
                                probe: Probe::Missing,
                            },
                            cx,
                        )
                        .unwrap();
                    assert!(
                        !command
                            .args
                            .iter()
                            .any(|arg| arg == "--session-id" || arg == "--resume")
                    );
                    let instance = app.workspace.instance(id).unwrap();
                    assert!(instance.session_id.is_none() && instance.session_path.is_none());
                    assert!(!instance.session_started);
                })
                .unwrap();
        });
    }
}
