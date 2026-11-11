//! `send --once` and `send --when-idle`: typing a prompt into a pane at most
//! once, and waiting in muxel for a moment it is safe to type it.
//!
//! Both are for automation that can't watch the pane, such as a mail doorbell.
//! Neither is durable: the keys and the waiting sends live in this process.

use super::*;
use std::collections::VecDeque;

/// `--once` keys each pane keeps, newest last.
const ONCE_KEYS_PER_PANE: usize = 64;
/// `send --when-idle` calls one muxel holds at once. Each holds one of the
/// control server's connections, and interactive `ctl` calls need the rest.
pub(super) const MAX_QUEUED_SENDS: usize = 8;

#[derive(Clone, Debug)]
enum OnceState {
    /// The send with this key is still running. `typing` once it may have
    /// typed: its paste has been written.
    Pending { typing: bool },
    /// The send typed and ended with this reply.
    Done(CtlResult<Value>),
}

/// Each pane's recent `--once` keys. A key is used once the send that carries
/// it may have typed. A send refused before typing leaves its key unused.
#[derive(Default)]
pub(in crate::app) struct OnceKeys {
    by_pane: HashMap<Uuid, VecDeque<(String, OnceState)>>,
}

impl OnceKeys {
    /// Take `key` for a new send to `pane`, or the answer a repeat gets:
    /// the first receipt with `duplicate: true`, `duplicate` with the first
    /// failure, or `busy` while the first is still running.
    pub(super) fn reserve(&mut self, pane: Uuid, key: &str) -> Option<CtlResult<Value>> {
        let keys = self.by_pane.entry(pane).or_default();
        if let Some((_, state)) = keys.iter().find(|(held, _)| held == key) {
            return Some(match state {
                OnceState::Pending { typing } => {
                    let busy = ctl::Error::new(
                        ErrorCode::Busy,
                        "the first send with this --once key hasn't finished: ask again later, \
                         or read it with `once-status`",
                    )
                    .with_reason("once_pending");
                    Err(if *typing { busy.after_paste() } else { busy })
                }
                OnceState::Done(Ok(receipt)) => {
                    let mut repeat = receipt.clone();
                    repeat["duplicate"] = json!(true);
                    Ok(repeat)
                }
                OnceState::Done(Err(first)) => Err(ctl::Error::duplicate_of(first)),
            });
        }
        if keys.len() >= ONCE_KEYS_PER_PANE {
            let Some(oldest) = keys
                .iter()
                .position(|(_, state)| matches!(state, OnceState::Done(_)))
            else {
                return Some(Err(ctl::Error::new(
                    ErrorCode::Busy,
                    "64 sends with --once keys are still running for this pane",
                )
                .with_reason("once_pending")));
            };
            keys.remove(oldest);
        }
        keys.push_back((key.to_owned(), OnceState::Pending { typing: false }));
        None
    }

    /// The send holding `key` has written its paste.
    pub(super) fn typing(&mut self, pane: Uuid, key: &str) {
        if let Some(state) = self.state_mut(pane, key)
            && let OnceState::Pending { typing } = state
        {
            *typing = true;
        }
    }

    /// The send holding `key` ended with `result`. One that typed nothing
    /// gives the key back.
    pub(super) fn finish(&mut self, pane: Uuid, key: &str, result: &CtlResult<Value>) {
        let typed = match result {
            Ok(_) => true,
            Err(error) => error.pasted == Some(true),
        };
        let Some(keys) = self.by_pane.get_mut(&pane) else {
            return;
        };
        let Some(at) = keys
            .iter()
            .position(|(held, state)| held == key && matches!(state, OnceState::Pending { .. }))
        else {
            return;
        };
        if typed {
            keys[at].1 = OnceState::Done(result.clone());
        } else {
            keys.remove(at);
        }
    }

    /// `once-status`: what became of `key` on `pane`, without changing it.
    pub(super) fn status(&self, pane: Uuid, key: &str) -> Value {
        let state = self
            .by_pane
            .get(&pane)
            .and_then(|keys| keys.iter().find(|(held, _)| held == key))
            .map(|(_, state)| state);
        match state {
            None => json!({ "state": "unused" }),
            Some(OnceState::Pending { typing: false }) => json!({ "state": "pending" }),
            Some(OnceState::Pending { typing: true }) => json!({ "state": "typing" }),
            Some(OnceState::Done(result)) => json!({
                "state": "done",
                "result": match result {
                    Ok(receipt) => ctl::Response::ok(receipt.clone()),
                    Err(error) => ctl::Response::err(error.clone()),
                },
            }),
        }
    }

