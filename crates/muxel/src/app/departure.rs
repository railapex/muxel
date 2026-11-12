//! Confirmed Quit and Restart. Every confirmed exit comes through
//! [`MuxelApp::confirm_exit`].
//!
//! Both interrupt the input operations still pending: each stops at its next
//! stage boundary and none resumes, and no new one starts. A Quit then takes
//! the path it always has. A Restart also refuses new launches and automatic
//! turns, lets launches and editor moves already under way finish, holds every
//! terminal's input and waits for the writes queued before the hold, saves,
//! starts the new version, and only then quits. Until the new version has
//! started it can give up, and this one stays running.
//!
//! While a Restart prepares, the workspace's routine saves wait for its own,
//! which comes last and holds every change made meanwhile. Once the new
//! version has started, nothing more is written: closing windows and the
//! app's shutdown change nothing the new version loads.

use super::recoverable_close::CloseDeparture;
use super::{MuxelApp, NotifKind, PopOut, UpdateState, popout_terminal_views};
use crate::i18n::{t, tf};
use crate::update::RelaunchPlan;
use gpui::{App, AsyncApp, Context, Task, WeakEntity};
use muxel_core::leaving::{self, Answer, Leave, Pending, Question, Title};
use muxel_core::recoverable_close::CloseFacts;
use muxel_terminal::{InputFence, TerminalSession};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use uuid::Uuid;

/// Which exit a confirmation is for. A Restart ends this process too, so it
/// wins when both are asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum ExitKind {
    #[default]
    Quit,
    Restart,
}

/// How far a confirmed Restart has got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stage {
    /// Input is interrupted and new work refused. It can still give up.
    Preparing,
    /// The new version has started and this one is quitting. It starts the new
    /// version only from `Preparing`, so it does so once.
    Committed,
}

/// How long a Quit or Restart counts down when every pane looks idle.
pub(super) const EXIT_COUNTDOWN: Duration = Duration::from_secs(5);

/// A Quit or Restart counting down, which Cancel stops. It goes at the end
/// only if nothing it checked at the start has changed.
pub(super) struct ExitCountdown {
    pub(super) kind: ExitKind,
    /// Whole seconds left, for the overlay.
    pub(super) left: u64,
    /// Tells countdowns apart, so each one's line of time starts full.
    pub(super) serial: u64,
    /// Each terminal's close facts when it started.
    facts: Vec<(Uuid, CloseFacts)>,
    task: Task<()>,
}

/// The longest a Restart waits for launches and editor moves under way.
const SETTLE: Duration = Duration::from_secs(5);
/// The longest it waits for every terminal's writes queued before its hold.
const FENCE: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_millis(20);

/// Why a Restart gave up.
#[derive(Debug)]
enum Abort {
    /// A launch or editor move was still under way after [`SETTLE`].
    Settle,
    Failed(String),
    /// The app went away under it.
    Gone,
}

/// Starts nothing: the default relaunch in tests, which must never start muxel.
pub(super) fn refuse_relaunch(_: &RelaunchPlan) -> std::io::Result<()> {
    Err(std::io::Error::other("tests don't start muxel"))
}

/// A native file dialog, counted as open until it answers or is dropped.
struct DialogOpen(Rc<Cell<usize>>);

impl DialogOpen {
    fn new(count: &Rc<Cell<usize>>) -> Self {
        count.set(count.get() + 1);
        Self(count.clone())
    }
}

impl Drop for DialogOpen {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

/// The panes whose close facts differ between a countdown's start and its
/// end, and the panes added since. A pane closed meanwhile cuts nothing off.
fn changed_panes(before: &[(Uuid, CloseFacts)], now: &[(Uuid, CloseFacts)]) -> Vec<Uuid> {
    now.iter()
        .filter(|(iid, facts)| {
            before
                .iter()
                .find(|(was, _)| was == iid)
                .is_none_or(|(_, was)| was != facts)
        })
        .map(|(iid, _)| *iid)
        .collect()
}

impl MuxelApp {
    /// Quit or Restart as the user asked for it: Quit, Ctrl+Q, the tray's
    /// Quit. During a countdown it goes now; with the question showing it
    /// answers yes; from the first-run screens it quits at once. Otherwise it
    /// counts down or asks, over the main window.
    pub(super) fn request_quit(&mut self, cx: &mut Context<Self>) {
        if self.exit_countdown.is_some() {
            self.exit_now(cx);
        } else if self.show_quit_confirm {
            self.accept_quit_dialog(cx);
        } else if self.show_terms || self.show_workspace_selector {
            self.quit_from_entry_screen(cx);
        } else {
            self.ask_exit(ExitKind::Quit, cx);
            // The question renders in the main window, which may be minimized
            // (to the tray, say) or behind a pop-out: raise it so it's seen.
            if let Some(main) = self.main_window {
                let _ = main.update(cx, |_, window, _| window.activate_window());
            }
            cx.notify();
        }
    }

