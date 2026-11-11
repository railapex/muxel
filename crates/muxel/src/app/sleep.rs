//! Sleeping idle local agents: end the program, keep the pane as a dormant
//! placeholder, and resume the same conversation when it is opened or woken.
//! The rules live in `muxel_core::sleep`; this gathers their facts and does the
//! stopping. Dormant restore's placeholder, focus wake and ctl refusals do the
//! rest, so a sleeping pane behaves like one deferred at launch.

use super::*;
use muxel_core::control::{self as ctl, AgentState, ErrorCode};
use muxel_core::sleep::{SleepFacts, SleepRefusal, SleepRequest, SleepStatus, sleep_refusal};

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
            unsent_text: runtime.is_some_and(|runtime| runtime.draft_latched),
            pinned: inst.pinned,
            // A pane woken from outside is about to receive what it was woken
            // for; the timer must not put it straight back to sleep first.
            last_active_at: inst
                .activity
                .last_active_at()
                .max(self.woken_at.get(&iid).copied()),
        })
    }

    /// Whether `inst` resumes its saved conversation when its program starts
    /// again: what makes waking it bring the same work back.
    pub(super) fn instance_resume_trusted(&self, inst: &Instance) -> bool {
        inst.preset_id
            .and_then(|id| self.presets.iter().find(|preset| preset.id == id))
            .or_else(|| {
                self.presets
                    .iter()
                    .find(|preset| preset.name == inst.preset)
            })
            .is_some_and(|preset| muxel_core::resume_trusted(preset, inst))
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
        match self.sleep_facts(iid, cx) {
            Some(facts) => sleep_refusal(&facts, request),
            None => Some(SleepRefusal::NotAgent),
        }
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
        let name = match self.workspace.instance_mut(iid) {
            Some(inst) => {
                inst.asleep_since = Some(now);
                inst.display_name().to_string()
            }
            None => String::new(),
        };
        self.dormant_instances.insert(iid);
        self.terminal_launches.remove(&iid);
        self.woken_at.remove(&iid);
        muxel_store::append_event_log(&format!("sleep: \"{name}\" [{iid}] by {}", cause.word()));
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
        match self.sleep_refusal_for(iid, SleepRequest::ByHand, cx) {
            None => Some(Ok(())),
            Some(SleepRefusal::NotAgent | SleepRefusal::NotLocal) => None,
            Some(refusal) => Some(Err(sleep_refusal_label(refusal))),
        }
    }

    /// The pane menu's Sleep. Its state can change while the menu is open.
    pub(super) fn sleep_from_menu(&mut self, iid: Uuid, cx: &mut Context<Self>) {
        if let Err(refusal) = self.sleep_instance(iid, SleepRequest::ByHand, SleepCause::Pane, cx) {
            self.add_event(
                NotifKind::Error,
                sleep_refusal_label(refusal),
                String::new(),
            );
            cx.notify();
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
                self.sleep_instance(*iid, SleepRequest::ByHand, SleepCause::IdleAgents, cx)
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

    /// The idle timer: once a minute, sleep agents idle past the setting.
    pub(super) fn auto_sleep_pass(&mut self, cx: &mut Context<Self>) {
        let hours = self.settings.sleep_after_hours;
        if hours == 0
            || self.current_workspace.is_none()
            || self
                .auto_sleep_checked
                .is_some_and(|at| at.elapsed() < AUTO_SLEEP_EVERY)
        {
            return;
        }
        self.auto_sleep_checked = Some(Instant::now());
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
        self.dormant_instances.remove(&iid);
        self.spawn_terminal_deferred(iid, muxel_core::TerminalLaunchReason::Restore, window, cx);
        if self.terminal_launching.contains_key(&iid) || self.terminals.contains_key(&iid) {
            self.woken_at
                .insert(iid, chrono::Utc::now().timestamp_millis());
            cx.notify();
            true
        } else {
            self.dormant_instances.insert(iid);
            false
        }
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

    /// The word a dormant pane's tab and sidebar badge carry.
    pub(super) fn dormant_badge(&self, iid: Uuid) -> SharedString {
        if self.is_asleep(iid) {
            t("asleep")
        } else {
            t("dormant")
        }
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
                        "A sleeping agent's program stops, freeing its memory. Opening it resumes the same conversation. Working, asking, focused and pinned agents, and shells, keep running.",
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
        SleepRefusal::Focused => t("you're in it"),
        SleepRefusal::RecentlyActive => t("recently active"),
    };
    tf("Can't sleep: {why}", &[("why", why.as_ref())])
}