    pub(super) fn retain(&mut self, live: &HashSet<Uuid>) {
        self.by_pane.retain(|pane, _| live.contains(pane));
    }

    pub(in crate::app) fn clear(&mut self) {
        self.by_pane.clear();
    }

    fn state_mut(&mut self, pane: Uuid, key: &str) -> Option<&mut OnceState> {
        self.by_pane
            .get_mut(&pane)?
            .iter_mut()
            .find(|(held, _)| held == key)
            .map(|(_, state)| state)
    }
}

/// A `send --when-idle` waiting for its pane.
pub(in crate::app) struct QueuedSend {
    text: String,
    once: Option<String>,
    /// The terminal launch it types into: `--launch-id`, else the first one
    /// it sees.
    launch: Option<Uuid>,
    /// The provider conversation it types into: `--session-id`, else the one
    /// muxel could vouch for when it was queued.
    conversation: Option<String>,
    queued_at: Instant,
    deadline: Instant,
    /// What it is waiting on, for its `timed_out` reason.
    waiting_on: &'static str,
    reply: std::sync::mpsc::Sender<ctl::Response>,
    cancelled: Arc<AtomicBool>,
    /// A check of the tmux session's other clients is running.
    checking_clients: bool,
    /// When a check last found another client attached. Over SSH each check
    /// is a command on the host, so they are spaced out.
    clients_seen: Option<Instant>,
}

/// How long after finding another tmux client a waiting send asks again.
const CLIENTS_RECHECK: Duration = Duration::from_secs(5);

