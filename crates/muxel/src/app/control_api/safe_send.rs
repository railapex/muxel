//! `send --once` and `send --when-idle`: typing a prompt into a pane at most
//! once, and waiting in muxel for a moment it is safe to type it.
//!
//! Both are for automation that can't watch the pane, such as a mail doorbell.
//! With `--detach` nobody waits on the connection either: muxel answers
//! `queued` and keeps the send, and `once-status` reports how it went. A
//! detached send and its key are on disk as well (see [`send_journal`]), so
//! both outlive a restart. Attached sends and their keys live in this process.

use super::send_journal::{self, Journal, JournalKey, JournalState, SendRecord, Snapshot};
use super::*;
use std::collections::VecDeque;

/// `--once` keys each pane keeps, newest last.
const ONCE_KEYS_PER_PANE: usize = 64;
/// Attached `send --when-idle` calls one muxel holds at once. Each holds one
/// of the control server's connections, and interactive `ctl` calls need the
/// rest. Detached sends hold none, and are bounded by one per pane.
pub(super) const MAX_QUEUED_SENDS: usize = 8;

/// This muxel process, as `queued`, `once-status` and `status` name it: new
/// each time muxel starts, so a caller can tell that muxel restarted since it
/// queued a send.
pub(in crate::app) fn boot_id() -> Uuid {
    static BOOT_ID: std::sync::LazyLock<Uuid> = std::sync::LazyLock::new(Uuid::new_v4);
    *BOOT_ID
}

#[derive(Clone, Debug)]
enum OnceState {
    /// The send with this key is still running. `typing` once it may have
    /// typed: its paste has been written.
    Pending {
        typing: bool,
        detached: Option<Detached>,
    },
    /// The send typed and ended with this reply.
    Done(CtlResult<Value>),
    /// A detached send ended with this refusal without typing. The key is
    /// free again; this only answers `once-status` until it is reused.
    Dropped(ctl::Error),
}

/// What a detached send's key keeps while the send runs.
#[derive(Clone, Debug)]
struct Detached {
    /// The `queued` reply, which a repeat gets again.
    receipt: Value,
    /// Set by `once-cancel`: the send refuses before its paste.
    withdraw: Arc<AtomicBool>,
    /// What a restart needs to carry the send on.
    record: SendRecord,
    /// Its enqueue is on disk, so its caller has the `queued` reply. Until
    /// then it neither types nor answers a repeat.
    recorded: bool,
}

impl Detached {
    fn withdrawn(&self) -> bool {
        self.withdraw.load(Ordering::Acquire)
    }
}

/// One pane's `--once` key.
#[derive(Clone, Debug)]
struct OnceEntry {
    key: String,
    state: OnceState,
    /// It is a detached send's, so it is on disk too.
    durable: bool,
    /// It came back from disk when muxel started.
    restored: bool,
}

/// A detached send that was waiting when muxel last stopped, to wait on.
pub(super) struct Restored {
    pane: Uuid,
    key: String,
    send: SendRecord,
    withdraw: Arc<AtomicBool>,
}

/// The refusal a detached send gets when muxel restarted before it typed and
/// it isn't one a restart carries on.
fn restarted_error() -> ctl::Error {
    ctl::Error::new(
        ErrorCode::LaunchMismatch,
        "muxel restarted before the send typed, and only a send pinned with --session-id \
         (and no --launch-id) waits on through a restart: nothing was typed",
    )
    .with_reason("restarted")
}

/// The reply a detached send gets when it had reached its paste as muxel
/// stopped: nobody can know whether the paste went in, and it never types
/// again.
fn interrupted_receipt(pane: Uuid) -> Value {
    json!({
        "sent": true,
        "pasted": null,
        "outcome": "interrupted",
        "agent": ctl::short_id(pane),
        "uuid": pane,
    })
}

/// The refusal a withdrawn send ends with.
pub(super) fn withdrawn_error(detached: bool) -> ctl::Error {
    ctl::Error::new(
        ErrorCode::Cancelled,
        if detached {
            "once-cancel withdrew the send before it typed"
        } else {
            "the caller disconnected before the queued send was typed"
        },
    )
}

/// Each pane's recent `--once` keys. A key is used once the send that carries
/// it may have typed. A send refused before typing leaves its key unused.
#[derive(Default)]
pub(in crate::app) struct OnceKeys {
    by_pane: HashMap<Uuid, VecDeque<OnceEntry>>,
}

impl OnceKeys {
    /// Take `key` for a new send to `pane`, or the answer a repeat gets:
    /// the first receipt with `duplicate: true`, `duplicate` with the first
    /// failure, or `busy` while the first is still running. A `detached`
    /// repeat of a detached send still running gets its `queued` reply again,
    /// so a caller whose reply was lost can simply ask again.
    pub(super) fn reserve(
        &mut self,
        pane: Uuid,
        key: &str,
        detached: bool,
    ) -> Option<CtlResult<Value>> {
        let keys = self.by_pane.entry(pane).or_default();
        if let Some(at) = keys.iter().position(|entry| entry.key == key) {
            let repeat = match &keys[at].state {
                // A dropped send typed nothing: the key starts a new one.
                OnceState::Dropped(_) => None,
                OnceState::Pending {
                    detached: Some(first),
                    ..
                } if detached && first.recorded && !first.withdrawn() => {
                    let mut repeat = first.receipt.clone();
                    repeat["duplicate"] = json!(true);
                    repeat["boot_id"] = json!(boot_id());
                    Some(Ok(repeat))
                }
                OnceState::Pending { typing, .. } => {
                    let busy = ctl::Error::new(
                        ErrorCode::Busy,
                        "the first send with this --once key hasn't finished: ask again later, \
                         or read it with `once-status`",
                    )
                    .with_reason("once_pending");
                    Some(Err(if *typing { busy.after_paste() } else { busy }))
                }
                OnceState::Done(Ok(receipt)) => {
                    let mut repeat = receipt.clone();
                    repeat["duplicate"] = json!(true);
                    Some(Ok(repeat))
                }
                OnceState::Done(Err(first)) => Some(Err(ctl::Error::duplicate_of(first))),
            };
            if repeat.is_some() {
                return repeat;
            }
            keys.remove(at);
        }
        if keys.len() >= ONCE_KEYS_PER_PANE {
            let Some(oldest) = keys.iter().position(|entry| {
                matches!(entry.state, OnceState::Done(_) | OnceState::Dropped(_))
            }) else {
                return Some(Err(ctl::Error::new(
                    ErrorCode::Busy,
                    "64 sends with --once keys are still running for this pane",
                )
                .with_reason("once_pending")));
            };
            keys.remove(oldest);
        }
        keys.push_back(OnceEntry {
            key: key.to_owned(),
            state: OnceState::Pending {
                typing: false,
                detached: None,
            },
            durable: false,
            restored: false,
        });
        None
    }

