//! Outside control: the protocol behind `muxel ctl`, which lets another program on
//! this computer — an orchestrating agent such as Grok Bot — list muxel's projects,
//! panes and agents, see what each agent is doing, read what it was last asked and
//! what it answered, and type into it.
//!
//! This is the pure half. The running app (`muxel` crate, `control.rs`) listens on
//! a loopback port named in [`ENDPOINT_FILE`] and answers each [`Request`] from its
//! live state; the `muxel ctl` command line is the client. Parsing the command line,
//! resolving "which agent", naming keys, finding the question an agent is blocked
//! on and deciding when a turn is over all live here, deterministic and tested.

use crate::pane::PaneNode;
use crate::{AgentActivity, SplitDirection};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use uuid::Uuid;

/// Work an agent's screen says is still running beside its turn. Read from the
/// screen every time, so it is never persisted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackgroundWork {
    pub shells: u32,
    pub monitors: u32,
    /// Running subagents, which the agent's screen may name without a count.
    pub subagents: bool,
}

impl BackgroundWork {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }
}

mod new;
pub use new::{MAX_LABEL_CHARS, MAX_PROMPT_CHARS, NewOptions, NewPlacement, place_new_pane};
mod startup;
pub use startup::{StartupFailure, StartupTurn, StartupTurnOwner, StartupTurnState};
mod delivery;
pub use delivery::{PromptDelivery, PromptOutcome, StartupBlocker, StartupBlockerKind};
mod card;
pub use card::{CardPrompt, card_lines, card_prompt, card_value, is_hidden_char};
mod approval;
pub use approval::{
    APPROVAL_ARM_DELAY_MS, APPROVAL_TIMEOUT_MS, ApprovalQueue, CreationDecision, CreationPolicy,
    LaunchClass, PendingApproval, creation_decision, creation_refusal,
};

/// Endpoint and plain-request version. Guards and plain creation require v2.
pub const PROTOCOL_VERSION: u32 = 1;
/// An older v1 host ignores unknown fields but rejects this request version
/// before forwarding a guarded send to the app.
pub const GUARDED_SEND_VERSION: u32 = 2;
/// Model/effort fields must never be silently ignored by older creation hosts.
pub const PARAMETERIZED_NEW_VERSION: u32 = 3;
/// `send --once` and `send --when-idle` must be refused by hosts that would
/// ignore them and type at once, or type a repeat again.
pub const SAFE_SEND_VERSION: u32 = 4;
/// `send --session-id` must be refused by hosts that would ignore it and type
/// into whatever conversation the pane has moved on to.
pub const PINNED_SEND_VERSION: u32 = 5;
/// `send --detach` and `once-cancel` must be refused by hosts that would
/// ignore `--detach` and hold the caller's connection for the whole wait.
pub const DETACHED_SEND_VERSION: u32 = 6;
/// The newest version this host accepts, which `status` and `panes` report as
/// `protocol` so a client can learn it without sending anything.
pub const NEWEST_VERSION: u32 = DETACHED_SEND_VERSION;
/// How long `send --when-idle` waits for a safe moment by default, and at most.
pub const WHEN_IDLE_TIMEOUT_SECS: u64 = 600;
pub const WHEN_IDLE_TIMEOUT_MAX_SECS: u64 = 3600;
/// How long a `send --when-idle --detach` waits by default, and at most: a
/// day, which outlasts an overnight turn or a draft left overnight. Past that
/// a doorbell's news is stale, and a pane misread as working mustn't hold its
/// queue place and `--once` key for good.
pub const DETACHED_TIMEOUT_SECS: u64 = 86_400;

/// A `--once` key: 1–128 bytes, not blank, no control characters.
pub fn valid_once_key(key: &str) -> bool {
    !key.trim().is_empty() && key.len() <= 128 && !key.chars().any(char::is_control)
}

/// The checks a `send` carrying `--once`, `--when-idle`, `--detach`,
/// `--timeout` or `--session-id` must pass before it reaches the app. The CLI
/// applies them too; a raw client gets them here, from the host.
pub fn check_safe_send(command: &Command) -> Result<(), String> {
    let Command::Send {
        force,
        session_id,
        once,
        when_idle,
        detach,
        timeout_secs,
        ..
    } = command
    else {
        return Ok(());
    };
    // Nobody waits for a detached send's reply: `once-status KEY` is the only
    // way to learn how it went.
    if *detach && (!when_idle || once.is_none()) {
        return Err("--detach requires --when-idle and --once KEY".into());
    }
    if once.as_deref().is_some_and(|key| !valid_once_key(key)) {
        return Err("--once KEY must be 1-128 bytes without control characters".into());
    }
    // Every provider's conversation id fits a once key's shape.
    if session_id.as_deref().is_some_and(|id| !valid_once_key(id)) {
        return Err("--session-id ID must be 1-128 bytes without control characters".into());
    }
    if timeout_secs.is_some() && !when_idle {
        return Err("send --timeout requires --when-idle".into());
    }
    let max = if *detach {
        DETACHED_TIMEOUT_SECS
    } else {
        WHEN_IDLE_TIMEOUT_MAX_SECS
    };
    if timeout_secs.is_some_and(|secs| secs == 0 || secs > max) {
        return Err(if *detach {
            format!("send --detach --timeout must be 1-{max} seconds")
        } else {
            format!(
                "send --timeout must be 1-{max} seconds (up to {DETACHED_TIMEOUT_SECS} with --detach)"
            )
        });
    }
    if *when_idle && *force {
        return Err("--when-idle waits for the checks --force skips: pass one or the other".into());
    }
    Ok(())
}

/// The file, in muxel's data directory, where the running app says how to reach
/// it. Owner-only: the token in it is the whole of the authorization.
pub const ENDPOINT_FILE: &str = "control.json";

/// The environment variable that tells a program muxel starts in a pane which
/// pane it runs in: the pane's `uuid`, which works as AGENT. Only a pane on this
/// computer has one; a remote pane is kept from taking one (see `ssh`).
pub const INSTANCE_ID_ENV: &str = "MUXEL_INSTANCE_ID";

/// How long after a prompt is sent an agent may take to visibly start working
/// before `wait` stops expecting it to.
pub const REPLY_GRACE_MS: i64 = 30_000;

/// The same for an answer to a question: an agent takes one in at once, and one
/// that goes straight back to idle ("No, and tell me what to do instead") never
/// shows a turn at all, so `wait` gives up expecting one sooner.
pub const ANSWER_GRACE_MS: i64 = 5_000;

/// Lines `screen` returns by default, and at most.
pub const SCREEN_LINES: usize = 60;
pub const SCREEN_LINES_MAX: usize = 2000;

/// How to reach the running app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub version: u32,
    pub port: u16,
    pub token: String,
    pub pid: u32,
    /// The running muxel binary — how an agent on a computer whose muxel lives
    /// somewhere unexpected finds the command to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
}

/// One request: one line of JSON on a fresh connection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub version: u32,
    pub token: String,
    /// The calling pane, if this client runs inside one. A declared label,
    /// not an authenticated identity; older hosts ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller_instance_id: Option<Uuid>,
    pub command: Command,
}

/// What a request asks for. The app answers each from its live state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    /// Every project, with a count of its agents by status.
    Projects,
    /// The pane layout and every agent, of one project or all of them.
    Panes {
        #[serde(default)]
        project: Option<String>,
    },
    /// Open a terminal in a project: a tab in the calling pane's group, else in
    /// the focused one.
    New {
        project: String,
        #[serde(flatten)]
        options: NewOptions,
    },
    /// Poll a creation approval without keeping a connection open.
    NewStatus { request_id: Uuid },
    /// One agent's state, or every agent's.
    Status {
        #[serde(default)]
        agent: Option<String>,
    },
    /// Deliberately open an existing pane, waking it if asleep.
    Focus { agent: String },
    /// Wake an asleep pane in the background, without focusing it.
    Wake { agent: String },
    /// An agent's last prompt, last reply and pending question.
    Show {
        agent: String,
        /// The whole last turn rather than only its final message.
        #[serde(default)]
        full: bool,
    },
    /// The raw text of an agent's terminal.
    Screen {
        agent: String,
        #[serde(default)]
        lines: Option<usize>,
    },
    /// Type a prompt and press Enter.
    Send {
        agent: String,
        text: String,
        /// Type even though the agent is working or blocked.
        #[serde(default)]
        force: bool,
        /// Refuse if the pane no longer has this terminal launch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        launch_id: Option<Uuid>,
        /// Refuse unless the pane is in this provider conversation: its
        /// `session_id` in `status`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        /// Type this text into this pane at most once: a repeat with the same
        /// key returns the first attempt's result and types nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        once: Option<String>,
        /// Wait in muxel until the pane is safe to type into, then send.
        #[serde(default, skip_serializing_if = "is_false")]
        when_idle: bool,
        /// With `when_idle` and `once`: answer `queued` at once and keep the
        /// send in muxel with nobody waiting for it; `once-status` reports it.
        #[serde(default, skip_serializing_if = "is_false")]
        detach: bool,
        /// How long `when_idle` waits, in seconds.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout_secs: Option<u64>,
    },
    /// What became of a `send --once KEY` to this agent, without typing.
    OnceStatus { agent: String, key: String },
    /// Withdraw a detached `send --once KEY` that hasn't typed yet.
    OnceCancel { agent: String, key: String },
    /// Pick a numbered option of the question the agent is blocked on.
    Answer {
        agent: String,
        option: String,
        /// Override the caller's own-pane guard only.
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
    /// Press keys (see [`key_bytes`]).
    Keys {
        agent: String,
        keys: Vec<String>,
        /// Override the caller's own-pane guard only.
        #[serde(default, skip_serializing_if = "is_false")]
        force: bool,
    },
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// Require explicit versions so older hosts refuse fields they cannot honor.
pub fn request_version(command: &Command) -> u32 {
    if matches!(
        command,
        Command::Send { detach: true, .. } | Command::OnceCancel { .. }
    ) {
        return DETACHED_SEND_VERSION;
    }
    if matches!(
        command,
        Command::Send {
            session_id: Some(_),
            ..
        }
    ) {
        return PINNED_SEND_VERSION;
    }
    if let Command::Send {
        once,
        when_idle,
        timeout_secs,
        ..
    } = command
        && (once.is_some() || *when_idle || timeout_secs.is_some())
    {
        return SAFE_SEND_VERSION;
    }
    if matches!(command, Command::OnceStatus { .. }) {
        return SAFE_SEND_VERSION;
    }
    if matches!(command, Command::New { options, .. } if options.model.is_some() || options.effort.is_some())
    {
        return PARAMETERIZED_NEW_VERSION;
    }
    if matches!(
        command,
        Command::Send {
            launch_id: Some(_),
            ..
        } | Command::New { .. }
            | Command::NewStatus { .. }
    ) {
        GUARDED_SEND_VERSION
    } else {
        PROTOCOL_VERSION
    }
}

/// The reply to a [`Request`]: one line of JSON.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Why it failed, when that is one of the [`ErrorCode`]s.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "known_code"
    )]
    pub code: Option<ErrorCode>,
    /// A stable detail of a refusal, when its code alone is too broad.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Present and true once input may have reached the pane: after a paste that
    /// may have gone in, or a key of `keys` or `answer`. A writer error may leave
    /// partial input; a paste tmux refused typed nothing and leaves it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pasted: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Deadline (unix ms) until which the user can undo this pane's close.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_until: Option<i64>,
    /// With `duplicate`: the first attempt's failure, as its own reply body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<Value>,
}

impl Response {
    pub fn ok(result: Value) -> Self {
        Self {
            ok: true,
            result: Some(result),
            code: None,
            reason: None,
            pasted: None,
            error: None,
            undo_until: None,
            original: None,
        }
    }

    pub fn err(error: impl Into<Error>) -> Self {
        let error = error.into();
        Self {
            ok: false,
            result: None,
            code: error.code,
            reason: error.reason,
            pasted: error.pasted,
            error: Some(error.message),
            undo_until: error.undo_until,
            original: error.original.map(|original| *original),
        }
    }

    /// The result, or why there is none.
    pub fn into_result(self) -> Result<Value, Error> {
        if self.ok {
            return Ok(self.result.unwrap_or(Value::Null));
        }
        Err(Error {
            code: self.code,
            reason: self.reason,
            pasted: self.pasted,
            message: self
                .error
                .unwrap_or_else(|| "muxel refused the request".into()),
            undo_until: self.undo_until,
            original: self.original.map(Box::new),
        })
    }
}

/// A code from a newer muxel, which this one doesn't know, reads as none rather
/// than making the whole reply unreadable.
fn known_code<'de, D: serde::Deserializer<'de>>(de: D) -> Result<Option<ErrorCode>, D::Error> {
    Ok(Option::<Value>::deserialize(de)?.and_then(|code| serde_json::from_value(code).ok()))
}

/// Why `muxel ctl` didn't type into an agent, or couldn't reach muxel, as a word a
/// script can act on; `error` says the same for people and may be reworded. Other
/// failures (a bad command line, no agent by that name) carry only `error`.
///
/// These refusals type nothing, except `input_changed` (its prompt was pasted,
/// but Enter was withheld) and `keys_partly_pressed` (some keys went in). A lost
/// reply after sending has an uncertain outcome;
/// inspect the agent before retrying.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Outside control was revoked before the mutation.
    ControlDisabled,
    /// Outside terminal creation is Off.
    CreationDisabled,
    /// Literal argv or a shell launch needs the shell/command grant.
    CommandPermissionRequired,
    /// The user denied this creation request.
    ApprovalDenied,
    /// No approval arrived within 120 seconds of admission.
    ApprovalTimeout,
    /// Only configured non-shell terminal presets accept user tasks.
    PromptUnsupported,
    /// A preset cannot safely express the requested model or effort override,
    /// or a `send --session-id` targets a pane that takes ctl input through
    /// tmux, where muxel can't hold the paste to a conversation (`reason:
    /// tmux_delivery`; nothing was typed).
    ParameterUnsupported,
    /// The agent is working: `wait` for it (or `send --force`).
    Working,
    /// The agent is waiting on a question: `answer` it, or press `keys`.
    Blocked,
    /// The agent is still starting: try again in a moment.
    Starting,
    /// Input is refused while the pane is held for closing. Reads still work.
    /// `undo_until` is present only during its countdown; otherwise `reason`
    /// describes `counting_down`, `checking`, `awaiting_decision` or `restoring`
    /// with no deadline. A countdown whose wall-clock deadline elapsed (for
    /// example across suspend) omits that expired deadline too.
    Closing,
    /// The pane isn't running this session, or its program failed to start.
    NotRunning,
    /// The agent's program has exited, or the addressed UUID is no longer in
    /// this workspace. A missing pane does not prove a remote process is dead.
    Exited,
    /// Another request holds the shared tmux claim, or another `send` from this
    /// muxel hasn't pressed its Enter yet.
    ClaimedElsewhere,
    /// The pane's terminal was replaced since the caller inspected it.
    LaunchMismatch,
    /// The prompt was pasted but its delayed Enter was withheld.
    InputChanged,
    /// The provider cannot insert these characters literally. Nothing was typed.
    UnsupportedInput,
    /// A pane behind tmux takes ctl input only through tmux, at the agent's own
    /// tmux pane, and that pane couldn't be found or has gone. Nothing was typed.
    TmuxPaneUnresolved,
    /// A tmux pane mode is open. Leave it before retrying; nothing was typed.
    TmuxPaneInMode,
    /// tmux refused the input, or couldn't be run. Nothing was typed.
    TmuxRefused,
    /// Some of a `keys` request's keys were pressed, then the rest failed.
    KeysPartlyPressed,
    /// The caller selected its own pane without explicitly forcing the action.
    SelfTarget,
    /// The pane's composer holds text: a person's half-typed prompt, or one an
    /// earlier send pasted without pressing Enter. Nothing was typed.
    DraftPresent,
    /// Something is already in progress for this pane: `reason` is `queued`
    /// (another `send --when-idle` waits for it), `queue_full` (this muxel has
    /// as many waiting as it takes) or `once_pending` (the first send with this
    /// `--once` key hasn't finished; `pasted` says whether it has typed).
    /// This request typed nothing.
    Busy,
    /// A repeat of a `--once` key whose first send failed after it typed.
    /// `original` holds that failure; nothing was typed this time.
    Duplicate,
    /// `send --when-idle` saw no safe moment before its timeout. `reason` names
    /// what it was still waiting on. Nothing was typed.
    TimedOut,
    /// A waiting send was withdrawn before it typed: `once-cancel` withdrew a
    /// detached one, or an attached one's caller disconnected.
    Cancelled,
    /// `muxel ctl` found no muxel to ask: it isn't running, outside control is
    /// off, its `control.json` can't be read or reached, or a read-only creation
    /// status cannot be answered.
    HostUnavailable,
    /// The endpoint's owner process did not pass the liveness check.
    StaleEndpoint,
}

/// A failed request: what to tell the user, and its [`ErrorCode`] when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub code: Option<ErrorCode>,
    pub reason: Option<String>,
    pub pasted: Option<bool>,
    pub message: String,
    pub undo_until: Option<i64>,
    /// See [`Response::original`].
    pub original: Option<Box<Value>>,
}

impl Error {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            reason: None,
            pasted: None,
            message: message.into(),
            undo_until: None,
            original: None,
        }
    }

    pub fn closing(undo_until: i64) -> Self {
        Self {
            code: Some(ErrorCode::Closing),
            reason: None,
            pasted: None,
            message: "the pane is closing: undo the close in muxel before sending more input"
                .into(),
            undo_until: Some(undo_until),
            original: None,
        }
    }

    /// The reply a repeated `--once` key gets when its first send failed
    /// after typing: `duplicate`, with that failure as `original`, and
    /// `pasted` as it was, since the first send's text may be in the pane.
    pub fn duplicate_of(original: &Error) -> Self {
        Self {
            code: Some(ErrorCode::Duplicate),
            reason: None,
            pasted: original.pasted,
            message: "this --once key was already used and its send failed after typing; \
                      nothing was typed now: check the agent's `screen`"
                .into(),
            undo_until: None,
            original: serde_json::to_value(Response::err(original.clone()))
                .ok()
                .map(Box::new),
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn after_paste(mut self) -> Self {
        self.pasted = Some(true);
        self
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self {
            code: None,
            reason: None,
            pasted: None,
            message,
            undo_until: None,
            original: None,
        }
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        message.to_string().into()
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Check immediately before pasting, including after asynchronous claim work.
/// `--force` only overrides busy state; it never overrides a launch mismatch.
pub fn check_launch_id(expected: Option<Uuid>, current: Option<Uuid>) -> Result<(), Error> {
    if let Some(expected) = expected
        && current != Some(expected)
    {
        return Err(Error::new(
            ErrorCode::LaunchMismatch,
            format!(
                "the agent's terminal was replaced since you saw launch_id {expected}: check `status` before sending again"
            ),
        ));
    }
    Ok(())
}

/// What muxel can vouch for about the provider conversation a pane is in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conversation {
    /// The running terminal's live binding confirms this id or, with nothing
    /// running, it is the id the pane will resume.
    Known(String),
    /// Nothing muxel can vouch for: the provider has named a conversation
    /// muxel hasn't confirmed yet, or there is none.
    Unconfirmed,
}

impl Conversation {
    /// The pane's [`AgentInfo::session_id`].
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::Known(id) => Some(id),
            Self::Unconfirmed => None,
        }
    }

    pub fn into_id(self) -> Option<String> {
        match self {
            Self::Known(id) => Some(id),
            Self::Unconfirmed => None,
        }
    }
}