    /// Quit or restart after a short countdown when every pane looks idle and
    /// no input is pending, and ask otherwise. Nothing to ask while a Restart
    /// is already under way.
    pub(super) fn ask_exit(&mut self, kind: ExitKind, cx: &mut Context<Self>) {
        if self.departing.is_some() {
            return;
        }
        if let Some(countdown) = &mut self.exit_countdown {
            // A Restart asked for during a Quit's countdown takes it over.
            if kind == ExitKind::Restart {
                countdown.kind = kind;
                cx.notify();
            }
            return;
        }
        match self.exit_plan(kind, &[], cx) {
            Ok(facts) => self.start_exit_countdown(kind, facts, cx),
            Err(question) => self.show_exit_dialog(kind, &question, cx),
        }
    }

    /// The Quit or Restart question, with `question`'s rows.
    fn show_exit_dialog(&mut self, kind: ExitKind, question: &Question, cx: &mut Context<Self>) {
        self.quit_kill_tmux_local = false;
        self.quit_kill_tmux_remote = false;
        self.quit_confirm_kind = kind;
        self.quit_leave = Some(question.clone());
        self.show_quit_confirm = true;
        cx.notify();
    }

    /// What the countdown watches ([`leaving::countdown_facts`]), when the
    /// exit would cut nothing off and so counts down. Otherwise the question
    /// it asks: a row for every pane
    /// that would ask when closed, by the rule a pane close uses, and for
    /// whatever else it would cut short (pending input, the app's or any
    /// terminal's own such as a preset's instructions waiting out their
    /// startup delay, a launch, an editor move, an open file dialog, an
    /// undecided close, an unsaved editor). `changed` names panes whose state
    /// moved during a countdown; each gets a row too.
    fn exit_plan(
        &self,
        kind: ExitKind,
        changed: &[Uuid],
        cx: &App,
    ) -> Result<Vec<(Uuid, CloseFacts)>, Question> {
        let panes: Vec<leaving::Pane> = self
            .workspace
            .instances
            .iter()
            .filter(|inst| !self.is_closing(inst.id))
            .filter_map(|inst| self.leave_pane(inst.id, cx))
            .collect();
        let mut pending = self.leave_pending(cx);
        pending.extend(changed.iter().map(|&iid| Pending::Changed(iid)));
        let leave = match kind {
            ExitKind::Quit => Leave::Quit,
            ExitKind::Restart => Leave::Restart,
        };
        if let Some(question) = leaving::plan(leave, &panes, &pending, true) {
            return Err(question);
        }
        Ok(leaving::countdown_facts(&panes))
    }

    /// The question's Quit or Restart, by click, Enter or a second Quit.
    pub(super) fn accept_quit_dialog(&mut self, cx: &mut Context<Self>) {
        let kind = self.quit_confirm_kind;
        if kind == ExitKind::Quit {
            self.kill_checked_tmux_sessions();
        }
        self.confirm_exit(kind, cx);
    }

    /// An answer to the Quit or Restart question.
    pub(super) fn answer_quit(&mut self, answer: Answer, cx: &mut Context<Self>) {
        match answer {
            Answer::Confirm => self.accept_quit_dialog(cx),
            Answer::Cancel | Answer::IdleOnly => {
                self.show_quit_confirm = false;
                self.quit_leave = None;
                cx.notify();
            }
        }
    }

    /// An answer to the countdown.
    pub(super) fn answer_countdown(&mut self, answer: Answer, cx: &mut Context<Self>) {
        match answer {
            Answer::Confirm => self.exit_now(cx),
            Answer::Cancel | Answer::IdleOnly => self.cancel_exit_countdown(cx),
        }
    }

    fn start_exit_countdown(
        &mut self,
        kind: ExitKind,
        facts: Vec<(Uuid, CloseFacts)>,
        cx: &mut Context<Self>,
    ) {
        let seconds = EXIT_COUNTDOWN.as_secs();
        let task = cx.spawn(async move |this, cx| {
            for left in (0..seconds).rev() {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let going = this.update(cx, |app, cx| {
                    let Some(countdown) = &mut app.exit_countdown else {
                        return false;
                    };
                    countdown.left = left;
                    if left == 0 {
                        app.expire_exit_countdown(cx);
                    }
                    cx.notify();
                    true
                });
                if !going.unwrap_or(false) {
                    return;
                }
            }
        });
        static COUNTDOWNS: AtomicU64 = AtomicU64::new(0);
        self.exit_countdown = Some(ExitCountdown {
            kind,
            left: seconds,
            serial: COUNTDOWNS.fetch_add(1, Ordering::Relaxed),
            facts,
            task,
        });
        cx.notify();
    }