    /// The send holding `key` was queued detached: `receipt` is its `queued`
    /// reply, `withdraw` what `once-cancel` sets, and `record` what a restart
    /// needs. From here its key is on disk; it counts as queued once that
    /// first write lands ([`OnceKeys::recorded`]).
    pub(super) fn detach(
        &mut self,
        pane: Uuid,
        key: &str,
        receipt: Value,
        withdraw: Arc<AtomicBool>,
        record: SendRecord,
    ) {
        if let Some(entry) = self.entry_mut(pane, key)
            && let OnceState::Pending { detached, .. } = &mut entry.state
        {
            *detached = Some(Detached {
                receipt,
                withdraw,
                record,
                recorded: false,
            });
            entry.durable = true;
        }
    }

    /// The detached send holding `key` is on disk: it may type, and a repeat
    /// gets its `queued` reply.
    pub(super) fn recorded(&mut self, pane: Uuid, key: &str) {
        if let Some(OnceEntry {
            state:
                OnceState::Pending {
                    detached: Some(detached),
                    ..
                },
            ..
        }) = self.entry_mut(pane, key)
        {
            detached.recorded = true;
        }
    }

    /// The detached send holding `key` couldn't be put on disk, and its caller
    /// is told so: the key is unused again.
    pub(super) fn abandon(&mut self, pane: Uuid, key: &str) {
        if let Some(keys) = self.by_pane.get_mut(&pane) {
            keys.retain(|entry| {
                entry.key != key || !matches!(entry.state, OnceState::Pending { .. })
            });
        }
    }

    /// Whether `key` on `pane` is a detached send's, kept on disk.
    pub(super) fn durable(&self, pane: Uuid, key: &str) -> bool {
        self.entry(pane, key).is_some_and(|entry| entry.durable)
    }

    /// `once-cancel` for a detached send that has left the queue: it refuses
    /// before its paste, as long as it hasn't written one. Whether it was
    /// withdrawn.
    pub(super) fn withdraw(&mut self, pane: Uuid, key: &str) -> bool {
        match self.entry_mut(pane, key).map(|entry| &mut entry.state) {
            Some(OnceState::Pending {
                typing: false,
                detached: Some(detached),
            }) => {
                detached.withdraw.store(true, Ordering::Release);
                true
            }
            _ => false,
        }
    }

    /// The send holding `key` is about to write its paste, or has.
    pub(super) fn typing(&mut self, pane: Uuid, key: &str) {
        if let Some(OnceEntry {
            state: OnceState::Pending { typing, .. },
            ..
        }) = self.entry_mut(pane, key)
        {
            *typing = true;
        }
    }

    /// The send holding `key` ended with `result`. One that typed nothing
    /// gives the key back; a detached one also leaves its refusal for
    /// `once-status`, since nobody received it. Whether the key is on disk,
    /// which then has to follow.
    pub(super) fn finish(&mut self, pane: Uuid, key: &str, result: &CtlResult<Value>) -> bool {
        let typed = match result {
            Ok(_) => true,
            Err(error) => error.pasted == Some(true),
        };
        let Some(keys) = self.by_pane.get_mut(&pane) else {
            return false;
        };
        let Some(at) = keys
            .iter()
            .position(|entry| entry.key == key && matches!(entry.state, OnceState::Pending { .. }))
        else {
            return false;
        };
        let entry = &mut keys[at];
        let detached = matches!(
            entry.state,
            OnceState::Pending {
                detached: Some(_),
                ..
            }
        );
        let durable = entry.durable;
        match result {
            _ if typed => entry.state = OnceState::Done(result.clone()),
            Err(error) if detached => entry.state = OnceState::Dropped(error.clone()),
            _ => {
                keys.remove(at);
            }
        }
        durable
    }

    /// `once-status`: what became of `key` on `pane`, without changing it.
    pub(super) fn status(&self, pane: Uuid, key: &str) -> Value {
        let entry = self.entry(pane, key);
        let dropped = |error: &ctl::Error| json!({ "state": "dropped", "result": ctl::Response::err(error.clone()) });
        let mut status = match entry.map(|entry| &entry.state) {
            None => json!({ "state": "unused" }),
            // Withdrawn, it can no longer type: it only has to finish.
            Some(OnceState::Pending {
                typing: false,
                detached: Some(detached),
            }) if detached.withdrawn() => dropped(&withdrawn_error(true)),
            Some(OnceState::Pending {
                typing: false,
                detached: Some(detached),
            }) if detached.recorded => json!({
                "state": "queued",
                "expires_at": detached.receipt["expires_at"],
            }),
            Some(OnceState::Pending { typing: false, .. }) => json!({ "state": "pending" }),
            Some(OnceState::Pending { typing: true, .. }) => json!({ "state": "typing" }),
            Some(OnceState::Done(result)) => json!({
                "state": "done",
                "result": send_journal::response(result),
            }),
            Some(OnceState::Dropped(error)) => dropped(error),
        };
        if entry.is_some_and(|entry| entry.restored) {
            status["restored"] = json!(true);
        }
        status
    }

    /// What goes on disk: every detached key, as a restart should find it.
    /// A send withdrawn on its way to the paste is already dropped.
    pub(super) fn journal(&self) -> Journal {
        let keys = self.by_pane.iter().flat_map(|(pane, keys)| {
            keys.iter()
                .filter(|entry| entry.durable)
                .filter_map(move |entry| {
                    let state = match &entry.state {
                        OnceState::Pending { typing: true, .. } => JournalState::Typing,
                        OnceState::Pending {
                            detached: Some(detached),
                            ..
                        } if detached.withdrawn() => JournalState::Dropped {
                            result: ctl::Response::err(withdrawn_error(true)),
                        },
                        OnceState::Pending {
                            detached: Some(detached),
                            ..
                        } => JournalState::Queued {
                            receipt: detached.receipt.clone(),
                            send: detached.record.clone(),
                        },
                        OnceState::Pending { detached: None, .. } => return None,
                        OnceState::Done(result) => JournalState::Done {
                            result: send_journal::response(result),
                        },
                        OnceState::Dropped(error) => JournalState::Dropped {
                            result: ctl::Response::err(error.clone()),
                        },
                    };
                    Some(JournalKey {
                        pane: *pane,
                        key: entry.key.clone(),
                        state,
                    })
                })
        });
        Journal {
            version: 0,
            keys: keys.collect(),
        }
    }