/// Checked beside [`check_launch_id`], and again immediately before pasting:
/// a send pinned to a conversation (`--session-id`, or the one a waiting send
/// queued in) types only while the pane is still in it. A pane muxel can't
/// vouch for refuses too, with its own reason, since it may settle back into
/// the pinned conversation. `--force` never overrides this.
pub fn check_session_id(expected: Option<&str>, current: &Conversation) -> Result<(), Error> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let (message, reason) = match current {
        Conversation::Known(current) if current == expected => return Ok(()),
        Conversation::Known(current) => (
            format!(
                "the agent's pane is in conversation {current}, not {expected}: nothing was \
                 typed. Check `status` before sending again"
            ),
            "conversation_changed",
        ),
        Conversation::Unconfirmed => (
            format!(
                "muxel can't yet confirm which conversation the agent's pane is in, so not \
                 {expected} either: nothing was typed. Check `status` for its session_id"
            ),
            "conversation_unconfirmed",
        ),
    };
    Err(Error::new(ErrorCode::LaunchMismatch, message).with_reason(reason))
}

/// A pane cannot direct input at itself by accident. This uses the caller's
/// declared environment value for convenience, not as an authorization check.
pub fn check_self_target(caller: Option<Uuid>, target: Uuid, force: bool) -> Result<(), Error> {
    if caller == Some(target) && !force {
        return Err(Error::new(
            ErrorCode::SelfTarget,
            "this command targets your own pane; pass --force if you mean to type into it",
        ));
    }
    Ok(())
}

/// A successful send receipt, created only after the paste was admitted to the
/// PTY writer. Admission does not confirm delivery to or consumption by the agent.
pub fn pasted_prompt_receipt(pane: Uuid, launch: Uuid) -> Value {
    json!({ "sent": true, "pasted": true, "agent": short_id(pane), "uuid": pane, "launch_id": launch })
}

/// What an agent pane is doing, as `muxel ctl` reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    /// Generating or running tools.
    Working,
    /// Alive and waiting for a prompt.
    Idle,
    /// Waiting on a permission or choice prompt.
    Blocked,
    /// Finished a turn.
    Done,
    /// The program ended.
    Exited,
    /// Launching, or reconnecting to its host.
    Starting,
    /// In the layout, but not started this session.
    NotRunning,
    /// Could not be launched at all.
    Failed,
}

/// The current evidence behind a terminal's reported lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatusSource {
    Process,
    Program,
    Hook,
    Title,
    Screen,
    Bell,
    Output,
    Quiet,
    Settling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentBlockedOn {
    Permission,
    Question,
    Auth,
}

/// One pane's entry in `panes` / `status`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentInfo {
    /// Short id: the first eight hex digits of `uuid`.
    pub id: String,
    pub uuid: Uuid,
    pub name: String,
    /// Stable outside-control alias, retained after an agent changes its title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub project: String,
    pub project_id: String,
    /// `terminal`, `editor` or `browser`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<String>,
    pub preset: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A coding agent rather than a shell (or an editor/browser pane).
    pub is_agent: bool,
    /// The sidebar's lifecycle badge; `None` for editors and browsers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentState>,
    /// Live facts used for control decisions when `status` holds a sidebar badge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_status: Option<AgentState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<AgentStatusSource>,
    /// Bounded, single-line activity supplied by the current provider binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_on: Option<AgentBlockedOn>,
    /// Why a terminal is not running. `dormant` and `sleeping` mean it starts
    /// when woken (Enter or typing in it, Wake, `focus` or `wake`); selecting
    /// it in muxel only shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_running_reason: Option<String>,
    /// For an asleep agent: whether waking it resumes its conversation
    /// (`false` when the saved conversation is gone), checked
    /// when asked. Absent while it runs, or when muxel can't tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumes: Option<bool>,
    /// When the agent last started working, finished or blocked (unix ms).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_since: Option<i64>,
    /// This run of the pane's terminal: a new id each time muxel opens a terminal
    /// for the pane (starting or restarting its program, reconnecting or
    /// recovering its session, reopening its workspace), so a caller can tell
    /// that what it saw may be out of date. An agent kept in tmux can still be in
    /// the same conversation under a new id, and an agent that changes
    /// conversation by itself (Claude's `/resume`) keeps its id. `None` while it
    /// isn't running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<Uuid>,
    /// The provider conversation the pane is in, as the provider names it.
    /// For a running agent, only once its live terminal binding confirms it:
    /// the launch opened it, muxel adopted it in this binding, or the
    /// provider's current title names it. It is `None` while the agent is
    /// switching conversation (`/clear`, `/resume`, a new Codex thread) and
    /// muxel hasn't confirmed the new one yet. For a pane that isn't running,
    /// it is the id muxel will resume. `None` for shells and agents that
    /// haven't named a conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// A prompt sent through `muxel ctl` whose turn hasn't ended yet.
    pub awaiting_reply: bool,
    /// What the agent's own screen says is still running beside its turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<BackgroundWork>,
    /// The foreground turn is over with background work still listed. Only
    /// subagents hold the pane Working; shells and monitors remain metadata.
    /// `wait --turn` stops here even during a subagent hold.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub turn_over: bool,
    /// A startup task that failed before provider acceptance. The terminal itself
    /// may still be running, for example at a folder-trust menu.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_delivery: Option<PromptDelivery>,
    /// The pane the user has focused.
    pub focused: bool,
    /// The pane (tab group) it is a tab of, numbered in layout order from 1.
    /// `None` when popped out into its own window or held outside the layout
    /// for a recoverable close.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Current Git branch from the launch folder's background cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Runs on a remote host over SSH.
    pub remote: bool,
}

impl AgentInfo {
    /// State used by control decisions, independent of a retained sidebar badge.
    pub fn live_state(&self) -> Option<AgentState> {
        self.live_status.or(self.status)
    }
}

/// Functional state from a status/show entry, including replies from older hosts.
/// A present but malformed live state must not fall back to a finished badge.
pub fn reported_live_state(info: &serde_json::Value) -> Option<AgentState> {
    let state = info
        .get("live_status")
        .filter(|state| !state.is_null())
        .or_else(|| info.get("status"))?;
    serde_json::from_value(state.clone()).ok()
}

/// An instance's short id, as tmux session names and `muxel ctl` show it.
pub fn short_id(id: Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

/// The latest lifecycle transition an agent has recorded (unix ms).
pub fn status_since(activity: &AgentActivity) -> Option<i64> {
    activity.last_work_at()
}

/// The [`AgentInfo::launch_id`] of an agent in `state` whose pane's terminal has
/// session id `terminal`: that id while the program runs, none while it starts
/// and none once it has exited, though its pane keeps the terminal open.
pub fn launch_id(state: Option<AgentState>, terminal: Option<Uuid>) -> Option<Uuid> {
    match state? {
        AgentState::Working | AgentState::Idle | AgentState::Blocked | AgentState::Done => terminal,
        AgentState::Starting | AgentState::Exited | AgentState::NotRunning | AgentState::Failed => {
            None
        }
    }
}

/// The value given to `flag` in `args` (`--model x` or `--model=x`).
pub fn flag_value(args: &[String], flag: &str) -> Option<String> {
    let flag = flag.trim();
    if flag.is_empty() {
        return None;
    }
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == flag {
            return it.next().cloned();
        }
        if let Some(value) = arg.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
            return Some(value.to_string());
        }
    }
    None
}

// --- Which agent ---------------------------------------------------------------

/// What an agent reference is matched against.
#[derive(Clone, Copy, Debug)]
pub struct AgentEntry<'a> {
    pub id: Uuid,
    pub name: &'a str,
    pub label: Option<&'a str>,
    pub project: &'a str,
}

/// A name as it is matched: lowercase letters, digits and single spaces, so the
/// status glyphs agents put in their titles (`✳ Fix the pager`) don't count.
fn normalize(name: &str) -> String {
    let kept: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `query` can be an id: a full UUID, or at least four hex digits.
fn id_like(query: &str) -> bool {
    query.len() >= 4 && query.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

fn id_matches(id: Uuid, query: &str) -> bool {
    let query = query.replace('-', "").to_ascii_lowercase();
    id.simple().to_string().starts_with(&query)
}

/// Resolve an agent reference: `focused`, a UUID or short id (any unique prefix of
/// four or more hex digits), a display name or retained label, `project/name`
/// when names repeat, or an id prefixed by `pane` that no agent's name or label
/// matches. Each pane contributes at most one candidate.
pub fn resolve_agent(
    query: &str,
    agents: &[AgentEntry],
    focused: Option<Uuid>,
) -> Result<Uuid, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("no agent given".into());
    }
    if matches!(query, "focused" | "active" | ".") {
        return focused
            .filter(|id| agents.iter().any(|a| a.id == *id))
            .ok_or_else(|| "no pane is focused".into());
    }
    let by_id = |id_query: &str| -> Option<Result<Uuid, String>> {
        let hits: Vec<&AgentEntry> = agents
            .iter()
            .filter(|a| id_matches(a.id, id_query))
            .collect();
        match hits.as_slice() {
            [one] => Some(Ok(one.id)),
            [] => None,
            many => Some(Err(ambiguous(query, many))),
        }
    };
    if id_like(query)
        && let Some(found) = by_id(query)
    {
        return found;
    }
    let (project, name) = match query.split_once('/') {
        Some((project, name)) => (Some(normalize(project)), normalize(name)),
        None => (None, normalize(query)),
    };
    let hits: Vec<&AgentEntry> = agents
        .iter()
        .filter(|a| {
            normalize(a.name) == name
                || a.label
                    .is_some_and(|label| !label.trim().is_empty() && normalize(label) == name)
        })
        .filter(|a| project.as_ref().is_none_or(|p| normalize(a.project) == *p))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.id),
        // `pane<ID>` only once no agent has that name or label.
        [] => query
            .strip_prefix("pane")
            .filter(|id| id_like(id))
            .and_then(&by_id)
            .unwrap_or_else(|| {
                Err(format!(
                    "no agent matches '{query}' — run `muxel ctl panes` to list them"
                ))
            }),
        many => Err(ambiguous(query, many)),
    }
}

fn ambiguous(query: &str, hits: &[&AgentEntry]) -> String {
    let list: Vec<String> = hits
        .iter()
        .map(|a| format!("{} ({} in {})", short_id(a.id), a.name, a.project))
        .collect();
    format!(
        "'{query}' matches more than one agent: {} — use an id",
        list.join(", ")
    )
}

/// Resolve a project reference: `active`, an id (unique prefix), or a name.
pub fn resolve_project(
    query: &str,
    projects: &[(Uuid, &str)],
    active: Option<Uuid>,
) -> Result<Uuid, String> {
    let query = query.trim();
    if matches!(query, "active" | "focused" | ".") {
        return active.ok_or_else(|| "no project is active".into());
    }
    if id_like(query) {
        let hits: Vec<Uuid> = projects
            .iter()
            .filter(|(id, _)| id_matches(*id, query))
            .map(|(id, _)| *id)
            .collect();
        if let [one] = hits.as_slice() {
            return Ok(*one);
        }
    }
    let name = normalize(query);
    let hits: Vec<Uuid> = projects
        .iter()
        .filter(|(_, n)| normalize(n) == name)
        .map(|(id, _)| *id)
        .collect();
    match hits.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!(
            "no project matches '{query}' — run `muxel ctl projects` to list them"
        )),
        _ => Err(format!(
            "'{query}' matches more than one project — use its id"
        )),
    }
}

// --- The layout ------------------------------------------------------------------

/// A project's pane tree as JSON, and the pane (numbered in layout order from 1)
/// each instance is a tab of.
pub fn describe_layout(node: &PaneNode) -> (Value, HashMap<Uuid, usize>) {
    fn walk(node: &PaneNode, next: &mut usize, panes: &mut HashMap<Uuid, usize>) -> Value {
        match node {
            PaneNode::Leaf(leaf) => {
                *next += 1;
                for tab in &leaf.tabs {
                    panes.insert(*tab, *next);
                }
                json!({
                    "pane": *next,
                    "tabs": leaf.tabs.iter().map(|t| short_id(*t)).collect::<Vec<_>>(),
                    "active_tab": leaf.tabs.get(leaf.active).map(|t| short_id(*t)),
                })
            }
            PaneNode::Split {
                direction,
                sizes,
                children,
            } => json!({
                "split": match direction {
                    SplitDirection::Horizontal => "side_by_side",
                    SplitDirection::Vertical => "stacked",
                },
                "sizes": sizes,
                "children": children
                    .iter()
                    .map(|child| walk(child, next, panes))
                    .collect::<Vec<_>>(),
            }),
        }
    }
    let mut panes = HashMap::new();
    let value = walk(node, &mut 0, &mut panes);
    (value, panes)
}

// --- Keys --------------------------------------------------------------------------

/// One key `ctl keys` can press. A terminal muxel writes to directly gets its
/// [`Key::bytes`]; a pane behind tmux gets the tmux key it names (see
/// `tmux::key_name`), which tmux encodes for that pane's own modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Enter,
    Escape,
    Tab,
    BackTab,
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    Backspace,
    Delete,
    PageUp,
    PageDown,
    Space,
    /// `ctrl-<letter>`, lowercase ASCII.
    Ctrl(char),
    /// Any single character, typed as itself.
    Char(char),
}

impl Key {
    /// `enter`, `esc`, `tab`, `shift-tab`, the arrows, `backspace`, `delete`,
    /// `space`, `home`, `end`, `pageup`, `pagedown`, `ctrl-<letter>`, or any
    /// single character.
    pub fn parse(key: &str) -> Option<Self> {
        let mut chars = key.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            return Some(Self::Char(c));
        }
        let name = key.trim().to_ascii_lowercase().replace('_', "-");
        Some(match name.as_str() {
            "enter" | "return" => Self::Enter,
            "esc" | "escape" => Self::Escape,
            "tab" => Self::Tab,
            "shift-tab" | "backtab" => Self::BackTab,
            "up" => Self::Up,
            "down" => Self::Down,
            "right" => Self::Right,
            "left" => Self::Left,
            "home" => Self::Home,
            "end" => Self::End,
            "backspace" => Self::Backspace,
            "delete" | "del" => Self::Delete,
            "pageup" | "page-up" => Self::PageUp,
            "pagedown" | "page-down" => Self::PageDown,
            "space" => Self::Space,
            _ => {
                let letter = name.strip_prefix("ctrl-")?;
                let mut it = letter.chars();
                match (it.next(), it.next()) {
                    (Some(c), None) if c.is_ascii_lowercase() => Self::Ctrl(c),
                    _ => return None,
                }
            }
        })
    }

    /// The bytes this key sends to a terminal muxel writes to itself. Arrows
    /// (and home/end) follow the terminal's cursor-key mode, as a real keypress
    /// would.
    pub fn bytes(self, app_cursor: bool) -> Vec<u8> {
        let arrow = |c: char| {
            let lead = if app_cursor { "\x1bO" } else { "\x1b[" };
            format!("{lead}{c}").into_bytes()
        };
        match self {
            Self::Enter => b"\r".to_vec(),
            Self::Escape => b"\x1b".to_vec(),
            Self::Tab => b"\t".to_vec(),
            Self::BackTab => b"\x1b[Z".to_vec(),
            Self::Up => arrow('A'),
            Self::Down => arrow('B'),
            Self::Right => arrow('C'),
            Self::Left => arrow('D'),
            Self::Home => arrow('H'),
            Self::End => arrow('F'),
            Self::Backspace => b"\x7f".to_vec(),
            Self::Delete => b"\x1b[3~".to_vec(),
            Self::PageUp => b"\x1b[5~".to_vec(),
            Self::PageDown => b"\x1b[6~".to_vec(),
            Self::Space => b" ".to_vec(),
            Self::Ctrl(c) => vec![c as u8 - b'a' + 1],
            Self::Char(c) => c.to_string().into_bytes(),
        }
    }
}

/// The bytes a key name sends (see [`Key::parse`] and [`Key::bytes`]).
pub fn key_bytes(key: &str, app_cursor: bool) -> Option<Vec<u8>> {
    Key::parse(key).map(|key| key.bytes(app_cursor))
}

/// Whether pressing `key` submits what's typed.
pub fn key_submits(key: &str) -> bool {
    matches!(key.trim().to_ascii_lowercase().as_str(), "enter" | "return")
}

// --- The question an agent is blocked on ------------------------------------------

/// A numbered choice in an agent's prompt (`❯ 1. Yes`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub key: String,
    pub label: String,
    /// The option the prompt's cursor is on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub selected: bool,
}

/// What a blocked agent is asking, read off its screen.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub text: String,
    pub options: Vec<QuestionOption>,
}

/// Glyphs a prompt puts in front of the option its cursor is on.
const SELECT_GLYPHS: &[char] = &['❯', '>', '›', '▶', '→', '➜'];
/// How far up from the bottom of the screen a prompt's options are looked for.
const QUESTION_REACH: usize = 30;
/// Lines of question text kept above the options (or of screen, with none).
const QUESTION_LINES: usize = 14;

fn is_box_char(c: char) -> bool {
    ('\u{2500}'..='\u{257F}').contains(&c)
}

/// `line` without the box frame some prompts are drawn in, or its indent.
fn unbox(line: &str) -> &str {
    line.trim()
        .trim_start_matches(['│', '┃', '║'])
        .trim_end_matches(['│', '┃', '║'])
        .trim()
}

/// A horizontal rule or a frame's top or bottom edge (`────`, `╭──╮`) — not an
/// empty line inside a frame (`│    │`), which is only its sides.
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty()
        && t.chars().all(|c| is_box_char(c) || c.is_whitespace())
        && t.chars()
            .any(|c| is_box_char(c) && !matches!(c, '│' | '┃' | '║'))
}

/// `❯ 1. Yes` → ("1", "Yes", selected).
fn option_line(line: &str) -> Option<QuestionOption> {
    let s = unbox(line);
    let (selected, s) = match s.strip_prefix(SELECT_GLYPHS) {
        Some(rest) => (true, rest.trim_start()),
        None => (false, s),
    };
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    let rest = s[digits.len()..].strip_prefix(['.', ')'])?;
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let label = rest.trim();
    (!label.is_empty()).then(|| QuestionOption {
        key: digits,
        label: label.to_string(),
        selected,
    })
}