/// What a waiting send does now.
#[derive(Debug, PartialEq)]
enum Verdict {
    Wait(&'static str),
    Send,
    Refuse(ctl::Error),
}

/// What a pane looks like to a waiting send. Built by the app, decided here.
struct PaneFacts {
    cancelled: bool,
    expired: bool,
    /// `control_writable`: the launch it may type into, and what muxel can
    /// vouch for about its conversation.
    writable: CtlResult<(Uuid, ctl::Conversation)>,
    /// What an ordinary send would refuse with now, if anything.
    refusal: Option<ctl::Error>,
    send_in_flight: bool,
}

/// Wait while the pane is busy in a way that passes; refuse once it can't
/// take this prompt at all; otherwise send.
fn verdict(queued: &QueuedSend, facts: PaneFacts) -> Verdict {
    if facts.cancelled {
        return Verdict::Refuse("the caller disconnected before the queued send was typed".into());
    }
    if facts.expired {
        return Verdict::Refuse(
            ctl::Error::new(
                ErrorCode::TimedOut,
                format!(
                    "the agent wasn't ready for the queued send in time (still {}): nothing was typed",
                    queued.waiting_on.replace('_', " ")
                ),
            )
            .with_reason(queued.waiting_on),
        );
    }
    let (launch, conversation) = match facts.writable {
        Ok(current) => current,
        Err(error) if error.code == Some(ErrorCode::Starting) => {
            return Verdict::Wait("starting");
        }
        Err(error) => return Verdict::Refuse(error),
    };
    if let Err(error) = ctl::check_launch_id(queued.launch, Some(launch)) {
        return Verdict::Refuse(error);
    }
    match ctl::check_session_id(queued.conversation.as_deref(), &conversation) {
        Ok(()) => {}
        // A pane muxel can't vouch for yet may settle back into this one.
        Err(error) if error.reason.as_deref() == Some("conversation_unconfirmed") => {
            return Verdict::Wait("conversation_unconfirmed");
        }
        Err(error) => return Verdict::Refuse(error),
    }
    if facts.send_in_flight {
        return Verdict::Wait("send_in_flight");
    }
    match facts.refusal {
        None => Verdict::Send,
        Some(refusal) => Verdict::Wait(match (refusal.code, refusal.reason.as_deref()) {
            (Some(ErrorCode::Working), Some("just_submitted")) => "just_submitted",
            (Some(ErrorCode::Blocked), _) => "blocked",
            (Some(ErrorCode::DraftPresent), _) => "draft_present",
            _ => "working",
        }),
    }
}

/// The `send --when-idle` request, before it waits.
pub(in crate::app) struct QueueRequest {
    pub(super) iid: Uuid,
    pub(super) text: String,
    pub(super) launch_id: Option<Uuid>,
    pub(super) session_id: Option<String>,
    pub(super) once: Option<String>,
    pub(super) timeout_secs: Option<u64>,
}

impl MuxelApp {
    /// Queue a `send --when-idle`, or refuse it, and try it at once.
    pub(super) fn queue_control_send(
        &mut self,
        request: QueueRequest,
        reply: std::sync::mpsc::Sender<ctl::Response>,
        cancelled: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        let QueueRequest {
            iid,
            text,
            launch_id,
            session_id,
            once,
            timeout_secs,
        } = request;
        let refuse = |app: &mut Self, error: ctl::Error| {
            if let Some(key) = once.as_deref() {
                app.control_once.finish(iid, key, &Err(error.clone()));
            }
            let _ = reply.send(ctl::Response::err(error));
        };
        if text.trim().is_empty() {
            return refuse(self, "nothing to send".into());
        }
        if self.control_queue.contains_key(&iid) {
            return refuse(
                self,
                ctl::Error::new(
                    ErrorCode::Busy,
                    "another send already waits for this agent: nothing was typed",
                )
                .with_reason("queued"),
            );
        }
        if self.control_queue.len() >= MAX_QUEUED_SENDS {
            return refuse(
                self,
                ctl::Error::new(
                    ErrorCode::Busy,
                    format!(
                        "{MAX_QUEUED_SENDS} sends already wait in this muxel: nothing was typed"
                    ),
                )
                .with_reason("queue_full"),
            );
        }
        // Another conversation refuses at once, even while the pane starts,
        // when it is matched on the id it will resume. One muxel can't vouch
        // for yet is waited out like any other busy state.
        let current = self.control_conversation(iid, cx);
        if let Err(error) = ctl::check_session_id(session_id.as_deref(), &current)
            && error.reason.as_deref() != Some("conversation_unconfirmed")
        {
            return refuse(self, error);
        }
        // A pane still starting has no launch to check yet; the first one
        // seen is the one it types into.
        let launch = match self.control_writable(iid, cx) {
            Ok(session) => {
                if let Err(error) = ctl::check_launch_id(launch_id, Some(session.id)) {
                    return refuse(self, error);
                }
                Some(session.id)
            }
            Err(error) if error.code == Some(ErrorCode::Starting) => launch_id,
            Err(error) => return refuse(self, error),
        };
        let queued_at = Instant::now();
        let wait = Duration::from_secs(timeout_secs.unwrap_or(ctl::WHEN_IDLE_TIMEOUT_SECS));
        let conversation = session_id.or_else(|| current.into_id());
        self.control_queue.insert(
            iid,
            QueuedSend {
                text,
                once,
                launch,
                conversation,
                queued_at,
                deadline: queued_at + wait,
                waiting_on: "working",
                reply,
                cancelled,
                checking_clients: false,
                clients_seen: None,
            },
        );
        self.pump_control_queue(cx);
    }

    fn queued_send_facts(&self, iid: Uuid, queued: &QueuedSend, cx: &App) -> PaneFacts {
        let writable = self
            .control_writable(iid, cx)
            .map(|session| (session.id, self.control_conversation(iid, cx)));
        let refusal = writable
            .as_ref()
            .ok()
            .and_then(|_| check_send_state(self.control_send_admission(iid, false, cx)).err());
        PaneFacts {
            cancelled: queued.cancelled.load(Ordering::Acquire),
            expired: Instant::now() >= queued.deadline,
            writable,
            refusal,
            send_in_flight: SendInFlight::held(iid),
        }
    }