    /// The countdown ran out. Like an Undo expiry, it checks again: if any
    /// pane's facts changed, a pane was added or removed, or input is now
    /// pending, it asks instead, with a row for each pane that changed.
    fn expire_exit_countdown(&mut self, cx: &mut Context<Self>) {
        let Some(countdown) = self.exit_countdown.take() else {
            return;
        };
        // This runs inside the countdown's own timer task, which ends once it
        // returns; dropping the task here would cancel the running code.
        countdown.task.detach();
        let kind = countdown.kind;
        let changed = match self.exit_plan(kind, &[], cx) {
            Ok(facts) if facts == countdown.facts => return self.confirm_exit(kind, cx),
            Ok(facts) => changed_panes(&countdown.facts, &facts),
            Err(question) => return self.show_exit_dialog(kind, &question, cx),
        };
        // Only a pane closed meanwhile: nothing to name, but it still asks.
        let question = self
            .exit_plan(kind, &changed, cx)
            .err()
            .unwrap_or_else(|| Question {
                leave: match kind {
                    ExitKind::Quit => Leave::Quit,
                    ExitKind::Restart => Leave::Restart,
                },
                title: match kind {
                    ExitKind::Quit => Title::Quit,
                    ExitKind::Restart => Title::Restart,
                },
                rows: Vec::new(),
                idle: None,
                danger: false,
            });
        self.show_exit_dialog(kind, &question, cx);
    }

    /// The countdown's Cancel.
    pub(super) fn cancel_exit_countdown(&mut self, cx: &mut Context<Self>) {
        self.exit_countdown = None;
        cx.notify();
    }

    /// The countdown's Quit now or Restart now, or a second Quit asked for
    /// during it.
    pub(super) fn exit_now(&mut self, cx: &mut Context<Self>) {
        let Some(countdown) = self.exit_countdown.take() else {
            return;
        };
        // Dropping it cancels its timer. Left running, the timer would tick a
        // countdown started after this exit gave up, and end it early.
        let kind = countdown.kind;
        drop(countdown);
        self.confirm_exit(kind, cx);
    }

    /// Every confirmed Quit and Restart comes through here.
    pub(super) fn confirm_exit(&mut self, kind: ExitKind, cx: &mut Context<Self>) {
        self.exit_countdown = None;
        match kind {
            ExitKind::Quit => self.confirm_quit_exit(cx),
            ExitKind::Restart => self.begin_restart(cx),
        }
    }

    /// `dialog` counted as open until it answers or is dropped: a confirmed
    /// Restart doesn't start while a native file dialog is open, since what the
    /// dialog does next (a Save As, a new project) passes no departure gate.
    pub(super) fn native_dialog<F: Future>(
        &self,
        dialog: F,
    ) -> impl Future<Output = F::Output> + use<F> {
        let open = DialogOpen::new(&self.native_dialogs);
        async move {
            let answer = dialog.await;
            drop(open);
            answer
        }
    }

    fn confirm_quit_exit(&mut self, cx: &mut Context<Self>) {
        match self.departing {
            // A Restart under way ends this process itself. If it gives up,
            // the Quit runs then.
            Some(Stage::Preparing) => {
                self.quit_after_restart = true;
                return;
            }
            Some(Stage::Committed) => return,
            None => {}
        }
        self.interrupt_input(cx);
        self.confirm_quit = true;
        self.quit_after_closes(cx);
    }

    fn begin_restart(&mut self, cx: &mut Context<Self>) {
        if self.departing.is_some() {
            return;
        }
        let UpdateState::Ready(plan) = &self.update_state else {
            return;
        };
        let plan = plan.clone();
        self.show_quit_confirm = false;
        if self.native_dialogs.get() > 0 {
            self.add_event(
                NotifKind::Error,
                t("Restart didn't start"),
                t("Close the file dialog first, then restart again."),
            );
            cx.notify();
            return;
        }
        // It ends this process too, so it takes over a Quit still waiting for
        // tmux sessions to die.
        self.confirm_quit = false;
        if matches!(self.close_departure, Some(CloseDeparture::Quit)) {
            self.close_departure = None;
            self.close_departure_task = None;
        }
        let interrupted = self.interrupt_input(cx);
        self.departing = Some(Stage::Preparing);
        self.restart_task = Some(cx.spawn(async move |this, cx| {
            if let Err(abort) = Self::prepare_restart(&this, &plan, cx).await {
                let _ = this.update(cx, |app, cx| app.abort_restart(abort, interrupted, cx));
            }
        }));
        cx.notify();
    }

