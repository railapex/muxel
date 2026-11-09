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

use super::pending_input::{PendingEntry, PendingKind};
use super::recoverable_close::CloseDeparture;
use super::{MuxelApp, NotifKind, PopOut, UpdateState, popout_terminal_views};
use crate::i18n::{t, tf, tn};
use crate::update::RelaunchPlan;
use gpui::{App, AsyncApp, Context, WeakEntity};
use muxel_terminal::{InputFence, TerminalSession};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
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

/// What a confirmed exit cuts short, one line for each kind of pending input.
/// `name` names a pane, and `host` the remote host a pane's uploads go to.
pub(super) fn loss_lines(
    entries: &[PendingEntry],
    name: impl Fn(Uuid) -> Option<String>,
    host: impl Fn(Uuid) -> Option<String>,
) -> Vec<String> {
    let of = |kind: PendingKind| -> Vec<&PendingEntry> {
        entries.iter().filter(|entry| entry.kind == kind).collect()
    };
    let pane_name = |entry: &PendingEntry| {
        entry
            .pane
            .and_then(&name)
            .unwrap_or_else(|| t("A pane").to_string())
    };
    let mut lines = Vec::new();
    let sends = of(PendingKind::CtlSend);
    if let Some(first) = sends.first() {
        let n = sends.len().to_string();
        lines.push(tn(
            "{name}'s ctl send won't press Enter; text already pasted stays in the pane.",
            "{n} ctl sends won't press Enter; text already pasted stays in their panes.",
            sends.len(),
            &[("name", pane_name(first).as_str()), ("n", n.as_str())],
        ));
    }
    let keys = of(PendingKind::CtlKeys);
    if let Some(first) = keys.first() {
        let n = keys.len().to_string();
        lines.push(tn(
            "{name}'s remaining ctl keys won't be pressed; keys already pressed stay.",
            "{n} ctl key sequences stop; keys already pressed stay.",
            keys.len(),
            &[("name", pane_name(first).as_str()), ("n", n.as_str())],
        ));
    }
    if !of(PendingKind::Dictation).is_empty() {
        lines.push(t("Dictation in progress will be discarded.").to_string());
    }
    let uploads = of(PendingKind::Upload);
    if !uploads.is_empty() {
        let mut hosts: Vec<String> = uploads
            .iter()
            .filter_map(|entry| entry.pane.and_then(&host))
            .collect();
        hosts.sort();
        hosts.dedup();
        lines.push(match hosts.as_slice() {
            [only] => tf(
                "Files already uploaded to {host} stay there. They won't be pasted.",
                &[("host", only.as_str())],
            ),
            _ => t("Files already uploaded stay on their hosts. They won't be pasted.").to_string(),
        });
    }
    if !of(PendingKind::WakeSweep).is_empty() {
        lines.push(t("Waking panes stops. Panes not yet woken stay as they are.").to_string());
    }
    lines
}

impl MuxelApp {
    /// Ask before quitting or restarting. Nothing to ask while a Restart is
    /// already under way.
    pub(super) fn ask_exit(&mut self, kind: ExitKind, cx: &mut Context<Self>) {
        if self.departing.is_some() {
            return;
        }
        self.quit_kill_tmux_local = false;
        self.quit_kill_tmux_remote = false;
        self.quit_confirm_kind = kind;
        self.show_quit_confirm = true;
        cx.notify();
    }

    /// Every confirmed Quit and Restart comes through here.
    pub(super) fn confirm_exit(&mut self, kind: ExitKind, cx: &mut Context<Self>) {
        match kind {
            ExitKind::Quit => self.confirm_quit_exit(cx),
            ExitKind::Restart => self.begin_restart(cx),
        }
    }

    /// The quit confirmation's lines for the input it would cut short.
    pub(super) fn exit_loss_lines(&self) -> Vec<String> {
        loss_lines(
            &self.pending_inputs.pending(),
            |iid| {
                self.workspace
                    .instance(iid)
                    .map(|i| i.display_name().to_string())
            },
            |iid| {
                self.remote_host_for_instance(iid)
                    .map(|host| host.name.clone())
            },
        )
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
    fn live_sessions(&self, cx: &App) -> Vec<(Uuid, Arc<TerminalSession>)> {
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

    /// The Restart's one save, and its last write. It writes the workspace's
    /// bytes, then checks that the workspace still serializes to them, so the
    /// new version loads what this one holds.
    fn save_for_departure(&mut self) -> Result<(), Abort> {
        let failed = || Abort::Failed(t("The workspace couldn't be saved.").to_string());
        let Some(id) = self.current_workspace else {
            return Ok(());
        };
        let path = self.workspace_doc_path(id).ok_or_else(failed)?;
        let json = match muxel_store::workspace_json(&self.workspace) {
            Ok(json) => json,
            Err(error) => {
                self.note_workspace_save(&path, Err(error));
                return Err(failed());
            }
        };
        let written = muxel_store::write_workspace_json(&path, &json);
        if !self.note_workspace_save(&path, written) {
            return Err(failed());
        }
        #[cfg(test)]
        if let Some(hook) = self.after_departure_save {
            hook(self);
        }
        if muxel_store::workspace_json(&self.workspace).ok().as_ref() != Some(&json) {
            return Err(Abort::Failed(
                t("The workspace changed as it was saved.").to_string(),
            ));
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
    use super::loss_lines;
    use crate::app::pending_input::{PendingEntry, PendingKind};
    use uuid::Uuid;

    fn entry(kind: PendingKind, pane: Option<Uuid>) -> PendingEntry {
        PendingEntry { kind, pane }
    }

    /// One line per kind, in a fixed order, naming the pane or host when there
    /// is just one, and saying what already happened stays.
    #[test]
    fn the_confirmation_names_each_kind_of_input_it_cuts_short_once() {
        let alpha = Uuid::new_v4();
        let beta = Uuid::new_v4();
        let name = |iid: Uuid| (iid == alpha).then(|| "Alpha".to_string());
        let host = |iid: Uuid| (iid == beta).then(|| "host-x".to_string());
        assert!(loss_lines(&[], name, host).is_empty());
        assert_eq!(
            loss_lines(
                &[
                    entry(PendingKind::WakeSweep, None),
                    entry(PendingKind::Upload, Some(beta)),
                    entry(PendingKind::CtlSend, Some(alpha)),
                    entry(PendingKind::Dictation, None),
                    entry(PendingKind::CtlKeys, Some(beta)),
                ],
                name,
                host,
            ),
            [
                "Alpha's ctl send won't press Enter; text already pasted stays in the pane.",
                "A pane's remaining ctl keys won't be pressed; keys already pressed stay.",
                "Dictation in progress will be discarded.",
                "Files already uploaded to host-x stay there. They won't be pasted.",
                "Waking panes stops. Panes not yet woken stay as they are.",
            ]
        );
        assert_eq!(
            loss_lines(
                &[
                    entry(PendingKind::CtlSend, Some(alpha)),
                    entry(PendingKind::CtlSend, Some(beta)),
                    entry(PendingKind::Upload, Some(alpha)),
                ],
                name,
                host,
            ),
            [
                "2 ctl sends won't press Enter; text already pasted stays in their panes.",
                "Files already uploaded stay on their hosts. They won't be pasted.",
            ]
        );
    }
}
