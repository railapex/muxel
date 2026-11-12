//! Sleeping idle local agents: end the program, keep the pane as a dormant
//! placeholder, and resume the same conversation when it is opened or woken.
//! The rules live in `muxel_core::sleep`; this gathers their facts and does the
//! stopping. Dormant restore's placeholder, focus wake and ctl refusals do the
//! rest, so a sleeping pane behaves like one deferred at launch.

use super::*;
use muxel_core::control::{self as ctl, AgentState, ErrorCode};
use muxel_core::sleep::{
    ResumeFile, SleepFacts, SleepRefusal, SleepRequest, SleepStatus, sleep_refusal,
};

/// How often the idle timer looks for agents to put to sleep.
const AUTO_SLEEP_EVERY: Duration = Duration::from_secs(60);
/// Ctrl+C, pressed twice: how an idle Claude or Codex TUI is quit by hand,
/// saving its conversation on the way out. A provider that ignores it is
/// ended by PID after the grace below.
const QUIT_KEY: &[u8] = b"\x03";
/// Long enough to land as a second press, inside each TUI's confirm window.
const QUIT_PRESS_GAP: Duration = Duration::from_millis(300);
/// How often a quitting agent is checked, and how many checks it gets (five
/// seconds) to quit by itself before muxel ends it by PID.
pub(super) const QUIT_POLL: Duration = Duration::from_millis(100);
const QUIT_POLLS: u32 = 50;

/// Who asked, for the event log.
#[derive(Clone, Copy)]
pub(super) enum SleepCause {
    Pane,
    IdleAgents,
    Timer,
}

impl SleepCause {
    fn word(self) -> &'static str {
        match self {
            Self::Pane => "pane menu",
            Self::IdleAgents => "sleep idle agents",
            Self::Timer => "idle timer",
        }
    }
}

impl MuxelApp {
    /// What the sleep rules need to know about pane `iid`, now.
    pub(super) fn sleep_facts(&self, iid: Uuid, cx: &App) -> Option<SleepFacts> {
        let inst = self.workspace.instance(iid)?;
        if inst.kind != InstanceKind::Terminal {
            return None;
        }
        let view = self.terminals.get(&iid);
        let status = if self.is_closing(iid) || self.is_dormant(iid) {
            SleepStatus::NotRunning
        } else if self.terminal_restarts.contains(iid) || self.terminal_launching.contains_key(&iid)
        {
            SleepStatus::Starting
        } else if view.is_none() {
            // A popped-out terminal runs in its own window, which the user
            // keeps open to watch it: it counts as focused, below.
            if self.popouts.contains_key(&iid) {
                SleepStatus::Idle
            } else {
                SleepStatus::NotRunning
            }
        } else {
            match self.control_state(inst, cx) {
                Some(AgentState::Idle) => SleepStatus::Idle,
                Some(AgentState::Done) => SleepStatus::Done,
                Some(AgentState::Working) => SleepStatus::Working,
                Some(AgentState::Blocked) => SleepStatus::Asking,
                Some(AgentState::Starting) => SleepStatus::Starting,
                _ => SleepStatus::NotRunning,
            }
        };
        let local = self
            .workspace
            .project(inst.project_id)
            .is_some_and(|project| !project.is_remote());
        let runtime = view.map(|view| view.read(cx).runtime_snapshot());
        Some(SleepFacts {
            agent: !inst.literal_argv
                && muxel_core::is_known_agent_program(inst.program.as_deref()),
            local_pty: local && !inst.use_tmux,
            resume_trusted: self.instance_resume_trusted(inst),
            status,
            // A turn held at Working for its background work, what the
            // provider's screen says still runs beside its turn, or a loop run
            // muxel is still watching through to its post-run step.
            background_held: runtime.is_some_and(|runtime| {
                runtime.reason == muxel_terminal::TerminalStatusReason::BackgroundWork
                    || runtime.background.is_some_and(|work| !work.is_empty())
            }) || self.running_loops.contains_key(&iid),
            focused: self.pane_focused(iid),
            popped_out: self.popouts.contains_key(&iid),
            unsent_text: runtime.is_some_and(|runtime| runtime.draft_latched),
            pinned: inst.pinned,
            // Counts the last wake: see `start_dormant`.
            last_active_at: inst.activity.last_active_at(),
        })
    }

