//! Cast replay: recorded provider sessions played through muxel's own parser
//! and readers.
//!
//! A cast (`fixtures/casts/*.cast.jsonl`) is one provider session as its PTY
//! reader saw it: a JSON header line, then `[t_ms, kind, payload]` lines, where
//! `out` is one read, `mark` says what was true about the provider at that
//! moment, and `in`, `reply`, `resize`, `focus`, `hook`, `exit` record the rest.
//! See `fixtures/casts/README.md`.
//!
//! Replay feeds every read to a [`TerminalSession::replay`] session at its
//! recorded time, drains and parses it as a pane does, reconciles status the
//! way the view does after each read, and at each mark
//! settles the screen and asks the readers what they see. A mark's truth is
//! compared with those verdicts through [`compare`]; known disagreements are
//! listed in [`EXPECTED_GAPS`] with the finding each belongs to. Adding a cast
//! adds coverage with no new code.

use super::{
    AgentStatus, RuntimeStatusState, TerminalStatusReason, TitleProvider, startup_empty_composer,
    startup_ready, status_coverage,
};
use crate::session::{
    ProfiledPtyChunk, ProfiledPtyReceiver, PtyOutput, ReplayFeed, ReplayWrites, TerminalSession,
};
use muxel_core::control::{QuestionOption, StartupBlockerKind};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// A known disagreement: `file#mark`, the compared key, the wrong answer the
/// reader gives there (its `read`, exactly), and the finding it belongs to.
type Gap = (&'static str, &'static str, &'static str, &'static str);

