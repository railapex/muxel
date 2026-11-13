//! `muxel ctl close`: closing panes from outside muxel, all or none.
//!
//! No person confirmed such a close, so every pane goes through the same
//! countdown and Undo as a close made in muxel, named as the caller's. A pane
//! muxel would ask about before closing refuses unless `--force`, and a
//! refusal closes nothing. Notifications keep a line for each close after the
//! countdown and its Undo are gone.

use super::*;
use muxel_core::recoverable_close::{CloseDecision, close_refusal, decide};

/// A `close` whose panes all passed their first checks, on its way to the
/// tmux client probe that busy needs.
pub(in crate::app) struct CloseRequest {
    /// Each pane, with the AGENT the caller named it by.
    panes: Vec<(String, Uuid)>,
    force: bool,
    caller: Option<Uuid>,
}

/// The refusal of a whole `close`: the first refused pane's code, and every
/// refused pane in `refused`.
fn close_refused(refused: Vec<ctl::Refusal>) -> ctl::Error {
    let reasons: Vec<&str> = refused.iter().map(|r| r.error.as_str()).collect();
    ctl::Error {
        code: refused.first().and_then(|r| r.code),
        reason: refused.first().and_then(|r| r.reason.clone()),
        message: format!("nothing was closed: {}", reasons.join("; ")),
        refused,
        ..ctl::Error::from(String::new())
    }
}

fn refusal(agent: &str, uuid: Option<Uuid>, error: ctl::Error) -> ctl::Refusal {
    ctl::Refusal {
        agent: agent.to_string(),
        uuid,
        code: error.code,
        reason: error.reason,
        error: error.message,
    }
}

/// What a busy pane's refusal says, by [`close_refusal`]'s code and reason.
fn busy_message(name: &str, code: ErrorCode, reason: Option<&str>) -> String {
    let what = match (code, reason) {
        (ErrorCode::Working, _) => format!("'{name}' is working"),
        (ErrorCode::Blocked, _) => format!("'{name}' is waiting on a question"),
        (ErrorCode::Starting, _) => format!("'{name}' is still starting"),
        (_, Some("settling")) => format!("muxel is still settling what '{name}' is doing"),
        (_, Some("awaiting_reply")) => {
            format!("'{name}' was just sent a prompt and hasn't shown its turn yet")
        }
        (_, Some("another_client")) => {
            format!("'{name}' may have another tmux client attached, whose typing muxel can't see")
        }
        (_, Some("session_may_outlive")) => {
            format!("'{name}''s tmux session may still be running its agent")
        }
        _ => format!("muxel can't confirm that '{name}' is idle"),
    };
    format!("{what}: pass --force to close it anyway")
}

impl MuxelApp {
    /// Resolve and check every pane `close` names. Any refusal refuses all;
    /// two names for one pane close it once.
    pub(super) fn control_close_request(
        &self,
        agents: Vec<String>,
        force: bool,
        caller: Option<Uuid>,
        cx: &App,
    ) -> Answer {
        if agents.is_empty() {
            return Answer::Now(Err(
                "`close` needs at least one AGENT (an id from `muxel ctl panes`)".into(),
            ));
        }
        let mut panes: Vec<(String, Uuid)> = Vec::new();
        let mut refused = Vec::new();
        for agent in agents {
            let iid = match self.control_resolve(&agent) {
                Ok(iid) => iid,
                Err(error) => {
                    refused.push(refusal(&agent, None, error));
                    continue;
                }
            };
            if panes.iter().any(|&(_, pane)| pane == iid) {
                continue;
            }
            match self.control_closable(iid, cx) {
                Ok(()) => panes.push((agent, iid)),
                Err(error) => refused.push(refusal(&agent, Some(iid), error)),
            }
        }
        if !refused.is_empty() {
            return Answer::Now(Err(close_refused(refused)));
        }
        Answer::Close(CloseRequest {
            panes,
            force,
            caller,
        })
    }

    /// Whether outside control may close this pane at all, whatever it is
    /// doing: the same grants as typing into it.
    fn control_closable(&self, iid: Uuid, cx: &App) -> CtlResult<()> {
        if !self.settings.control_enabled {
            return Err(ctl::creation_refusal(ErrorCode::ControlDisabled));
        }
        if let Some(error) = self.closing_control_error(iid) {
            return Err(error);
        }
        // A close the user started in muxel is waiting on them: its prompt, a
        // worktree's question, or its own tmux client probe.
        if self.close_requests.contains_key(&iid)
            || self
                .pending_worktree_dispose
                .iter()
                .any(|d| d.close_after.contains(&iid) || d.close_rest.contains(&iid))
        {
            return Err(ctl::Error::new(
                ErrorCode::Closing,
                "a close of this pane is already waiting on the user in muxel",
            )
            .with_reason("requested"));
        }
        let inst = self
            .workspace
            .instance(iid)
            .ok_or("that agent has just closed")?;
        if inst.kind != InstanceKind::Terminal {
            return Err(format!(
                "'{}' isn't a terminal pane: only the user closes it",
                inst.display_name()
            )
            .into());
        }
        let fallback_shell = self
            .terminal_view(iid)
            .is_some_and(|view| view.read(cx).launch_error().is_some());
        control_program_writable(inst, self.settings.control_allow_shells, fallback_shell)?;
        Ok(())
    }