    /// Interrupt every pending input operation and the terminals' startup
    /// automation, and admit no new operation. Returns how many it interrupted.
    fn interrupt_input(&mut self, cx: &App) -> usize {
        let operations = self.pending_inputs.depart().len();
        let automation: usize = self
            .live_sessions(cx)
            .iter()
            .map(|(_, session)| session.interrupt_pending_input())
            .sum();
        operations + automation
    }

    /// Every live terminal, docked or popped out.
    pub(super) fn live_sessions(&self, cx: &App) -> Vec<(Uuid, Arc<TerminalSession>)> {
        let popped = popout_terminal_views(&self.terminals, &self.popouts, PopOut::terminal_view)
            .map(|(iid, _, view)| (iid, view));
        self.terminals
            .iter()
            .chain(popped)
            .map(|(iid, view)| (*iid, view.read(cx).session().clone()))
            .collect()
    }

    /// No launch or editor move is under way.
    fn restart_settled(&self) -> bool {
        self.terminal_launching.is_empty()
            && self.pending_editor_rebuild.is_empty()
            && self.pending_editor_redock.is_empty()
    }

    /// Hold every terminal's input for the Restart, and fence its writers.
    fn hold_for_restart(&self, cx: &App) -> Vec<(String, InputFence)> {
        self.live_sessions(cx)
            .into_iter()
            .map(|(iid, session)| {
                let name = self
                    .workspace
                    .instance(iid)
                    .map(|inst| inst.display_name().to_string())
                    .unwrap_or_default();
                (name, session.hold_for_departure())
            })
            .collect()
    }

    async fn prepare_restart(
        this: &WeakEntity<Self>,
        plan: &RelaunchPlan,
        cx: &mut AsyncApp,
    ) -> Result<(), Abort> {
        // Launches and editor moves already under way finish first, so no
        // terminal is installed after the holds.
        let mut waited = Duration::ZERO;
        while !this
            .update(cx, |app, _| app.restart_settled())
            .map_err(|_| Abort::Gone)?
        {
            if waited >= SETTLE {
                return Err(Abort::Settle);
            }
            cx.background_executor().timer(TICK).await;
            waited += TICK;
        }

        // Every write queued before the holds is written or has failed. One
        // deadline covers the whole wait, PTY and tmux alike.
        let fences = this
            .update(cx, |app, cx| app.hold_for_restart(cx))
            .map_err(|_| Abort::Gone)?;
        let (done, answered) = async_channel::bounded(1);
        cx.spawn(async move |_| {
            let mut unknown = None;
            for (name, fence) in fences {
                if let Err(error) = fence.acked().await {
                    unknown.get_or_insert((name, error));
                }
            }
            let _ = done.send(unknown).await;
        })
        .detach();
        let mut waited = Duration::ZERO;
        let unknown = loop {
            if let Ok(unknown) = answered.try_recv() {
                break unknown;
            }
            if waited >= FENCE {
                return Err(Abort::Failed(
                    t("Input already sent to a terminal didn't finish within 2 seconds.")
                        .to_string(),
                ));
            }
            cx.background_executor().timer(TICK).await;
            waited += TICK;
        };
        if let Some((name, error)) = unknown {
            return Err(Abort::Failed(tf(
                "{name}'s input couldn't be confirmed: {error}",
                &[
                    ("name", name.as_str()),
                    ("error", error.to_string().as_str()),
                ],
            )));
        }

        // Finish the panes still closing and let their tmux sessions die, as a
        // Quit does. Then save and start the new version in one turn, so
        // nothing changes between them. A pane closed during the wait is
        // finished the same way first.
        loop {
            this.update(cx, |app, cx| {
                app.finalize_all_closes(cx);
            })
            .map_err(|_| Abort::Gone)?;
            while !this
                .update(cx, |app, _| {
                    app.close_tmux_kills.retain(|task| !task.is_ready());
                    app.close_tmux_kills.is_empty()
                })
                .map_err(|_| Abort::Gone)?
            {
                cx.background_executor().timer(TICK).await;
            }
            let started = this
                .update(cx, |app, cx| {
                    if !app.close_groups.is_empty() {
                        return Ok(false);
                    }
                    app.save_for_departure()?;
                    app.commit_restart(plan, cx).map(|()| true)
                })
                .map_err(|_| Abort::Gone)??;
            if started {
                return Ok(());
            }
        }
    }