/// Known disagreements between a cast's truth and muxel's readers. A gap holds
/// only its own wrong answer: a different wrong answer fails, as a different
/// fault may hide behind the listed one. A gap that closes fails until its
/// entry is removed, so a fix on the branch that owns it shows up here, and a
/// gap no comparison reaches fails as lost coverage.
#[rustfmt::skip]
const EXPECTED_GAPS: &[Gap] = &[
    ("claude-2.1.310-windows-question#blocked", "overlay/options", "4 Chat about this", CLAUDE_QUESTION_RULE),
    ("claude-2.1.312-windows-clear#slash-menu-clear", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("claude-2.1.322-windows-composer-tour#idle-first", "draft/empty", "not empty", HELPER_CPR),
    ("claude-2.1.322-windows-composer-tour#idle-first", "takes_prompt/startup", "startup_ready=false", HELPER_CPR),
    ("claude-2.1.322-windows-composer-tour#draft-end", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("claude-2.1.322-windows-composer-tour#draft-home", "draft/empty", "empty", HOME_DRAFT),
    ("claude-2.1.322-windows-composer-tour#draft-home", "takes_prompt/startup", "startup_ready=true", HOME_DRAFT),
    ("claude-2.1.322-windows-composer-tour#draft-home", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("claude-2.1.322-windows-composer-tour#draft-mid", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("claude-2.1.322-windows-composer-tour#slash-menu-model", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("claude-2.1.322-windows-composer-tour#model-picker", "takes_prompt/send", "send admitted=true", SEND_COVERED),
    ("claude-2.1.322-windows-composer-tour#slash-menu-ex", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("claude-2.1.322-windows-permission-parked-draft#idle-first", "draft/empty", "not empty", HELPER_CPR),
    ("claude-2.1.322-windows-permission-parked-draft#idle-first", "takes_prompt/startup", "startup_ready=false", HELPER_CPR),
    ("claude-2.1.322-windows-permission-parked-draft#after-answer", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("claude-2.1.322-windows-question#question-card", "overlay/options", "4 Chat about this", CLAUDE_QUESTION_RULE),
    ("codex-0.164.0-windows-command-approval#idle", "composer", "no box", CODEX_FOOTER),
    ("codex-0.164.0-windows-command-approval#idle", "draft/empty", "not empty", CODEX_FOOTER),
    ("codex-0.164.0-windows-command-approval#idle", "takes_prompt/startup", "startup_ready=false", CODEX_TITLE_UNSET),
    ("codex-0.164.0-windows-question#idle", "composer", "no box", CODEX_FOOTER),
    ("codex-0.164.0-windows-question#idle", "draft/empty", "not empty", CODEX_FOOTER),
    ("codex-0.164.0-windows-question#idle", "takes_prompt/startup", "startup_ready=false", CODEX_TITLE_UNSET),
    ("codex-0.164.0-windows-question#blocked", "overlay/options", "1 Alpha (Recommended)  Choose Alpha for this request. (selected) | 2 Beta                 Choose Beta for this request. | 3 None of the above    Optionally, add details in notes (tab)", CODEX_OPTION_DESCRIPTION),
    ("codex-0.164.0-windows-trust-card#trust-card", "takes_prompt/send", "send admitted=true", SEND_COVERED),
    ("codex-0.164.0-windows-turn#idle", "composer", "no box", CODEX_FOOTER),
    ("codex-0.164.0-windows-turn#idle", "draft/empty", "not empty", CODEX_FOOTER),
    ("codex-0.164.0-windows-turn#post-turn-idle-candidate", "composer", "no box", CODEX_FOOTER),
    ("codex-0.164.0-windows-turn#post-turn-idle-candidate", "draft/empty", "not empty", CODEX_FOOTER),
    ("codex-0.164.0-windows-turn#post-turn-idle-candidate", "takes_prompt/startup", "startup_ready=false", CODEX_TITLE_UNSET),
    ("codex-0.167.2-windows-composer-tour#idle-first", "composer", "no box", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#idle-first", "draft/empty", "not empty", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#idle-first", "takes_prompt/startup", "startup_ready=false", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#draft-end", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#draft-home", "draft/empty", "empty", HOME_DRAFT),
    ("codex-0.167.2-windows-composer-tour#draft-home", "takes_prompt/startup", "startup_ready=true", HOME_DRAFT),
    ("codex-0.167.2-windows-composer-tour#draft-home", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#draft-mid", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#draft-backspaced", "composer", "no box", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#draft-backspaced", "draft/empty", "not empty", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#draft-backspaced", "takes_prompt/startup", "startup_ready=false", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#ctrl-u-cleared", "composer", "no box", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#ctrl-u-cleared", "draft/empty", "not empty", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#ctrl-u-cleared", "takes_prompt/startup", "startup_ready=false", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#after-turn", "composer", "no box", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#after-turn", "draft/empty", "not empty", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#after-turn", "takes_prompt/startup", "startup_ready=false", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#up-recall", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#slash-menu-open", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#slash-menu-model", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#idle-last", "composer", "no box", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#idle-last", "draft/empty", "not empty", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#idle-last", "takes_prompt/startup", "startup_ready=false", CODEX_AGENTS_HINT),
    ("grok-1.0.70-windows-question#idle", "takes_prompt/startup", "startup_ready=true", GROK_DRAFT),
    ("grok-1.0.70-windows-question#idle", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("grok-1.0.70-windows-question#blocked", "overlay", "question with 0 options", GROK_OPTIONS),
    ("grok-1.0.70-windows-question#blocked", "overlay/options", "", GROK_OPTIONS),
    ("grok-1.0.70-windows-trust-card#trust-card", "overlay", "none", GROK_TRUST),
    ("grok-1.0.70-windows-trust-card#trust-card", "takes_prompt/startup", "startup_ready=true", GROK_TRUST),
    ("grok-1.0.70-windows-trust-card#trust-card", "takes_prompt/send", "send admitted=true", SEND_COVERED),
    ("grok-1.0.77-windows-composer-tour#trust-card", "overlay", "none", GROK_TRUST),
    ("grok-1.0.77-windows-composer-tour#trust-card", "takes_prompt/startup", "startup_ready=true", GROK_TRUST),
    ("grok-1.0.77-windows-composer-tour#trust-card", "takes_prompt/send", "send admitted=true", SEND_COVERED),
    ("grok-1.0.77-windows-composer-tour#draft-end", "takes_prompt/startup", "startup_ready=true", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#draft-end", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("grok-1.0.77-windows-composer-tour#draft-home", "takes_prompt/startup", "startup_ready=true", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#draft-home", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("grok-1.0.77-windows-composer-tour#draft-mid", "takes_prompt/startup", "startup_ready=true", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#draft-mid", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("grok-1.0.77-windows-composer-tour#turn-running", "turn", "Idle (ProviderTitle)", GROK_SPINNER),
    ("grok-1.0.77-windows-composer-tour#after-turn", "finished", "Idle", GROK_SPINNER),
    ("grok-1.0.77-windows-composer-tour#up-recall", "takes_prompt/startup", "startup_ready=true", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#up-recall", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("grok-1.0.77-windows-composer-tour#slash-menu-open", "takes_prompt/startup", "startup_ready=true", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#slash-menu-open", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
    ("grok-1.0.77-windows-composer-tour#slash-menu-model", "takes_prompt/startup", "startup_ready=true", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#slash-menu-model", "takes_prompt/send", "send admitted=true", SEND_DRAFT),
];

const SEND_DRAFT: &str = "ctl send doesn't read the composer, so it adds to a draft";
const SEND_COVERED: &str =
    "ctl send doesn't read the composer, so it types while a picker or card covers it";
const HOME_DRAFT: &str =
    "a draft with its cursor at Home reads as an empty composer (Claude, Codex)";
const CODEX_FOOTER: &str = "the Codex composer reader needs a footer it knows; 0.164.0's status \
    line opens with the model's display name, which here has no digit, and the first frame has \
    no footer yet";
const CODEX_AGENTS_HINT: &str = "while its box is empty, Codex 0.167.2 ends the status line with \
    `· ← for agents`; the footer check takes that segment for a key hint and rejects the line, \
    and no other footer matches, so an empty Codex box is never found";
const GROK_DRAFT: &str = "Grok has no composer reader, so startup input can't see a draft";
const CLAUDE_QUESTION_RULE: &str = "Claude draws a rule between a question's choices and \
    `Chat about this`, and the option reader stops at it: ctl show lists only `4 Chat about this`, \
    and `ctl answer 1` is refused";
const CODEX_OPTION_DESCRIPTION: &str = "a Codex option's label runs on into its description \
    column; ctl answer picks by key, so only the label ctl show lists and the reply echoes is wrong";
const GROK_OPTIONS: &str =
    "ctl show and ctl answer find no numbered options in Grok's question rows (`1 (○) Alpha`)";
const GROK_TRUST: &str = "Grok's trust card (`Do you trust the contents of this directory?`, \
    1.0.70 and 1.0.77) isn't read as a startup blocker";
const GROK_SPINNER: &str = "Grok 1.0.77 spins its title with `| / - \\`, and muxel knows only the \
    braille frames, so a turn reads Idle throughout";
/// A fault of the recording, not of muxel: these Codex casts were launched
/// without the terminal-title override muxel gives every Codex pane, so their
/// title is the bare thread id with no run-state, and startup input, which
/// waits for an Idle title, never fires. Goes when they are re-recorded.
const CODEX_TITLE_UNSET: &str =
    "recording: Codex ran without muxel's terminal-title override, so no Idle title";
/// A fault of the recording, not of muxel: the recorder answered Claude's
/// cursor-position query with 1;1 while the cursor sat in the box, and Claude
/// moved its cursor there. Muxel answers with the real position. Goes when
/// these casts are recorded through muxel's own session.
const HELPER_CPR: &str = "recording: cursor-position reply was 1;1, so the cursor left the box";

struct Cast {
    name: String,
    header: Map<String, Value>,
    events: Vec<(u64, String, Value)>,
}

fn decode_bytes(payload: &Value) -> Vec<u8> {
    match payload {
        Value::String(text) => text.as_bytes().to_vec(),
        Value::Object(object) => {
            let hex = object["hex"].as_str().expect("hex payload");
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex digit"))
                .collect()
        }
        other => panic!("bytes payload must be a string or {{hex}}, got {other}"),
    }
}

fn load(path: &Path) -> Cast {
    let text = std::fs::read_to_string(path).expect("read cast");
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let header: Map<String, Value> =
        serde_json::from_str(lines.next().expect("header line")).expect("header is an object");
    let events = lines
        .map(|line| {
            let value: Value = serde_json::from_str(line).expect("event line is JSON");
            let parts = value.as_array().expect("event is [t, kind, payload]");
            (
                parts[0].as_u64().expect("t is whole milliseconds"),
                parts[1].as_str().expect("kind").to_string(),
                parts[2].clone(),
            )
        })
        .collect();
    Cast {
        name: path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".cast.jsonl"))
            .expect("named *.cast.jsonl")
            .to_string(),
        header,
        events,
    }
}

fn casts_in(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.ends_with(".cast.jsonl"))
                })
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
}

fn repo_casts() -> Vec<Cast> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/fixtures/casts");
    let casts: Vec<_> = casts_in(&dir).iter().map(|path| load(path)).collect();
    assert!(!casts.is_empty(), "no casts under {}", dir.display());
    casts
}

/// What muxel's readers say at one moment.
#[derive(Clone, Debug, PartialEq)]
struct Verdicts {
    /// The runtime status, as the sidebar and `ctl status` start from it.
    status: AgentStatus,
    reason: TerminalStatusReason,
    /// `ctl send`'s admission without `--force`: an attributable Idle or Done,
    /// or a turn held only for background work.
    send_admitted: bool,
    /// The options `ctl show`/`ctl answer` read while the status is Blocked.
    question_options: Option<Vec<QuestionOption>>,
    /// The startup blocker `ctl new` reports (folder trust, login, update).
    blocker: Option<String>,
    /// The provider's input box as status finds it: its row range and the text
    /// after the prompt glyph (placeholder or draft; the screen can't tell).
    composer: Option<(usize, usize)>,
    composer_text: Option<String>,
    /// The empty-composer reading startup input relies on.
    empty_composer: bool,
    /// Whether startup input would type a task now (`startup_ready`, empty).
    ready: bool,
}

/// One cast played through a session with no child.
struct Replay {
    session: Arc<TerminalSession>,
    /// Gone once the cast's child has exited.
    feed: Option<ReplayFeed>,
    output: ProfiledPtyReceiver,
    writes: ReplayWrites,
    runtime: RuntimeStatusState,
    provider: TitleProvider,
    working: Vec<String>,
    blocked: Vec<String>,
    base: Instant,
    base_wall: SystemTime,
    deadline: Option<Instant>,
    exited: bool,
}

impl Replay {
    fn new(cast: &Cast) -> Self {
        let provider_name = cast.header["provider"].as_str().expect("provider");
        let cols = cast.header["cols"].as_u64().expect("cols") as u16;
        let rows = cast.header["rows"].as_u64().expect("rows") as u16;
        let (session, feed, output, writes) = TerminalSession::replay(cols, rows);
        let provider = TitleProvider::from_program(provider_name);
        // The built-in preset's markers, as a fresh pane gets them.
        let preset = muxel_core::AgentPreset::defaults()
            .into_iter()
            .find(|preset| preset.program.as_deref() == Some(provider_name));
        let (working, blocked) = preset
            .map(|preset| (preset.working_markers, preset.blocked_markers))
            .unwrap_or_default();
        Self {
            session,
            feed: Some(feed),
            output,
            writes,
            runtime: RuntimeStatusState::new(status_coverage(provider, &working, &blocked)),
            provider,
            working,
            blocked,
            base: Instant::now(),
            base_wall: SystemTime::now(),
            deadline: None,
            exited: false,
        }
    }

    fn at(&self, t: u64) -> (Instant, SystemTime) {
        let offset = Duration::from_millis(t);
        (self.base + offset, self.base_wall + offset)
    }

    /// One reconcile, keeping the earliest pending deadline as the view does.
    fn reconcile(&mut self, now: Instant) {
        let binding = self.runtime.binding_rev;
        let (_, deadline) = self.runtime.reconcile(
            &self.session,
            self.provider,
            &self.working,
            &self.blocked,
            self.exited,
            now,
        );
        let identity_changed = binding != self.runtime.binding_rev;
        match (self.deadline, deadline) {
            (Some(current), Some(next)) if current <= next && !identity_changed => {}
            _ => self.deadline = deadline,
        }
    }

    /// Fire the runtime's own timers that fall due by `now`, in order.
    fn run_due(&mut self, now: Instant) {
        for _ in 0..64 {
            match self.deadline {
                Some(due) if due <= now => {
                    self.deadline = None;
                    self.reconcile(due);
                }
                _ => return,
            }
        }
    }

    /// Parse what the feed queued, as a pane's drain does.
    fn drain(&mut self) {
        let mut output = PtyOutput::default();
        while let Ok(chunk) = self.output.try_recv() {
            match chunk {
                ProfiledPtyChunk::Output { bytes, read_at, .. } => output.push(&bytes, read_at),
                ProfiledPtyChunk::Exit { .. } => self.exited = true,
            }
        }
        if !output.is_empty() {
            self.session.process_drained(&output);
        }
    }

    fn read(&mut self, t: u64, bytes: &[u8]) {
        let (now, wall) = self.at(t);
        self.run_due(now);
        if let Some(feed) = &mut self.feed {
            feed.read(bytes, wall);
        }
        self.drain();
        self.reconcile(now);
    }

    /// What was typed, written as a pane's keyboard writes it: an Enter on its
    /// own announces a turn first, stamped with the cast's clock. The bytes
    /// land in the replay writer.
    fn input(&mut self, t: u64, bytes: &[u8]) {
        let (now, wall) = self.at(t);
        self.run_due(now);
        if super::is_submission_input(bytes) {
            self.session.mark_turn_submitted_at(wall);
        }
        self.session.write_input(bytes);
        self.reconcile(now);
    }

    fn exit(&mut self, t: u64, code: Option<i32>) {
        let (now, _) = self.at(t);
        self.run_due(now);
        if let Some(feed) = self.feed.take() {
            feed.exit(code);
        }
        self.drain();
        self.reconcile(now);
    }

    /// A mark's truth is the settled screen: every read before it processed
    /// and no synchronized frame left open.
    fn settle(&mut self, t: u64) {
        let (now, wall) = self.at(t);
        self.run_due(now);
        self.session.end_sync_frame_at(wall);
        // Status scans the screen at most every 50 ms, so a read just before
        // the mark may not be scanned yet. The view scans it moments later;
        // the mark asks what that scan finds.
        self.runtime.last_screen_capture = None;
        self.reconcile(now);
    }

    fn verdicts(&self) -> Verdicts {
        let snapshot = self.runtime.snapshot(&self.session);
        let binding = self.session.binding_revision();
        let screen = self.session.startup_screen();
        let title = self.session.title();
        let background_over = snapshot.binding_rev == binding
            && ((snapshot.status == AgentStatus::Working
                && snapshot.reason == TerminalStatusReason::BackgroundWork)
                || (matches!(snapshot.status, AgentStatus::Idle | AgentStatus::Done)
                    && snapshot.background.is_some_and(|work| !work.is_empty())));
        let send_admitted = snapshot.is_attributable_to(binding)
            && (matches!(snapshot.status, AgentStatus::Idle | AgentStatus::Done)
                || background_over);
        let question_options = (snapshot.status == AgentStatus::Blocked)
            .then(|| muxel_core::control::pending_question(&self.session.visible_text()))
            .flatten()
            .map(|question| question.options);
        let blocker = screen.startup_blocker().map(|blocker| match blocker.kind {
            StartupBlockerKind::FolderTrust => "folder_trust".to_string(),
            other => format!("{other:?}"),
        });
        let (region, glyph) = match self.provider {
            TitleProvider::Claude => (screen.claude_composer_region(), '❯'),
            TitleProvider::Codex => (screen.codex_composer_region(), '›'),
            _ => (None, ' '),
        };
        let composer_text = region.clone().map(|region| {
            let rows: Vec<_> = screen.text.lines().collect();
            let first = rows[region.start].trim_start().trim_start_matches(glyph);
            let text = std::iter::once(first)
                .chain(rows[region.start + 1..region.end].iter().copied())
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ");
            crate::startup::normalized(&text)
        });
        Verdicts {
            status: snapshot.status,
            reason: snapshot.reason,
            send_admitted,
            question_options,
            blocker,
            composer: region.map(|region| (region.start, region.end)),
            composer_text,
            empty_composer: startup_empty_composer(self.provider, &screen),
            ready: startup_ready(
                self.provider,
                &screen,
                title.as_deref(),
                &self.blocked,
                true,
            ),
        }
    }

    /// The rows a verdict came from, for a failure report: the input box when
    /// one was found, else the bottom of the screen.
    fn rows_used(&self, verdicts: &Verdicts) -> String {
        let screen = self.session.startup_screen();
        let rows: Vec<_> = screen.text.lines().collect();
        let range = match verdicts.composer {
            Some((start, end)) => start.saturating_sub(1)..(end + 1).min(rows.len()),
            None => {
                let bottom = rows
                    .iter()
                    .rposition(|row| !row.trim().is_empty())
                    .map_or(0, |bottom| bottom + 1);
                bottom.saturating_sub(8)..bottom
            }
        };
        let mut out = format!(
            "      title {:?}, cursor row {} col {}{}\n",
            self.session.title(),
            screen.cursor_row,
            screen.cursor_column,
            if screen.cursor_visible {
                ""
            } else {
                " (hidden)"
            }
        );
        for index in range {
            out.push_str(&format!("      {index:>2}|{}\n", rows[index].trim_end()));
        }
        out
    }
}

/// One truth key held against the reader that answers it.
struct Check {
    key: &'static str,
    truth: String,
    read: String,
    agrees: bool,
}

/// One check at one mark (`file#mark`), with the screen rows the verdict came
/// from for a failure report.
struct Compared {
    at: String,
    check: Check,
    rows: String,
}

/// How a run's checks stand against the known gaps.
#[derive(Debug, Default)]
struct Ledger {
    agree: usize,
    /// Disagreements that match a gap's wrong answer exactly.
    known: usize,
    /// Disagreements no gap lists.
    unexpected: Vec<String>,
    /// Listed gaps whose reader now gives a different wrong answer.
    changed: Vec<String>,
    /// Listed gaps whose reader now agrees: a fix, so remove the entry.
    closed: Vec<String>,
    /// Listed gaps no check reached: lost coverage, not a fix.
    uncompared: Vec<String>,
}

impl Ledger {
    fn clean(&self) -> bool {
        self.unexpected.is_empty()
            && self.changed.is_empty()
            && self.closed.is_empty()
            && self.uncompared.is_empty()
    }
}

/// Holds every check against `gaps`: a gap covers only its own mark, key and
/// wrong answer.
fn judge(compared: &[Compared], gaps: &[Gap]) -> Ledger {
    let mut ledger = Ledger::default();
    let mut reached = vec![false; gaps.len()];
    for Compared { at, check, rows } in compared {
        let gap = gaps
            .iter()
            .position(|(gap_at, key, _, _)| gap_at == at && *key == check.key);
        if let Some(index) = gap {
            reached[index] = true;
        }
        let (key, truth, read) = (check.key, &check.truth, &check.read);
        match (check.agrees, gap.map(|index| gaps[index])) {
            (true, None) => ledger.agree += 1,
            (true, Some((_, _, _, finding))) => {
                ledger.agree += 1;
                ledger
                    .closed
                    .push(format!("{at} {key}: reads {read} ({finding})"));
            }
            (false, None) => ledger
                .unexpected
                .push(format!("{at} {key}: truth {truth}, read {read}\n{rows}")),
            (false, Some((_, _, wrong, _))) if wrong == read => ledger.known += 1,
            (false, Some((_, _, wrong, finding))) => ledger.changed.push(format!(
                "{at} {key}: truth {truth}, the gap expects {wrong:?}, read {read:?} ({finding})\n{rows}"
            )),
        }
    }
    for (index, (at, key, wrong, finding)) in gaps.iter().enumerate() {
        if !reached[index] {
            ledger
                .uncompared
                .push(format!("{at} {key}: expected {wrong:?} ({finding})"));
        }
    }
    ledger
}

/// Each truth key the readers can answer. Keys with no reader for this
/// provider are left out, never guessed.
fn compare(provider: TitleProvider, truth: &Map<String, Value>, v: &Verdicts) -> Vec<Check> {
    let mut out = Vec::new();
    let mut check = |key, truth: String, read: String, agrees| {
        out.push(Check {
            key,
            truth,
            read,
            agrees,
        })
    };
    let turn = truth.get("turn").and_then(Value::as_str);
    let overlay = truth.get("overlay").and_then(Value::as_str);
    if let Some(turn) = turn {
        let agrees = match turn {
            // A trust card needs the person before any turn runs, and muxel
            // may show it as Blocked, which also keeps `ctl send` out.
            "none" => {
                matches!(v.status, AgentStatus::Idle | AgentStatus::Done)
                    || (v.status == AgentStatus::Blocked && overlay == Some("trust"))
            }
            "running" => v.status == AgentStatus::Working,
            "waiting" => v.status == AgentStatus::Blocked,
            "background" => {
                v.reason == TerminalStatusReason::BackgroundWork
                    || matches!(v.status, AgentStatus::Idle | AgentStatus::Done)
            }
            other => panic!("unknown turn {other:?}"),
        };
        let read = format!("{:?} ({:?})", v.status, v.reason);
        check("turn", turn.to_string(), read, agrees);
    }
    if let Some(finished) = truth.get("finished").and_then(Value::as_u64)
        && turn.is_none_or(|turn| turn == "none")
    {
        let agrees = (v.status == AgentStatus::Done) == (finished > 0);
        check(
            "finished",
            finished.to_string(),
            format!("{:?}", v.status),
            agrees,
        );
    }
    if let Some(overlay) = overlay
        // No reader names a picker or a slash menu; the composer and
        // takes_prompt keys say whether muxel would type into one.
        && !matches!(overlay, "picker" | "slash_menu" | "other")
    {
        let read = match (&v.blocker, &v.question_options) {
            (Some(blocker), _) => blocker.clone(),
            (None, Some(options)) => format!("question with {} options", options.len()),
            (None, None) => "none".to_string(),
        };
        let agrees = match overlay {
            "none" => v.blocker.is_none() && v.question_options.is_none(),
            "trust" => v.blocker.as_deref() == Some("folder_trust"),
            "question" | "permission" => v
                .question_options
                .as_ref()
                .is_some_and(|options| !options.is_empty()),
            other => panic!("unknown overlay {other:?}"),
        };
        check("overlay", overlay.to_string(), read, agrees);
    }
    // `ctl answer` picks an option by its key, so every option must be read:
    // its key, its label, and which one the cursor is on. A truth that can't
    // see the cursor records no `selected`, and then selection isn't compared.
    if let Some(want) = truth.get("options") {
        let want: Vec<QuestionOption> =
            serde_json::from_value(want.clone()).expect("options truth is [{key, label}]");
        let with_cursor = want.iter().any(|option| option.selected);
        let listed = |options: &[QuestionOption]| {
            options
                .iter()
                .map(|option| {
                    let cursor = if with_cursor && option.selected {
                        " (selected)"
                    } else {
                        ""
                    };
                    format!("{} {}{cursor}", option.key, option.label.trim())
                })
                .collect::<Vec<_>>()
                .join(" | ")
        };
        let read = v
            .question_options
            .as_deref()
            .map_or_else(|| "no question".to_string(), listed);
        let truth = listed(&want);
        let agrees = read == truth;
        check("overlay/options", truth, read, agrees);
    }
    if matches!(provider, TitleProvider::Claude | TitleProvider::Codex) {
        if let Some(composer) = truth.get("composer").and_then(Value::as_str) {
            let read = match v.composer {
                Some((start, end)) => format!("box at rows {start}..{end}"),
                None => "no box".to_string(),
            };
            let agrees = (composer == "shown") == v.composer.is_some();
            check("composer", composer.to_string(), read, agrees);
        }
        // A draft parked behind an overlay isn't on the screen, so no screen
        // reader can answer for it; takes_prompt still covers the gate.
        let draft_visible = !matches!(
            truth.get("composer").and_then(Value::as_str),
            Some("hidden" | "absent")
        );
        if let Some(draft) = truth.get("draft").and_then(Value::as_str)
            && draft_visible
        {
            // Startup input's empty reading: empty when the cursor sits at the
            // start of the box, whatever the box holds.
            let read = if v.empty_composer {
                "empty"
            } else {
                "not empty"
            };
            let agrees = v.empty_composer == draft.is_empty();
            check(
                "draft/empty",
                format!("{draft:?}"),
                read.to_string(),
                agrees,
            );
            // The rows the reader took for the box must hold the draft. An
            // empty box shows a placeholder the text can't tell from a draft,
            // so only a non-empty draft is compared.
            if !draft.is_empty() && v.composer.is_some() {
                let text = v.composer_text.clone().unwrap_or_default();
                let agrees = text == crate::startup::normalized(draft);
                check(
                    "draft/rows",
                    format!("{draft:?}"),
                    format!("{text:?}"),
                    agrees,
                );
            }
        }
    }
    if let Some(takes) = truth.get("takes_prompt").and_then(Value::as_bool) {
        let read = format!("startup_ready={}", v.ready);
        check(
            "takes_prompt/startup",
            takes.to_string(),
            read,
            v.ready == takes,
        );
        let read = format!("send admitted={}", v.send_admitted);
        check(
            "takes_prompt/send",
            takes.to_string(),
            read,
            v.send_admitted == takes,
        );
    }
    out
}

/// What replay found at one mark.
struct MarkRead {
    id: String,
    truth: Map<String, Value>,
    verdicts: Verdicts,
    /// The screen rows the verdicts came from, for a failure report.
    rows: String,
}

/// Play a cast and read every mark.
fn replay(cast: &Cast) -> Vec<MarkRead> {
    let mut replay = Replay::new(cast);
    let mut marks = Vec::new();
    for (t, kind, payload) in &cast.events {
        match kind.as_str() {
            "out" => replay.read(*t, &decode_bytes(payload)),
            "in" => replay.input(*t, &decode_bytes(payload)),
            // The session settles a resize on the real clock, so replay would
            // only set a target the grid never reaches. Refuse rather than
            // replay a different screen.
            "resize" => panic!("{}: resize at {t} isn't replayed yet", cast.name),
            "exit" => replay.exit(*t, payload["code"].as_i64().map(|code| code as i32)),
            "mark" => {
                replay.settle(*t);
                let verdicts = replay.verdicts();
                let rows = replay.rows_used(&verdicts);
                marks.push(MarkRead {
                    id: payload["id"].as_str().expect("mark id").to_string(),
                    truth: payload["truth"].as_object().cloned().unwrap_or_default(),
                    verdicts,
                    rows,
                });
            }
            // Replies, focus and hooks don't touch the grid in replay.
            _ => {}
        }
    }
    marks
}

/// What muxel wrote back for the last read holding `query` before `mark`.
fn answer_before(cast: &Cast, query: &[u8], mark: &str) -> Vec<u8> {
    let mut replay = Replay::new(cast);
    let mut answer = Vec::new();
    for (t, kind, payload) in &cast.events {
        match kind.as_str() {
            "out" => {
                let bytes = decode_bytes(payload);
                replay.writes.take();
                replay.read(*t, &bytes);
                if bytes.windows(query.len()).any(|window| window == query) {
                    answer = replay.writes.take();
                }
            }
            "mark" if payload["id"] == mark => return answer,
            _ => {}
        }
    }
    panic!("{}: no mark {mark}", cast.name);
}

/// The replay session catches what muxel writes back. Claude asks for the
/// cursor position just after drawing its first idle frame, with the cursor
/// in the box at row 38, column 3. Muxel answers with that position, where
/// the recorder answered 1;1 and so moved Claude's cursor out of the box
/// (`HELPER_CPR`).
#[test]
fn muxel_answers_claudes_cursor_query_with_where_the_cursor_is() {
    let cast = repo_casts()
        .into_iter()
        .find(|cast| cast.name == "claude-2.1.322-windows-composer-tour")
        .expect("composer tour cast");
    let answer = answer_before(&cast, b"\x1b[6n", "idle-first");
    assert_eq!(String::from_utf8_lossy(&answer), "\x1b[38;3R");
}

#[test]
fn every_mark_in_every_cast_agrees_with_the_readers() {
    let mut compared = Vec::new();
    let (mut casts, mut marks) = (0, 0);
    for cast in repo_casts() {
        casts += 1;
        let provider =
            TitleProvider::from_program(cast.header["provider"].as_str().expect("provider"));
        for MarkRead {
            id,
            truth,
            verdicts,
            rows,
        } in replay(&cast)
        {
            marks += 1;
            let at = format!("{}#{id}", cast.name);
            for check in compare(provider, &truth, &verdicts) {
                compared.push(Compared {
                    at: at.clone(),
                    check,
                    rows: rows.clone(),
                });
            }
        }
    }
    let ledger = judge(&compared, EXPECTED_GAPS);
    eprintln!(
        "{casts} casts, {marks} marks, {} checks: {} agree, {} known gaps",
        compared.len(),
        ledger.agree,
        ledger.known,
    );
    assert!(
        ledger.clean(),
        "{} comparisons.\n\n\
         Disagreements not in EXPECTED_GAPS:\n{}\n\
         Known gaps whose wrong answer changed (another fault may stand behind the listed \
         one; update the entry only once you know which):\n{}\n\
         Gaps that closed: the reader agrees now (remove them):\n{}\n\
         Gaps no check reached: lost coverage, not a fix:\n{}",
        compared.len(),
        ledger.unexpected.join("\n"),
        ledger.changed.join("\n"),
        ledger.closed.join("\n"),
        ledger.uncompared.join("\n"),
    );
}

/// A gap holds its mark, key and wrong answer and nothing else, and the four
/// ways a gap can stand are told apart.
#[test]
fn a_gap_holds_only_its_own_wrong_answer() {
    const GAPS: &[Gap] = &[("cast#mark", "turn", "Idle (ProviderTitle)", "a finding")];
    let one = |at: &str, agrees, read: &str| {
        vec![Compared {
            at: at.to_string(),
            check: Check {
                key: "turn",
                truth: "running".to_string(),
                read: read.to_string(),
                agrees,
            },
            rows: String::new(),
        }]
    };
    let known = judge(&one("cast#mark", false, "Idle (ProviderTitle)"), GAPS);
    assert!(known.clean() && known.known == 1);
    let changed = judge(&one("cast#mark", false, "Done (ProviderTitle)"), GAPS);
    assert_eq!((changed.changed.len(), changed.known), (1, 0));
    let closed = judge(&one("cast#mark", true, "Working (ProviderTitle)"), GAPS);
    assert_eq!((closed.closed.len(), closed.agree), (1, 1));
    let elsewhere = judge(&one("cast#other", false, "Idle (ProviderTitle)"), GAPS);
    assert_eq!(
        (elsewhere.unexpected.len(), elsewhere.uncompared.len()),
        (1, 1)
    );
    let unreached = judge(&[], GAPS);
    assert_eq!((unreached.uncompared.len(), unreached.closed.len()), (1, 0));
}

/// Every option counts: one dropped, one relabelled or the cursor on another
/// disagrees, though the overlay still reads as a question.
#[test]
fn a_question_agrees_only_with_all_its_options() {
    let option = |key: &str, label: &str, selected| QuestionOption {
        key: key.to_string(),
        label: label.to_string(),
        selected,
    };
    let shown = vec![
        option("1", "Alpha", true),
        option("2", "Beta", false),
        option("3", "Type something.", false),
    ];
    let checks = |truth: Value, read: Vec<QuestionOption>| {
        let truth: Map<String, Value> = serde_json::from_value(truth).expect("truth");
        let verdicts = Verdicts {
            status: AgentStatus::Blocked,
            reason: TerminalStatusReason::BlockedMarker,
            send_admitted: false,
            question_options: Some(read),
            blocker: None,
            composer: None,
            composer_text: None,
            empty_composer: false,
            ready: false,
        };
        let found = compare(TitleProvider::Claude, &truth, &verdicts);
        let agrees = |key| {
            found
                .iter()
                .find(|check| check.key == key)
                .map(|c| c.agrees)
        };
        (agrees("overlay"), agrees("overlay/options"))
    };
    let truth = serde_json::json!({
        "overlay": "question",
        "options": [
            { "key": "1", "label": "Alpha", "selected": true },
            { "key": "2", "label": "Beta" },
            { "key": "3", "label": "Type something." },
        ],
    });
    assert_eq!(
        checks(truth.clone(), shown.clone()),
        (Some(true), Some(true))
    );
    assert_eq!(
        checks(truth.clone(), shown[..2].to_vec()),
        (Some(true), Some(false))
    );
    let mut relabelled = shown.clone();
    relabelled[1].label = "Gamma".to_string();
    assert_eq!(checks(truth.clone(), relabelled), (Some(true), Some(false)));
    let mut moved = shown.clone();
    (moved[0].selected, moved[1].selected) = (false, true);
    assert_eq!(checks(truth, moved.clone()), (Some(true), Some(false)));
    // A truth that can't see the cursor doesn't compare it.
    let unseen = serde_json::json!({
        "options": [
            { "key": "1", "label": "Alpha" },
            { "key": "2", "label": "Beta" },
            { "key": "3", "label": "Type something." },
        ],
    });
    assert_eq!(checks(unseen, moved), (None, Some(true)));
}

/// The format's own promises, so a hand edit or a converter bug can't slip a
/// broken cast in: a sanitized v1 header with the fields replay needs, times
/// that never go back, known event kinds, and mark ids unique in the file
/// (a scenario points at `file#id`).
#[test]
fn casts_are_well_formed() {
    for cast in repo_casts() {
        for field in [
            "provider",
            "version",
            "os",
            "cols",
            "rows",
            "tap",
            "out_sha256",
        ] {
            assert!(
                cast.header.contains_key(field),
                "{}: header lacks {field}",
                cast.name
            );
        }
        assert_eq!(
            cast.header["muxel_cast"], 1,
            "{}: format version",
            cast.name
        );
        assert_eq!(
            cast.header["sanitized"], true,
            "{}: not sanitized",
            cast.name
        );
        let mut last = 0;
        let mut ids = std::collections::HashSet::new();
        for (t, kind, payload) in &cast.events {
            assert!(*t >= last, "{}: time goes back at {t}", cast.name);
            last = *t;
            match kind.as_str() {
                "out" | "in" | "reply" => {
                    decode_bytes(payload);
                }
                "mark" => {
                    let id = payload["id"].as_str().expect("mark id");
                    assert!(ids.insert(id.to_string()), "{}: mark {id} twice", cast.name);
                }
                "resize" | "focus" | "hook" | "exit" => {}
                other => panic!("{}: unknown event kind {other}", cast.name),
            }
        }
    }
}

/// The first personal detail in `text`: a home directory, an email address, or
/// a session id other than the placeholder casts use.
fn personal_detail(text: &str) -> Option<String> {
    const PLACEHOLDER_ID: &str = "00000000-0000-7000-8000-000000000001";
    let lower = text.to_ascii_lowercase();
    for home in [":\\users\\", ":/users/", "/home/"] {
        if let Some(at) = lower.find(home) {
            let from = text.get(at.saturating_sub(2)..).unwrap_or(&text[at..]);
            return Some(from.chars().take(40).collect());
        }
    }
    let bytes = text.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b"._+-".contains(&b);
    for (at, _) in text.match_indices('@') {
        let before = at > 0 && word(bytes[at - 1]);
        let domain: String = text[at + 1..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-')
            .collect();
        if before && domain.contains('.') && !domain.ends_with('.') {
            return Some(format!("…@{domain}"));
        }
    }
    // A session id, whole or cut short the way a title cuts one
    // (`0123abcd-4567-7890-abcd-ef012...`): a run shaped like at least the
    // start of one, `xxxxxxxx-xxxx`, that isn't the start of the placeholder.
    let shaped = |i: usize, b: u8| {
        if [8, 13, 18, 23].contains(&i) {
            b == b'-'
        } else {
            b.is_ascii_hexdigit()
        }
    };
    for start in 0..bytes.len() {
        if start > 0 && bytes[start - 1].is_ascii_hexdigit() {
            continue;
        }
        let run = &bytes[start..];
        let len = run
            .iter()
            .take(PLACEHOLDER_ID.len())
            .enumerate()
            .take_while(|&(i, &b)| shaped(i, b))
            .count();
        if len >= 13 && !run[..len].eq_ignore_ascii_case(&PLACEHOLDER_ID.as_bytes()[..len]) {
            return Some(String::from_utf8_lossy(&run[..len]).into_owned());
        }
    }
    None
}

/// Every key and string inside `value`, walked whole.
fn strings_in(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| strings_in(item, out)),
        Value::Object(map) => {
            for (key, item) in map {
                out.push(key.clone());
                strings_in(item, out);
            }
        }
        _ => {}
    }
}

/// Where in `cast` the first personal detail sits, and what it is. Reads every
/// string in the header and in every event that isn't bytes (mark notes and
/// truth, hook fields), and each byte stream joined, so nothing hides across a
/// read boundary. It reads bytes, not the drawn screen: text a program
/// assembles with cursor moves isn't seen whole.
fn personal_detail_in(cast: &Cast) -> Option<(String, String)> {
    let mut places: Vec<(String, Vec<String>)> = Vec::new();
    let mut header = Vec::new();
    strings_in(&Value::Object(cast.header.clone()), &mut header);
    places.push(("header".to_string(), header));
    for stream in ["out", "in", "reply"] {
        let bytes: Vec<u8> = cast
            .events
            .iter()
            .filter(|(_, kind, _)| kind == stream)
            .flat_map(|(_, _, payload)| decode_bytes(payload))
            .collect();
        places.push((
            format!("{stream} stream"),
            vec![String::from_utf8_lossy(&bytes).into_owned()],
        ));
    }
    for (t, kind, payload) in &cast.events {
        if !matches!(kind.as_str(), "out" | "in" | "reply") {
            let mut texts = Vec::new();
            strings_in(payload, &mut texts);
            places.push((format!("{kind} at {t}"), texts));
        }
    }
    places.into_iter().find_map(|(place, texts)| {
        texts
            .iter()
            .find_map(|text| personal_detail(text))
            .map(|found| (place, found))
    })
}

/// Nothing personal reaches the repo: no home directory, email address or
/// real session id anywhere in a cast. Casts use one fixed placeholder id
/// where a provider prints a session id.
#[test]
fn casts_hold_no_personal_data() {
    for cast in repo_casts() {
        if let Some((place, found)) = personal_detail_in(&cast) {
            panic!("{}: personal data in the {place}: {found}", cast.name);
        }
    }
}

/// The scan reads every part of a cast: an id planted in any one of them is
/// found and its place named.
#[test]
fn the_personal_data_scan_reads_every_part_of_a_cast() {
    const ID: &str = "0123abcd-4567-7890-abcd-ef0123456789";
    let clean = || Cast {
        name: "canary".to_string(),
        header: serde_json::from_value(serde_json::json!({
            "provider": "claude", "launch": ["--model", "sonnet"], "recorded": "2026-11-12",
        }))
        .expect("header"),
        events: vec![
            (0, "out".to_string(), Value::from("hello ")),
            (1, "out".to_string(), Value::from("world")),
            (2, "in".to_string(), Value::from("x")),
            (3, "reply".to_string(), Value::from("\x1b[?6c")),
            (4, "hook".to_string(), serde_json::json!({ "name": "Stop" })),
            (
                5,
                "mark".to_string(),
                serde_json::json!({ "id": "m", "by": "hand", "truth": {} }),
            ),
        ],
    };
    assert_eq!(personal_detail_in(&clean()), None);
    // Split so that only the joined stream holds a whole id.
    const SPLIT: usize = 10;
    assert!(personal_detail(&ID[..SPLIT]).is_none() && personal_detail(&ID[SPLIT..]).is_none());
    /// Where an id is planted, and how.
    type Plant = (&'static str, fn(&mut Cast));
    let plants: [Plant; 9] = [
        ("header", |c| {
            c.header.insert("recorded".into(), ID.into());
        }),
        ("header", |c| {
            c.header
                .insert("launch".into(), serde_json::json!(["--resume", ID]));
        }),
        ("header", |c| {
            c.header.insert(ID.into(), Value::Bool(true));
        }),
        ("out stream", |c| {
            c.events[0].2 = Value::from(&ID[..SPLIT]);
            c.events[1].2 = Value::from(&ID[SPLIT..]);
        }),
        ("in stream", |c| c.events[2].2 = Value::from(ID)),
        ("reply stream", |c| c.events[3].2 = Value::from(ID)),
        ("hook at 4", |c| {
            c.events[4].2 = serde_json::json!({ "name": "Stop", "prompt": ID });
        }),
        ("mark at 5", |c| {
            c.events[5].2["note"] = Value::from(format!("seen in {ID}"));
        }),
        ("mark at 5", |c| {
            c.events[5].2["truth"] = serde_json::json!({ "draft": ID });
        }),
    ];
    for (place, plant) in plants {
        let mut cast = clean();
        plant(&mut cast);
        assert_eq!(
            personal_detail_in(&cast).map(|(at, _)| at).as_deref(),
            Some(place),
            "an id planted in the {place}"
        );
    }
}

#[test]
fn the_personal_data_scan_catches_each_kind() {
    assert!(personal_detail(r"C:\Users\someone\project").is_some());
    assert!(personal_detail("/home/someone/project").is_some());
    assert!(personal_detail("mail me at someone@example.com").is_some());
    assert!(personal_detail("session 019f95d7-db31-7db0-904d-9e08330e0016").is_some());
    assert!(personal_detail("session 00000000-0000-7000-8000-000000000001").is_none());
    assert!(personal_detail("0123abcd-4567-7890-abcd-ef012... | Ready").is_some());
    assert!(personal_detail("00000000-0000-7000-8000-00000... | Ready").is_none());
    assert!(personal_detail(r"D:\work\demo · @filename").is_none());
}

/// Sanitizing must not change a single verdict. Run it where the unsanitized
/// casts are (`--include-ignored`): `MUXEL_RAW_CASTS` names a folder whose
/// subfolders hold `raw.cast.jsonl` files; one whose header `name` gives a
/// repo cast's file name is compared with it.
#[test]
#[ignore = "needs MUXEL_RAW_CASTS: the unsanitized casts never enter the repo"]
fn sanitizing_changes_no_verdict() {
    let root = std::env::var_os("MUXEL_RAW_CASTS")
        .expect("MUXEL_RAW_CASTS names the folder of unsanitized casts");
    let repo: std::collections::HashMap<_, _> = repo_casts()
        .into_iter()
        .map(|cast| (format!("{}.cast.jsonl", cast.name), cast))
        .collect();
    let mut compared = 0;
    for entry in std::fs::read_dir(&root)
        .expect("MUXEL_RAW_CASTS is a folder")
        .flatten()
    {
        let path = entry.path().join("raw.cast.jsonl");
        if !path.exists() {
            continue;
        }
        let raw = load(&path);
        // Not every recording earns a place in the repo; one that does names
        // its repo cast.
        let Some(clean) = raw
            .header
            .get("name")
            .and_then(|name| name.as_str())
            .and_then(|name| repo.get(name))
        else {
            continue;
        };
        let name = &clean.name;
        // The box's text is the one verdict sanitizing changes on purpose;
        // its replacements keep every width, so its length must not change.
        let verdicts = |cast: &Cast| -> Vec<(String, Verdicts)> {
            replay(cast)
                .into_iter()
                .map(|mut read| {
                    read.verdicts.composer_text = read
                        .verdicts
                        .composer_text
                        .map(|text| format!("{} chars", text.chars().count()));
                    (read.id, read.verdicts)
                })
                .collect()
        };
        assert_eq!(
            verdicts(&raw),
            verdicts(clean),
            "{name}: sanitizing changed a verdict"
        );
        compared += 1;
    }
    assert!(
        compared > 0,
        "no raw casts under {}",
        root.to_string_lossy()
    );
}