/// The question at the bottom of a blocked agent's screen: its numbered options,
/// counted back from the last one to `1`, and the text above them up to the rule
/// the prompt is drawn under. A prompt with no numbered options (`Allow? (y/n)`)
/// comes back as the last lines of the screen, `None` when there are none.
pub fn pending_question(screen: &str) -> Option<Question> {
    let lines: Vec<&str> = screen.lines().collect();
    let bottom = lines.iter().rposition(|l| !l.trim().is_empty())?;
    let reach = bottom.saturating_sub(QUESTION_REACH);

    let last = (reach..=bottom)
        .rev()
        .find(|&i| option_line(lines[i]).is_some());
    let mut options = Vec::new();
    let mut first = None;
    if let Some(last) = last {
        let mut gap = 0;
        let mut expect: Option<u32> = None;
        for i in (reach..=last).rev() {
            match option_line(lines[i]) {
                Some(opt) => {
                    let n: u32 = opt.key.parse().unwrap_or(0);
                    if expect.is_some_and(|e| n != e) {
                        break;
                    }
                    options.push(opt);
                    first = Some(i);
                    gap = 0;
                    if n <= 1 {
                        break;
                    }
                    expect = Some(n - 1);
                }
                // A label wrapped onto the next line, or a hint under an option.
                None if gap < 2 && !lines[i].trim().is_empty() && !is_rule(lines[i]) => gap += 1,
                None => break,
            }
        }
        options.reverse();
    }

    let text_end = first.unwrap_or(bottom + 1);
    let mut text: Vec<&str> = Vec::new();
    let mut blanks = 0;
    for i in (0..text_end).rev() {
        let line = lines[i];
        if is_rule(line) || text.len() >= QUESTION_LINES {
            break;
        }
        if unbox(line).is_empty() {
            blanks += 1;
            if blanks >= 2 && !text.is_empty() {
                break;
            }
            continue;
        }
        if blanks > 0 && !text.is_empty() {
            text.push("");
        }
        blanks = 0;
        text.push(unbox(line));
    }
    text.reverse();
    if text.is_empty() && options.is_empty() {
        return None;
    }
    Some(Question {
        text: text.join("\n"),
        options,
    })
}

// --- The last prompt -----------------------------------------------------------------

/// The last prompt the user sent in a Claude session transcript
/// (`~/.claude/projects/<slug>/<session>.jsonl`, or just its tail). Tool results,
/// meta entries, subagents' prompts and interruption notes are skipped; a slash
/// command comes back as `/name args`.
pub fn prompt_from_claude_transcript(jsonl: &str) -> Option<String> {
    let mut last = None;
    for line in jsonl.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("user")
            || entry.get("isMeta").and_then(Value::as_bool) == Some(true)
            || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let content = &entry["message"]["content"];
        let text = match content {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .filter(|t| !t.trim_start().starts_with("<system-reminder>"))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => continue,
        };
        if let Some(prompt) = user_prompt(&text) {
            last = Some(prompt);
        }
    }
    last
}

/// A user entry's text as the prompt it was, or `None` when it is bookkeeping.
fn user_prompt(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty()
        || text.starts_with("[Request interrupted")
        || text.starts_with("<local-command-")
        || text.starts_with("Caveat:")
    {
        return None;
    }
    if let Some(name) = between(text, "<command-name>", "</command-name>") {
        let args = between(text, "<command-args>", "</command-args>").unwrap_or("");
        let name = name.trim();
        let name = if name.starts_with('/') {
            name.to_string()
        } else {
            format!("/{name}")
        };
        return Some(format!("{name} {}", args.trim()).trim().to_string());
    }
    Some(text.to_string())
}

fn between<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)? + start;
    Some(&text[start..end])
}

// --- Waiting for a turn -----------------------------------------------------------------

/// Whether a prompt (or answer) `muxel ctl` sent at `sent_at` (unix ms) is still
/// waiting on its turn to end. A working agent is mid-turn. A quiet one is done
/// with it once it has worked, finished or blocked since the prompt — or, having
/// shown no sign of starting, once `grace_ms` ([`REPLY_GRACE_MS`] or
/// [`ANSWER_GRACE_MS`]) has passed.
///
/// A blocked agent is asking something new only if it blocked after the prompt: a
/// block from before it is the question just answered, still on screen until the
/// agent redraws (or until the once-a-second status refresh catches up).
pub fn awaiting_reply(
    sent_at: Option<i64>,
    grace_ms: i64,
    state: Option<AgentState>,
    activity: &AgentActivity,
    now: i64,
) -> bool {
    let Some(sent) = sent_at else {
        return false;
    };
    let since = |at: Option<i64>| at.is_some_and(|at| at >= sent);
    match state {
        Some(AgentState::Working | AgentState::Starting) => true,
        Some(AgentState::Idle | AgentState::Done | AgentState::Blocked) => {
            !(since(activity.work_started_at)
                || since(activity.completed_at)
                || since(activity.blocked_at))
                && now - sent < grace_ms
        }
        _ => false,
    }
}

/// Why `wait` stopped waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitOutcome {
    /// The startup task never obtained provider-owned acceptance.
    SubmissionFailed,
    /// The turn ended; the reply is ready.
    Finished,
    /// The agent is asking something (see `question`).
    Blocked,
    /// The program ended or failed to start.
    Exited,
    /// The pane isn't running this session.
    NotRunning,
    /// `--timeout` passed with the agent still working.
    TimedOut,
}

/// Whether `wait` is over for an agent in `state`, and why. `None` keeps waiting.
pub fn wait_outcome(state: Option<AgentState>, awaiting: bool) -> Option<WaitOutcome> {
    match state? {
        AgentState::Blocked => (!awaiting).then_some(WaitOutcome::Blocked),
        AgentState::Exited | AgentState::Failed => Some(WaitOutcome::Exited),
        AgentState::NotRunning => Some(WaitOutcome::NotRunning),
        AgentState::Working | AgentState::Starting => None,
        AgentState::Idle | AgentState::Done => (!awaiting).then_some(WaitOutcome::Finished),
    }
}

// --- Sharing an agent with muxel on other computers -----------------------------------

/// The tmux session option in which a muxel records that it has just typed a
/// prompt or answer into the agent. The session lives wherever the agent runs, so
/// every muxel attached to it — a remote project opened on several computers, or a
/// local one a peer attached to over SSH — reads the same value.
pub const CLAIM_OPTION: &str = "@muxel-ctl";

/// Which muxel last typed into an agent through `muxel ctl`, and when: stored on
/// the agent's tmux session as `v1|owner|host|at|grace[|nonce]`. Older readers
/// ignore the optional suffix; their claims remain readable without it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// A random id for the muxel process (not secret; it only tells muxels apart).
    pub owner: String,
    /// The computer that muxel runs on, for the message another one shows.
    pub host: String,
    /// When the prompt or answer was submitted (unix ms, the owner's clock).
    pub at: i64,
    /// [`REPLY_GRACE_MS`] or [`ANSWER_GRACE_MS`], as for [`awaiting_reply`].
    pub grace_ms: i64,
    /// Unique per send, including concurrent sends from the same process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<Uuid>,
}

fn claim_field(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '|' || c.is_whitespace() || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect()
}

fn claim_command_field(s: &str) -> String {
    claim_field(s)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '-'
            }
        })
        .collect()
}

impl Claim {
    pub fn encode(&self) -> String {
        let mut value = format!(
            "v1|{}|{}|{}|{}",
            claim_field(&self.owner),
            claim_field(&self.host),
            self.at,
            self.grace_ms
        );
        if let Some(nonce) = self.nonce {
            value.push('|');
            value.push_str(&nonce.to_string());
        }
        value
    }

    /// A claim for a nested tmux command string. Keep `encode`'s host display
    /// compatibility, but replace metacharacters before this value is parsed by
    /// tmux 2.x, which cannot decode octal escapes in that position.
    pub fn encode_tmux_command(&self) -> String {
        let mut value = format!(
            "v1|{}|{}|{}|{}",
            claim_command_field(&self.owner),
            claim_command_field(&self.host),
            self.at,
            self.grace_ms,
        );
        if let Some(nonce) = self.nonce {
            value.push('|');
            value.push_str(&nonce.to_string());
        }
        value
    }

    /// Both admission and cleanup must distinguish same-process, same-ms sends.
    pub fn same_turn(&self, other: &Self) -> bool {
        self.owner == other.owner && self.nonce.is_some() && self.nonce == other.nonce
    }

    /// A tmux `#{m:}` (fnmatch) pattern for one send's claim. The trailing `*`
    /// keeps matching a claim a later muxel writes with extra fields after the
    /// nonce, which [`Claim::decode`] already accepts.
    pub fn pattern(owner: &str, nonce: Uuid) -> String {
        format!("v1|{}|*|*|*|{nonce}*", claim_field(owner))
    }

    /// A safe tmux format pattern for comparing the claim before replacing it.
    /// A nonce identifies modern sends even when a hostname needs quoting;
    /// legacy claims need every encoded byte to be literal and format-safe.
    pub fn compare_pattern(&self) -> Option<String> {
        if let Some(nonce) = self.nonce {
            let owner = claim_field(&self.owner);
            owner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
                .then(|| Self::pattern(&self.owner, nonce))
        } else {
            let value = self.encode();
            value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "|-_.".contains(c))
                .then_some(value)
        }
    }

    /// `None` for an unset option, or a value this version doesn't understand.
    pub fn decode(value: &str) -> Option<Self> {
        let mut parts = value.trim().split('|');
        if parts.next()? != "v1" {
            return None;
        }
        let owner = parts.next()?.to_string();
        let host = parts.next()?.to_string();
        let at = parts.next()?.parse().ok()?;
        let grace_ms = parts.next()?.parse().ok()?;
        let nonce = parts.next().map(Uuid::parse_str).transpose().ok()?;
        (!owner.is_empty()).then_some(Self {
            owner,
            host,
            at,
            grace_ms,
            nonce,
        })
    }
}

/// Whether another muxel's claim still holds the agent: its turn hasn't visibly
/// ended, judged from what this muxel sees by the same rule as [`awaiting_reply`].
/// A claim left by a muxel that quit or crashed lapses the same way — when the
/// agent is seen to finish, or when its grace runs out without a turn starting.
/// (The claim's time is the other computer's clock; they're assumed to agree to
/// within a second or so, as NTP keeps them.)
pub fn claim_holds(
    claim: &Claim,
    me: &str,
    state: Option<AgentState>,
    activity: &AgentActivity,
    now: i64,
) -> bool {
    claim.owner != me && awaiting_reply(Some(claim.at), claim.grace_ms, state, activity, now)
}

// --- The command line --------------------------------------------------------------

/// What `muxel ctl …` asked for.
#[derive(Clone, Debug, PartialEq)]
#[allow(
    clippy::large_enum_variant,
    reason = "only one command is parsed per CLI invocation; boxing adds an allocation without reducing a collection"
)]
pub enum CliCommand {
    /// A request the app answers directly.
    Request(Command),
    /// Poll an agent until its turn ends, then show it.
    Wait {
        agent: String,
        timeout_secs: u64,
        full: bool,
        /// Stop when the turn ends, even with background work still running.
        turn: bool,
    },
    /// Print the instructions for an orchestrating agent.
    Skill,
    Help(Option<String>),
}

/// `wait`'s default patience.
pub const WAIT_TIMEOUT_SECS: u64 = 600;

/// Usage text for `muxel ctl help`.
pub const USAGE: &str = "\
usage: muxel ctl <command> [args]

  projects                       list projects
  panes [PROJECT]                pane layout and agents (all projects, or one)
  new PROJECT [--cwd DIR] [--name TEXT | --name-fixed TEXT] [--tab | --split right|down]
              [--focus] [--top-level] [--prompt TEXT [--wait-delivery]] [--from LABEL] [--no-wait]
              [--preset NAME] [--model MODEL] [--effort LEVEL] [-- PROGRAM [ARGS...]]
                                 open in background; --focus selects and raises;
                                 from a pane, nests under its session in Recent work
                                 unless --top-level; --background is a compatibility
                                 no-op; --prompt - reads stdin
  new-status REQUEST_ID          poll a pending creation's final reply
  status [AGENT]                 an agent's status (or every agent's)
  focus AGENT                    open a pane; wake it if asleep
  wake AGENT                     wake an asleep pane, without focus
  show AGENT [--full]            last prompt, last reply, pending question
  screen AGENT [--lines N]       raw terminal text (default 60 lines)
  send AGENT TEXT [--force] [--launch-id ID] [--session-id ID] [--once KEY]
       [--when-idle [--detach] [--timeout SECS]]
                                 type TEXT and press Enter ('-' reads stdin);
                                 --launch-id refuses a replaced terminal;
                                 --session-id refuses another conversation;
                                 --once never types the same KEY twice;
                                 --when-idle waits until the agent can take it;
                                 --detach answers at once and muxel keeps it
  once-status AGENT KEY          what became of a send --once KEY
  once-cancel AGENT KEY          withdraw a detached send that hasn't typed
  wait AGENT [--timeout SECS] [--full] [--turn]
                                 wait for the agent to finish, then show it;
                                 --turn stops at an observable turn end, even
                                 with background work; a provider still
                                 reporting Working keeps waiting
  answer AGENT OPTION [--force]  pick a numbered option of a pending question
  keys AGENT KEY... [--force]    press keys: enter esc tab shift-tab up down
                                 left right backspace space ctrl-c, or a character
  skill                          instructions for an orchestrating agent
  help [COMMAND]                 show general or command-specific help

AGENT is an id from `panes` (also pane<ID>), a name, project/name, or `focused`.
Never re-send text after an uncertain receipt. Follow the read-only `next` command to inspect the current pane; a pending_in_composer receipt does not authorize a later Enter.
send, answer and keys refuse the caller's own pane unless --force is explicit.
Everything after `--` is taken literally (for text that starts with -).
Input to a pane kept in tmux goes to its tmux pane through tmux, never through the tmux client; agent panes need no shell grant.
For `new` without --prompt, presets/defaults keep toolbar injection, resume and hooks; raw argv bypasses them.
ALL `new` requests require outside control AND Settings > Outside control > Outside terminal creation set to Ask or Allow.
Ask requires a click for every request. Allow covers configured non-shell terminal presets.
Literal argv and shell presets always Ask; the click card never takes keyboard focus.
Each card disables Allow and Deny for 750 ms after appearing and ignores double-clicks.
Both mouse-down and release must belong to that same armed card.
`--prompt` submits a user task through the one-shot runner path, once on first launch; only non-shell terminal presets support it.
It replaces the preset's configured system prompt with the task; preset arguments are kept.
The typed task stays exact; automatic instructions use a separate flag/config channel when available.
In Git Bash, set MSYS_NO_PATHCONV=1 for leading-slash prompts, or use --prompt - with stdin.
Its task text is discarded after the first launch and is not re-typed on a later launch.
Ask returns a pending request id immediately; the CLI polls once per second until the final reply.
Connection and host-availability failures retry up to three times against the same id; creation is never reissued.
Uncoded status errors also name the id to query with `new-status`; do not repeat `new` while the original may still wait.
`--no-wait` returns that id for scripts; poll with `new-status REQUEST_ID`. Requests expire after 120 seconds.
With --prompt the reply comes once the pane exists: its task submitted, refused, or prompt_pending with `status` as next.
`--wait-delivery` keeps reading until the task settles, for up to 90 seconds.
`--from` and the caller's MUXEL_INSTANCE_ID are declared labels, not verified identities.
All raw argv (even agent argv) and shell presets/defaults ALSO require \"Also allow typing into shell panes\".
All three grants default off; upgrades do not infer creation permission from the older grants.
Creation opt-out or outside control off refuses all pending requests. Existing panes, admitted launches and typing permissions are unchanged; toolbar creation is unaffected.
Outside control off stops the server; pollers report host_unavailable after their retries.
Commands run as the host muxel process, outside the caller's sandbox; agents/presets are not a sandbox.
Turning \"Also allow typing into shell panes\" off blocks subsequent control writes to all raw-argv panes and shell-classified panes.
Configured non-literal agents remain controllable while outside control is on. Revocation does not stop processes or undo input.
Raw argv panes restore stopped; only Run again replays their saved command.
`new` launch directories are never owned, registered or deleted by muxel.
On Windows, raw argv requires a native executable; use --preset for batch wrappers.
`new` opens terminal panes; browser presets (including a browser default) are not supported yet.
muxel must be running with Settings > Outside control > \"Allow outside tools to control muxel\" on.
To reach a muxel started with --user-data-dir DIR, run `muxel --user-data-dir DIR ctl ...`;
its own panes reach it without the flag.

Replies are JSON. A failure is {\"ok\": false, \"error\": ...}, with a \"code\" when a
script can act on it (see `muxel ctl skill`). Exit status: 0 success,
1 request failure (including no running muxel), 2 invalid command line or stdin.";

pub fn command_help(command: &str) -> Option<&'static str> {
    Some(match command {
        "projects" => "usage: muxel ctl projects\nList every project and its agent counts.",
        "panes" => {
            "usage: muxel ctl panes [PROJECT]\nShow pane layout and agents in all projects or one project."
        }
        "new" => {
            "usage: muxel ctl new PROJECT [--cwd DIR] [--name TEXT | --name-fixed TEXT] [--tab | --split right|down] [--focus] [--top-level] [--prompt TEXT [--wait-delivery]] [--from LABEL] [--no-wait] [--preset NAME] [--model MODEL] [--effort LEVEL] [-- PROGRAM [ARGS...]]\nOpen a terminal as a tab in the calling pane's group (else the focused group); --split makes a pane beside it instead. Opened from a pane, it nests under that pane's session in Recent work; --top-level opens it as a session of its own. --name sets a starting label that an agent title replaces; the label stays resolvable and is reported by status/panes. --name-fixed keeps a fixed name. --prompt - reads stdin. With --prompt the reply comes once the pane exists, with its task submitted, refused, or prompt_pending (follow next); --wait-delivery keeps reading until it settles, for up to 90 seconds. Blockers are reported as blocked_by with a screen excerpt; trust is never answered automatically. Never repeat new: its pane already exists. Never re-send text after an uncertain receipt; follow next. not_delivered means nothing was typed: resolve the blocker before sending the task to the existing pane. -- starts literal program arguments. Creation permissions apply."
        }
        "new-status" => {
            "usage: muxel ctl new-status REQUEST_ID\nPoll a pending creation by its full request UUID."
        }
        "status" => {
            "usage: muxel ctl status [AGENT]\nShow one agent's status or every agent's status."
        }
        "focus" => "usage: muxel ctl focus AGENT\nOpen an existing pane, waking it if asleep.",
        "wake" => {
            "usage: muxel ctl wake AGENT\nWake an asleep pane in the background; focus stays where it is."
        }
        "show" => {
            "usage: muxel ctl show AGENT [--full]\nShow the last prompt, reply and pending question; --full includes the whole reply."
        }
        "screen" => {
            "usage: muxel ctl screen AGENT [--lines N]\nShow raw terminal text; default 60 lines, maximum 2000."
        }
        "send" => {
            "usage: muxel ctl send AGENT TEXT [--force] [--launch-id ID] [--session-id ID] [--once KEY] [--when-idle [--detach] [--timeout SECS]]\nPaste TEXT and press Enter. '-' reads stdin; `--` makes following text literal, including -h and --help. Provider confirmation can take up to 63 seconds. Inspect outcome and follow the read-only next command. Never re-send text after an uncertain receipt. pending_in_composer describes the observed draft; it does not authorize a later Enter. not_delivered means nothing was typed: resolve the refusal before trying again. Text already in the agent's composer refuses with draft_present. --force overrides busy, draft and own-pane refusal, but never a launch mismatch. --session-id ID types only while the agent's live terminal confirms that conversation (its session_id in status), checked again just before the paste. --once KEY types TEXT at most once into this pane: a repeat returns the first result and types nothing. --when-idle waits in muxel until the agent can take the prompt (600 seconds by default, --timeout up to 3600), then sends; closing the connection cancels it before anything is typed. --detach (with --when-idle and --once KEY) answers queued at once and muxel keeps the send with nobody waiting (a day by default, --timeout up to 86400); read its outcome with once-status."
        }
        "once-status" => {
            "usage: muxel ctl once-status AGENT KEY\nShow what became of `send --once KEY` to AGENT, without typing: state unused, pending, queued (a detached send waiting), typing, done with the first reply as result, or dropped (a detached send that ended without typing, its refusal as result)."
        }
        "once-cancel" => {
            "usage: muxel ctl once-cancel AGENT KEY\nWithdraw a detached `send --once KEY` to AGENT that hasn't typed yet: it then never types, and once-status reports it dropped with code cancelled."
        }
        "wait" => {
            "usage: muxel ctl wait AGENT [--timeout SECS] [--full] [--turn]\nWait for the turn and any subagent hold to finish, or for the agent to block, exit or time out, then show its result. Shell and monitor listings do not delay completion. Subagent holds last at most three hours. --turn stops at an observable turn end even during a subagent hold; it cannot override a provider still reporting Working."
        }
        "answer" => {
            "usage: muxel ctl answer AGENT OPTION [--force]\nPick a numbered option of a pending question. --force permits your own pane."
        }
        "keys" => {
            "usage: muxel ctl keys AGENT KEY... [--force]\nPress keys such as enter, esc, tab, arrows, backspace and ctrl-c. --force permits your own pane."
        }
        "skill" => "usage: muxel ctl skill\nPrint instructions for an orchestrating agent.",
        "help" => {
            "usage: muxel ctl help [COMMAND]\nShow general or command-specific help. Use `--` before literal prompt text that starts with -."
        }
        _ => return None,
    })
}