    /// Decide every waiting send: run on the status tick and on arrival. A
    /// send ready to go through tmux first checks, off the UI thread, that no
    /// other tmux client is attached, since nothing here sees that client's
    /// typing.
    pub(in crate::app) fn pump_control_queue(&mut self, cx: &mut Context<Self>) {
        let panes: Vec<Uuid> = self.control_queue.keys().copied().collect();
        for iid in panes {
            let Some(queued) = self.control_queue.get(&iid) else {
                continue;
            };
            let facts = self.queued_send_facts(iid, queued, cx);
            let first_launch = facts.writable.as_ref().ok().map(|(launch, _)| *launch);
            let decided = verdict(queued, facts);
            let ask_clients_later = queued.checking_clients
                || queued
                    .clients_seen
                    .is_some_and(|seen| seen.elapsed() < CLIENTS_RECHECK);
            let Some(queued) = self.control_queue.get_mut(&iid) else {
                continue;
            };
            if queued.launch.is_none() {
                queued.launch = first_launch;
            }
            match decided {
                Verdict::Refuse(error) => {
                    if let Some(queued) = self.control_queue.remove(&iid) {
                        self.finish_queued_send(iid, queued, Err(error));
                    }
                }
                Verdict::Wait(reason) => queued.waiting_on = reason,
                Verdict::Send if ask_clients_later => {}
                Verdict::Send => {
                    let site = self
                        .workspace
                        .instance(iid)
                        .and_then(|inst| self.control_claim_site(inst));
                    let Some(site) = site else {
                        self.send_queued(iid, cx);
                        continue;
                    };
                    if let Some(queued) = self.control_queue.get_mut(&iid) {
                        queued.checking_clients = true;
                    }
                    cx.spawn(async move |this: WeakEntity<Self>, cx| {
                        let clients = cx
                            .background_executor()
                            .spawn(async move {
                                integrations::tmux_attached_clients(&site.loc, &site.session)
                            })
                            .await;
                        let _ = this.update(cx, |app, cx| {
                            let Some(queued) = app.control_queue.get_mut(&iid) else {
                                return;
                            };
                            queued.checking_clients = false;
                            // An unanswered count never reads as alone.
                            if clients.is_some_and(|clients| clients <= 1) {
                                app.send_queued(iid, cx);
                            } else {
                                queued.waiting_on = "tmux_clients";
                                queued.clients_seen = Some(Instant::now());
                            }
                        });
                    })
                    .detach();
                }
            }
        }
    }

    /// Send a waiting send whose pane is still ready, through the same path
    /// and every check an ordinary send has.
    fn send_queued(&mut self, iid: Uuid, cx: &mut Context<Self>) {
        let Some(queued) = self.control_queue.remove(&iid) else {
            return;
        };
        let facts = self.queued_send_facts(iid, &queued, cx);
        match verdict(&queued, facts) {
            Verdict::Send => {}
            Verdict::Wait(reason) => {
                self.control_queue.insert(
                    iid,
                    QueuedSend {
                        waiting_on: reason,
                        ..queued
                    },
                );
                return;
            }
            Verdict::Refuse(error) => return self.finish_queued_send(iid, queued, Err(error)),
        }
        // The conversation it queued in rides along to the paste, which
        // checks it once more.
        let pinned = queued.conversation.clone();
        match self.control_send(iid, queued.text.clone(), false, queued.launch, pinned, cx) {
            Ok(mut typing) => {
                typing.once = queued.once;
                typing.waited_ms =
                    Some(u64::try_from(queued.queued_at.elapsed().as_millis()).unwrap_or(u64::MAX));
                typing.cancelled = Some(queued.cancelled.clone());
                self.deliver_control_answer(
                    Answer::Type(Box::new(typing)),
                    queued.reply,
                    queued.cancelled,
                    cx,
                );
            }
            Err(error) => self.finish_queued_send(iid, queued, Err(error)),
        }
    }

    fn finish_queued_send(&mut self, iid: Uuid, queued: QueuedSend, result: CtlResult<Value>) {
        if let Some(key) = queued.once.as_deref() {
            self.control_once.finish(iid, key, &result);
        }
        let _ = queued.reply.send(match result {
            Ok(value) => ctl::Response::ok(value),
            Err(error) => ctl::Response::err(error),
        });
    }