    /// Read the panes' tmux clients off the UI thread, then decide and close
    /// them, or refuse all of them, and reply.
    pub(super) fn control_close(
        &mut self,
        request: CloseRequest,
        reply: std::sync::mpsc::Sender<ctl::Response>,
        cx: &mut Context<Self>,
    ) {
        let ids: Vec<Uuid> = request.panes.iter().map(|&(_, iid)| iid).collect();
        let probe = self.close_client_probe(&ids, cx);
        let workspace = self.current_workspace;
        cx.spawn(async move |this, cx| {
            let counts = match probe {
                Some(probe) => probe.await,
                None => Vec::new(),
            };
            let result = this
                .update(cx, |app, cx| {
                    if app.current_workspace != workspace {
                        return Err("the workspace changed before the panes closed".into());
                    }
                    app.close_client_counts.extend(counts);
                    app.finish_control_close(request, cx)
                })
                .unwrap_or_else(|_| Err("muxel is shutting down".into()));
            let _ = reply.send(match result {
                Ok(value) => ctl::Response::ok(value),
                Err(error) => ctl::Response::err(error),
            });
        })
        .detach();
    }

    fn finish_control_close(
        &mut self,
        request: CloseRequest,
        cx: &mut Context<Self>,
    ) -> CtlResult<Value> {
        let mut steps = Vec::new();
        let mut refused = Vec::new();
        for (agent, iid) in &request.panes {
            // Again: the probe took time, and the pane may have changed.
            if let Err(error) = self.control_closable(*iid, cx) {
                refused.push(refusal(agent, Some(*iid), error));
                continue;
            }
            let facts = self.close_facts(*iid, cx);
            let step = match decide(facts, true) {
                CloseDecision::Immediate => None,
                CloseDecision::Hold { forced } => Some(forced),
                CloseDecision::Ask if request.force => Some(true),
                CloseDecision::Ask => {
                    let (code, reason) =
                        close_refusal(facts).unwrap_or((ErrorCode::Busy, Some("unconfirmed_idle")));
                    let name = self
                        .workspace
                        .instance(*iid)
                        .map(|inst| inst.display_name().to_string())
                        .unwrap_or_default();
                    let mut error = ctl::Error::new(code, busy_message(&name, code, reason));
                    error.reason = reason.map(str::to_string);
                    refused.push(refusal(agent, Some(*iid), error));
                    continue;
                }
            };
            steps.push((*iid, step));
        }
        if !refused.is_empty() {
            return Err(close_refused(refused));
        }
        let caller = request
            .caller
            .and_then(|id| self.workspace.instance(id))
            .map_or_else(
                || "muxel ctl".to_string(),
                |inst| inst.display_name().to_string(),
            );
        let named: Vec<(Uuid, String, Option<bool>)> = steps
            .iter()
            .map(|&(iid, step)| {
                let name = self
                    .workspace
                    .instance(iid)
                    .map(|inst| inst.display_name().to_string())
                    .unwrap_or_default();
                (iid, name, step)
            })
            .collect();
        self.accept_control_close(steps, caller.clone(), cx);
        self.record_control_close(&caller, request.caller, &named);
        let panes: Vec<Value> = named
            .iter()
            .map(|(iid, _, step)| {
                let undo_until = step.and_then(|_| self.closing_until(*iid));
                json!({ "agent": ctl::short_id(*iid), "uuid": iid, "undo_until": undo_until })
            })
            .collect();
        let undo_until = named
            .iter()
            .filter(|(_, _, step)| step.is_some())
            .find_map(|(iid, _, _)| self.closing_until(*iid));
        Ok(json!({ "closed": true, "undo_until": undo_until, "panes": panes }))
    }

    /// The line a close leaves after its countdown and Undo are gone: who
    /// closed what, and which panes it forced. In Notifications for whoever
    /// comes back to the screen, and in the event log for good.
    fn record_control_close(
        &mut self,
        caller: &str,
        caller_id: Option<Uuid>,
        closed: &[(Uuid, String, Option<bool>)],
    ) {
        let names: Vec<&str> = closed.iter().map(|(_, name, _)| name.as_str()).collect();
        let forced: Vec<&str> = closed
            .iter()
            .filter(|(_, _, step)| *step == Some(true))
            .map(|(_, name, _)| name.as_str())
            .collect();
        let subtitle = if forced.is_empty() {
            t("closed through muxel ctl").to_string()
        } else {
            tf(
                "closed through muxel ctl, forced: {names}",
                &[("names", &forced.join(", "))],
            )
        };
        self.add_event(
            NotifKind::Success,
            tf(
                "{caller} closed {names}",
                &[("caller", caller), ("names", &names.join(", "))],
            ),
            subtitle,
        );
        let caller_id = caller_id.map_or_else(|| "-".to_string(), |id| id.to_string());
        for (iid, name, step) in closed {
            let how = match step {
                None => "now",
                Some(false) => "held",
                Some(true) => "forced",
            };
            muxel_store::append_event_log(&format!(
                "ctl-close: \"{caller}\" [{caller_id}] closed \"{name}\" [{iid}] {how}"
            ));
        }
    }
}