fn unknown_command_error(command: &str) -> String {
    let suggestion = if command == "list" {
        Some("panes")
    } else {
        const VERBS: &[&str] = &[
            "projects",
            "panes",
            "new",
            "new-status",
            "status",
            "focus",
            "wake",
            "show",
            "screen",
            "send",
            "once-status",
            "once-cancel",
            "wait",
            "answer",
            "keys",
            "skill",
            "help",
        ];
        VERBS
            .iter()
            .map(|verb| (*verb, edit_distance(command, verb)))
            .min_by_key(|(_, distance)| *distance)
            .filter(|(_, distance)| *distance <= 3)
            .map(|(verb, _)| verb)
    };
    let hint = suggestion.map_or(String::new(), |verb| format!("\ndid you mean `{verb}`?"));
    format!("unknown command '{command}'{hint}\n\n{USAGE}")
}

fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (row, ch) in left.chars().enumerate() {
        let mut current = vec![row + 1; right.len() + 1];
        for (col, other) in right.iter().enumerate() {
            current[col + 1] = (previous[col + 1] + 1)
                .min(current[col] + 1)
                .min(previous[col] + usize::from(ch != *other));
        }
        previous = current;
    }
    previous[right.len()]
}

/// Parse the arguments after `muxel ctl`.
pub fn parse_cli(args: &[String]) -> Result<CliCommand, String> {
    let Some((cmd, rest)) = args.split_first() else {
        return Ok(CliCommand::Help(None));
    };
    if matches!(cmd.as_str(), "help" | "-h" | "--help") {
        return match rest {
            [] => Ok(CliCommand::Help(None)),
            [target] if command_help(target).is_some() => {
                Ok(CliCommand::Help(Some(target.clone())))
            }
            [target] => Err(unknown_command_error(target)),
            _ => Err("usage: muxel ctl help [COMMAND]".into()),
        };
    }
    if rest
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| matches!(arg.as_str(), "-h" | "--help"))
    {
        return if command_help(cmd).is_some() {
            Err(format!(
                "{cmd} was not run: use `muxel ctl help {cmd}` for help, or `--` before literal -h/--help text"
            ))
        } else {
            Err(unknown_command_error(cmd))
        };
    }
    if cmd == "new" {
        return new::parse(rest);
    }
    if cmd == "new-status" {
        return match rest {
            [id] => Ok(CliCommand::Request(Command::NewStatus {
                request_id: id
                    .parse()
                    .map_err(|_| "new-status needs a full request UUID")?,
            })),
            _ => Err("usage: muxel ctl new-status REQUEST_ID".into()),
        };
    }
    let mut words: Vec<String> = Vec::new();
    let mut full = false;
    let mut turn = false;
    let mut force = false;
    let mut launch_id: Option<Uuid> = None;
    let mut session_id: Option<String> = None;
    let mut once: Option<String> = None;
    let mut when_idle = false;
    let mut detach = false;
    let mut lines: Option<usize> = None;
    let mut timeout: Option<u64> = None;
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--" => {
                words.extend(it.by_ref().cloned());
                break;
            }
            "--full" => full = true,
            "--turn" => turn = true,
            "--force" => force = true,
            "--when-idle" => when_idle = true,
            "--detach" => detach = true,
            "--once" => {
                let key = it.next().ok_or("--once needs a KEY")?;
                if once.replace(key.clone()).is_some() {
                    return Err("--once can be given only once".into());
                }
            }
            "--launch-id" => {
                let value = it.next().ok_or("--launch-id needs an ID")?;
                launch_id = Some(Uuid::parse_str(value).map_err(|_| "--launch-id needs a UUID")?);
            }
            "--session-id" => {
                let id = it.next().ok_or("--session-id needs an ID")?;
                if session_id.replace(id.clone()).is_some() {
                    return Err("--session-id can be given only once".into());
                }
            }
            "--lines" => lines = Some(number(it.next(), "--lines")?),
            "--timeout" => timeout = Some(number(it.next(), "--timeout")?),
            flag if flag.starts_with("--") && flag.len() > 2 => {
                return Err(format!("unknown option {flag}"));
            }
            _ => words.push(arg.clone()),
        }
    }
    if launch_id.is_some() && cmd != "send" {
        return Err("--launch-id is only valid with send".into());
    }
    if session_id.is_some() && cmd != "send" {
        return Err("--session-id is only valid with send".into());
    }
    if (once.is_some() || when_idle || detach) && cmd != "send" {
        return Err("--once, --when-idle and --detach are only valid with send".into());
    }
    let agent = |words: &[String]| -> Result<String, String> {
        words
            .first()
            .cloned()
            .ok_or_else(|| format!("`{cmd}` needs an AGENT (an id from `muxel ctl panes`)"))
    };
    let at_most = |n: usize| -> Result<(), String> {
        if words.len() > n {
            Err(format!("unexpected argument '{}'", words[n]))
        } else {
            Ok(())
        }
    };
    let command = match cmd.as_str() {
        "projects" => {
            at_most(0)?;
            Command::Projects
        }
        "panes" => {
            at_most(1)?;
            Command::Panes {
                project: words.first().cloned(),
            }
        }
        "status" => {
            at_most(1)?;
            Command::Status {
                agent: words.first().cloned(),
            }
        }
        "focus" => {
            at_most(1)?;
            Command::Focus {
                agent: agent(&words)?,
            }
        }
        "wake" => {
            at_most(1)?;
            Command::Wake {
                agent: agent(&words)?,
            }
        }
        "show" => {
            at_most(1)?;
            Command::Show {
                agent: agent(&words)?,
                full,
            }
        }
        "screen" => {
            at_most(1)?;
            Command::Screen {
                agent: agent(&words)?,
                lines,
            }
        }
        "send" => {
            let agent = agent(&words)?;
            let text = words[1..].join(" ");
            if text.is_empty() {
                return Err("`send` needs the TEXT to type ('-' reads it from stdin)".into());
            }
            let send = Command::Send {
                agent,
                text,
                force,
                launch_id,
                session_id,
                once,
                when_idle,
                detach,
                timeout_secs: timeout,
            };
            check_safe_send(&send)?;
            send
        }
        "once-status" | "once-cancel" => {
            at_most(2)?;
            let agent = agent(&words)?;
            let key = words
                .get(1)
                .cloned()
                .ok_or_else(|| format!("`{cmd}` needs the KEY a `send --once` used"))?;
            if !valid_once_key(&key) {
                return Err("--once KEY must be 1-128 bytes without control characters".into());
            }
            if cmd == "once-status" {
                Command::OnceStatus { agent, key }
            } else {
                Command::OnceCancel { agent, key }
            }
        }
        "answer" => {
            at_most(2)?;
            let agent = agent(&words)?;
            let option = words
                .get(1)
                .cloned()
                .ok_or("`answer` needs the OPTION number to pick")?;
            Command::Answer {
                agent,
                option,
                force,
            }
        }
        "keys" => {
            let agent = agent(&words)?;
            let keys = words[1..].to_vec();
            if keys.is_empty() {
                return Err("`keys` needs at least one KEY".into());
            }
            if let Some(bad) = keys.iter().find(|k| key_bytes(k, false).is_none()) {
                return Err(format!("unknown key '{bad}'"));
            }
            Command::Keys { agent, keys, force }
        }
        "wait" => {
            at_most(1)?;
            return Ok(CliCommand::Wait {
                agent: agent(&words)?,
                timeout_secs: timeout.unwrap_or(WAIT_TIMEOUT_SECS),
                full,
                turn,
            });
        }
        "skill" => return Ok(CliCommand::Skill),
        other => return Err(unknown_command_error(other)),
    };
    Ok(CliCommand::Request(command))
}

fn number<T: std::str::FromStr>(value: Option<&String>, flag: &str) -> Result<T, String> {
    value
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("{flag} needs a number"))
}

// --- Instructions for an orchestrating agent -----------------------------------------

/// The skill an orchestrating agent (Grok Bot, …) is given so it can drive muxel:
/// how to find muxel on whichever computer it is working on, the commands, what the
/// statuses mean, and the rules for using them. `exe` is the muxel binary on the
/// computer it was made on — the first place to look, not the only one, since one
/// skill may serve agents on several computers.
pub fn skill(exe: &str) -> String {
    SKILL.replace("MUXEL_PATH", &crate::ssh::sh_quote(exe))
}

/// `markdown` with its hard line breaks inside paragraphs and list items joined,
/// for showing in a box of any width: text wrapped at 80 columns and shown in a
/// narrower one breaks raggedly, a short line after every long one. Headings, list
/// items, rules, front matter and indented code keep their own lines.
pub fn reflow_for_display(markdown: &str) -> String {
    #[derive(PartialEq)]
    enum Last {
        Blank,
        Text,
        Fixed,
        Code,
    }
    let mut out: Vec<String> = Vec::new();
    let mut last = Last::Blank;
    let mut front_matter = false;
    for (i, line) in markdown.lines().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        let starts_item = trimmed.starts_with("- ")
            || trimmed.starts_with("* ")
            || trimmed
                .split_once(". ")
                .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if trimmed.is_empty() {
            out.push(String::new());
            last = Last::Blank;
        } else if trimmed == "---" {
            // Front matter opens with a rule on the first line and ends at the next.
            front_matter = i == 0;
            out.push(line.to_string());
            last = Last::Fixed;
        } else if front_matter || trimmed.starts_with('#') {
            out.push(line.to_string());
            last = Last::Fixed;
        } else if indent >= 4 && matches!(last, Last::Blank | Last::Code) {
            out.push(line.to_string());
            last = Last::Code;
        } else if last == Last::Text && !starts_item {
            let joined = out.last_mut().expect("text follows a line");
            joined.push(' ');
            joined.push_str(trimmed);
        } else {
            out.push(line.to_string());
            last = Last::Text;
        }
    }
    out.join("\n")
}

const SKILL: &str = r#"---
name: muxel
description: Drive the coding agents running in muxel on this computer - list its projects, panes and agents, see which are working, done or waiting on a question, read an agent's last prompt and reply, send it a prompt, and answer its questions.
---

# muxel

muxel is a desktop app on the user's computer that runs coding agents (Claude,
Codex, opencode, Grok, ...) and shells side by side, grouped into projects. You
control it with its `ctl` command, run on the **local computer** (not a cloud
computer). The user may run muxel on several computers; each one's `muxel ctl`
only reaches the muxel on that computer.

## Find muxel on this computer

Do this once on each computer. Below, `muxel` means the command you found.

1. `MUXEL_PATH` - where muxel was when these instructions were made. Use it if
   it exists on this computer.
2. Otherwise read `exe` from muxel's control file, which muxel writes while it is
   running with outside control on:
   - macOS: `~/Library/Application Support/dev.muxel.muxel/control.json`
   - Linux: `$XDG_DATA_HOME/muxel/control.json` (usually
     `~/.local/share/muxel/control.json`)
   - Windows: `%APPDATA%\muxel\muxel\data\control.json`
3. Otherwise look where it is usually installed:
   - Linux, from the .deb or .rpm package: `/usr/bin/muxel` (on the PATH as
     `muxel`).
   - Linux AppImage: the `.AppImage` file itself, wherever the user saved it
     (often `~/Applications` or `~/Downloads`, named like
     `muxel-linux-x86_64.AppImage`). Run it like any command:
     `~/Applications/muxel-linux-x86_64.AppImage ctl projects`.
   - Linux tarball or install script: `~/.local/bin/muxel` or
     `/usr/local/bin/muxel`.
   - macOS: `/Applications/muxel.app/Contents/MacOS/muxel`.
   - Anywhere: `muxel` on the PATH.

Then run `muxel ctl projects`. Every reply includes `host`: the name of the
computer whose muxel answered. If you work with more than one computer, check
it's the one you meant.

If muxel won't run:

- An AppImage that says it needs FUSE: put `APPIMAGE_EXTRACT_AND_RUN=1` in front
  of the command (slower, but works without FUSE).
- `error while loading shared libraries` or `symbol lookup error`: the library
  path you run commands with (often set when you are an AppImage yourself) doesn't
  suit muxel. Run it as `env -u LD_LIBRARY_PATH muxel ctl ...`.
- It says muxel isn't running when the user says it is: your commands may see a
  different home directory than muxel does. Find its control file (step 2) and
  put `MUXEL_CONTROL=/path/to/control.json` in front of the command.

Every command prints JSON. A failure exits non-zero and prints
`{"ok": false, "error": "..."}` - the error says what went wrong and what to do,
and a `code` is added when there is something to act on (see Errors below). If
it says muxel isn't running or outside control is off, ask the user to open
muxel and turn on Settings > Outside control > "Allow outside tools to control muxel".

## Commands

- `muxel ctl projects` - every project: id, name, folder, git branch, and how
  many agents are working / blocked / done / idle.
- `muxel ctl panes [PROJECT]` - the pane layout and every agent in it: id, name,
  program, model, status, which pane it's in, and which one the user has focused.
- `muxel ctl focus AGENT` - deliberately select and raise an existing pane,
  waking it through normal restore if asleep. This changes keyboard focus.
- `muxel ctl wake AGENT` - wake an asleep pane in the background on
  its saved conversation, leaving focus, the window and the active tab alone.
  Replies `{woken: true, status: "starting"}`, or `{woken: false,
  already_running: true, status}` when it already runs, so a retry is harmless.
- `muxel ctl new PROJECT [--cwd DIR] [--name TEXT | --name-fixed TEXT] [--tab | --split right|down]
  [--focus] [--top-level] [--preset NAME] [--model MODEL] [--effort LEVEL] [-- PROGRAM [ARGS...]]` - open a terminal as a
  new tab: in your own pane's group when you run it from one of that project's
  panes, otherwise in the group the user has focused. `--split right|down` makes
  a new pane beside that group instead. PROJECT resolves as for `panes`.
  `--name` supplies a starting label. A useful agent title received after launch
  replaces it on screen. The label remains a resolver alias and a `label` field
  in `panes`/`status`; the row tooltip keeps it visible. `--name-fixed` instead
  keeps a sticky name, like an inline rename. The two options are exclusive.
  Run from one of the project's panes, the new session nests under your session
  in the sidebar's Recent work order, so the user sees it as your work; a
  question it asks lifts your row with it. Pass `--top-level` for a session that
  is independent work. Nesting never closes it with you.
  DIR must be an existing absolute directory; it defaults to the project root
  and may be a linked worktree or another directory. A preset matches its name
  or id; omitting both launch choices uses the toolbar's preset for that project.
  `--model` and `--effort` override the preset's values for this pane, including
  prompt launches; restarts retain the chosen arguments. Values must contain
  1–64 characters from `[A-Za-z0-9._:/@+-]` with no leading `-`. Each override
  requires its Model flag / Effort flag in Settings > Agents, and that flag must
  be absent from Extra args. A flag containing `{}` emits one argument with the
  value substituted; other flags emit flag then value. Existing presets are not
  migrated; fill in their flag fields. Literal `-- PROGRAM` cannot use overrides.
  New Codex presets use `--config=model_reasoning_effort={}`; existing `-c` or
  `--config` arguments setting that key also conflict with an effort override.
  Unsupported combinations receive `parameter_unsupported`. Parameterized
  presets remain eligible for Allow; Ask shows the requested model and effort.
  Everything after `--` is literal argv, including spaces, quotes and empty
  arguments. `--prompt TEXT` sends a user task through the existing runner path;
  `--prompt -` reads stdin. In Git Bash, set `MSYS_NO_PATHCONV=1` for leading-slash
  prompts, or use stdin. This replaces the preset's configured system prompt
  with the task; preset arguments are kept. The typed task stays exact.
  Automatic instructions use the preset's separate flag/config transport when
  available; they are omitted for TypeIn-only presets. The task is submitted
  once and its text is discarded after first launch. Restart, restore and resume neither
  re-type nor resubmit it, including launches without a resumable conversation.
  Prompts for literal, shell and browser launches receive
  `prompt_unsupported`. Creation preserves the selected tab, focus, project and
  window by default, without raising the window. `--focus` selects the new pane
  and activates its window. `--background` is accepted as a compatibility no-op,
  including alongside `--focus`. JSON callers use `focus` (default false);
  the legacy `background` field remains accepted and does not control focus.
  Every `new` request requires outside control plus Settings > Outside control >
  Outside terminal creation set to Ask or Allow. Ask requires a click for every
  request; Allow permits configured non-shell terminal presets. All literal argv,
  even an agent
  executable, and shell presets/defaults additionally require "Also allow typing
  into shell panes". Configured non-literal presets not classified as shells need
  only the first two grants. Literal argv and shell presets always Ask, even
  under Allow. The existing shell/non-shell classifier is a soft
  policy for trusted configured presets, not an executable allowlist.
  All three permissions default off; upgrades leave creation off even if both
  older grants are on. Ask the user to enable the required settings themselves.
  Ask returns `{ "pending": true, "request_id": "UUID" }` immediately, without
  holding a connection. The CLI polls every second and prints the final reply.
  Temporary connection and host-availability failures retry up to three times
  against that same UUID; a successful pending reply resets the retry count.
  The CLI never reissues `new`. If retries fail, its error names the UUID to
  query with `new-status`. Uncoded status errors, including an older host's
  uncoded timeout, also give that advice. Do not repeat `new` while the original
  may still wait.
  `--no-wait` prints the pending reply for scripts; use `ctl new-status UUID` to
  poll it. Requests expire 120 seconds after admission, including time behind
  another card. Each visible card disables both buttons for 750 ms and ignores
  double-clicks. Mouse-down and release must belong to the same armed card.
  The card offers click-only Allow once / Deny, keeps terminal
  keyboard focus, and shows the waiting count in FIFO order. `--from LABEL` and
  the caller's MUXEL_INSTANCE_ID are displayed as declared information, not
  authenticated identity (the token is the whole authorization).
  Final replies remain available for ten minutes, bounded to the latest 256;
  up to 64 approvals may wait. Approvals are runtime-only and do not survive a
  host restart. Plain creation and status requests use protocol v2; creation with
  model or effort uses v3 so older hosts cannot silently discard the choices.
  The host requires the exact command version, including for JSON callers.
  Turning creation or outside control off refuses all pending requests. Outside
  control off also stops the server, so pollers receive `host_unavailable`
  after their connection retries rather than the recorded `control_disabled`.
  Revocation leaves
  existing panes, already admitted launches and
  typing permissions unchanged. Deferred requests recheck current permission;
  human toolbar creation is unaffected.
  Commands run as the host muxel process with its permissions and environment,
  outside the caller's sandbox. Coding agents and configurable presets are not
  a security sandbox. Ask the user to grant authority; do not change settings.
  Turning "Also allow typing into shell panes" off also refuses later control
  writes to every raw argv pane, even agent-looking or interpreter programs, and
  shell-classified panes. Configured non-literal agents remain controllable while
  outside control is enabled. Disabling all outside control is a separate setting. Revocation
  does not stop the process or undo input already delivered; queued Enter/keys
  recheck the shell-input grant and terminal/session availability before each write.
  The JSON reply contains `id` (short), `uuid` (full), `project`, `project_id`,
  `cwd`, `program` and `args`. Without `--prompt`, presets and defaults keep toolbar
  system-prompt injection (including TypeIn), resume and hooks; raw argv bypasses those and tmux.
  Raw argv panes restore stopped. Project selection, Restart and wake never replay
  their opening brief; the pane's explicit Run again action reruns the saved argv.
  DIR is a launch directory, never an owned worktree; closing the pane does not
  remove it and worktree migration does not register it.
  On Windows raw argv requires a native executable; use `--preset` for batch
  wrappers. Installed Codex resolves to its packaged native executable.
  Remote projects are currently refused.
  `new` opens terminal panes; browser presets, including a browser default, are
  not supported yet and receive a refusal.