    /// The keys `journal` holds for panes still in the workspace, as muxel
    /// left them at `now_ms`, and the waiting sends to carry on. One that had
    /// reached its paste is done, since it may have typed; a waiting one a
    /// restart can't carry on, or whose time ran out, is dropped.
    pub(super) fn restore(
        journal: Journal,
        live: &HashSet<Uuid>,
        now_ms: i64,
    ) -> (Self, Vec<Restored>) {
        let mut restored = Self::default();
        let mut waiting = Vec::new();
        for JournalKey { pane, key, state } in journal.keys {
            let keys = restored.by_pane.entry(pane).or_default();
            if !live.contains(&pane)
                || keys.len() >= ONCE_KEYS_PER_PANE
                || keys.iter().any(|entry| entry.key == key)
            {
                continue;
            }
            let state = match state {
                JournalState::Queued { send, .. } if !send.carries_on() || send.text.is_none() => {
                    OnceState::Dropped(restarted_error())
                }
                JournalState::Queued { send, .. } if now_ms >= send.expires_at => {
                    OnceState::Dropped(
                        ctl::Error::new(
                            ErrorCode::TimedOut,
                            "the send's time ran out while muxel was closed: nothing was typed",
                        )
                        .with_reason("restarted"),
                    )
                }
                JournalState::Queued { receipt, send } => {
                    let withdraw = Arc::new(AtomicBool::new(false));
                    waiting.push(Restored {
                        pane,
                        key: key.clone(),
                        send: send.clone(),
                        withdraw: withdraw.clone(),
                    });
                    OnceState::Pending {
                        typing: false,
                        detached: Some(Detached {
                            receipt,
                            withdraw,
                            record: send,
                            recorded: true,
                        }),
                    }
                }
                JournalState::Typing => OnceState::Done(Ok(interrupted_receipt(pane))),
                JournalState::Done { result } => OnceState::Done(result.into_result()),
                JournalState::Dropped { result } => match result.into_result() {
                    Err(error) => OnceState::Dropped(error),
                    Ok(receipt) => OnceState::Done(Ok(receipt)),
                },
            };
            keys.push_back(OnceEntry {
                key,
                state,
                durable: true,
                restored: true,
            });
        }
        restored.by_pane.retain(|_, keys| !keys.is_empty());
        (restored, waiting)
    }

    pub(super) fn retain(&mut self, live: &HashSet<Uuid>) {
        self.by_pane.retain(|pane, _| live.contains(pane));
    }

    pub(in crate::app) fn clear(&mut self) {
        self.by_pane.clear();
    }

    fn entry(&self, pane: Uuid, key: &str) -> Option<&OnceEntry> {
        self.by_pane
            .get(&pane)?
            .iter()
            .find(|entry| entry.key == key)
    }

    fn entry_mut(&mut self, pane: Uuid, key: &str) -> Option<&mut OnceEntry> {
        self.by_pane
            .get_mut(&pane)?
            .iter_mut()
            .find(|entry| entry.key == key)
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
    /// `conversation` is the caller's `--session-id`, which must hold to the
    /// paste or not type at all.
    pinned: bool,
    /// `--detach`: its caller already has the `queued` reply, so `reply`
    /// reaches nobody and `cancelled` is set only by `once-cancel`.
    detached: bool,
    queued_at: Instant,
    /// `queued_at` on the wall clock, to compare with submission times.
    queued_ms: i64,
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
    /// A read of the pane's Claude end record for its conversation is running.
    checking_end: bool,
    /// It is on disk, as a detached send must be before it may type, or it
    /// needn't be.
    recorded: bool,
    /// It came back from disk when muxel started, so its pane may not be
    /// running yet.
    restored: bool,
}

impl QueuedSend {
    /// A detached send muxel restored from disk at `now_ms`, waiting on as it
    /// was: its key, its pin, its deadline. It binds to the first launch it
    /// sees, since the one it saw is gone.
    fn restored(restored: Restored, now_ms: i64) -> Self {
        let Restored {
            key,
            send,
            withdraw,
            ..
        } = restored;
        let now = Instant::now();
        let ms = |delta: i64| Duration::from_millis(u64::try_from(delta).unwrap_or(0));
        QueuedSend {
            text: send.text.unwrap_or_default(),
            once: Some(key),
            launch: None,
            conversation: send.session_id,
            pinned: true,
            detached: true,
            queued_at: now.checked_sub(ms(now_ms - send.queued_ms)).unwrap_or(now),
            queued_ms: send.queued_ms,
            deadline: now + ms(send.expires_at - now_ms),
            waiting_on: "starting",
            reply: std::sync::mpsc::channel().0,
            cancelled: withdraw,
            checking_clients: false,
            clients_seen: None,
            checking_end: false,
            recorded: true,
            restored: true,
        }
    }
}

/// How long after finding another tmux client a waiting send asks again.
const CLIENTS_RECHECK: Duration = Duration::from_secs(5);
/// How long after someone else's submission a waiting send gives that turn to
/// show, when it shows none (`/model`, a `/clear`). One holding a conversation
/// gets this only where muxel reads the provider's conversation ends (a local
/// Claude pane), since an end the turn didn't show is then still seen.
const SUBMISSION_SETTLE_MS: i64 = 3_000;

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
    /// When the latest submission into the pane went in (wall-clock ms).
    submitted_at: Option<i64>,
    /// When the pane last started a turn or asked a question (wall-clock ms).
    turn_started_at: Option<i64>,
    /// muxel reads the pane's Claude end records, so a conversation that
    /// ended shows in `writable` without a turn to show it.
    ends_recorded: bool,
    now_ms: i64,
}