    /// The preset `inst` launches with: by id, else by name.
    fn instance_preset(&self, inst: &Instance) -> Option<&AgentPreset> {
        inst.preset_id
            .and_then(|id| self.presets.iter().find(|preset| preset.id == id))
            .or_else(|| {
                self.presets
                    .iter()
                    .find(|preset| preset.name == inst.preset)
            })
    }

    /// Whether `inst` resumes its saved conversation when its program starts
    /// again: what makes waking it bring the same work back.
    pub(super) fn instance_resume_trusted(&self, inst: &Instance) -> bool {
        self.instance_preset(inst)
            .is_some_and(|preset| muxel_core::resume_trusted(preset, inst))
    }

    /// A bounded look for the file a resume of `inst` would read: one stat for
    /// Claude, one or two for Codex with a saved path, and a few dated
    /// directories for Codex without one and for Muse. It never walks a
    /// provider's whole store, so the UI thread may ask it, and it is asked
    /// only when the answer is needed: as the last sleep rule, and for an
    /// asleep pane's `resumes`. The launch path makes its own, fuller check.
    pub(super) fn resume_file(&self, inst: &Instance) -> ResumeFile {
        let Some(id) = inst.session_id.as_deref() else {
            return ResumeFile::Unknown;
        };
        let program = self
            .instance_preset(inst)
            .and_then(|preset| preset.program.as_deref())
            .or(inst.program.as_deref());
        if is_claude_program(program) {
            let path = match (&inst.session_path, home_dir()) {
                (Some(path), _) => path.clone(),
                (None, Some(home)) => {
                    let Some(cwd) = inst.working_directory().map(Path::to_path_buf).or_else(|| {
                        self.workspace
                            .project(inst.project_id)
                            .map(|project| project.root_path.clone())
                    }) else {
                        return ResumeFile::Unknown;
                    };
                    muxel_core::claude_session_path(&home, &cwd, id)
                }
                (None, None) => return ResumeFile::Unknown,
            };
            return match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => ResumeFile::Present,
                Ok(_) => ResumeFile::Gone,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => ResumeFile::Gone,
                Err(_) => ResumeFile::Unknown,
            };
        }
        if is_codex_program(program) {
            let Some(home) = home_dir() else {
                return ResumeFile::Unknown;
            };
            return match session_discovery::resume_codex_session(
                &home,
                id,
                inst.session_path.as_deref(),
            ) {
                Ok(muxel_core::CodexResumePath::Found(_)) => ResumeFile::Present,
                Ok(muxel_core::CodexResumePath::Missing) => ResumeFile::Gone,
                Ok(muxel_core::CodexResumePath::Unknown) | Err(_) => ResumeFile::Unknown,
            };
        }
        if muxel_core::muse::is_program(program) {
            let env: Vec<(String, String)> = inst
                .env
                .iter()
                .map(|entry| (entry.key.clone(), entry.value.clone()))
                .collect();
            return match crate::session_binding::muse_data_dir(&env)
                .map(|root| crate::session_binding::muse_session_exists(&root, id))
            {
                Some(Ok(true)) => ResumeFile::Present,
                Some(Ok(false)) => ResumeFile::Gone,
                Some(Err(_)) | None => ResumeFile::Unknown,
            };
        }
        // Grok keeps a session under a directory named by its own encoding of
        // the working directory, which muxel doesn't reproduce.
        ResumeFile::Unnamed
    }

    /// Outside control's `resumes`: for an asleep local agent, whether waking
    /// it brings its conversation back, checked on disk now. `None` while it
    /// runs, for anything else, or when muxel can't tell.
    pub(super) fn resumes(&self, inst: &Instance) -> Option<bool> {
        let local = self
            .workspace
            .project(inst.project_id)
            .is_some_and(|project| !project.is_remote());
        if !local
            || !self.is_dormant(inst.id)
            || inst.literal_argv
            || !muxel_core::is_known_agent_program(inst.program.as_deref())
        {
            return None;
        }
        if !self.instance_resume_trusted(inst) {
            return Some(false);
        }
        self.resume_file(inst).resumes()
    }

    /// The pane the user is in: the focused pane, one popped out into its own
    /// window, or the pane a project window shows as its current one.
    fn pane_focused(&self, iid: Uuid) -> bool {
        self.active_instance == Some(iid)
            || self.popouts.contains_key(&iid)
            || self.secondary_windows.iter().any(|window| {
                self.workspace
                    .project(window.pid)
                    .and_then(|project| project.preferred_instance())
                    == Some(iid)
            })
    }

    /// Why pane `iid` may not sleep now, or `None` when it may.
    pub(super) fn sleep_refusal_for(
        &self,
        iid: Uuid,
        request: SleepRequest,
        cx: &App,
    ) -> Option<SleepRefusal> {
        let Some(facts) = self.sleep_facts(iid, cx) else {
            return Some(SleepRefusal::NotAgent);
        };
        sleep_refusal(&facts, request).or_else(|| {
            self.workspace
                .instance(iid)
                .and_then(|inst| self.resume_file(inst).sleep_refusal())
        })
    }

    /// Put pane `iid` to sleep: mark it dormant and asleep at once, so its
    /// placeholder shows and nothing types into it, then quit its program.
    pub(super) fn sleep_instance(
        &mut self,
        iid: Uuid,
        request: SleepRequest,
        cause: SleepCause,
        cx: &mut Context<Self>,
    ) -> Result<(), SleepRefusal> {
        if let Some(refusal) = self.sleep_refusal_for(iid, request, cx) {
            return Err(refusal);
        }
        let Some(view) = self.terminals.remove(&iid) else {
            return Err(SleepRefusal::NotRunning);
        };
        let now = chrono::Utc::now().timestamp_millis();
        let (name, preset) = match self.workspace.instance_mut(iid) {
            Some(inst) => {
                inst.asleep_since = Some(now);
                (inst.display_name().to_string(), inst.preset.clone())
            }
            None => Default::default(),
        };
        self.dormant_instances.insert(iid);
        // Sleep needs the file a resume reads, so it was there just now.
        self.conversations_gone.remove(&iid);
        self.terminal_launches.remove(&iid);
        // The preset names the provider, so a quit that needed a kill says whose.
        muxel_store::append_event_log(&format!(
            "sleep: \"{name}\" [{iid}] ({preset}) by {}",
            cause.word()
        ));
        self.persist();
        self.quit_sleeping_terminal(iid, view, cx);
        cx.notify();
        Ok(())
    }

    /// Ask the agent's own TUI to quit, then end it by PID if it hasn't after
    /// [`QUIT_POLLS`] checks. The view waits out of `terminals`, so status
    /// polling, close-on-exit and outside control never see the exit it causes.
    fn quit_sleeping_terminal(
        &mut self,
        iid: Uuid,
        view: Entity<TerminalView>,
        cx: &mut Context<Self>,
    ) {
        let entity = view.entity_id();
        view.read(cx).session().write_input(QUIT_KEY);
        self.sleep_exits.insert(iid, view);
        let started = Instant::now();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(QUIT_PRESS_GAP).await;
            let _ = this.update(cx, |this, cx| {
                if let Some(view) = this.sleep_exit(iid, entity)
                    && !view.read(cx).exited()
                {
                    view.read(cx).session().write_input(QUIT_KEY);
                }
            });
            for _ in 0..QUIT_POLLS {
                cx.background_executor().timer(QUIT_POLL).await;
                let exited = this
                    .update(cx, |this, cx| {
                        this.sleep_exit(iid, entity)
                            .map(|view| view.read(cx).exited())
                    })
                    .ok()
                    .flatten();
                match exited {
                    // Already ended: woken, closed, or the app moved on.
                    None => return,
                    Some(true) => break,
                    Some(false) => {}
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.end_sleeping_terminal(iid, Some(entity), started, cx)
            });
        })
        .detach();
    }

    fn sleep_exit(&self, iid: Uuid, entity: EntityId) -> Option<&Entity<TerminalView>> {
        self.sleep_exits
            .get(&iid)
            .filter(|view| view.entity_id() == entity)
    }

    /// End a sleeping pane's program now if it is still quitting: a launch for
    /// the pane must never run beside the process it replaces. `entity` limits
    /// it to one quit; `None` ends whichever is pending.
    pub(super) fn end_sleeping_terminal(
        &mut self,
        iid: Uuid,
        entity: Option<EntityId>,
        started: Instant,
        cx: &mut Context<Self>,
    ) {
        if entity.is_some_and(|entity| self.sleep_exit(iid, entity).is_none()) {
            return;
        }
        let Some(view) = self.sleep_exits.remove(&iid) else {
            return;
        };
        let quit = view.read(cx).exited();
        if !quit {
            // Exactly this pane's child. Dropping the view then closes its
            // pseudo console, which ends what the child left running, as a
            // close does.
            view.read(cx).session().kill();
        }
        muxel_store::append_event_log(&format!(
            "sleep: [{iid}] {} after {} ms",
            if quit { "quit" } else { "killed" },
            started.elapsed().as_millis()
        ));
    }

    /// [`Self::end_sleeping_terminal`] for whatever quit is pending.
    pub(super) fn end_pending_sleep(&mut self, iid: Uuid, cx: &mut Context<Self>) {
        if self.sleep_exits.contains_key(&iid) {
            self.end_sleeping_terminal(iid, None, Instant::now(), cx);
        }
    }

    /// What a pane's sidebar menu offers: `Ok` to sleep it, `Err` with why it
    /// can't now, `None` for no item (dormant already, or never sleepable).
    pub(super) fn sleep_menu_state(&self, iid: Uuid, cx: &App) -> Option<Result<(), String>> {
        if self.is_dormant(iid) {
            return None;
        }
        match self.sleep_refusal_for(iid, SleepRequest::Pane, cx) {
            None => Some(Ok(())),
            Some(SleepRefusal::NotAgent | SleepRefusal::NotLocal) => None,
            Some(refusal) => Some(Err(sleep_refusal_label(refusal))),
        }
    }

    /// The pane menu's Sleep. Its state can change while the menu is open.
    ///
    /// It sleeps the pane even when the user is in it, since they named it.
    /// Focus then moves the way a close moves it: to the tab its group shows
    /// next, else another running pane of the project. Focusing a dormant
    /// pane starts it, so leaving focus there would let the next project
    /// switch wake it straight back up. With no running pane to go to, focus
    /// stays and the pane shows its dormant placeholder.
    pub(super) fn sleep_from_menu(
        &mut self,
        iid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_active = self.active_instance == Some(iid);
        if let Err(refusal) = self.sleep_instance(iid, SleepRequest::Pane, SleepCause::Pane, cx) {
            self.add_event(
                NotifKind::Error,
                sleep_refusal_label(refusal),
                String::new(),
            );
            cx.notify();
            return;
        }
        if was_active && let Some(next) = self.focus_after_sleep(iid) {
            self.focus_instance_with_attendance(next, false, window, cx);
        }
    }

    /// Where focus goes after the focused pane `iid` falls asleep: its group's
    /// next tab, else the project's first other pane, skipping dormant ones.
    fn focus_after_sleep(&self, iid: Uuid) -> Option<Uuid> {
        let project = self
            .workspace
            .instance(iid)
            .and_then(|inst| self.workspace.project(inst.project_id))?;
        project
            .layout
            .as_ref()
            .and_then(|layout| layout.surviving_active_after_remove(iid))
            .into_iter()
            .chain(project.instances())
            .find(|other| *other != iid && !self.is_dormant(*other))
    }

    /// Sleep every idle agent in project `pid` the rules allow; say how many.
    pub(super) fn sleep_idle_agents(&mut self, pid: Uuid, cx: &mut Context<Self>) {
        let panes = self
            .workspace
            .project(pid)
            .map(|project| project.instances())
            .unwrap_or_default();
        let slept = panes
            .into_iter()
            .filter(|iid| {
                self.sleep_instance(*iid, SleepRequest::Project, SleepCause::IdleAgents, cx)
                    .is_ok()
            })
            .count();
        if slept == 0 {
            self.add_event(
                NotifKind::Success,
                t("No idle agents to put to sleep"),
                t("Working, asking, focused and pinned agents keep running."),
            );
        } else {
            self.add_event(
                NotifKind::Success,
                tf("Put {n} idle agents to sleep", &[("n", &slept.to_string())]),
                t("Each resumes its conversation when you open it."),
            );
        }
        cx.notify();
    }

    /// The idle timer: once a minute, sleep agents idle past the setting. The
    /// same minute re-checks which asleep panes' conversations are gone, since
    /// a provider deletes old ones whether or not muxel sleeps anything.
    pub(super) fn auto_sleep_pass(&mut self, cx: &mut Context<Self>) {
        if self.current_workspace.is_none()
            || self
                .auto_sleep_checked
                .is_some_and(|at| at.elapsed() < AUTO_SLEEP_EVERY)
        {
            return;
        }
        self.auto_sleep_checked = Some(Instant::now());
        if self.refresh_conversations_gone() {
            cx.notify();
        }
        let hours = self.settings.sleep_after_hours;
        if hours == 0 {
            return;
        }
        let request = SleepRequest::Auto {
            after_hours: hours,
            now: chrono::Utc::now().timestamp_millis(),
        };
        let due: Vec<Uuid> = self
            .terminals
            .keys()
            .copied()
            .filter(|iid| self.sleep_refusal_for(*iid, request, cx).is_none())
            .collect();
        for iid in due {
            let _ = self.sleep_instance(iid, request, SleepCause::Timer, cx);
        }
    }

    /// Start dormant pane `iid` through the restore path: opening it, pinning
    /// it and `ctl wake` all wake it here. The wake restarts its idle clock, so
    /// it gets the whole threshold before the timer can stop it again, even if
    /// nobody types into it. `false` when it wasn't dormant or its launch was
    /// refused; a refused launch leaves it out of `dormant_instances`, where
    /// its failure shows.
    pub(super) fn start_dormant(
        &mut self,
        iid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.dormant_instances.remove(&iid) {
            return false;
        }
        self.conversations_gone.remove(&iid);
        self.spawn_terminal_deferred(iid, muxel_core::TerminalLaunchReason::Restore, window, cx);
        if !self.terminal_launching.contains_key(&iid) && !self.terminals.contains_key(&iid) {
            return false;
        }
        if let Some(inst) = self.workspace.instance_mut(iid) {
            inst.activity.woken_at = Some(chrono::Utc::now().timestamp_millis());
        }
        self.persist();
        cx.notify();
        true
    }

    /// Start a sleeping or dormant pane through the ordinary restore path,
    /// leaving focus, windows and the active tab alone. `false` when it isn't
    /// dormant or its launch was refused (it is then left dormant).
    pub(super) fn wake_instance(
        &mut self,
        iid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.is_dormant(iid) {
            return false;
        }
        if self.start_dormant(iid, window, cx) {
            return true;
        }
        self.dormant_instances.insert(iid);
        false
    }

    /// `ctl wake`: start a sleeping or dormant pane without focusing it. A pane
    /// that already runs or starts answers `woken: false`, so retries are safe.
    pub(super) fn control_wake(
        &mut self,
        iid: Uuid,
        workspace: Option<Uuid>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<serde_json::Value, ctl::Error> {
        if self.current_workspace != workspace {
            return Err("the pane's workspace has changed".into());
        }
        let Some(inst) = self.workspace.instance(iid) else {
            return Err("that agent has just closed".into());
        };
        if !self.settings.control_enabled {
            return Err(ctl::creation_refusal(ErrorCode::ControlDisabled));
        }
        if let Some(until) = self.closing_until(iid) {
            return Err(ctl::Error::closing(until));
        }
        let name = inst.display_name().to_string();
        if inst.kind != InstanceKind::Terminal {
            return Err(format!("'{name}' is not a terminal").into());
        }
        let agent = ctl::short_id(iid);
        if self.is_dormant(iid) {
            return if self.wake_instance(iid, window, cx) {
                Ok(serde_json::json!({
                    "woken": true,
                    "agent": agent,
                    "uuid": iid,
                    "status": "starting",
                }))
            } else {
                Err(ctl::Error::new(
                    ErrorCode::NotRunning,
                    format!("'{name}' couldn't be started: open it in muxel to see why"),
                ))
            };
        }
        let stopped = inst.is_stopped_literal(
            self.terminal_view(iid).is_some(),
            self.terminal_launching.contains_key(&iid),
        );
        match self.control_state(inst, cx) {
            Some(AgentState::Exited) => Err(ctl::Error::new(
                ErrorCode::Exited,
                format!("'{name}' has exited"),
            )),
            Some(AgentState::Failed) => Err(ctl::Error::new(
                ErrorCode::NotRunning,
                format!("'{name}' failed to start"),
            )
            .with_reason("failed")),
            Some(AgentState::NotRunning) if stopped => Err(ctl::Error::new(
                ErrorCode::NotRunning,
                format!("'{name}' is a stopped command: run it again in muxel"),
            )
            .with_reason("stopped")),
            Some(AgentState::NotRunning) | None => Err(ctl::Error::new(
                ErrorCode::NotRunning,
                format!("'{name}' isn't running this session: the user has to open it in muxel"),
            )),
            Some(state) => Ok(serde_json::json!({
                "woken": false,
                "already_running": true,
                "status": state,
                "agent": agent,
                "uuid": iid,
            })),
        }
    }

    /// `"sleeping"` for a pane put to sleep, `"dormant"` for one deferred at
    /// launch, `None` for anything else.
    pub(super) fn dormant_reason(&self, iid: Uuid) -> Option<&'static str> {
        self.is_dormant(iid).then(|| {
            if self.is_asleep(iid) {
                "sleeping"
            } else {
                "dormant"
            }
        })
    }

    /// Re-check which asleep panes' conversations are gone; whether that
    /// changed. One bounded look per asleep agent (see [`Self::resume_file`]).
    pub(super) fn refresh_conversations_gone(&mut self) -> bool {
        let gone: HashSet<Uuid> = self
            .dormant_instances
            .iter()
            .copied()
            .filter(|iid| {
                self.workspace
                    .instance(*iid)
                    .is_some_and(|inst| self.resumes(inst) == Some(false))
            })
            .collect();
        let changed = gone != self.conversations_gone;
        self.conversations_gone = gone;
        changed
    }

    /// An asleep pane in words, for its tooltip and placeholder: one word for
    /// a pane put to sleep and one deferred at launch, its saved status, and
    /// how long ago it last worked.
    pub(super) fn asleep_words(&self, iid: Uuid) -> String {
        let Some(inst) = self.workspace.instance(iid) else {
            return t("Asleep").to_string();
        };
        let now = chrono::Utc::now().timestamp_millis();
        match (
            saved_activity_status(&inst.activity),
            muxel_core::sleep::asleep_age(&inst.activity, now),
        ) {
            (_, None) => t("Asleep").to_string(),
            (AgentStatus::Done, Some(age)) => tf("Asleep · finished {age} ago", &[("age", &age)]),
            // Its turn or question ended with the program.
            (AgentStatus::Working | AgentStatus::Blocked, Some(age)) => {
                tf("Asleep · interrupted {age} ago", &[("age", &age)])
            }
            (AgentStatus::Idle, Some(age)) => {
                tf("Asleep · last active {age} ago", &[("age", &age)])
            }
        }
    }

    /// An asleep pane's sidebar pill: a moon and the age of its last work,
    /// outlined in its saved status color (green finished, amber interrupted,
    /// grey idle), with `∅` when its conversation is gone. Filled pills are
    /// left to running panes.
    pub(super) fn asleep_pill(&self, iid: Uuid, cx: &App) -> Tag {
        let inst = self.workspace.instance(iid);
        let color = self.asleep_color(iid, cx);
        let tag = match self.asleep_status(iid) {
            AgentStatus::Done => Tag::success(),
            AgentStatus::Working | AgentStatus::Blocked => Tag::warning(),
            AgentStatus::Idle => Tag::new(),
        };
        let age = inst.and_then(|inst| {
            muxel_core::sleep::asleep_age(&inst.activity, chrono::Utc::now().timestamp_millis())
        });
        tag.outline()
            .small()
            .gap_1()
            .text_color(color)
            .child(
                svg()
                    .path("icons/moon.svg")
                    .size(px(10.0))
                    .flex_none()
                    .text_color(color),
            )
            .children(age)
            .when(self.conversations_gone.contains(&iid), |tag| tag.child("∅"))
    }

    /// The status an asleep pane saved when its program stopped.
    fn asleep_status(&self, iid: Uuid) -> AgentStatus {
        self.workspace
            .instance(iid)
            .map(|inst| saved_activity_status(&inst.activity))
            .unwrap_or(AgentStatus::Idle)
    }

    /// An asleep pane's saved status color: green finished, amber interrupted
    /// (its turn or question ended with the program), grey idle.
    fn asleep_color(&self, iid: Uuid, cx: &App) -> Hsla {
        match self.asleep_status(iid) {
            AgentStatus::Done => cx.theme().success,
            AgentStatus::Working | AgentStatus::Blocked => cx.theme().warning,
            AgentStatus::Idle => cx.theme().muted_foreground,
        }
    }

    /// An asleep row's agent icon: its saved status color, dimmed the way a
    /// read finish is, so nothing asleep reads as running. An unread finish
    /// stays bright; idle is already the muted grey.
    pub(super) fn asleep_icon_color(&self, iid: Uuid, finish_unread: bool, cx: &App) -> Hsla {
        let color = self.asleep_color(iid, cx);
        match self.asleep_status(iid) {
            AgentStatus::Done if finish_unread => color,
            AgentStatus::Idle => color,
            _ => color.opacity(READ_DONE_OPACITY),
        }
    }

    /// What waking an asleep pane whose conversation is gone does.
    pub(super) fn conversation_gone_words() -> SharedString {
        t("The saved conversation is gone. Waking starts a new one in the same folder.")
    }

    pub(super) fn is_asleep(&self, iid: Uuid) -> bool {
        self.workspace
            .instance(iid)
            .is_some_and(|inst| inst.asleep_since.is_some())
    }
}