- `muxel ctl status [AGENT]` - one agent's status, or every agent's. Terminal
  entries in `status`, `panes` and `show.agent` include `source`, optional
  current `activity`, and `blocked_on` (`permission`, `question`, or `auth`)
  when a program report identifies why it is blocked. `source` describes the
  current status evidence: `program`, `hook`, `title`, `screen`, `bell`,
  `output`, `quiet`, or `process`. `settling` means the evidence cannot yet be
  attributed to this conversation; its status stays `working`.
  `status` matches the sidebar, including a completed turn held over Idle.
  When that badge differs from live facts, `live_status` carries the state used
  by `wait` and send admission. Attending a completed pane marks its notification
  read without erasing Done. `activity` is absent when no current source supplies it.
- `muxel ctl show AGENT [--full]` - the agent's last prompt, its last reply (the
  final message; `--full` for everything it wrote that turn), and, when it is
  blocked, the `question` it's asking with its numbered `options`. `controller`
  says which computer's muxel last typed into the agent through `muxel ctl`
  (`this_muxel` is true when it was this one) and whether that turn is still open.
- `muxel ctl screen AGENT [--lines N]` - the raw text of the agent's terminal.
  Use it when `show` doesn't explain what's on screen.
- `muxel ctl send AGENT TEXT` - type TEXT into the agent and press Enter. Refused
  while the agent is working, blocked, or still in the grace after a submitted
  control turn, while muxel on another computer is mid-turn with it, or while
  another `send` to it from this muxel hasn't pressed Enter yet. For long
  or multi-line text, pass `-` and pipe the text on stdin. Use `--` before
  literal prompt text that starts with `-`; `-h` and `--help` before `--` refuse
  with exit 2 and are never typed. Use `muxel ctl help COMMAND` for help.
  The reply waits for Enter. If keys, pastes, mouse or wheel reports reach the
  pane after the paste, Enter is withheld and `send` fails with the prompt left
  in its input. A closed or restarted terminal also withholds Enter. Check
  `screen` before sending again. Focus reports, terminal replies and local scrollback do not
  count; input from another tmux client is outside this local guard.
  Add `--launch-id ID` with the full ID from `panes` or `status` to refuse a replaced
  terminal. This guard is checked again immediately before pasting after any
  asynchronous claim work, and `--force` never bypasses it. A successful reply
  includes the actual `launch_id` and `"pasted": true` for admission to the PTY
  writer (for an agent kept in tmux: tmux pasted it into the agent's tmux pane),
  not confirmation of agent delivery. A lost reply does not make a
  retry safe; inspect the agent before typing again. A failure with `pasted: true`
  means the prompt may have reached the agent, in whole or in part; a paste tmux
  refused typed nothing and has no `pasted`. Check `screen` before
  retrying. A `working` error with `reason: just_submitted` means the prior
  control turn is still in its grace on a pane with provider status or configured
  screen markers; `wait` for its reply. Activity-only panes allow a later send.
  Codex, Claude and Grok sends also observe provider acceptance for up to sixty
  seconds (three more after a retry). `submission_confirmed` is true only for a fresh provider receipt and
  false when acceptance remains unknown, with `unconfirmed_reason` `timed_out`,
  `input_changed` (other input arrived; no further Enter is sent),
  `interrupted` (the pane closed, restarted, switched conversation or lost
  permission) or `retry_unanswered` (tmux didn't answer whether it pressed the
  retry Enter, so it may have arrived). Once Enter is in, the reply is this
  receipt, never an error.
  It is null, without waiting, when the pane can't show a receipt: an
  unsupported provider, or no readable idle status title (tmux hides titles)
  with a Grok, multi-paragraph or repeated prompt.
  `enter_retries` reports zero or one additional Enter. Windows Codex alone may
  receive that retry after five seconds while its exact, fully visible ASCII
  prose remains in the idle composer and no local input intervened. Clipped,
  multiline, Unicode or completion drafts get no retry. Text is never pasted
  twice. Retry also requires a compact composer frame from before the paste
  and an added blank cursor line proving that the first Enter was consumed;
  a pending Enter never authorizes another.
  An unconfirmed receipt requires inspecting `screen` before more input.
  `outcome` is `submitted` only with provider acceptance, `pending_in_composer`
  only with the complete retained draft and current guards, or `not_delivered`
  when input was refused before delivery. `unconfirmed` preserves uncertainty,
  including clipped or collapsed drafts. Follow the exact command in `next`.
  Never re-send the text. `next` inspects the current pane; a pending receipt
  cannot guard a later Enter after the draft or launch changes. Switch and
  input protection covers muxel-observed changes; another tmux client's input
  is untracked.
  Native Windows Codex launches disable heuristic paste bursts only after a
  version probe verifies the matching codec. Configured Codex 0.167+ preserves
  line breaks and Unicode; CRLF becomes one newline. Unsupported controller
  characters, including tabs, are refused before typing. Its clipboard encoder
  also refuses unsupported controls before writing and reports the refusal.
  Text in the agent's composer refuses `send` with `draft_present` and nothing
  is typed: a person's half-typed prompt, or a prompt an earlier send left
  there. Claude and Codex are judged by their drawn composer (placeholder
  suggestions don't count); other agents by whether text was typed into the
  pane since its last Enter. Escape, arrows, ctrl keys and `answer` don't count.
  `--force` types anyway.
  For automation, use `send AGENT TEXT --when-idle --once KEY --launch-id ID
  --session-id SID`.
  `--session-id SID` pins the send to a provider conversation: the agent's
  `session_id` from `status`. It types only while the running terminal's live
  binding confirms that conversation, checked when the send arrives,
  throughout a `--when-idle` wait, and again immediately before the paste.
  Another conversation fails with `launch_mismatch`, `reason:
  conversation_changed`. A conversation muxel can't confirm yet (the agent
  is switching, or names none) fails with `reason: conversation_unconfirmed`;
  `--when-idle` waits through it instead. Either way nothing is typed. A pane
  that isn't running is matched on the id it will resume, then checked again
  once it runs. muxel learns of a Claude `/clear` or `/resume` from its hook a
  moment after it happens, so the pin holds against muxel's latest
  knowledge. `--force` never overrides it. It needs protocol v5: check
  `protocol` in `status` first. An agent muxel types into through tmux
  refuses it with `parameter_unsupported`, `reason: tmux_delivery`, and types
  nothing: there the paste waits in tmux's input queue after the last check,
  so muxel can't hold it to a conversation. Send to that agent without it.
  `--once KEY` (1-128 bytes) types TEXT into this pane at most once. A repeat
  whose first send went in returns that first receipt with `duplicate: true`
  and types nothing. A repeat whose first send failed after typing fails with
  `duplicate`, the first failure under `original`, and its `pasted`. A first
  send refused before typing leaves the key unused, so the same key can try
  again. A repeat while the first is still going fails with `busy`, `reason:
  once_pending` (and `pasted: true` once it has started typing). A repeat is
  answered before anything about the pane is checked (closing, own pane,
  `--launch-id`, status, draft). Each pane keeps its last 64 keys while muxel
  runs; a restart forgets them.
  `--when-idle` waits in muxel until the agent can take the prompt: not
  working, blocked or starting, no reply due to a control turn, no other send
  in flight, no draft, and for an agent kept in tmux no other attached tmux
  client. When someone else submits while it waits (a person pressing Enter
  on the draft it waited behind), it also waits for that turn to show: a
  send holding a conversation until the turn starts, since a `/clear` starts
  none and muxel hears of the new conversation a moment later; one holding
  none for up to three seconds. Then it sends with every check above, and
  the reply adds `waited_ms`. It waits 600 seconds by default, `--timeout SECS` up to 3600,
  then fails with `timed_out` and `reason` naming what it was still waiting on
  (`working`, `blocked`, `starting`, `just_submitted`, `send_in_flight`,
  `draft_present`, `tmux_clients` or `conversation_unconfirmed`). The agent
  exiting, closing or being replaced ends the wait with that refusal; a
  conversation change fails with `launch_mismatch`, `reason:
  conversation_changed`. One send waits per pane
  (`busy`, `reason: queued`), and at most eight per muxel (`reason:
  queue_full`). Closing the connection cancels a waiting send before it types;
  once typing starts the send finishes, and its `--once` key records the result.
  `--when-idle` and `--force` don't combine. These options need protocol v4.
  `--detach` (with `--when-idle` and `--once KEY`) lets the caller go: muxel
  checks the send as above, answers at once with `{"queued": true, "agent",
  "uuid", "once", "expires_at"}` (Unix milliseconds), and keeps it with nobody
  waiting. It waits and types exactly as `--when-idle` does: at a turn end,
  never into a working turn or a question, held by a draft until it clears or
  its turn ends, its conversation and launch checked throughout and again just
  before the paste. It waits a day by default, `--timeout SECS` up to 86400.
  Read its outcome with `once-status KEY`; a repeat while it waits or types
  returns the same reply with `duplicate: true`. Anything that ends it untyped
  (a conversation change, the agent replaced, exiting, closing or put to
  sleep, the timeout, a workspace switch) leaves it `dropped` with that
  refusal. A closed pane takes its sends with it, and muxel exiting drops
  them all: after a restart `once-status` answers `unused`. Detached sends
  don't count toward the eight per muxel, but still take their pane's one
  place. A pane that isn't running refuses it: wake it first. It needs
  protocol v6.
- `muxel ctl once-status AGENT KEY` - what became of `send --once KEY` to the
  agent, typing nothing: `state` is `unused` (no send with it typed, so it can
  be sent), `pending` (waiting or about to type), `queued` (a detached send
  waiting, with `waiting_on` while it waits for the pane and its
  `expires_at`), `typing` (it may have typed and hasn't finished), `done`, with
  `result` holding its first reply exactly as `send` gave it, or `dropped` (a
  detached send that ended without typing; `result` holds its refusal, and the
  key can be sent again). Use it to settle a send whose reply was lost.
- `muxel ctl once-cancel AGENT KEY` - withdraw a detached send that hasn't
  typed: `{"cancelled": true}`, after which it never types and `once-status`
  reports `dropped` with code `cancelled`. One already typing or finished,
  or an attached send, answers `{"cancelled": false}` with its `once-status`
  `state` (and `result`). Protocol v6.
- `muxel ctl wait AGENT [--timeout SECS] [--full] [--turn]` - wait until the agent's
  turn and any subagent hold have finished (or it blocks on a question, or exits),
  then print what `show` prints. Shell and monitor listings do not delay completion;
  they remain visible in `wait.background`. Subagents hold a finished turn for at
  most three hours. `--turn` stops at an observable foreground turn end instead,
  even during a subagent hold; it cannot override a provider still reporting
  Working. Either way `wait.background` lists anything still running.
  Startup tasks stay pending until fresh provider-owned acceptance, not merely
  our Enter. Codex may receive one extra Enter only while its exact task remains
  on one exact ASCII row in its live idle editor, with no new user row. Wrapped
  or ambiguous drafts get no retry; task text is never pasted again. A missing acceptance
  within sixty seconds, failed readiness or unsupported provider sets
  `agent.startup_error` in show (or `startup_error` in status). Inspect the pane
  before sending another task; do not repeat creation. Muse and other providers
  without a verified startup protocol receive no task input.
  The `wait.outcome` values are finished, blocked, exited, not_running,
  submission_failed or timed_out. Default
  timeout 600 seconds.
- `muxel ctl answer AGENT OPTION` - pick numbered option OPTION of the question
  the agent is blocked on.
- `muxel ctl keys AGENT KEY...` - press keys: enter, esc, tab, shift-tab, up, down,
  left, right, backspace, space, ctrl-c, or any single character.

For an agent kept in tmux, `send`, `answer`, `keys` and a `new --prompt` task
go to the agent's own tmux pane through tmux (`paste-buffer`, `send-keys`),
never through the tmux client muxel shows, so tmux's prefix and key bindings
never act on them, and agent panes need no shell grant. The pane is the agent's
own, the one its tmux session was created with, whichever pane or window the
user has opened or made active. A prompt arrives as tmux's own `paste-buffer -p`
pastes: line breaks as CR, without ESC. The reply to `keys` comes once tmux
has taken every key, one tmux command per key; over SSH without a shared
connection each is a new login. A `keys` or `answer` failure once a key may
have gone in has `pasted: true`. If the agent's pane is gone or can't be
reached, input is refused with `tmux_pane_unresolved` and nothing is typed;
after a failed lookup the next waits a little longer each time, up to two
minutes. A remote agent's input needs muxel's background SSH commands to that
host to work.

Newly created agents record their pane id before the program starts. That
record survives pane moves and splits; reattaches preserve it. Older or adopted
sessions use the lowest-pane lookup. Copy mode refuses input with
`tmux_pane_in_mode`, leaving the user's scroll position intact. Leave copy mode
before retrying. If the prompt was already pasted before Enter was refused,
the failure has `pasted: true`: inspect `screen` before resending. An unanswered
Enter also guards the turn as just submitted, because it may have landed.
Servers reporting version 2.4+ check mode and write in one tmux command queue.
Older or unreported server versions check separately before each write, adding
a round trip. That legacy path has a small race if the user enters copy mode
between the check and Enter; stock 2.2/2.3 use it too (server version reporting
arrived in 2.4). There is no new tmux version requirement.

AGENT is an agent's `id` from `panes` (like `1a2b3c4d`, also `pane1a2b3c4d`), its name (`Claude`),
`project/name` when names repeat, or `focused` for the pane the user is looking
at. PROJECT is a project's id or name. muxels on different computers that share
a project usually show its agents under the same ids, but each command only
reaches the muxel on the computer it runs on.

If you are running in a muxel pane yourself, the environment variable
`MUXEL_INSTANCE_ID` holds your own pane's `uuid`, which works as AGENT. `send`,
`answer`, and `keys` refuse that pane unless you pass `--force`. Older muxel
hosts ignore this caller ID, so check the target when driving one.

## Statuses

- `working` - generating or running tools. Don't type into it; `wait` for it.
- `blocked` - waiting on a permission or choice prompt; `show` has the `question`.
- `done` - finished its turn; the reply is ready to read.
- `idle` - alive and waiting for a prompt.
- `starting` - launching. `exited` - the program ended. `failed` - it couldn't
  start. `not_running` - the pane hasn't been started this session; selecting it
  in muxel only shows it, so the user wakes it there (Enter, typing or Wake) or
  you call `focus` or `wake`.

Old local sessions may open asleep, and idle ones may be put to sleep: `status`
and `panes` report `status: "not_running"` with `not_running_reason: "dormant"`
(asleep since launch) or `"sleeping"` (put to sleep by the user or the idle
timer). `show` includes this reason; `screen`, `send`, `keys`, and `answer`
refuse with `not_running` and that `reason`, even with `--force`. `focus` opens
and resumes the session; `wake` resumes it without focusing it, then `status`
moves from `starting` to idle with a new `launch_id`.
For an asleep agent, `status` and `panes` also report `resumes`,
checked on disk when you ask: `true` when a wake brings its conversation back,
`false` when it will start a new one (the provider deleted the saved
conversation, or muxel never captured one). It is left out when muxel can't tell
(Grok, whose files muxel can't locate) and while the agent runs.
An approved `new --focus` also leaves other asleep sessions alone.

Each agent also has a `launch_id`, which changes whenever muxel opens a new
terminal for it: when its program starts or restarts, when muxel reconnects to
it, and when the user reopens its workspace. A different one means the agent may
have been restarted since you last looked (one kept in tmux can still be in the
same conversation), so check `show` before relying on what you saw. It stays the
same when the agent switches conversation by itself, as with Claude's `/resume`,
and is left out while the agent is starting and once it has exited.

`session_id` is the provider's own id for the conversation the agent is in.
For a running agent it appears only once the live terminal confirms it: the
launch opened it, muxel adopted it since the last switch, or the provider's
current title names it. While the agent switches conversation (`/clear`,
`/resume`, a new Codex thread) it is left out until muxel confirms the new
one. A pane that isn't running shows the id it will resume. Shells and agents
that haven't named a conversation have none. Pass it to `send --session-id`
to pin a send to that conversation.

`status` and `panes` also report `protocol`, the newest control protocol
version this muxel accepts. An older muxel leaves it out.

## Errors

Creation permission/approval refusals, typing refusals from `send`, `answer` or
`keys`, and host connection failures have a `code`:

- `working`, `blocked`, `starting` - not now: `wait`, answer its question, or try
  again in a moment.
- `not_running`, `exited` - the agent isn't running; the user has to open it.
- `claimed_elsewhere` - another request holds the shared tmux claim, or another
  `send` to the agent hasn't pressed Enter yet.
- `closing` - input is refused while a pane is held for close. Reads (`status`, `show`, `screen`, `wait`) still work by id/name. `undo_until` is the countdown deadline (unix ms); once that clock deadline expires it is omitted and `reason` is `counting_down`, `checking`, `awaiting_decision` or `restoring`. Use Keep or Undo in muxel, or wait for restoration, before sending input. If the pane closes while `wait` polls or collects its result, it returns `exited` with an explanation instead of waiting indefinitely.
- `launch_mismatch` - the terminal was replaced since the supplied `launch_id`,
  or, with `reason: conversation_changed`, the pane isn't in the conversation
  the send was pinned to, or, with `reason: conversation_unconfirmed`, muxel
  can't confirm which conversation it is in yet. Nothing was typed; check
  `status`.
- `input_changed` - the prompt was pasted but Enter was withheld. Inspect
  `screen` before retrying; the input is still there.
- `unsupported_input` - the native provider cannot insert the supplied control
  characters or its multiline protocol could not be verified. Nothing was typed.
- `tmux_pane_unresolved` - the agent's tmux pane, which takes its input, is
  gone or couldn't be found; nothing was typed. Check `screen`; the user may
  need to restart the agent.
- `keys_partly_pressed` - some keys were pressed, then the rest failed. Inspect
  `screen` before pressing any again.
- `tmux_pane_in_mode` - the pane is in copy mode; nothing was typed. Leave
  copy mode before retrying. A failure after a paste instead has `pasted: true`.
- `tmux_refused` - tmux refused the input or couldn't be run; nothing was
  typed. Check `screen` before retrying.
- `self_target` - this command would type into its caller's own pane; select a
  different target or pass `--force` deliberately.
- `draft_present` - the agent's composer holds text; nothing was typed. Leave
  it to the user, or pass `--force` deliberately.
- `busy` - something is already in progress for this pane (`reason` `queued`,
  `queue_full` or `once_pending`); this request typed nothing.
- `duplicate` - this `--once` key's first send failed after typing; `original`
  holds that failure. Nothing was typed now; inspect `screen`.
- `timed_out` - `send --when-idle` saw no safe moment in time; `reason` says
  what it waited on. Nothing was typed.
- `cancelled` - a waiting send was withdrawn before it typed: `once-cancel`
  withdrew a detached one (its `once-status` result). Nothing was typed.
- `host_unavailable`, `stale_endpoint` - no running muxel to ask.
- `control_disabled` - outside control was turned off before the mutation.
- `creation_disabled` - Outside terminal creation is Off; the user must choose Ask or Allow.
- `command_permission_required` - literal argv or a shell launch needs the shell/command grant.
- `approval_denied` - the user clicked Deny; nothing was created.
- `approval_timeout` - no approval arrived within 120 seconds; nothing was created.
- `prompt_unsupported` - prompts require a configured non-shell terminal preset.
- `parameter_unsupported` - model/effort require a configured preset flag absent from Extra args; literal commands cannot use them. Also `send --session-id` to an agent behind tmux (`reason: tmux_delivery`); nothing was typed.

A failure without a code after the request went out (no answer came back) may or
may not have been carried out: check `status`, `show` or `screen` before typing
anything again. After a paste, pane closure, restart or a writer failure can
also withhold Enter and return an error without a code; inspect the input first.

## How to work

1. Run `panes` to find the agent. Use one whose status is `idle` or `done`.
2. `send` the prompt, then `wait` on the same agent. `wait` returns when the turn
   ends and includes the reply, so you don't need to poll.
3. If `wait` comes back `blocked`, read `question`. Permission prompts (run a
   command, edit a file, fetch a URL) are the user's decision: approve only what
   the user has told you to approve; otherwise tell the user what the agent is
   asking and let them choose. Answer with `answer AGENT N`, then `wait` again.
   If it's still blocked afterwards, check `screen` and use `keys` (often
   `enter`) to confirm.
4. If the reply ends with a question for the user (status `done`, no `question`),
   pass it on to the user, or answer it with `send` if you've been told how.
5. If `wait` times out, the agent is still working: `wait` again, or report
   progress to the user.

## Rules

- One prompt at a time per agent. Never `send` to an agent that is `working` or
  `blocked`.
- Never re-send prompt text after a pasted or uncertain receipt. Follow the
  read-only `next` command to inspect the current screen. `pending_in_composer`
  describes an observed draft; it does not guard or authorize a later Enter.
  `not_delivered` means nothing was typed; resolve its refusal before
  sending the task. Never repeat `new`; use its existing pane. `new --prompt`
  replies once the pane exists, with the task `submitted`, refused, or still
  `prompt_pending` with `status` as `next`; `--wait-delivery` reads until it
  settles, for up to 90 seconds. `--no-wait` replies before approval, with
  `new-status` as `next`. A failed `send` or `new --prompt` has an `outcome`
  too: `not_delivered` only for a coded refusal before any input, otherwise
  `unconfirmed`, as when `new`'s approval poll is lost while the task can
  still arrive. Other commands' failures carry no outcome. `status` and
  `show.agent` include `prompt_delivery`. A folder trust, update or login blocker
  reports `blocked_by` and `screen_excerpt`; leave trust decisions to the user.
- Only type into shell panes (`"is_agent": false`) when the user asks; muxel
  refuses unless the user has also allowed it in its settings.
- To stop an agent mid-turn press `esc` (`keys AGENT esc`). Don't send `ctrl-c`
  to an agent unless asked - twice exits it.
- An agent can be shared with muxel on another computer (a remote project opened
  on both, or one the other attached to). If `send` or `answer` says muxel on
  another computer is using the agent, don't `--force` it: `wait` on the agent
  and try again, or tell the user. Several of you may be driving the same agents.
- Create panes only when asked, using `new` within the user's granted authority.
  This interface can't close or rearrange existing panes; the layout is the user's.
"#;

#[cfg(test)]
mod tests {
    use super::{
        AgentEntry, AgentInfo, AgentState, CliCommand, Command, Error, ErrorCode, PaneNode,
        Request, Response, WaitOutcome, awaiting_reply, describe_layout, flag_value, key_bytes,
        parse_cli, pending_question, prompt_from_claude_transcript, resolve_agent, resolve_project,
        short_id, skill, wait_outcome,
    };
    use crate::{AgentActivity, SplitDirection};
    use uuid::Uuid;

    fn words(s: &[&str]) -> Vec<String> {
        s.iter().map(|w| w.to_string()).collect()
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn claim_matches(pattern: &str, value: &str) -> bool {
        match pattern.split_once('*') {
            None => pattern == value,
            Some((head, tail)) => {
                value.starts_with(head)
                    && (head.len()..=value.len()).any(|at| claim_matches(tail, &value[at..]))
            }
        }
    }

    #[test]
    fn same_process_claims_in_the_same_millisecond_have_distinct_identity() {
        let first = super::Claim {
            owner: "same-process".into(),
            host: "desk".into(),
            at: 1234,
            grace_ms: 30_000,
            nonce: Some(id(1)),
        };
        let second = super::Claim {
            nonce: Some(id(2)),
            ..first.clone()
        };
        assert!(first.same_turn(&super::Claim::decode(&first.encode()).unwrap()));
        assert!(!first.same_turn(&second));
        let pattern = super::Claim::pattern(&first.owner, first.nonce.unwrap());
        assert!(claim_matches(&pattern, &first.encode()));
        assert!(
            !claim_matches(&pattern, &second.encode()),
            "cleanup must preserve a newer same-ms claim"
        );
        let legacy = super::Claim {
            nonce: None,
            ..first.clone()
        };
        assert!(!claim_matches(&pattern, &legacy.encode()));
        assert_eq!(super::Claim::decode(&legacy.encode()), Some(legacy));
    }

    #[test]
    fn claim_pattern_matches_a_later_version_claim_with_extra_fields() {
        let claim = super::Claim {
            owner: "later".into(),
            host: "desk".into(),
            at: 1234,
            grace_ms: 30_000,
            nonce: Some(id(3)),
        };
        let later = format!("{}|field-from-a-later-muxel", claim.encode_tmux_command());
        let decoded = super::Claim::decode(&later).unwrap();
        assert!(decoded.same_turn(&claim));
        let pattern = decoded.compare_pattern().unwrap();
        assert_eq!(pattern, super::Claim::pattern("later", id(3)));
        assert!(pattern.ends_with(&format!("{}*", id(3))), "{pattern}");
        assert!(claim_matches(&pattern, &later));
        assert!(claim_matches(&pattern, &claim.encode_tmux_command()));
        let other = super::Claim {
            nonce: Some(id(4)),
            ..claim
        };
        assert!(!claim_matches(&pattern, &other.encode_tmux_command()));
    }

    #[test]
    fn claims_from_before_the_nonce_field_still_deserialize() {
        let legacy: super::Claim =
            serde_json::from_str(r#"{"owner":"old","host":"desk","at":1,"grace_ms":30000}"#)
                .unwrap();
        assert_eq!(legacy.nonce, None);
        assert_eq!(super::Claim::decode("v1|old|desk|1|30000"), Some(legacy));
    }

    #[test]
    fn requests_round_trip_as_tagged_json() {
        let req = Request {
            version: 1,
            token: "t".into(),
            caller_instance_id: None,
            command: Command::Show {
                agent: "claude".into(),
                full: true,
            },
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains(r#""cmd":"show""#), "{line}");
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), req);
        let unit: Request =
            serde_json::from_str(r#"{"version":1,"token":"t","command":{"cmd":"projects"}}"#)
                .unwrap();
        assert_eq!(unit.command, Command::Projects);
        assert_eq!(unit.caller_instance_id, None);
        let defaults: Request = serde_json::from_str(
            r#"{"version":1,"token":"t","command":{"cmd":"send","agent":"a","text":"hi"}}"#,
        )
        .unwrap();
        assert_eq!(
            defaults.command,
            Command::Send {
                agent: "a".into(),
                text: "hi".into(),
                force: false,
                launch_id: None,
                session_id: None,
                once: None,
                when_idle: false,
                detach: false,
                timeout_secs: None,
            }
        );
        // No launch guard: older requests retain their original shape.
        let line = serde_json::to_string(&defaults).unwrap();
        assert!(!line.contains("launch_id"), "{line}");
        assert!(!line.contains("once"), "{line}");
        assert!(!line.contains("when_idle"), "{line}");
        assert!(!line.contains("timeout_secs"), "{line}");
        assert!(!line.contains("caller_instance_id"), "{line}");
        let caller = Request {
            caller_instance_id: Some(id(7)),
            command: Command::Keys {
                agent: "self".into(),
                keys: vec!["esc".into()],
                force: true,
            },
            ..req
        };
        let line = serde_json::to_string(&caller).unwrap();
        assert!(line.contains("caller_instance_id"), "{line}");
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), caller);
    }

    #[test]
    fn closing_refusal_round_trips_its_undo_deadline() {
        let deadline = 1_800_000_000_015;
        let error = Error::closing(deadline);
        assert_eq!(error.code, Some(ErrorCode::Closing));
        assert_eq!(error.undo_until, Some(deadline));
        let wire = serde_json::to_value(Response::err(error.clone())).unwrap();
        assert_eq!(wire["code"], "closing");
        assert_eq!(wire["undo_until"], deadline);
        assert_eq!(wire["ok"], false);
        let response: Response = serde_json::from_value(wire).unwrap();
        assert_eq!(response.into_result(), Err(error));
    }

    #[test]
    fn replies_without_undo_deadlines_keep_their_legacy_shape() {
        for wire in [
            r#"{"ok":false,"code":"working","error":"busy"}"#,
            r#"{"ok":false,"error":"no match"}"#,
            r#"{"ok":true,"result":{"sent":true}}"#,
        ] {
            let response: Response = serde_json::from_str(wire).unwrap();
            assert_eq!(response.undo_until, None);
            assert_eq!(
                serde_json::to_value(&response).unwrap(),
                serde_json::from_str::<serde_json::Value>(wire).unwrap()
            );
            if !response.ok {
                assert_eq!(response.into_result().unwrap_err().undo_until, None);
            }
        }
        assert_eq!(Error::new(ErrorCode::Working, "busy").undo_until, None);
        assert_eq!(Error::from("no match").undo_until, None);
    }

    #[test]
    fn unknown_refusal_codes_preserve_the_message_and_undo_deadline() {
        let response: Response = serde_json::from_str(
            r#"{"ok":false,"code":"from_later","error":"keep this message","undo_until":1800000000015}"#,
        )
        .unwrap();
        let error = response.into_result().unwrap_err();
        assert_eq!(error.code, None);
        assert_eq!(error.message, "keep this message");
        assert_eq!(error.undo_until, Some(1_800_000_000_015));
        let reemitted = serde_json::to_value(Response::err(error)).unwrap();
        assert!(reemitted.get("code").is_none());
        assert_eq!(reemitted["undo_until"], 1_800_000_000_015_i64);
    }

    #[test]
    fn a_failure_carries_a_code_when_there_is_something_to_act_on() {
        let line = |reply: &Response| serde_json::to_string(reply).unwrap();
        let working = Response::err(Error::new(ErrorCode::Working, "the agent is working"));
        assert_eq!(
            line(&working),
            r#"{"ok":false,"code":"working","error":"the agent is working"}"#
        );
        // The rest keep the shape they had.
        assert_eq!(
            line(&Response::err("no agent matches 'x'")),
            r#"{"ok":false,"error":"no agent matches 'x'"}"#
        );
        assert_eq!(
            line(&Response::ok(serde_json::json!({ "sent": true }))),
            r#"{"ok":true,"result":{"sent":true}}"#
        );
        let back: Response = serde_json::from_str(&line(&working)).unwrap();
        assert_eq!(
            back.into_result(),
            Err(Error::new(ErrorCode::Working, "the agent is working"))
        );
        // A code this muxel doesn't know (from a newer one) reads as none.
        let newer: Response =
            serde_json::from_str(r#"{"ok":false,"code":"from_later","error":"x"}"#).unwrap();
        assert_eq!(newer.code, None);
        assert_eq!(newer.error.as_deref(), Some("x"));
        let uncertain = Response::err(
            Error::new(ErrorCode::Working, "check screen")
                .with_reason("just_submitted")
                .after_paste(),
        );
        let back: Response = serde_json::from_str(&line(&uncertain)).unwrap();
        assert_eq!(back.reason.as_deref(), Some("just_submitted"));
        assert_eq!(back.pasted, Some(true));
        for (code, word) in [
            (ErrorCode::Working, "working"),
            (ErrorCode::Blocked, "blocked"),
            (ErrorCode::Starting, "starting"),
            (ErrorCode::Closing, "closing"),
            (ErrorCode::NotRunning, "not_running"),
            (ErrorCode::Exited, "exited"),
            (ErrorCode::ClaimedElsewhere, "claimed_elsewhere"),
            (ErrorCode::LaunchMismatch, "launch_mismatch"),
            (ErrorCode::InputChanged, "input_changed"),
            (ErrorCode::TmuxPaneUnresolved, "tmux_pane_unresolved"),
            (ErrorCode::TmuxPaneInMode, "tmux_pane_in_mode"),
            (ErrorCode::TmuxRefused, "tmux_refused"),
            (ErrorCode::KeysPartlyPressed, "keys_partly_pressed"),
            (ErrorCode::SelfTarget, "self_target"),
            (ErrorCode::DraftPresent, "draft_present"),
            (ErrorCode::Busy, "busy"),
            (ErrorCode::Duplicate, "duplicate"),
            (ErrorCode::TimedOut, "timed_out"),
            (ErrorCode::Cancelled, "cancelled"),
            (ErrorCode::HostUnavailable, "host_unavailable"),
            (ErrorCode::StaleEndpoint, "stale_endpoint"),
        ] {
            assert_eq!(serde_json::to_value(code).unwrap(), word);
            let response = Response::err(Error::new(code, "refused"));
            let line = serde_json::to_string(&response).unwrap();
            let decoded: Response = serde_json::from_str(&line).unwrap();
            assert_eq!(decoded.into_result(), Err(Error::new(code, "refused")));
        }
    }

    #[test]
    fn guarded_sends_use_a_version_legacy_hosts_reject() {
        assert_eq!(super::request_version(&Command::Projects), 1);
        let mut send = Command::Send {
            agent: "a".into(),
            text: "hi".into(),
            force: false,
            launch_id: None,
            session_id: None,
            once: None,
            when_idle: false,
            detach: false,
            timeout_secs: None,
        };
        assert_eq!(super::request_version(&send), 1);
        if let Command::Send { launch_id, .. } = &mut send {
            *launch_id = Some(id(42));
        }
        assert_eq!(super::request_version(&send), 2);
    }

    /// A host that doesn't know `once` or `when_idle` ignores unknown fields,
    /// so it would type at once, or type a repeat again. Only the version
    /// makes it refuse.
    #[test]
    fn once_and_when_idle_need_protocol_v4() {
        let parsed = parse_cli(&words(&[
            "send",
            "a",
            "hi",
            "--once",
            "d-9f2a",
            "--when-idle",
            "--timeout",
            "47",
        ]))
        .unwrap();
        let CliCommand::Request(send) = parsed else {
            panic!("expected a request")
        };
        assert_eq!(
            send,
            Command::Send {
                agent: "a".into(),
                text: "hi".into(),
                force: false,
                launch_id: None,
                session_id: None,
                once: Some("d-9f2a".into()),
                when_idle: true,
                detach: false,
                timeout_secs: Some(47),
            }
        );
        assert_eq!(super::request_version(&send), super::SAFE_SEND_VERSION);
        for args in [&["--once", "k"][..], &["--when-idle"]] {
            let mut line = vec!["send", "a", "hi"];
            line.extend_from_slice(args);
            let Ok(CliCommand::Request(send)) = parse_cli(&words(&line)) else {
                panic!("{line:?}")
            };
            assert_eq!(super::request_version(&send), 4, "{line:?}");
        }
        for args in [
            words(&["send", "a", "hi", "--once"]),
            words(&["send", "a", "hi", "--once", ""]),
            words(&["send", "a", "hi", "--once", "a\tb"]),
            words(&["send", "a", "hi", "--once", &"k".repeat(129)]),
            words(&["send", "a", "hi", "--once", "a", "--once", "b"]),
            words(&["send", "a", "hi", "--timeout", "5"]),
            words(&["send", "a", "hi", "--when-idle", "--timeout", "0"]),
            words(&["send", "a", "hi", "--when-idle", "--timeout", "3601"]),
            words(&["send", "a", "hi", "--when-idle", "--force"]),
            words(&["status", "a", "--once", "k"]),
            words(&["keys", "a", "enter", "--when-idle"]),
        ] {
            assert!(parse_cli(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn launch_guard_accepts_only_the_expected_terminal() {
        let launch = id(42);
        assert_eq!(super::check_launch_id(None, None), Ok(()));
        assert_eq!(super::check_launch_id(None, Some(launch)), Ok(()));
        assert_eq!(super::check_launch_id(Some(launch), Some(launch)), Ok(()));
        for current in [None, Some(id(43))] {
            let error = super::check_launch_id(Some(launch), current).unwrap_err();
            assert_eq!(error.code, Some(ErrorCode::LaunchMismatch));
        }
    }

    /// Wire pins a doorbell to the conversation whose owner it checked. Any
    /// other conversation, or one muxel can't vouch for, refuses before typing,
    /// each with its own reason.
    #[test]
    fn a_conversation_pin_accepts_only_that_conversation() {
        use super::Conversation::{Known, Unconfirmed};
        let check = super::check_session_id;
        assert_eq!(check(None, &Unconfirmed), Ok(()));
        assert_eq!(check(None, &Known("a".into())), Ok(()));
        assert_eq!(check(Some("a"), &Known("a".into())), Ok(()));
        for (current, reason) in [
            (Known("b".into()), "conversation_changed"),
            (Unconfirmed, "conversation_unconfirmed"),
        ] {
            let error = check(Some("a"), &current).unwrap_err();
            assert_eq!(error.code, Some(ErrorCode::LaunchMismatch), "{current:?}");
            assert_eq!(error.reason.as_deref(), Some(reason));
            assert_eq!(error.pasted, None, "a refused pin typed nothing");
        }
        assert_eq!(Known("a".into()).id(), Some("a"));
        assert_eq!(Unconfirmed.id(), None);
    }

    /// A host that doesn't know `session_id` ignores it and would type into
    /// whatever conversation the pane is in. Only the version makes it refuse.
    #[test]
    fn a_conversation_pinned_send_needs_protocol_v5() {
        let sid = "01a1543c-9700-7000-8000-000000000001";
        let Ok(CliCommand::Request(send)) = parse_cli(&words(&[
            "send",
            "a",
            "hi",
            "--session-id",
            sid,
            "--once",
            "d-1",
            "--when-idle",
        ])) else {
            panic!("--session-id parses");
        };
        assert!(matches!(
            &send,
            Command::Send { session_id: Some(pinned), once: Some(_), when_idle: true, .. }
                if pinned == sid
        ));
        assert_eq!(super::request_version(&send), super::PINNED_SEND_VERSION);
        let line = serde_json::to_string(&send).unwrap();
        assert!(
            line.contains(&format!("\"session_id\":\"{sid}\"")),
            "{line}"
        );
        for args in [
            words(&["send", "a", "hi", "--session-id"]),
            words(&["send", "a", "hi", "--session-id", ""]),
            words(&["send", "a", "hi", "--session-id", "a\tb"]),
            words(&["send", "a", "hi", "--session-id", "a", "--session-id", "b"]),
            words(&["status", "a", "--session-id", sid]),
        ] {
            assert!(parse_cli(&args).is_err(), "{args:?}");
        }
        // A raw client gets the same check from the host.
        assert_eq!(super::check_safe_send(&send), Ok(()));
        let mut raw = send;
        if let Command::Send { session_id, .. } = &mut raw {
            *session_id = Some("a\nb".into());
        }
        assert!(super::check_safe_send(&raw).is_err());
    }

    /// A host that doesn't know `detach` ignores it and holds the caller's
    /// connection for the whole wait; one that doesn't know `once_cancel`
    /// can't withdraw anything. Only the version makes either refuse.
    #[test]
    fn a_detached_send_needs_protocol_v6_and_a_key_to_report_it() {
        let line = |extra: &[&str]| {
            let mut line = vec!["send", "a", "hi", "--when-idle", "--once", "d-1"];
            line.extend_from_slice(extra);
            words(&line)
        };
        let Ok(CliCommand::Request(send)) = parse_cli(&line(&["--detach", "--timeout", "86400"]))
        else {
            panic!("--detach parses");
        };
        assert!(matches!(
            &send,
            Command::Send {
                detach: true,
                when_idle: true,
                once: Some(_),
                timeout_secs: Some(86_400),
                ..
            }
        ));
        assert_eq!(super::request_version(&send), super::DETACHED_SEND_VERSION);
        assert_eq!(super::NEWEST_VERSION, super::DETACHED_SEND_VERSION);
        assert!(
            serde_json::to_string(&send)
                .unwrap()
                .contains("\"detach\":true")
        );
        // Wire sends the wire form directly.
        let raw: Command = serde_json::from_str(
            r#"{"cmd":"send","agent":"a","text":"hi","once":"d-1","when_idle":true,"detach":true}"#,
        )
        .unwrap();
        assert_eq!(super::request_version(&raw), super::DETACHED_SEND_VERSION);
        // An attached send leaves the field out, so older hosts read it as before.
        let Ok(CliCommand::Request(attached)) = parse_cli(&line(&[])) else {
            panic!("an attached send parses");
        };
        assert!(!serde_json::to_string(&attached).unwrap().contains("detach"));
        // A detached send may wait a day; an attached one still an hour.
        assert!(parse_cli(&line(&["--timeout", "3601"])).is_err());
        for args in [
            words(&["send", "a", "hi", "--detach", "--once", "d-1"]),
            words(&["send", "a", "hi", "--when-idle", "--detach"]),
            line(&["--detach", "--timeout", "86401"]),
            line(&["--detach", "--timeout", "0"]),
            words(&["status", "a", "--detach"]),
        ] {
            assert!(parse_cli(&args).is_err(), "{args:?}");
        }
        assert_eq!(
            parse_cli(&words(&["once-cancel", "a", "d-1"])),
            Ok(CliCommand::Request(Command::OnceCancel {
                agent: "a".into(),
                key: "d-1".into(),
            }))
        );
        let cancel: Command =
            serde_json::from_str(r#"{"cmd":"once_cancel","agent":"a","key":"d-1"}"#).unwrap();
        assert_eq!(
            super::request_version(&cancel),
            super::DETACHED_SEND_VERSION
        );
        for args in [
            words(&["once-cancel", "a"]),
            words(&["once-cancel", "a", "k", "extra"]),
            words(&["once-cancel", "a", "a\tb"]),
        ] {
            assert!(parse_cli(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn send_receipt_identifies_the_admitted_launch_and_paste() {
        let receipt = super::pasted_prompt_receipt(id(7), id(42));
        assert_eq!(receipt["agent"], short_id(id(7)));
        assert_eq!(receipt["launch_id"], id(42).to_string());
        assert_eq!(receipt["sent"], true);
        assert_eq!(receipt["pasted"], true);
    }

    #[test]
    fn cli_keeps_the_launch_guard_even_with_force_and_rejects_invalid_ids() {
        let launch = id(42).to_string();
        assert_eq!(
            parse_cli(&words(&[
                "send",
                "a",
                "hi",
                "--force",
                "--launch-id",
                &launch
            ])),
            Ok(CliCommand::Request(Command::Send {
                agent: "a".into(),
                text: "hi".into(),
                force: true,
                launch_id: Some(id(42)),
                session_id: None,
                once: None,
                when_idle: false,
                detach: false,
                timeout_secs: None,
            }))
        );
        for args in [
            words(&["send", "a", "hi", "--launch-id"]),
            words(&["send", "a", "hi", "--launch-id", "invalid"]),
            words(&["status", "a", "--launch-id", &launch]),
        ] {
            assert!(parse_cli(&args).is_err(), "{args:?}");
        }
        // A literal flag belongs to the prompt and does not set a guard.
        assert_eq!(
            parse_cli(&words(&[
                "send",
                "a",
                "--",
                "--launch-id",
                "--once",
                "text"
            ])),
            Ok(CliCommand::Request(Command::Send {
                agent: "a".into(),
                text: "--launch-id --once text".into(),
                force: false,
                launch_id: None,
                session_id: None,
                once: None,
                when_idle: false,
                detach: false,
                timeout_secs: None,
            }))
        );
    }

    #[test]
    fn cached_branch_is_additive_and_separate_from_worktree_creation_branch() {
        let mut old_response = serde_json::json!({
            "id": "11111111", "uuid": id(7), "name": "shell",
            "project": "repo", "project_id": "22222222", "kind": "terminal",
            "preset": "Shell", "is_agent": false, "awaiting_reply": false,
            "focused": false, "remote": false, "cwd": "/repo/env",
            "worktree_branch": "original-at-creation"
        });
        let old: AgentInfo = serde_json::from_value(old_response.clone()).unwrap();
        assert_eq!(old.branch, None);
        assert!(serde_json::to_value(old).unwrap().get("branch").is_none());
        old_response["branch"] = serde_json::json!("checked-out-now");
        let current: AgentInfo = serde_json::from_value(old_response).unwrap();
        assert_eq!(current.branch.as_deref(), Some("checked-out-now"));
        assert_eq!(
            current.worktree_branch.as_deref(),
            Some("original-at-creation")
        );
    }

    #[test]
    fn agent_info_optional_launch_id_and_label_round_trip() {
        let mut info = AgentInfo {
            id: short_id(id(7)),
            uuid: id(7),
            name: "Claude".into(),
            label: None,
            project: "p".into(),
            project_id: "11111111".into(),
            kind: "terminal".into(),
            program: Some("claude".into()),
            preset: "claude".into(),
            model: None,
            is_agent: true,
            status: Some(AgentState::NotRunning),
            live_status: None,
            source: None,
            activity: None,
            blocked_on: None,
            not_running_reason: None,
            resumes: None,
            status_since: None,
            launch_id: None,
            session_id: None,
            awaiting_reply: false,
            background: None,
            turn_over: false,
            startup_error: None,
            prompt_delivery: None,
            focused: false,
            pane: Some(1),
            worktree_branch: None,
            cwd: None,
            branch: None,
            remote: false,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert!(json.get("launch_id").is_none(), "{json}");
        assert!(json.get("session_id").is_none(), "{json}");
        for key in ["source", "activity", "blocked_on", "live_status"] {
            assert!(json.get(key).is_none(), "{json}");
        }
        assert!(
            json.get("label").is_none(),
            "unnamed replies keep their old shape"
        );
        assert_eq!(serde_json::from_value::<AgentInfo>(json).unwrap(), info);
        info.label = Some("starting label".into());
        info.status = Some(AgentState::Idle);
        info.launch_id = Some(id(42));
        info.session_id = Some("5a40c944-0000-4000-8000-000000000001".into());
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["launch_id"], id(42).to_string());
        assert_eq!(json["session_id"], "5a40c944-0000-4000-8000-000000000001");
        assert_eq!(json["label"], "starting label");
        assert_eq!(serde_json::from_value::<AgentInfo>(json).unwrap(), info);
        info.status = Some(AgentState::Blocked);
        info.live_status = None;
        info.source = Some(super::AgentStatusSource::Program);
        info.activity = Some("Reading files · 42%".into());
        info.blocked_on = Some(super::AgentBlockedOn::Question);
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["source"], "program");
        assert_eq!(json["blocked_on"], "question");
        assert_eq!(json["activity"], "Reading files · 42%");
        assert_eq!(serde_json::from_value::<AgentInfo>(json).unwrap(), info);
    }

    #[test]
    fn launch_id_is_the_terminals_only_while_its_program_runs() {
        let terminal = Some(id(42));
        for state in [
            AgentState::Working,
            AgentState::Idle,
            AgentState::Blocked,
            AgentState::Done,
        ] {
            assert_eq!(
                super::launch_id(Some(state), terminal),
                terminal,
                "{state:?}"
            );
        }
        // Its pane keeps the terminal of a program that exited, or of a session
        // being reconnected: that run is over, so it has no id to report.
        for state in [
            AgentState::Starting,
            AgentState::Exited,
            AgentState::NotRunning,
            AgentState::Failed,
        ] {
            assert_eq!(super::launch_id(Some(state), terminal), None, "{state:?}");
        }
        // Editors and browsers have no status, and no terminal either.
        assert_eq!(super::launch_id(None, terminal), None);
        assert_eq!(super::launch_id(Some(AgentState::Idle), None), None);
    }

    #[test]
    fn skill_explains_every_code() {
        let text = skill("/usr/bin/muxel");
        for code in [
            ErrorCode::Working,
            ErrorCode::Blocked,
            ErrorCode::Starting,
            ErrorCode::Closing,
            ErrorCode::NotRunning,
            ErrorCode::Exited,
            ErrorCode::ClaimedElsewhere,
            ErrorCode::LaunchMismatch,
            ErrorCode::InputChanged,
            ErrorCode::TmuxPaneUnresolved,
            ErrorCode::KeysPartlyPressed,
            ErrorCode::DraftPresent,
            ErrorCode::Busy,
            ErrorCode::Duplicate,
            ErrorCode::TimedOut,
            ErrorCode::Cancelled,
            ErrorCode::HostUnavailable,
            ErrorCode::StaleEndpoint,
        ] {
            let word = serde_json::to_value(code).unwrap();
            let word = format!("`{}`", word.as_str().unwrap());
            assert!(text.contains(&word), "the skill doesn't explain {word}");
        }
        assert!(text.contains("--launch-id ID"));
        assert!(text.contains("\"pasted\": true"));
        assert!(text.contains("--when-idle --once KEY --launch-id ID"));
        assert!(text.contains("launch_id"));
        assert!(text.contains("--session-id SID"));
        assert!(text.contains("`session_id`"));
        assert!(text.contains("`protocol`"));
        assert!(text.contains("conversation_changed"));
        assert!(text.contains("conversation_unconfirmed"));
        assert!(text.contains("tmux_delivery"));
        assert!(text.contains("`--detach`"));
        assert!(text.contains("muxel ctl once-cancel AGENT KEY"));
        assert!(text.contains("`dropped`"));
        assert!(text.contains("`queued`"));
    }

    #[test]
    fn a_duplicate_carries_the_first_failure_and_whether_it_typed() {
        let first = Error::new(ErrorCode::InputChanged, "Enter was withheld").after_paste();
        let repeat = Error::duplicate_of(&first);
        assert_eq!(repeat.code, Some(ErrorCode::Duplicate));
        assert_eq!(repeat.pasted, Some(true));
        let wire = serde_json::to_value(Response::err(repeat.clone())).unwrap();
        assert_eq!(wire["code"], "duplicate");
        assert_eq!(wire["pasted"], true);
        assert_eq!(wire["original"]["code"], "input_changed");
        assert_eq!(wire["original"]["pasted"], true);
        assert_eq!(wire["original"]["error"], "Enter was withheld");
        let back: Response = serde_json::from_value(wire).unwrap();
        assert_eq!(back.into_result(), Err(repeat));
    }

    #[test]
    fn agents_resolve_by_id_name_project_and_focus() {
        let a = id(0x1a2b3c4d_0000_0000_0000_000000000001);
        let b = id(0x9f000000_0000_0000_0000_000000000002);
        let c = id(0x9f001111_0000_0000_0000_000000000003);
        let agents = [
            AgentEntry {
                id: a,
                name: "✳ Claude",
                label: None,
                project: "muxel",
            },
            AgentEntry {
                id: b,
                name: "Codex",
                label: None,
                project: "muxel",
            },
            AgentEntry {
                id: c,
                name: "Codex",
                label: None,
                project: "Other Repo",
            },
        ];
        assert_eq!(resolve_agent("1a2b3c4d", &agents, None), Ok(a));
        assert_eq!(resolve_agent("1A2B", &agents, None), Ok(a));
        assert_eq!(resolve_agent(&a.to_string(), &agents, None), Ok(a));
        assert_eq!(resolve_agent("claude", &agents, None), Ok(a));
        assert_eq!(resolve_agent("other repo/codex", &agents, None), Ok(c));
        assert_eq!(resolve_agent("focused", &agents, Some(b)), Ok(b));
        assert!(resolve_agent("focused", &agents, None).is_err());
        let err = resolve_agent("codex", &agents, None).unwrap_err();
        assert!(
            err.contains("9f000000") && err.contains("9f001111"),
            "{err}"
        );
        // A shared id prefix is ambiguous rather than silently picking one.
        assert!(resolve_agent("9f00", &agents, None).is_err());
        assert_eq!(resolve_agent("9f000", &agents, None), Ok(b));
        assert!(resolve_agent("gemini", &agents, None).is_err());
    }

    #[test]
    fn ctl_ergonomics_accepts_pane_ids_without_changing_name_or_ambiguity_rules() {
        let a = id(0x1a2b3c4d_0000_0000_0000_000000000001);
        let b = id(0x1a2bffff_0000_0000_0000_000000000002);
        let agents = [
            AgentEntry {
                id: a,
                name: "panebench",
                label: None,
                project: "sample",
            },
            AgentEntry {
                id: b,
                name: "other",
                label: None,
                project: "sample",
            },
        ];
        assert_eq!(resolve_agent("pane1a2b3c4d", &agents, None), Ok(a));
        assert_eq!(resolve_agent(&format!("pane{a}"), &agents, None), Ok(a));
        assert!(
            resolve_agent("pane1a2b", &agents, None)
                .unwrap_err()
                .contains("more than one")
        );
        assert_eq!(resolve_agent("panebench", &agents, None), Ok(a));
    }

    #[test]
    fn a_name_spelled_like_a_pane_id_resolves_to_its_own_agent() {
        let named = id(0x0000aaaa_0000_0000_0000_000000000001);
        let other = id(0x12340000_0000_0000_0000_000000000002);
        let agents = [
            AgentEntry {
                id: named,
                name: "pane1234",
                label: None,
                project: "sample",
            },
            AgentEntry {
                id: other,
                name: "other",
                label: None,
                project: "sample",
            },
        ];
        assert_eq!(resolve_agent("pane1234", &agents, None), Ok(named));
        assert_eq!(resolve_agent("pane12340000", &agents, None), Ok(other));
    }

    #[test]
    fn projects_resolve_by_id_name_or_active() {
        let p = id(0xabcd0000_0000_0000_0000_000000000001);
        let projects = [(p, "My App")];
        assert_eq!(resolve_project("abcd", &projects, None), Ok(p));
        assert_eq!(resolve_project("my app", &projects, None), Ok(p));
        assert_eq!(resolve_project("active", &projects, Some(p)), Ok(p));
        assert!(resolve_project("nope", &projects, None).is_err());
    }

    #[test]
    fn layout_numbers_panes_in_tree_order() {
        let (a, b, c) = (id(1), id(2), id(3));
        let mut right = PaneNode::leaf(b);
        if let PaneNode::Leaf(leaf) = &mut right {
            leaf.tabs.push(c);
            leaf.active = 1;
        }
        let tree = PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![1.0, 1.0],
            children: vec![PaneNode::leaf(a), right],
        };
        let (value, panes) = describe_layout(&tree);
        assert_eq!(panes[&a], 1);
        assert_eq!(panes[&b], 2);
        assert_eq!(panes[&c], 2);
        assert_eq!(value["split"], "side_by_side");
        assert_eq!(value["children"][1]["active_tab"], short_id(c));
    }

    #[test]
    fn model_comes_from_either_flag_form() {
        let args = words(&["--model", "opus", "--x"]);
        assert_eq!(flag_value(&args, "--model"), Some("opus".into()));
        let args = words(&["--model=sonnet"]);
        assert_eq!(flag_value(&args, "--model"), Some("sonnet".into()));
        assert_eq!(flag_value(&args, ""), None);
        assert_eq!(flag_value(&words(&["--models", "x"]), "--model"), None);
    }

    #[test]
    fn keys_map_to_terminal_bytes() {
        assert_eq!(key_bytes("enter", false), Some(b"\r".to_vec()));
        assert_eq!(key_bytes("ESC", false), Some(b"\x1b".to_vec()));
        assert_eq!(key_bytes("up", false), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes("up", true), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes("shift-tab", false), Some(b"\x1b[Z".to_vec()));
        assert_eq!(key_bytes("ctrl-c", false), Some(vec![3]));
        assert_eq!(key_bytes("1", false), Some(b"1".to_vec()));
        assert_eq!(key_bytes("é", false), Some("é".as_bytes().to_vec()));
        assert_eq!(key_bytes("ctrl-shift", false), None);
        assert_eq!(key_bytes("hello", false), None);
    }

    #[test]
    fn claude_permission_prompt_parses_into_question_and_options() {
        let screen = "\
⏺ I'll clean the build directory.

────────────────────────────────────────────────────────────
 Bash command

   rm -rf build/
   Remove old build output

 Do you want to proceed?
 ❯ 1. Yes
   2. Yes, and don't ask again for rm commands in
      /Users/me/proj
   3. No, and tell Claude what to do differently (esc)

";
        let q = pending_question(screen).unwrap();
        assert_eq!(q.options.len(), 3);
        assert_eq!(q.options[0].key, "1");
        assert_eq!(q.options[0].label, "Yes");
        assert!(q.options[0].selected);
        assert!(!q.options[1].selected);
        assert!(q.options[2].label.starts_with("No, and tell Claude"));
        assert!(q.text.starts_with("Bash command"), "{}", q.text);
        assert!(q.text.contains("rm -rf build/"));
        assert!(q.text.ends_with("Do you want to proceed?"));
        assert!(!q.text.contains("clean the build"));
    }

    #[test]
    fn boxed_prompt_loses_its_frame() {
        let screen = "\
╭──────────────────────────────╮
│ Allow command?               │
│                              │
│ › 1. Yes (y)                 │
│   2. No (esc)                │
╰──────────────────────────────╯";
        let q = pending_question(screen).unwrap();
        assert_eq!(q.text, "Allow command?");
        assert_eq!(q.options.len(), 2);
        assert_eq!(q.options[1].label, "No (esc)");
        assert!(q.options[0].selected);
    }

    #[test]
    fn prompt_without_numbered_options_is_the_screen_tail() {
        let screen = "working on it\n\nOverwrite config.toml? (y/n)\n";
        let q = pending_question(screen).unwrap();
        assert!(q.options.is_empty());
        assert!(q.text.ends_with("Overwrite config.toml? (y/n)"));
        assert_eq!(pending_question("\n  \n"), None);
        // A screen that ends in a rule (a composer's frame) holds no question.
        assert_eq!(pending_question("⏺ Done.\n────────\n"), None);
    }

    #[test]
    fn numbered_list_in_a_reply_above_the_prompt_is_not_its_options() {
        let screen = "\
Steps:
1. Build
2. Test
────────
 Continue?
 ❯ 1. Yes
   2. No";
        let q = pending_question(screen).unwrap();
        assert_eq!(q.text, "Continue?");
        assert_eq!(
            q.options
                .iter()
                .map(|o| o.label.as_str())
                .collect::<Vec<_>>(),
            ["Yes", "No"]
        );
    }

    #[test]
    fn transcript_gives_the_last_real_prompt() {
        let jsonl = [
            r#"{"type":"user","message":{"content":"first prompt"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"second prompt"}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
            r#"{"type":"user","isMeta":true,"message":{"content":"meta"}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"content":"subagent task"}}"#,
            r#"{"type":"user","message":{"content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
            "{torn line",
        ]
        .join("\n");
        assert_eq!(
            prompt_from_claude_transcript(&jsonl),
            Some("second prompt".into())
        );
        let slash = r#"{"type":"user","message":{"content":"<command-name>/review</command-name>\n<command-args>src/app.rs</command-args>"}}"#;
        assert_eq!(
            prompt_from_claude_transcript(slash),
            Some("/review src/app.rs".into())
        );
        assert_eq!(prompt_from_claude_transcript(""), None);
    }

    #[test]
    fn a_sent_prompt_is_awaited_until_its_turn_ends() {
        let sent = 10_000;
        let quiet = AgentActivity {
            completed_at: Some(5_000),
            ..AgentActivity::default()
        };
        // Still showing the previous turn's Done: the new one hasn't started.
        assert!(awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Done),
            &quiet,
            sent + 500
        ));
        // Working on it.
        assert!(awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Working),
            &quiet,
            sent + 5_000
        ));
        // Finished after the prompt.
        let finished = AgentActivity {
            work_started_at: Some(sent + 400),
            completed_at: Some(sent + 9_000),
            ..AgentActivity::default()
        };
        assert!(!awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Done),
            &finished,
            sent + 9_500
        ));
        // Worked, then went idle without a completion mark.
        let idled = AgentActivity {
            work_started_at: Some(sent + 400),
            ..AgentActivity::default()
        };
        assert!(!awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Idle),
            &idled,
            sent + 3_000
        ));
        // Never visibly started: give up after the grace period.
        assert!(!awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Idle),
            &quiet,
            sent + super::REPLY_GRACE_MS
        ));
        // Just answered: the question is still on screen, blocked from before.
        let asked = AgentActivity {
            blocked_at: Some(sent - 2_000),
            ..AgentActivity::default()
        };
        assert!(awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Blocked),
            &asked,
            sent + 300
        ));
        // An answer that sent the agent straight back to idle: done once the
        // shorter answer grace has passed.
        assert!(awaiting_reply(
            Some(sent),
            super::ANSWER_GRACE_MS,
            Some(AgentState::Idle),
            &asked,
            sent + 1_000
        ));
        assert!(!awaiting_reply(
            Some(sent),
            super::ANSWER_GRACE_MS,
            Some(AgentState::Idle),
            &asked,
            sent + super::ANSWER_GRACE_MS
        ));
        // Blocked again after the answer: a new question.
        let asked_again = AgentActivity {
            work_started_at: Some(sent + 500),
            blocked_at: Some(sent + 4_000),
            ..AgentActivity::default()
        };
        assert!(!awaiting_reply(
            Some(sent),
            super::REPLY_GRACE_MS,
            Some(AgentState::Blocked),
            &asked_again,
            sent + 4_500
        ));
        // Nothing sent through muxel: nothing to await.
        assert!(!awaiting_reply(
            None,
            super::REPLY_GRACE_MS,
            Some(AgentState::Working),
            &quiet,
            sent
        ));
    }

    #[test]
    fn wait_ends_on_finish_block_or_exit() {
        assert_eq!(wait_outcome(Some(AgentState::Working), false), None);
        assert_eq!(wait_outcome(Some(AgentState::Done), true), None);
        assert_eq!(
            wait_outcome(Some(AgentState::Done), false),
            Some(WaitOutcome::Finished)
        );
        assert_eq!(
            wait_outcome(Some(AgentState::Blocked), false),
            Some(WaitOutcome::Blocked)
        );
        // The question just answered, not yet off the screen.
        assert_eq!(wait_outcome(Some(AgentState::Blocked), true), None);
        assert_eq!(
            wait_outcome(Some(AgentState::Failed), false),
            Some(WaitOutcome::Exited)
        );
        assert_eq!(
            wait_outcome(Some(AgentState::NotRunning), false),
            Some(WaitOutcome::NotRunning)
        );
        assert_eq!(wait_outcome(None, false), None);
    }

    #[test]
    fn cli_parses_commands_flags_and_literal_text() {
        assert_eq!(parse_cli(&[]), Ok(CliCommand::Help(None)));
        assert_eq!(
            parse_cli(&words(&["focus", "old-session"])),
            Ok(CliCommand::Request(Command::Focus {
                agent: "old-session".into()
            }))
        );
        assert!(parse_cli(&words(&["focus"])).is_err());
        assert!(parse_cli(&words(&["focus", "a", "b"])).is_err());
        assert_eq!(
            parse_cli(&words(&["wake", "old-session"])),
            Ok(CliCommand::Request(Command::Wake {
                agent: "old-session".into()
            }))
        );
        assert!(parse_cli(&words(&["wake"])).is_err());
        assert!(parse_cli(&words(&["wake", "a", "b"])).is_err());
        // Wire sends the wire form directly.
        assert_eq!(
            serde_json::from_str::<Command>(r#"{"cmd":"wake","agent":"abcd1234"}"#).unwrap(),
            Command::Wake {
                agent: "abcd1234".into()
            }
        );
        assert_eq!(
            parse_cli(&words(&["panes"])),
            Ok(CliCommand::Request(Command::Panes { project: None }))
        );
        assert_eq!(
            parse_cli(&words(&["show", "claude", "--full"])),
            Ok(CliCommand::Request(Command::Show {
                agent: "claude".into(),
                full: true
            }))
        );
        assert_eq!(
            parse_cli(&words(&["send", "a1b2", "fix", "the", "tests"])),
            Ok(CliCommand::Request(Command::Send {
                agent: "a1b2".into(),
                text: "fix the tests".into(),
                force: false,
                launch_id: None,
                session_id: None,
                once: None,
                when_idle: false,
                detach: false,
                timeout_secs: None,
            }))
        );
        assert_eq!(
            parse_cli(&words(&["send", "a1b2", "--", "--help", "--force"])),
            Ok(CliCommand::Request(Command::Send {
                agent: "a1b2".into(),
                text: "--help --force".into(),
                force: false,
                launch_id: None,
                session_id: None,
                once: None,
                when_idle: false,
                detach: false,
                timeout_secs: None,
            }))
        );
        assert_eq!(
            parse_cli(&words(&["wait", "a1b2", "--timeout", "30"])),
            Ok(CliCommand::Wait {
                agent: "a1b2".into(),
                timeout_secs: 30,
                full: false,
                turn: false
            })
        );
        assert_eq!(
            parse_cli(&words(&["wait", "a1b2", "--turn"])),
            Ok(CliCommand::Wait {
                agent: "a1b2".into(),
                timeout_secs: crate::control::WAIT_TIMEOUT_SECS,
                full: false,
                turn: true
            })
        );
        assert_eq!(
            parse_cli(&words(&["screen", "a1b2", "--lines", "200"])),
            Ok(CliCommand::Request(Command::Screen {
                agent: "a1b2".into(),
                lines: Some(200)
            }))
        );
        assert!(parse_cli(&words(&["send", "a1b2"])).is_err());
        assert!(parse_cli(&words(&["show"])).is_err());
        assert!(parse_cli(&words(&["keys", "a1b2", "warp"])).is_err());
        assert!(parse_cli(&words(&["wait", "a", "--timeout", "soon"])).is_err());
        assert!(parse_cli(&words(&["show", "a", "--nope"])).is_err());
        assert!(parse_cli(&words(&["frobnicate"])).is_err());
        assert!(parse_cli(&words(&["status", "a", "b"])).is_err());
    }

    #[test]
    fn dash_h_after_command_refuses_without_typing() {
        for args in [
            words(&["send", "agent", "-h"]),
            words(&["send", "--help"]),
            words(&["keys", "agent", "enter", "--help"]),
            words(&["new", "project", "-h"]),
        ] {
            let error = parse_cli(&args).unwrap_err();
            assert!(error.contains("was not run"), "{args:?}: {error}");
            assert!(error.contains("muxel ctl help"), "{args:?}: {error}");
            assert!(error.contains("`--`"), "{args:?}: {error}");
        }
        assert_eq!(
            parse_cli(&words(&["help", "send"])),
            Ok(CliCommand::Help(Some("send".into())))
        );
        assert!(super::command_help("send").unwrap().contains("--force"));
        assert!(matches!(
            parse_cli(&words(&["send", "agent", "--", "-h"])),
            Ok(CliCommand::Request(Command::Send { text, .. })) if text == "-h"
        ));
    }

    #[test]
    fn unknown_command_suggests_nearest_verb() {
        let error = parse_cli(&words(&["list"])).unwrap_err();
        assert!(error.contains("did you mean `panes`?"), "{error}");
    }

    #[test]
    fn send_to_own_pane_is_refused() {
        let me = id(7);
        let error = super::check_self_target(Some(me), me, false).unwrap_err();
        assert_eq!(error.code, Some(ErrorCode::SelfTarget));
        assert_eq!(super::check_self_target(Some(me), me, true), Ok(()));
        assert_eq!(super::check_self_target(Some(me), id(8), false), Ok(()));
        assert_eq!(super::check_self_target(None, me, false), Ok(()));
    }

    #[test]
    fn skill_names_the_binary_it_was_made_with_and_how_to_find_another() {
        let text = skill("/Applications/muxel.app/Contents/MacOS/muxel");
        assert!(text.contains("`/Applications/muxel.app/Contents/MacOS/muxel` - where muxel was"));
        assert!(!text.contains("MUXEL_PATH"));
        // Commands name the muxel found on whichever computer runs them.
        assert!(text.contains("`muxel ctl send AGENT TEXT`"));
        assert!(text.contains("dev.muxel.muxel/control.json"));
        // Every Linux install: package, AppImage, tarball.
        assert!(text.contains("`/usr/bin/muxel`"));
        assert!(text.contains("muxel-linux-x86_64.AppImage ctl projects"));
        assert!(text.contains("APPIMAGE_EXTRACT_AND_RUN=1"));
        assert!(text.contains("MUXEL_CONTROL="));
        assert!(text.contains("%APPDATA%\\muxel\\muxel\\data\\control.json"));
        let spaced = skill("/Users/me/My Apps/muxel");
        assert!(spaced.contains("`'/Users/me/My Apps/muxel'`"), "{spaced}");
    }

    #[test]
    fn skill_preview_joins_wrapped_lines_but_keeps_structure() {
        let md = "---\nname: muxel\ndescription: one line\n---\n\n# Title\n\nA paragraph\nwrapped here\nand here.\n\n    code line\n    another\n\n- item one\n  continued\n- item two\n   - nested: `a` (usually\n     `b`)\n1. step one\n   still one\n2. step two\n";
        assert_eq!(
            super::reflow_for_display(md),
            "---\nname: muxel\ndescription: one line\n---\n\n# Title\n\nA paragraph wrapped here and here.\n\n    code line\n    another\n\n- item one continued\n- item two\n   - nested: `a` (usually `b`)\n1. step one still one\n2. step two"
        );
        // The real skill: no line is a stray fragment of the one before.
        let shown = super::reflow_for_display(&skill("/usr/bin/muxel"));
        assert!(shown.contains("muxel is a desktop app on the user's computer that runs coding agents (Claude, Codex, opencode, Grok, ...) and shells side by side"));
        assert!(shown.contains("\n1. `/usr/bin/muxel` - where muxel was when these instructions were made. Use it if it exists on this computer.\n"));
        assert!(shown.contains(
            "\n   - macOS: `~/Library/Application Support/dev.muxel.muxel/control.json`\n"
        ));
    }

    #[test]
    fn claims_round_trip_and_survive_odd_host_names() {
        let claim = super::Claim {
            owner: "a1b2c3d4e5f6".into(),
            host: "Sam's Mac|mini".into(),
            at: 1_790_000_000_000,
            grace_ms: super::REPLY_GRACE_MS,
            nonce: None,
        };
        let value = claim.encode();
        assert_eq!(value, "v1|a1b2c3d4e5f6|Sam's-Mac-mini|1790000000000|30000");
        assert_eq!(
            claim.encode_tmux_command(),
            "v1|a1b2c3d4e5f6|Sam-s-Mac-mini|1790000000000|30000"
        );
        let back = super::Claim::decode(&value).unwrap();
        assert_eq!(back.owner, claim.owner);
        assert_eq!(back.host, "Sam's-Mac-mini");
        assert_eq!(back.at, claim.at);
        assert_eq!(super::Claim::decode(""), None);
        assert_eq!(super::Claim::decode("v2|x|y|1|2"), None);
        assert_eq!(super::Claim::decode("v1||y|1|2"), None);
        assert_eq!(super::Claim::decode("v1|x|y|soon|2"), None);
    }

    #[test]
    fn another_muxels_claim_holds_until_its_turn_ends() {
        let at = 50_000;
        let claim = super::Claim {
            owner: "other".into(),
            host: "desk".into(),
            at,
            grace_ms: super::REPLY_GRACE_MS,
            nonce: None,
        };
        let before = AgentActivity {
            completed_at: Some(at - 10_000),
            ..AgentActivity::default()
        };
        // Sent, not visibly started yet: held.
        assert!(super::claim_holds(
            &claim,
            "me",
            Some(AgentState::Done),
            &before,
            at + 800
        ));
        // Our own claim never blocks us.
        assert!(!super::claim_holds(
            &claim,
            "other",
            Some(AgentState::Done),
            &before,
            at + 800
        ));
        // Their turn finished: free.
        let finished = AgentActivity {
            work_started_at: Some(at + 1_000),
            completed_at: Some(at + 9_000),
            ..AgentActivity::default()
        };
        assert!(!super::claim_holds(
            &claim,
            "me",
            Some(AgentState::Done),
            &finished,
            at + 9_500
        ));
        // Left by a muxel that went away before anything happened: lapses.
        assert!(!super::claim_holds(
            &claim,
            "me",
            Some(AgentState::Idle),
            &before,
            at + super::REPLY_GRACE_MS
        ));
    }

    #[test]
    fn claim_compare_patterns_keep_modern_nonce_and_refuse_unsafe_legacy_values() {
        let claim = super::Claim {
            owner: "owner".into(),
            host: "desk'name".into(),
            at: 42,
            grace_ms: super::REPLY_GRACE_MS,
            nonce: Some(uuid::Uuid::from_u128(7)),
        };
        assert_eq!(
            claim.compare_pattern(),
            Some(super::Claim::pattern("owner", uuid::Uuid::from_u128(7)))
        );
        let legacy = super::Claim {
            nonce: None,
            host: "desk".into(),
            ..claim
        };
        assert_eq!(legacy.compare_pattern(), Some(legacy.encode()));
        assert!(
            super::Claim {
                host: "desk'name".into(),
                ..legacy
            }
            .compare_pattern()
            .is_none()
        );
    }
}