/// Wait while the pane is busy in a way that passes; refuse once it can't
/// take this prompt at all; otherwise send.
fn verdict(queued: &QueuedSend, facts: PaneFacts) -> Verdict {
    if facts.cancelled {
        return Verdict::Refuse(withdrawn_error(queued.detached));
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
        // After a restart a pane can stay closed until someone opens it.
        Err(error) if queued.restored && error.code == Some(ErrorCode::NotRunning) => {
            return Verdict::Wait("not_running");
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
    // Someone submitted while it waited: a person's Enter on their draft, or
    // another send. That can start a turn the pane doesn't show yet, or end
    // the conversation (`/clear` starts no turn). So the send waits for that
    // turn to show, or gives it a moment. One holding a conversation gets the
    // moment only where muxel reads the provider's conversation ends: Claude
    // writes the end record before its `/clear` or `/resume` finishes, so
    // the check above sees the end however long muxel takes to adopt the
    // next conversation. Elsewhere it waits for the turn, or for muxel's
    // reading of the conversation to move and the check above to settle it.
    if let Some(submitted) = facts.submitted_at.filter(|at| *at >= queued.queued_ms) {
        let shown = facts.turn_started_at.is_some_and(|at| at >= submitted);
        let settled = (queued.conversation.is_none() || facts.ends_recorded)
            && facts.now_ms - submitted >= SUBMISSION_SETTLE_MS;
        if !shown && !settled {
            return Verdict::Wait("just_submitted");
        }
    }
    if facts.send_in_flight {
        return Verdict::Wait("send_in_flight");
    }
    match facts.refusal {
        None => Verdict::Send,
        Some(refusal) => Verdict::Wait(match (refusal.code, refusal.reason.as_deref()) {
            (Some(ErrorCode::Working), Some("just_submitted")) => "just_submitted",
            (Some(ErrorCode::Blocked), Some("prompt_hidden")) => "prompt_hidden",
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
    pub(super) detach: bool,
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
            detach,
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
        let attached = self.control_queue.values().filter(|q| !q.detached).count();
        if !detach && attached >= MAX_QUEUED_SENDS {
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
        // A pin muxel can never hold refuses before anything is waited on.
        if let Some(view) = self.terminal_view(iid)
            && let Err(error) =
                super::check_pin_deliverable(view.read(cx).session(), session_id.as_deref())
        {
            return refuse(self, error);
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
        let (queued_at, queued_ms) = (Instant::now(), now_ms());
        let wait = Duration::from_secs(timeout_secs.unwrap_or(if detach {
            ctl::DETACHED_TIMEOUT_SECS
        } else {
            ctl::WHEN_IDLE_TIMEOUT_SECS
        }));
        let pinned = session_id.is_some();
        // A detached send is answered before it can type, and outlives its
        // connection: only `once-cancel` withdraws it, and its outcome goes
        // to its key. It is answered once it is on disk, so a restart after
        // the `queued` reply still finds it, and it types only after that.
        let (reply, cancelled, recorded) = match once.as_deref().filter(|_| detach) {
            Some(key) => {
                let withdraw = Arc::new(AtomicBool::new(false));
                let expires_at =
                    queued_ms.saturating_add(i64::try_from(wait.as_millis()).unwrap_or(i64::MAX));
                let receipt = json!({
                    "queued": true,
                    "agent": ctl::short_id(iid),
                    "uuid": iid,
                    "once": key,
                    "expires_at": expires_at,
                    "boot_id": boot_id(),
                });
                let record = SendRecord::new(
                    &text,
                    session_id.as_deref(),
                    launch_id,
                    queued_ms,
                    expires_at,
                );
                self.control_once
                    .detach(iid, key, receipt.clone(), withdraw.clone(), record);
                let written = self.record_control_sends(cx);
                let key = key.to_owned();
                cx.spawn(async move |this: WeakEntity<Self>, cx| {
                    let written = written.await;
                    let _ = this.update(cx, |app, cx| {
                        app.detached_send_recorded(iid, &key, written, receipt, reply, cx);
                    });
                })
                .detach();
                (std::sync::mpsc::channel().0, withdraw, false)
            }
            None => (reply, cancelled, true),
        };
        let conversation = session_id.or_else(|| current.into_id());
        self.control_queue.insert(
            iid,
            QueuedSend {
                text,
                once,
                launch,
                conversation,
                pinned,
                detached: detach,
                queued_at,
                queued_ms,
                deadline: queued_at + wait,
                waiting_on: "working",
                reply,
                cancelled,
                checking_clients: false,
                clients_seen: None,
                checking_end: false,
                recorded,
                restored: false,
            },
        );
        self.pump_control_queue(cx);
    }

    /// The write that puts a detached send on disk has landed, or failed.
    /// Landed, its caller gets the `queued` reply and the send may type.
    /// Failed, the send is withdrawn and its caller told why, its key unused.
    fn detached_send_recorded(
        &mut self,
        pane: Uuid,
        key: &str,
        written: Result<(), String>,
        receipt: Value,
        reply: std::sync::mpsc::Sender<ctl::Response>,
        cx: &mut Context<Self>,
    ) {
        let waiting = self
            .control_queue
            .get_mut(&pane)
            .filter(|queued| queued.once.as_deref() == Some(key));
        match written {
            Ok(()) => {
                self.control_once.recorded(pane, key);
                if let Some(queued) = waiting {
                    queued.recorded = true;
                }
                let _ = reply.send(ctl::Response::ok(receipt));
                self.pump_control_queue(cx);
            }
            Err(error) => {
                if waiting.is_some() {
                    self.control_queue.remove(&pane);
                }
                self.control_once.abandon(pane, key);
                let _ = reply.send(ctl::Response::err(format!(
                    "muxel couldn't record the detached send on disk, so it isn't queued and \
                     nothing was typed: {error}"
                )));
            }
        }
    }

    /// The snapshot of the open workspace's detached sends to put on disk.
    pub(super) fn control_sends_snapshot(&self) -> Option<Snapshot> {
        let path = muxel_store::send_journal_path(self.current_workspace?)?;
        Some(
            self.control_journal
                .snapshot(path, self.control_once.journal()),
        )
    }

    /// Put the open workspace's detached sends on disk, off the UI thread.
    /// The task ends once they are there.
    pub(super) fn record_control_sends(&self, cx: &App) -> Task<Result<(), String>> {
        let Some(snapshot) = self.control_sends_snapshot() else {
            return Task::ready(Ok(()));
        };
        let writer = self.control_journal.clone();
        cx.background_executor()
            .spawn(async move { writer.write(snapshot) })
    }

    /// `key`'s send on `pane` ended with `result`. A detached one's key is
    /// on disk, which follows; nothing waits for that write, since a restart
    /// before it lands finds the send at its paste and calls it done.
    pub(in crate::app) fn finish_once(
        &mut self,
        pane: Uuid,
        key: &str,
        result: &CtlResult<Value>,
        cx: &App,
    ) {
        if !self.control_once.finish(pane, key, result) {
            return;
        }
        let written = self.record_control_sends(cx);
        cx.background_executor()
            .spawn(async move {
                if let Err(error) = written.await {
                    log::warn!("couldn't record how a detached send ended: {error}");
                }
            })
            .detach();
    }

    /// Bring back the detached sends workspace `id` had when muxel last left
    /// it: each key as it was, and the waiting sends to wait on. Runs as the
    /// workspace opens, before any request can see it.
    pub(in crate::app) fn restore_control_sends(&mut self, id: Uuid, cx: &mut Context<Self>) {
        let Some(path) = muxel_store::send_journal_path(id) else {
            return;
        };
        let journal = match send_journal::read(&path) {
            Ok(Some(journal)) => journal,
            Ok(None) => return,
            Err(error) => {
                log::warn!("detached sends weren't restored: {error}");
                muxel_store::append_event_log(&format!("detached sends weren't restored: {error}"));
                return;
            }
        };
        let live: HashSet<Uuid> = self.workspace.instances.iter().map(|i| i.id).collect();
        let now = now_ms();
        let (keys, waiting) = OnceKeys::restore(journal, &live, now);
        self.control_once = keys;
        for restored in waiting {
            let pane = restored.pane;
            self.control_queue
                .insert(pane, QueuedSend::restored(restored, now));
        }
        // What the restart settled (sends at their paste now done, others
        // dropped) goes on disk too.
        let written = self.record_control_sends(cx);
        cx.background_executor()
            .spawn(async move {
                if let Err(error) = written.await {
                    log::warn!("couldn't record the restored detached sends: {error}");
                }
            })
            .detach();
    }

    /// `once-cancel`: withdraw the detached send holding `key` on `pane`
    /// before it types. Whether it was withdrawn: a send already typing or
    /// finished, an attached one, or no send at all, isn't.
    pub(super) fn withdraw_detached_send(
        &mut self,
        pane: Uuid,
        key: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let waiting = self
            .control_queue
            .get(&pane)
            .is_some_and(|queued| queued.detached && queued.once.as_deref() == Some(key));
        if waiting && let Some(queued) = self.control_queue.remove(&pane) {
            queued.cancelled.store(true, Ordering::Release);
            self.finish_queued_send(pane, queued, Err(withdrawn_error(true)), cx);
            return true;
        }
        // Out of the queue but not yet typing: its claim is being checked,
        // and its paste will refuse.
        self.control_once.withdraw(pane, key)
    }

    /// `once-status` for `key` on `pane`, with what a queued send waits on.
    pub(super) fn once_status(&self, pane: Uuid, key: &str) -> Value {
        let mut status = self.control_once.status(pane, key);
        if status["state"] == "queued"
            && let Some(queued) = self
                .control_queue
                .get(&pane)
                .filter(|queued| queued.once.as_deref() == Some(key))
        {
            status["waiting_on"] = json!(queued.waiting_on);
        }
        status["agent"] = json!(ctl::short_id(pane));
        status["uuid"] = json!(pane);
        status["key"] = json!(key);
        status["boot_id"] = json!(boot_id());
        status
    }

    fn queued_send_facts(&self, iid: Uuid, queued: &QueuedSend, cx: &App) -> PaneFacts {
        let writable = self
            .control_writable(iid, cx)
            .map(|session| (session.id, self.control_conversation(iid, cx)));
        let refusal = writable
            .as_ref()
            .ok()
            .and_then(|_| check_send_state(self.control_send_admission(iid, false, cx)).err());
        let submitted_at = self
            .terminal_view(iid)
            .and_then(|view| view.read(cx).session().last_submission_at())
            .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX));
        let turn_started_at = self.workspace.instance(iid).and_then(|inst| {
            [inst.activity.work_started_at, inst.activity.blocked_at]
                .into_iter()
                .flatten()
                .max()
        });
        PaneFacts {
            cancelled: queued.cancelled.load(Ordering::Acquire),
            expired: Instant::now() >= queued.deadline,
            writable,
            refusal,
            send_in_flight: SendInFlight::held(iid),
            submitted_at,
            turn_started_at,
            ends_recorded: self.claude_end_records_apply(iid, cx),
            now_ms: now_ms(),
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
            // A detached send not yet on disk doesn't type, nor does one whose
            // check is still running.
            let not_yet = !queued.recorded
                || queued.checking_clients
                || queued.checking_end
                || queued
                    .clients_seen
                    .is_some_and(|seen| seen.elapsed() < CLIENTS_RECHECK);
            let Some(queued) = self.control_queue.get_mut(&iid) else {
                continue;
            };
            if queued.launch.is_none() {
                queued.launch = first_launch;
            }
            let conversation = queued.conversation.clone();
            match decided {
                Verdict::Refuse(error) => {
                    if let Some(queued) = self.control_queue.remove(&iid) {
                        self.finish_queued_send(iid, queued, Err(error), cx);
                    }
                }
                Verdict::Wait(reason) => queued.waiting_on = reason,
                Verdict::Send if not_yet => {}
                Verdict::Send => {
                    let site = self
                        .workspace
                        .instance(iid)
                        .and_then(|inst| self.control_claim_site(inst));
                    let Some(site) = site else {
                        // A local Claude pane: read its end record for this
                        // conversation first, so an end muxel hasn't taken in
                        // yet makes the send wait rather than type, or fail at
                        // the paste.
                        let check = self.claude_end_check(iid, conversation.as_deref(), cx);
                        let Some(check) = check else {
                            self.send_queued(iid, cx);
                            continue;
                        };
                        if let Some(queued) = self.control_queue.get_mut(&iid) {
                            queued.checking_end = true;
                        }
                        cx.spawn(async move |this: WeakEntity<Self>, cx| {
                            let read = cx
                                .background_executor()
                                .spawn(async move { check.read() })
                                .await;
                            let _ = this.update(cx, |app, cx| {
                                let Some(queued) = app.control_queue.get_mut(&iid) else {
                                    return;
                                };
                                queued.checking_end = false;
                                if read.end.is_err() {
                                    // Unread is not "not ended": ask again on
                                    // the next tick.
                                    queued.waiting_on = "conversation_unconfirmed";
                                    return;
                                }
                                app.note_claude_end(read, cx);
                                app.send_queued(iid, cx);
                            });
                        })
                        .detach();
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
            Verdict::Refuse(error) => return self.finish_queued_send(iid, queued, Err(error), cx),
        }
        // The conversation it queued in rides along to the paste, which
        // checks it once more. Through tmux the paste can't be held to one,
        // so a conversation the caller didn't pin was held only while
        // waiting; the caller's own pin goes on, to be refused.
        let through_tmux = self
            .terminal_view(iid)
            .is_some_and(|view| view.read(cx).session().delivers_through_tmux());
        let pinned = queued
            .conversation
            .clone()
            .filter(|_| queued.pinned || !through_tmux);
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
            Err(error) => self.finish_queued_send(iid, queued, Err(error), cx),
        }
    }

    fn finish_queued_send(
        &mut self,
        iid: Uuid,
        queued: QueuedSend,
        result: CtlResult<Value>,
        cx: &App,
    ) {
        if let Some(key) = queued.once.as_deref() {
            self.finish_once(iid, key, &result, cx);
        }
        let _ = queued.reply.send(match result {
            Ok(value) => ctl::Response::ok(value),
            Err(error) => ctl::Response::err(error),
        });
    }

    /// An attached waiting send to `pane`, which needn't exist, holding a
    /// place under the cap until the next pump refuses it.
    #[cfg(all(test, windows))]
    pub(super) fn queue_attached_send_for_test(&mut self, pane: Uuid) {
        let now = Instant::now();
        self.control_queue.insert(
            pane,
            QueuedSend {
                text: "waiting".into(),
                once: None,
                launch: None,
                conversation: None,
                pinned: false,
                detached: false,
                queued_at: now,
                queued_ms: now_ms(),
                deadline: now + Duration::from_secs(600),
                waiting_on: "working",
                reply: std::sync::mpsc::channel().0,
                cancelled: Arc::default(),
                checking_clients: false,
                clients_seen: None,
                checking_end: false,
                recorded: true,
                restored: false,
            },
        );
    }

    /// A workspace switch ends every waiting send: its pane is gone. The
    /// detached ones are recorded so, in the workspace they leave.
    pub(in crate::app) fn refuse_control_queue(&mut self, cx: &App) {
        for (iid, queued) in std::mem::take(&mut self.control_queue) {
            self.finish_queued_send(
                iid,
                queued,
                Err(ctl::Error::new(
                    ErrorCode::Exited,
                    "the workspace was switched while the send waited: nothing was typed",
                )),
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{OnceKeys, PaneFacts, QueuedSend, SendRecord, Verdict, verdict};
    use crate::app::control_api::send_journal::Journal;
    use muxel_core::control::{self as ctl, ErrorCode};
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use std::sync::{Arc, atomic::AtomicBool};
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    #[test]
    fn a_repeat_after_a_submitted_send_returns_its_receipt_and_types_nothing() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        assert!(keys.reserve(pane, "d-9f2a", false).is_none());
        keys.typing(pane, "d-9f2a");
        let receipt = json!({"sent": true, "pasted": true, "outcome": "submitted"});
        keys.finish(pane, "d-9f2a", &Ok(receipt));
        let repeat = keys.reserve(pane, "d-9f2a", false).unwrap().unwrap();
        assert_eq!(repeat["duplicate"], true);
        assert_eq!(repeat["outcome"], "submitted");
        // Another pane, or another key, is a new send.
        assert!(keys.reserve(Uuid::new_v4(), "d-9f2a", false).is_none());
        assert!(keys.reserve(pane, "d-0000", false).is_none());
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
            assert!(keys.reserve(pane, "d-1", false).is_none());
            keys.finish(pane, "d-1", &Err(refusal));
            assert_eq!(keys.status(pane, "d-1"), json!({"state": "unused"}));
        }
    }

    #[test]
    fn a_repeat_after_enter_was_withheld_is_a_duplicate_that_says_it_typed() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        keys.reserve(pane, "d-2", false);
        keys.typing(pane, "d-2");
        let withheld = ctl::Error::new(ErrorCode::InputChanged, "Enter withheld").after_paste();
        keys.finish(pane, "d-2", &Err(withheld));
        let repeat = keys.reserve(pane, "d-2", false).unwrap().unwrap_err();
        assert_eq!(repeat.code, Some(ErrorCode::Duplicate));
        assert_eq!(repeat.pasted, Some(true));
        let body = serde_json::to_value(ctl::Response::err(repeat)).unwrap();
        assert_eq!(body["original"]["code"], "input_changed");
        assert_eq!(keys.status(pane, "d-2")["result"]["code"], "input_changed");
    }

    #[test]
    fn a_repeat_while_the_first_runs_is_busy_and_says_once_it_typed() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        keys.reserve(pane, "d-3", false);
        let waiting = keys.reserve(pane, "d-3", false).unwrap().unwrap_err();
        assert_eq!(waiting.code, Some(ErrorCode::Busy));
        assert_eq!(waiting.reason.as_deref(), Some("once_pending"));
        assert_eq!(waiting.pasted, None);
        keys.typing(pane, "d-3");
        assert_eq!(
            keys.reserve(pane, "d-3", false)
                .unwrap()
                .unwrap_err()
                .pasted,
            Some(true)
        );
        assert_eq!(keys.status(pane, "d-3"), json!({"state": "typing"}));
    }

    /// A detached send's caller has gone, so its key is how it learns what
    /// happened: queued while it waits, and the refusal once it is dropped.
    /// A retry of a lost `queued` reply gets that reply again; a dropped send
    /// typed nothing, so its key starts a new one.
    #[test]
    fn a_detached_send_is_queued_until_its_key_records_it_dropped() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        assert!(keys.reserve(pane, "d-4", true).is_none());
        let receipt = json!({"queued": true, "once": "d-4", "expires_at": 86_400_000});
        detach(
            &mut keys,
            pane,
            "d-4",
            receipt,
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(
            keys.status(pane, "d-4"),
            json!({"state": "queued", "expires_at": 86_400_000})
        );
        let retry = keys.reserve(pane, "d-4", true).unwrap().unwrap();
        assert_eq!(retry["queued"], true);
        assert_eq!(retry["duplicate"], true);
        // A caller waiting on the connection can't be told "queued".
        let attached = keys.reserve(pane, "d-4", false).unwrap().unwrap_err();
        assert_eq!(attached.reason.as_deref(), Some("once_pending"));
        let changed = ctl::Error::new(ErrorCode::LaunchMismatch, "another conversation")
            .with_reason("conversation_changed");
        keys.finish(pane, "d-4", &Err(changed));
        let status = keys.status(pane, "d-4");
        assert_eq!(status["state"], "dropped");
        assert_eq!(status["result"]["code"], "launch_mismatch");
        assert_eq!(status["result"]["reason"], "conversation_changed");
        assert!(keys.reserve(pane, "d-4", true).is_none());
        assert_eq!(keys.status(pane, "d-4"), json!({"state": "pending"}));
    }

    /// Once a detached send has typed, it is done like any other.
    #[test]
    fn a_detached_send_that_typed_is_done() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        keys.reserve(pane, "d-5", true);
        let queued = json!({"queued": true, "once": "d-5"});
        detach(
            &mut keys,
            pane,
            "d-5",
            queued,
            Arc::new(AtomicBool::new(false)),
        );
        keys.typing(pane, "d-5");
        assert_eq!(keys.status(pane, "d-5"), json!({"state": "typing"}));
        assert_eq!(
            keys.reserve(pane, "d-5", true).unwrap().unwrap()["queued"],
            true
        );
        let typed = json!({"sent": true, "outcome": "submitted"});
        keys.finish(pane, "d-5", &Ok(typed));
        assert_eq!(keys.status(pane, "d-5")["state"], "done");
        let repeat = keys.reserve(pane, "d-5", true).unwrap().unwrap();
        assert_eq!(repeat["outcome"], "submitted");
        assert_eq!(repeat["duplicate"], true);
    }

    /// `once-cancel` withdraws a detached send only before it has typed, and
    /// never an attached one, whose caller withdraws it by hanging up.
    #[test]
    fn only_a_detached_send_that_hasnt_typed_can_be_withdrawn() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        assert!(!keys.withdraw(pane, "d-6"), "an unused key");
        keys.reserve(pane, "d-6", false);
        assert!(!keys.withdraw(pane, "d-6"), "an attached send");
        let withdraw = Arc::new(AtomicBool::new(false));
        keys.reserve(pane, "d-7", true);
        detach(
            &mut keys,
            pane,
            "d-7",
            json!({"queued": true}),
            withdraw.clone(),
        );
        assert!(keys.withdraw(pane, "d-7"));
        assert!(withdraw.load(std::sync::atomic::Ordering::Acquire));
        // It can no longer type, so it reads as dropped while it finishes.
        let status = keys.status(pane, "d-7");
        assert_eq!(status["state"], "dropped");
        assert_eq!(status["result"]["code"], "cancelled");
        keys.reserve(pane, "d-8", true);
        detach(
            &mut keys,
            pane,
            "d-8",
            json!({"queued": true}),
            Arc::default(),
        );
        keys.typing(pane, "d-8");
        assert!(!keys.withdraw(pane, "d-8"), "a send that typed");
    }

    /// Queue `key` detached on `pane`, pinned to a conversation, as an
    /// enqueue that has reached the disk.
    fn detach(
        keys: &mut OnceKeys,
        pane: Uuid,
        key: &str,
        receipt: Value,
        withdraw: Arc<AtomicBool>,
    ) {
        let record = SendRecord::new("ring", Some("s-1"), None, QUEUED_MS, QUEUED_MS + 600_000);
        keys.detach(pane, key, receipt, withdraw, record);
        keys.recorded(pane, key);
    }

    /// `journal` written and read back, as a restart reads it.
    fn reread(journal: Journal) -> Journal {
        serde_json::from_str(&serde_json::to_string(&journal).unwrap()).unwrap()
    }

    /// A pinned detached send read back from each write as a restart would.
    /// From its enqueue it waits on, with its key; from the write before its
    /// paste it is done and never types again, since nobody knows whether
    /// the paste went in; from its finish it answers as it did.
    #[test]
    fn a_restart_finds_a_detached_send_as_its_last_write_left_it() {
        let pane = Uuid::new_v4();
        let live = HashSet::from([pane]);
        let (now, expires_at) = (QUEUED_MS + 1_000, QUEUED_MS + 600_000);
        let mut keys = OnceKeys::default();
        keys.reserve(pane, "d-10", true);
        let receipt = json!({"queued": true, "once": "d-10", "expires_at": expires_at});
        let record = SendRecord::new("ring", Some("s-1"), None, QUEUED_MS, expires_at);
        keys.detach(pane, "d-10", receipt, Arc::default(), record);
        // Until its enqueue is on disk its caller has no reply, and neither
        // does a repeat.
        assert_eq!(keys.status(pane, "d-10"), json!({"state": "pending"}));
        let early = keys.reserve(pane, "d-10", true).unwrap().unwrap_err();
        assert_eq!(early.reason.as_deref(), Some("once_pending"));
        let enqueued = keys.journal();
        keys.recorded(pane, "d-10");
        let (restored, waiting) = OnceKeys::restore(reread(enqueued), &live, now);
        assert_eq!(
            restored.status(pane, "d-10"),
            json!({"state": "queued", "expires_at": expires_at, "restored": true})
        );
        let [waiting] = <[_; 1]>::try_from(waiting).ok().unwrap();
        let send = QueuedSend::restored(waiting, now);
        assert_eq!(send.text, "ring");
        assert_eq!(send.conversation.as_deref(), Some("s-1"));
        assert_eq!(send.once.as_deref(), Some("d-10"));
        assert_eq!(send.launch, None, "it binds to the first launch it sees");
        assert!(send.pinned && send.detached && send.restored);
        // Before the paste.
        keys.typing(pane, "d-10");
        let (mut restored, waiting) = OnceKeys::restore(reread(keys.journal()), &live, now);
        assert!(waiting.is_empty());
        let status = restored.status(pane, "d-10");
        assert_eq!(status["state"], "done");
        let interrupted = &status["result"]["result"];
        assert_eq!(interrupted["outcome"], "interrupted");
        assert_eq!(interrupted.get("pasted"), Some(&Value::Null));
        let repeat = restored.reserve(pane, "d-10", true).unwrap().unwrap();
        assert_eq!(repeat["outcome"], "interrupted");
        assert_eq!(repeat["duplicate"], true);
        // At its finish.
        let typed = json!({"sent": true, "pasted": true, "outcome": "submitted"});
        keys.finish(pane, "d-10", &Ok(typed));
        let (restored, _) = OnceKeys::restore(reread(keys.journal()), &live, now);
        let status = restored.status(pane, "d-10");
        assert_eq!(status["result"]["result"]["outcome"], "submitted");
        assert_eq!(status["restored"], true);
    }

    /// A waiting send a restart can't carry on is dropped with the reason,
    /// and a pane gone from the workspace takes its keys with it.
    #[test]
    fn a_restart_drops_a_waiting_send_it_cant_carry_on() {
        let (pane, gone) = (Uuid::new_v4(), Uuid::new_v4());
        let expires_at = QUEUED_MS + 600_000;
        let mut keys = OnceKeys::default();
        for (at, key, pin, launch) in [
            (pane, "unpinned", None, None),
            (pane, "launch-id", Some("s-1"), Some(Uuid::new_v4())),
            (pane, "late", Some("s-1"), None),
            (gone, "gone", Some("s-1"), None),
        ] {
            keys.reserve(at, key, true);
            let record = SendRecord::new("ring", pin, launch, QUEUED_MS, expires_at);
            keys.detach(at, key, json!({"queued": true}), Arc::default(), record);
            keys.recorded(at, key);
        }
        let live = HashSet::from([pane]);
        let (restored, waiting) = OnceKeys::restore(reread(keys.journal()), &live, expires_at);
        assert!(waiting.is_empty());
        for (key, code) in [
            ("unpinned", "launch_mismatch"),
            ("launch-id", "launch_mismatch"),
            ("late", "timed_out"),
        ] {
            let status = restored.status(pane, key);
            assert_eq!(status["state"], "dropped", "{key}: {status}");
            assert_eq!(status["result"]["code"], code, "{key}");
            assert_eq!(status["result"]["reason"], "restarted", "{key}");
        }
        assert_eq!(restored.status(gone, "gone"), json!({"state": "unused"}));
    }

    /// A send `once-cancel` withdrew on its way to the paste is on disk as
    /// dropped, so a restart can't bring it back; attached keys aren't on
    /// disk at all.
    #[test]
    fn a_restart_keeps_a_withdrawal_and_forgets_attached_keys() {
        let pane = Uuid::new_v4();
        let mut keys = OnceKeys::default();
        keys.reserve(pane, "attached", false);
        keys.typing(pane, "attached");
        keys.reserve(pane, "d-11", true);
        detach(
            &mut keys,
            pane,
            "d-11",
            json!({"queued": true}),
            Arc::default(),
        );
        assert!(keys.withdraw(pane, "d-11"));
        let live = HashSet::from([pane]);
        let (restored, waiting) = OnceKeys::restore(reread(keys.journal()), &live, QUEUED_MS);
        assert!(waiting.is_empty());
        assert_eq!(restored.status(pane, "d-11")["result"]["code"], "cancelled");
        assert_eq!(
            restored.status(pane, "attached"),
            json!({"state": "unused"})
        );
    }

    /// After a restart a pane can stay closed until someone opens it: a send
    /// restored to it waits, where a new send is refused.
    #[test]
    fn a_restored_send_waits_for_its_pane_to_be_opened() {
        let not_running = PaneFacts {
            writable: Err(ctl::Error::new(ErrorCode::NotRunning, "closed")),
            ..ready(Uuid::new_v4())
        };
        let fresh = queued(None, Some("conversation"));
        assert!(matches!(
            verdict(&fresh, not_running),
            Verdict::Refuse(error) if error.code == Some(ErrorCode::NotRunning)
        ));
        let restored = QueuedSend {
            restored: true,
            ..queued(None, Some("conversation"))
        };
        let not_running = PaneFacts {
            writable: Err(ctl::Error::new(ErrorCode::NotRunning, "closed")),
            ..ready(Uuid::new_v4())
        };
        assert_eq!(
            verdict(&restored, not_running),
            Verdict::Wait("not_running")
        );
    }

    #[test]
    fn each_pane_keeps_its_last_64_finished_keys_and_every_running_one() {
        let (pane, mut keys) = (Uuid::new_v4(), OnceKeys::default());
        for i in 0..64 {
            keys.reserve(pane, &format!("k{i}"), false);
            keys.finish(pane, &format!("k{i}"), &Ok(json!({"sent": true})));
        }
        keys.reserve(pane, "k64", false);
        assert_eq!(keys.status(pane, "k0"), json!({"state": "unused"}));
        assert_eq!(keys.status(pane, "k1")["state"], "done");
        let mut running = OnceKeys::default();
        for i in 0..64 {
            assert!(running.reserve(pane, &format!("r{i}"), false).is_none());
        }
        let full = running.reserve(pane, "r64", false).unwrap().unwrap_err();
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
            pinned: false,
            detached: false,
            queued_at: now,
            queued_ms: QUEUED_MS,
            deadline: now + Duration::from_secs(600),
            waiting_on: "draft_present",
            reply: std::sync::mpsc::channel().0,
            cancelled: Arc::new(AtomicBool::new(false)),
            checking_clients: false,
            clients_seen: None,
            checking_end: false,
            recorded: true,
            restored: false,
        }
    }

    /// When [`queued`] sends were queued, on the wall clock.
    const QUEUED_MS: i64 = 1_000_000;

    fn ready(launch: Uuid) -> PaneFacts {
        PaneFacts {
            cancelled: false,
            expired: false,
            writable: Ok((launch, ctl::Conversation::Known("conversation".into()))),
            refusal: None,
            send_in_flight: false,
            submitted_at: Some(QUEUED_MS - 60_000),
            turn_started_at: Some(QUEUED_MS - 59_000),
            now_ms: QUEUED_MS + 1_000,
            ends_recorded: false,
        }
    }

    /// A person submits the draft a send waited behind. The pane reads idle
    /// at once, before the turn shows, and a `/clear` shows none at all while
    /// muxel hasn't heard of the new conversation yet. A send holding a
    /// conversation waits for the turn to show, unless muxel reads the
    /// provider's conversation ends; one holding none waits only a moment.
    #[test]
    fn a_waiting_send_waits_for_the_turn_someone_else_just_submitted() {
        let launch = Uuid::new_v4();
        let submitted = QUEUED_MS + 5_000;
        let just_submitted = |now_ms| PaneFacts {
            submitted_at: Some(submitted),
            now_ms,
            ..ready(launch)
        };
        let pinned = queued(Some(launch), Some("conversation"));
        for now_ms in [submitted, submitted + 60_000] {
            assert_eq!(
                verdict(&pinned, just_submitted(now_ms)),
                Verdict::Wait("just_submitted"),
                "{now_ms}"
            );
        }
        // Where muxel reads Claude's conversation ends, a `/model` (no turn,
        // no end) lets a pinned send go after the same moment: a `/clear`
        // would have left an end record, which the send checks before it
        // types.
        let reads_ends = |now_ms| PaneFacts {
            ends_recorded: true,
            ..just_submitted(now_ms)
        };
        assert_eq!(
            verdict(&pinned, reads_ends(submitted + 1_000)),
            Verdict::Wait("just_submitted")
        );
        assert_eq!(
            verdict(&pinned, reads_ends(submitted + 3_000)),
            Verdict::Send
        );
        // Once that turn has shown, the send goes at the next idle moment.
        let shown = PaneFacts {
            turn_started_at: Some(submitted + 800),
            ..just_submitted(submitted + 30_000)
        };
        assert_eq!(verdict(&pinned, shown), Verdict::Send);
        // Muxel adopts the conversation a `/clear` started: dropped.
        let cleared = PaneFacts {
            writable: Ok((launch, ctl::Conversation::Known("another".into()))),
            ..just_submitted(submitted + 2_000)
        };
        assert!(matches!(verdict(&pinned, cleared), Verdict::Refuse(_)));
        let unbound = queued(Some(launch), None);
        assert_eq!(
            verdict(&unbound, just_submitted(submitted + 1_000)),
            Verdict::Wait("just_submitted")
        );
        assert_eq!(
            verdict(&unbound, just_submitted(submitted + 3_000)),
            Verdict::Send
        );
        // A submission from before it was queued is no news.
        let earlier = PaneFacts {
            submitted_at: Some(QUEUED_MS - 1),
            turn_started_at: None,
            ..ready(launch)
        };
        assert_eq!(verdict(&pinned, earlier), Verdict::Send);
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
                ctl::Error::new(ErrorCode::Blocked, "picker").with_reason("prompt_hidden"),
                "prompt_hidden",
            ),
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
        assert_eq!(gone.code, Some(ErrorCode::Cancelled));
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