    /// A workspace switch ends every waiting send: its pane is gone.
    pub(in crate::app) fn refuse_control_queue(&mut self) {
        for (iid, queued) in std::mem::take(&mut self.control_queue) {
            self.finish_queued_send(
                iid,
                queued,
                Err(ctl::Error::new(
                    ErrorCode::Exited,
                    "the workspace was switched while the send waited: nothing was typed",
                )),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OnceKeys, PaneFacts, QueuedSend, Verdict, verdict};
    use muxel_core::control::{self as ctl, ErrorCode};
    use serde_json::json;
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    #[test]
    fn a_repeat_after_a_submitted_send_returns_its_receipt_and_types_nothing() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        assert!(keys.reserve(pane, "d-9f2a").is_none());
        keys.typing(pane, "d-9f2a");
        let receipt = json!({"sent": true, "pasted": true, "outcome": "submitted"});
        keys.finish(pane, "d-9f2a", &Ok(receipt));
        let repeat = keys.reserve(pane, "d-9f2a").unwrap().unwrap();
        assert_eq!(repeat["duplicate"], true);
        assert_eq!(repeat["outcome"], "submitted");
        // Another pane, or another key, is a new send.
        assert!(keys.reserve(Uuid::new_v4(), "d-9f2a").is_none());
        assert!(keys.reserve(pane, "d-0000").is_none());
    }

    #[test]
    fn a_refusal_before_typing_leaves_the_key_unused() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        for refusal in [
            ctl::Error::new(ErrorCode::DraftPresent, "draft"),
            ctl::Error::new(ErrorCode::TimedOut, "late").with_reason("working"),
            ctl::Error::new(ErrorCode::TmuxRefused, "tmux said no"),
            ctl::Error::from("the caller disconnected"),
        ] {
            assert!(keys.reserve(pane, "d-1").is_none());
            keys.finish(pane, "d-1", &Err(refusal));
            assert_eq!(keys.status(pane, "d-1"), json!({"state": "unused"}));
        }
    }

    #[test]
    fn a_repeat_after_enter_was_withheld_is_a_duplicate_that_says_it_typed() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        keys.reserve(pane, "d-2");
        keys.typing(pane, "d-2");
        let withheld = ctl::Error::new(ErrorCode::InputChanged, "Enter withheld").after_paste();
        keys.finish(pane, "d-2", &Err(withheld));
        let repeat = keys.reserve(pane, "d-2").unwrap().unwrap_err();
        assert_eq!(repeat.code, Some(ErrorCode::Duplicate));
        assert_eq!(repeat.pasted, Some(true));
        let body = serde_json::to_value(ctl::Response::err(repeat)).unwrap();
        assert_eq!(body["original"]["code"], "input_changed");
        assert_eq!(keys.status(pane, "d-2")["result"]["code"], "input_changed");
    }