impl MuxelApp {
    /// Settings → Behavior: automatic sleep, with its idle threshold.
    pub(super) fn render_sleep_setting(&self, cx: &mut Context<Self>) -> AnyElement {
        let hours = self.settings.sleep_after_hours;
        let shown = match hours {
            0 => muxel_core::sleep::DEFAULT_SLEEP_AFTER_HOURS,
            hours => hours,
        }
        .to_string();
        v_flex()
            .gap_1()
            .child(
                self.check_row(
                    Checkbox::new("b-sleep")
                        .checked(hours > 0)
                        .on_click(cx.listener(|this, checked: &bool, _w, cx| {
                            this.settings.sleep_after_hours = if *checked {
                                muxel_core::sleep::DEFAULT_SLEEP_AFTER_HOURS
                            } else {
                                0
                            };
                            this.persist_settings();
                            cx.notify();
                        })),
                    &tf(
                        "Put agents to sleep after {hours} hours idle",
                        &[("hours", &shown)],
                    ),
                ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("sleep-hours-less")
                            .small()
                            .label("−")
                            .disabled(hours <= 1)
                            .on_click(cx.listener(|this, _, _w, cx| {
                                this.settings.sleep_after_hours =
                                    this.settings.sleep_after_hours.saturating_sub(1).max(1);
                                this.persist_settings();
                                cx.notify();
                            })),
                    )
                    .child(tf("{hours} hours", &[("hours", &shown)]))
                    .child(
                        Button::new("sleep-hours-more")
                            .small()
                            .label("+")
                            .disabled(hours == 0)
                            .on_click(cx.listener(|this, _, _w, cx| {
                                this.settings.sleep_after_hours =
                                    this.settings.sleep_after_hours.saturating_add(1);
                                this.persist_settings();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(t(
                        "A sleeping agent's program stops, freeing its memory, and its row shows a moon and how long ago it last worked. Opening it resumes the same conversation, or starts a new one if the provider has deleted it. Working, asking, focused and pinned agents, and shells, keep running.",
                    )),
            )
            .into_any_element()
    }
}

/// The menu text for a pane that can't sleep now.
pub(super) fn sleep_refusal_label(refusal: SleepRefusal) -> String {
    let why = match refusal {
        SleepRefusal::NotAgent => t("not an agent"),
        SleepRefusal::NotLocal => t("not a local session"),
        SleepRefusal::NoResume => t("no saved conversation to resume"),
        SleepRefusal::Pinned => t("pinned"),
        SleepRefusal::NotRunning => t("not running"),
        SleepRefusal::Starting => t("starting"),
        SleepRefusal::Working => t("working"),
        SleepRefusal::Asking => t("asking a question"),
        SleepRefusal::Background => t("background work running"),
        SleepRefusal::UnsentText => t("unsent text in its input"),
        SleepRefusal::PoppedOut => t("popped out"),
        SleepRefusal::Focused => t("you're in it"),
        SleepRefusal::RecentlyActive => t("recently active"),
    };
    tf("Can't sleep: {why}", &[("why", why.as_ref())])
}
