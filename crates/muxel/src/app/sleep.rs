//! Sleeping idle local agents: end the program, keep the pane as a dormant
//! placeholder, and resume the same conversation when it is woken.
//! The rules live in `muxel_core::sleep`; this gathers their facts and does the
//! stopping. Dormant restore's placeholder and ctl refusals do the rest, so a
//! sleeping pane behaves like one deferred at launch. Selecting either shows
//! it; Enter or typing in it, Wake, pinning and `ctl focus`/`wake` start it.

use super::*;
use muxel_core::Waker;
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

/// What woke an asleep pane: who it counts as, for the idle clock, and what
/// the event log says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WakeCause {
    /// Enter or a typed character in the pane.
    Key,
    /// A Wake (or Start fresh) button: the pane's, its row's, or the row menu's.
    Button,
    Pin,
    /// `ctl focus`, which puts the pane in front of whoever is at the window.
    CtlFocus,
    /// `ctl wake`, which starts it in the background for a script.
    CtlWake,
    /// A `ctl send --when-idle` waiting to type into it.
    CtlSend,
}

impl WakeCause {
    fn word(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Button => "wake button",
            Self::Pin => "pin",
            Self::CtlFocus => "ctl focus",
            Self::CtlWake => "ctl wake",
            Self::CtlSend => "ctl send",
        }
    }

    fn waker(self) -> Waker {
        match self {
            // The person's own input in muxel.
            Self::Key | Self::Button | Self::Pin => Waker::User,
            // Everything through ctl. If someone is at the window, their first
            // look at the live pane makes it theirs; if nobody is, a script
            // shouldn't buy an agent the whole threshold.
            Self::CtlFocus | Self::CtlWake | Self::CtlSend => Waker::Machine,
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
            send_waiting: self.control_queue.contains_key(&iid),
            pinned: inst.pinned,
            // Counts the last wake: see `start_dormant`.
            last_active_at: inst.activity.last_active_at(),
            machine_woken: inst.activity.machine_woken_unattended(),
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
    /// The user stays in it and sees its asleep face: selecting never wakes a
    /// pane, so nothing starts it again until they press Enter, type or Wake.
    pub(super) fn sleep_from_menu(
        &mut self,
        iid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Err(refusal) = self.sleep_instance(iid, SleepRequest::Pane, SleepCause::Pane, cx) {
            self.add_event(
                NotifKind::Error,
                sleep_refusal_label(refusal),
                String::new(),
            );
            cx.notify();
            return;
        }
        // The terminal that held the keyboard is gone; the root takes it so
        // Enter or typing reaches the asleep pane's wake.
        if self.active_instance == Some(iid) {
            window.focus(&self.focus_handle, cx);
        }
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
        let title = match slept {
            0 => t("No idle agents to sleep").to_string(),
            1 => t("Slept 1 idle agent").to_string(),
            n => tf("Slept {n} idle agents", &[("n", &n.to_string())]),
        };
        self.add_event(NotifKind::Success, title, "");
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

    /// Start dormant pane `iid` through the restore path: every wake (Enter or
    /// typing in it, Wake, pinning, `ctl focus` and `ctl wake`) lands here. The
    /// wake restarts its idle clock, so the timer can't stop it again at once,
    /// even if nobody types into it: the user's wake gets the whole threshold,
    /// a machine's 30 minutes until someone looks (`muxel_core::sleep`). The
    /// pane keeps its asleep face until the terminal draws. `false` when it
    /// wasn't dormant or its launch was refused; a refused launch leaves it out
    /// of `dormant_instances`, where its failure shows.
    pub(super) fn start_dormant(
        &mut self,
        iid: Uuid,
        cause: WakeCause,
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
        self.wake_launches.insert(iid);
        let waker = cause.waker();
        if let Some(inst) = self.workspace.instance_mut(iid) {
            inst.activity.woken_at = Some(chrono::Utc::now().timestamp_millis());
            inst.activity.woken_by = Some(waker);
            // Whether a machine-woken pane gets a second job after its short
            // limit is what this line lets anyone count.
            muxel_store::append_event_log(&format!(
                "wake: \"{}\" [{iid}] by {} ({})",
                inst.display_name(),
                cause.word(),
                match waker {
                    Waker::User => "user",
                    Waker::Machine => "machine",
                }
            ));
        }
        self.persist();
        cx.notify();
        true
    }

    /// A woken pane that draws live while it is the focused pane of the active
    /// window has been looked at. Focus records a look only at a pane showing
    /// something, and a woken pane is usually focused before it draws (the
    /// one `ctl focus` put in front of someone, or one woken by Enter), so
    /// without this its first look would wait for the user to focus it again.
    /// A pane that draws in an inactive window gets its look once that window
    /// is active with the pane still in front.
    pub(super) fn record_woken_look(&mut self, cx: &App) {
        let Some(iid) = self.active_instance else {
            return;
        };
        if !self.wake_launches.contains(&iid)
            || !self.instance_window_active(iid)
            || !self
                .terminals
                .get(&iid)
                .is_some_and(|view| view.read(cx).has_visible_content())
        {
            return;
        }
        self.wake_launches.remove(&iid);
        let now = chrono::Utc::now().timestamp_millis();
        if let Some(inst) = self.workspace.instance_mut(iid)
            && inst.activity.attend(now)
        {
            self.persist();
        }
    }

    /// Wake asleep pane `iid` and show it: the Wake buttons (pane, row, row
    /// menu) and `ctl focus`. Selecting alone never starts a pane.
    pub(super) fn wake_and_show(
        &mut self,
        iid: Uuid,
        cause: WakeCause,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_instance(iid, window, cx);
        self.start_dormant(iid, cause, window, cx);
    }

    /// Enter or a typed character in the asleep pane the user is in wakes it.
    /// The key itself is dropped: nothing may reach a booting program, which
    /// could be showing a trust card or an update prompt. Shortcuts (Ctrl,
    /// Alt, the platform key), arrows, Tab and Escape leave it asleep.
    pub(super) fn wake_on_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        // Only when no input has the keyboard: the root holds focus exactly
        // while an asleep (viewless) pane is the one selected.
        if !self.focus_handle.is_focused(window) || !key_wakes(keystroke) {
            return false;
        }
        match self.active_instance {
            Some(iid) if self.is_dormant(iid) => {
                self.start_dormant(iid, WakeCause::Key, window, cx)
            }
            _ => false,
        }
    }

    /// Wake an asleep pane through the ordinary restore path, leaving focus,
    /// windows and the active tab alone. `false` when it isn't asleep or its
    /// launch was refused (it is then left asleep).
    pub(super) fn wake_instance(
        &mut self,
        iid: Uuid,
        cause: WakeCause,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.is_dormant(iid) {
            return false;
        }
        if self.start_dormant(iid, cause, window, cx) {
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
            return if self.wake_instance(iid, WakeCause::CtlWake, window, cx) {
                Ok(serde_json::json!({
                    "woken": true,
                    "agent": agent,
                    "uuid": iid,
                    "status": "starting",
                }))
            } else {
                Err(ctl::Error::new(
                    ErrorCode::NotRunning,
                    format!("'{name}' couldn't be woken: open it in muxel to see why"),
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

    /// An asleep pane in words, for its tooltip and face: "Asleep", how long
    /// ago it last worked, then only the exceptions, an interrupted turn and
    /// a conversation that is gone ("Asleep · 31d · no history"). A pane that
    /// will resume its conversation says nothing about it.
    pub(super) fn asleep_words(&self, iid: Uuid) -> String {
        let mut words = vec![t("Asleep").to_string()];
        if let Some(inst) = self.workspace.instance(iid) {
            let now = chrono::Utc::now().timestamp_millis();
            words.extend(muxel_core::sleep::asleep_age(&inst.activity, now));
            // Its turn or question ended with the program.
            if matches!(
                saved_activity_status(&inst.activity),
                AgentStatus::Working | AgentStatus::Blocked
            ) {
                words.push(t("interrupted").to_string());
            }
        }
        if self.conversations_gone.contains(&iid) {
            words.push(t("no history").to_string());
        }
        words.join(" · ")
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

    /// The Wake button an asleep row's pill turns into while the row (group
    /// `row`) is hovered. Hidden, it takes no clicks, so a stray click on the
    /// pill only selects the row, which never wakes it.
    pub(super) fn asleep_wake_button(
        &self,
        iid: Uuid,
        row: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let accent = cx.theme().primary;
        div()
            .id(SharedString::from(format!("inst-wake-{}", iid.simple())))
            .absolute()
            .inset_0()
            .invisible()
            .group_hover(row, |s| s.visible())
            .flex()
            .items_center()
            .justify_center()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(accent)
            .bg(cx.theme().sidebar)
            .text_xs()
            .text_color(accent)
            .cursor_pointer()
            .child(t("Wake"))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                // The row under it would only select.
                cx.stop_propagation();
                this.wake_and_show(iid, WakeCause::Button, window, cx);
            }))
    }

    pub(super) fn is_asleep(&self, iid: Uuid) -> bool {
        self.workspace
            .instance(iid)
            .is_some_and(|inst| inst.asleep_since.is_some())
    }

    /// Whether saved pane `instance` opens asleep instead of starting, on load
    /// and when its project opens (`sleep::opens_dormant`).
    pub(super) fn pane_opens_dormant(&self, instance: &Instance, now: i64) -> bool {
        muxel_core::sleep::opens_dormant(
            instance,
            self.relaunch_focus,
            self.instance_resume_trusted(instance),
            &self.settings,
            now,
        )
    }

    /// The pane with keyboard focus while it is awake (running, starting, or
    /// woken and starting): what a relaunch starts whatever its idle age. A
    /// pane asleep in front, put to sleep or opened asleep and only looked at,
    /// gives none.
    pub(super) fn awake_focus(&self) -> Option<Uuid> {
        self.active_instance.filter(|&iid| {
            self.workspace
                .instance(iid)
                .is_some_and(|inst| inst.kind == InstanceKind::Terminal)
                && !self.is_dormant(iid)
        })
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
                    &tf("Sleep agents idle for {hours} hours", &[("hours", &shown)]),
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
                        "An asleep agent's program stops and frees its memory. Selecting it shows it; Enter, typing or Wake resumes its conversation. Working, asking, focused and pinned agents keep running, and shells never sleep.",
                    )),
            )
            .into_any_element()
    }
}

/// The menu text for a pane that can't sleep now.
pub(super) fn sleep_refusal_label(refusal: SleepRefusal) -> String {
    let why = match refusal {
        SleepRefusal::NotAgent => t("not an agent"),
        SleepRefusal::NotLocal => t("not local"),
        SleepRefusal::NoResume => t("no history"),
        SleepRefusal::Pinned => t("pinned"),
        SleepRefusal::NotRunning => t("ended"),
        SleepRefusal::Starting => t("starting"),
        SleepRefusal::Working => t("working"),
        SleepRefusal::Asking => t("asking"),
        SleepRefusal::Background => t("background work"),
        SleepRefusal::UnsentText => t("unsent prompt"),
        SleepRefusal::SendWaiting => t("a send is waiting"),
        SleepRefusal::PoppedOut => t("popped out"),
        SleepRefusal::Focused => t("you're in it"),
        SleepRefusal::RecentlyActive => t("recently active"),
    };
    tf("Can't sleep: {why}", &[("why", why.as_ref())])
}

/// Whether a key pressed in an asleep pane wakes it: Enter, or a key that
/// types a character with no shortcut modifier held.
fn key_wakes(keystroke: &Keystroke) -> bool {
    let held = &keystroke.modifiers;
    if held.control || held.alt || held.platform || held.function {
        return false;
    }
    keystroke.key == "enter"
        || keystroke
            .key_char
            .as_deref()
            .is_some_and(|typed| !typed.is_empty() && !typed.chars().any(char::is_control))
}