    #[test]
    fn a_repeat_while_the_first_runs_is_busy_and_says_once_it_typed() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        keys.reserve(pane, "d-3");
        let waiting = keys.reserve(pane, "d-3").unwrap().unwrap_err();
        assert_eq!(waiting.code, Some(ErrorCode::Busy));
        assert_eq!(waiting.reason.as_deref(), Some("once_pending"));
        assert_eq!(waiting.pasted, None);
        keys.typing(pane, "d-3");
        assert_eq!(
            keys.reserve(pane, "d-3").unwrap().unwrap_err().pasted,
            Some(true)
        );
        assert_eq!(keys.status(pane, "d-3"), json!({"state": "typing"}));
    }

    #[test]
    fn each_pane_keeps_its_last_64_finished_keys_and_every_running_one() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        for i in 0..64 {
            keys.reserve(pane, &format!("k{i}"));
            keys.finish(pane, &format!("k{i}"), &Ok(json!({"sent": true})));
        }
        keys.reserve(pane, "k64");
        assert_eq!(keys.status(pane, "k0"), json!({"state": "unused"}));
        assert_eq!(keys.status(pane, "k1")["state"], "done");
        let mut running = OnceKeys::default();
        for i in 0..64 {
            assert!(running.reserve(pane, &format!("r{i}")).is_none());
        }
        let full = running.reserve(pane, "r64").unwrap().unwrap_err();
        assert_eq!(full.code, Some(ErrorCode::Busy));
        assert_eq!(running.status(pane, "r0"), json!({"state": "pending"}));
    }

    fn queued(launch: Option<Uuid>, conversation: Option<&str>) -> QueuedSend {
        let now = Instant::now();
        QueuedSend {
            text: "[wire v1 d-9f2a] unread mail".into(),
            once: Some("d-9f2a".into()),
            launch,
            conversation: conversation.map(str::to_owned),
            queued_at: now,
            deadline: now + Duration::from_secs(600),
            waiting_on: "draft_present",
            reply: std::sync::mpsc::channel().0,
            cancelled: Arc::new(AtomicBool::new(false)),
            checking_clients: false,
            clients_seen: None,
        }
    }

    fn ready(launch: Uuid) -> PaneFacts {
        PaneFacts {
            cancelled: false,
            expired: false,
            writable: Ok((launch, ctl::Conversation::Known("conversation".into()))),
            refusal: None,
            send_in_flight: false,
        }
    }

    #[test]
    fn a_waiting_send_waits_through_busy_states_and_sends_once_ready() {
        let launch = Uuid::new_v4();
        let send = queued(Some(launch), Some("conversation"));
        assert_eq!(verdict(&send, ready(launch)), Verdict::Send);
        for (refusal, reason) in [
            (ctl::Error::new(ErrorCode::Working, "busy"), "working"),
            (
                ctl::Error::new(ErrorCode::Working, "grace").with_reason("just_submitted"),
                "just_submitted",
            ),
            (ctl::Error::new(ErrorCode::Blocked, "asks"), "blocked"),
            (
                ctl::Error::new(ErrorCode::DraftPresent, "draft"),
                "draft_present",
            ),
        ] {
            let facts = PaneFacts {
                refusal: Some(refusal),
                ..ready(launch)
            };
            assert_eq!(verdict(&send, facts), Verdict::Wait(reason));
        }
        let in_flight = PaneFacts {
            send_in_flight: true,
            ..ready(launch)
        };
        assert_eq!(verdict(&send, in_flight), Verdict::Wait("send_in_flight"));
        let starting = PaneFacts {
            writable: Err(ctl::Error::new(ErrorCode::Starting, "starting")),
            ..ready(launch)
        };
        assert_eq!(verdict(&send, starting), Verdict::Wait("starting"));
        // The agent is naming a conversation muxel hasn't confirmed: it may
        // be this one again, so the send waits to see.
        let unconfirmed = PaneFacts {
            writable: Ok((launch, ctl::Conversation::Unconfirmed)),
            ..ready(launch)
        };
        assert_eq!(
            verdict(&send, unconfirmed),
            Verdict::Wait("conversation_unconfirmed")
        );
    }

    #[test]
    fn a_waiting_send_ends_when_its_pane_cannot_take_it() {
        let launch = Uuid::new_v4();
        let send = queued(Some(launch), Some("conversation"));
        let refused = |facts| match verdict(&send, facts) {
            Verdict::Refuse(error) => error,
            other => panic!("{other:?}"),
        };
        let timed_out = refused(PaneFacts {
            expired: true,
            ..ready(launch)
        });
        assert_eq!(timed_out.code, Some(ErrorCode::TimedOut));
        assert_eq!(timed_out.reason.as_deref(), Some("draft_present"));
        let gone = refused(PaneFacts {
            cancelled: true,
            ..ready(launch)
        });
        assert_eq!(gone.pasted, None);
        let exited = refused(PaneFacts {
            writable: Err(ctl::Error::new(ErrorCode::Exited, "exited")),
            ..ready(launch)
        });
        assert_eq!(exited.code, Some(ErrorCode::Exited));
        let replaced = refused(ready(Uuid::new_v4()));
        assert_eq!(replaced.code, Some(ErrorCode::LaunchMismatch));
        let switched = refused(PaneFacts {
            writable: Ok((launch, ctl::Conversation::Known("another".into()))),
            ..ready(launch)
        });
        assert_eq!(switched.code, Some(ErrorCode::LaunchMismatch));
        assert_eq!(switched.reason.as_deref(), Some("conversation_changed"));
        // A pane without a known conversation when queued may gain one.
        let unbound = queued(Some(launch), None);
        assert_eq!(verdict(&unbound, ready(launch)), Verdict::Send);
    }
}