    /// The Restart's one save, and its last write, so the new version loads
    /// what this one holds.
    fn save_for_departure(&mut self) -> Result<(), Abort> {
        let failed = || Abort::Failed(t("The workspace couldn't be saved.").to_string());
        let Some(id) = self.current_workspace else {
            return Ok(());
        };
        let path = self.workspace_doc_path(id).ok_or_else(failed)?;
        let written = muxel_store::save_workspace_to(&path, &self.workspace);
        if !self.note_workspace_save(&path, written) {
            return Err(failed());
        }
        Ok(())
    }

    /// Start the new version, then quit.
    fn commit_restart(&mut self, plan: &RelaunchPlan, cx: &mut Context<Self>) -> Result<(), Abort> {
        if self.departing != Some(Stage::Preparing) {
            return Ok(());
        }
        (self.relaunch)(&self.restart_plan(plan)).map_err(|error| {
            Abort::Failed(tf(
                "The new version didn't start: {error}",
                &[("error", error.to_string().as_str())],
            ))
        })?;
        self.departing = Some(Stage::Committed);
        self.end_after_restart(cx);
        Ok(())
    }

    /// `plan`, telling the new version to wait until this one has released
    /// its workspace.
    pub(super) fn restart_plan(&self, plan: &RelaunchPlan) -> RelaunchPlan {
        RelaunchPlan {
            app_args: crate::restart_handoff::args_for(self.current_workspace),
            ..plan.clone()
        }
    }

    /// Quit once the new version has started. Tests record it instead: the
    /// harness must never quit the real app.
    fn end_after_restart(&mut self, cx: &mut Context<Self>) {
        #[cfg(test)]
        {
            let _ = cx;
            self.restart_ended = true;
        }
        #[cfg(not(test))]
        {
            self.confirm_quit = true;
            cx.quit();
        }
    }

    /// The Restart gave up: input flows again and the app stays as it was,
    /// except that what it interrupted stays cancelled.
    fn abort_restart(&mut self, abort: Abort, interrupted: usize, cx: &mut Context<Self>) {
        if self.departing != Some(Stage::Preparing) {
            return;
        }
        for (_, session) in self.live_sessions(cx) {
            session.release_departure();
        }
        self.departing = None;
        // Saves were held for the Restart's own; write what they held.
        self.persist();
        self.pending_inputs.resume();
        if let Some(task) = self.restart_task.take() {
            task.detach();
        }
        let reason = match abort {
            Abort::Settle => {
                t("A terminal is still starting. Restart again in a moment.").to_string()
            }
            Abort::Failed(reason) => reason,
            Abort::Gone => return,
        };
        let detail = if interrupted > 0 {
            format!(
                "{reason} {}",
                t("Interrupted sends, keys and dictation were cancelled and won't resume.")
            )
        } else {
            reason
        };
        self.add_event(NotifKind::Error, t("Restart failed"), detail);
        if std::mem::take(&mut self.quit_after_restart) {
            self.confirm_quit_exit(cx);
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::changed_panes;
    use muxel_core::control::AgentState;
    use muxel_core::recoverable_close::{CloseEvidence, CloseFacts};
    use uuid::Uuid;

    fn facts(status: AgentState) -> CloseFacts {
        CloseFacts {
            status,
            evidence: CloseEvidence::Title,
            attributable: true,
            awaiting_reply: false,
            another_tmux_client: false,
            session_may_outlive: false,
            launch_id: None,
            binding_rev: 0,
            input_rev: 0,
            completion_rev: 0,
            shell_marks: 0,
        }
    }

    /// The countdown's question names the panes that changed under it and
    /// the panes added, not the ones that stayed as they were or closed.
    #[test]
    fn a_countdown_names_what_changed_under_it() {
        let [same, moved, added, closed] = [(); 4].map(|_| Uuid::new_v4());
        let before = [
            (same, facts(AgentState::Idle)),
            (moved, facts(AgentState::Idle)),
            (closed, facts(AgentState::Done)),
        ];
        let now = [
            (same, facts(AgentState::Idle)),
            (
                moved,
                CloseFacts {
                    input_rev: 1,
                    ..facts(AgentState::Idle)
                },
            ),
            (added, facts(AgentState::Done)),
        ];
        assert_eq!(changed_panes(&before, &now), [moved, added]);
        assert!(changed_panes(&before, &before).is_empty());
    }
}
