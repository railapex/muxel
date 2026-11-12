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
use muxel_core::control::StartupBlockerKind;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

/// Known disagreements between a cast's truth and muxel's readers, keyed by
/// `file#mark` and the compared key, each with the finding it belongs to. A
/// listed gap that closes fails the test until its entry is removed, so a fix
/// on the branch that owns it shows up here.
#[rustfmt::skip]
const EXPECTED_GAPS: &[(&str, &str, &str)] = &[
    ("claude-2.1.312-windows-clear#slash-menu-clear", "takes_prompt/send", SEND_DRAFT),
    ("claude-2.1.322-windows-composer-tour#idle-first", "draft/empty", HELPER_CPR),
    ("claude-2.1.322-windows-composer-tour#idle-first", "takes_prompt/startup", HELPER_CPR),
    ("claude-2.1.322-windows-composer-tour#draft-end", "takes_prompt/send", SEND_DRAFT),
    ("claude-2.1.322-windows-composer-tour#draft-home", "draft/empty", HOME_DRAFT),
    ("claude-2.1.322-windows-composer-tour#draft-home", "takes_prompt/startup", HOME_DRAFT),
    ("claude-2.1.322-windows-composer-tour#draft-home", "takes_prompt/send", SEND_DRAFT),
    ("claude-2.1.322-windows-composer-tour#draft-mid", "takes_prompt/send", SEND_DRAFT),
    ("claude-2.1.322-windows-composer-tour#slash-menu-model", "takes_prompt/send", SEND_DRAFT),
    ("claude-2.1.322-windows-composer-tour#model-picker", "takes_prompt/send", SEND_COVERED),
    ("claude-2.1.322-windows-composer-tour#slash-menu-ex", "takes_prompt/send", SEND_DRAFT),
    ("claude-2.1.322-windows-permission-parked-draft#idle-first", "draft/empty", HELPER_CPR),
    ("claude-2.1.322-windows-permission-parked-draft#idle-first", "takes_prompt/startup", HELPER_CPR),
    ("claude-2.1.322-windows-permission-parked-draft#after-answer", "takes_prompt/send", SEND_DRAFT),
    ("codex-0.164.0-windows-command-approval#idle", "composer", CODEX_FOOTER),
    ("codex-0.164.0-windows-command-approval#idle", "draft/empty", CODEX_FOOTER),
    ("codex-0.164.0-windows-command-approval#idle", "takes_prompt/startup", CODEX_TITLE_UNSET),
    ("codex-0.164.0-windows-question#idle", "composer", CODEX_FOOTER),
    ("codex-0.164.0-windows-question#idle", "draft/empty", CODEX_FOOTER),
    ("codex-0.164.0-windows-question#idle", "takes_prompt/startup", CODEX_TITLE_UNSET),
    ("codex-0.164.0-windows-trust-card#trust-card", "takes_prompt/send", SEND_COVERED),
    ("codex-0.164.0-windows-turn#idle", "composer", CODEX_FOOTER),
    ("codex-0.164.0-windows-turn#idle", "draft/empty", CODEX_FOOTER),
    ("codex-0.164.0-windows-turn#post-turn-idle-candidate", "composer", CODEX_FOOTER),
    ("codex-0.164.0-windows-turn#post-turn-idle-candidate", "draft/empty", CODEX_FOOTER),
    ("codex-0.164.0-windows-turn#post-turn-idle-candidate", "takes_prompt/startup", CODEX_TITLE_UNSET),
    ("codex-0.167.2-windows-composer-tour#idle-first", "composer", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#idle-first", "draft/empty", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#idle-first", "takes_prompt/startup", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#draft-end", "takes_prompt/send", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#draft-home", "draft/empty", HOME_DRAFT),
    ("codex-0.167.2-windows-composer-tour#draft-home", "takes_prompt/startup", HOME_DRAFT),
    ("codex-0.167.2-windows-composer-tour#draft-home", "takes_prompt/send", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#draft-mid", "takes_prompt/send", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#draft-backspaced", "composer", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#draft-backspaced", "draft/empty", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#draft-backspaced", "takes_prompt/startup", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#ctrl-u-cleared", "composer", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#ctrl-u-cleared", "draft/empty", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#ctrl-u-cleared", "takes_prompt/startup", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#after-turn", "composer", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#after-turn", "draft/empty", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#after-turn", "takes_prompt/startup", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#up-recall", "takes_prompt/send", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#slash-menu-open", "takes_prompt/send", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#slash-menu-model", "takes_prompt/send", SEND_DRAFT),
    ("codex-0.167.2-windows-composer-tour#idle-last", "composer", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#idle-last", "draft/empty", CODEX_AGENTS_HINT),
    ("codex-0.167.2-windows-composer-tour#idle-last", "takes_prompt/startup", CODEX_AGENTS_HINT),
    ("grok-1.0.70-windows-question#idle", "takes_prompt/startup", GROK_DRAFT),
    ("grok-1.0.70-windows-question#idle", "takes_prompt/send", SEND_DRAFT),
    ("grok-1.0.70-windows-question#blocked", "overlay", GROK_OPTIONS),
    ("grok-1.0.70-windows-trust-card#trust-card", "overlay", GROK_TRUST),
    ("grok-1.0.70-windows-trust-card#trust-card", "takes_prompt/startup", GROK_TRUST),
    ("grok-1.0.70-windows-trust-card#trust-card", "takes_prompt/send", SEND_COVERED),
    ("grok-1.0.77-windows-composer-tour#trust-card", "overlay", GROK_TRUST),
    ("grok-1.0.77-windows-composer-tour#trust-card", "takes_prompt/startup", GROK_TRUST),
    ("grok-1.0.77-windows-composer-tour#trust-card", "takes_prompt/send", SEND_COVERED),
    ("grok-1.0.77-windows-composer-tour#draft-end", "takes_prompt/startup", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#draft-end", "takes_prompt/send", SEND_DRAFT),
    ("grok-1.0.77-windows-composer-tour#draft-home", "takes_prompt/startup", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#draft-home", "takes_prompt/send", SEND_DRAFT),
    ("grok-1.0.77-windows-composer-tour#draft-mid", "takes_prompt/startup", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#draft-mid", "takes_prompt/send", SEND_DRAFT),
    ("grok-1.0.77-windows-composer-tour#turn-running", "turn", GROK_SPINNER),
    ("grok-1.0.77-windows-composer-tour#after-turn", "finished", GROK_SPINNER),
    ("grok-1.0.77-windows-composer-tour#up-recall", "takes_prompt/startup", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#up-recall", "takes_prompt/send", SEND_DRAFT),
    ("grok-1.0.77-windows-composer-tour#slash-menu-open", "takes_prompt/startup", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#slash-menu-open", "takes_prompt/send", SEND_DRAFT),
    ("grok-1.0.77-windows-composer-tour#slash-menu-model", "takes_prompt/startup", GROK_DRAFT),
    ("grok-1.0.77-windows-composer-tour#slash-menu-model", "takes_prompt/send", SEND_DRAFT),
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
    /// Numbered options `ctl show`/`ctl answer` read while the status is Blocked.
    question_options: Option<usize>,
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
            .map(|question| question.options.len());
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
        let read = match (&v.blocker, v.question_options) {
            (Some(blocker), _) => blocker.clone(),
            (None, Some(options)) => format!("question with {options} options"),
            (None, None) => "none".to_string(),
        };
        let agrees = match overlay {
            "none" => v.blocker.is_none() && v.question_options.is_none(),
            "trust" => v.blocker.as_deref() == Some("folder_trust"),
            "question" | "permission" => v.question_options.is_some_and(|options| options > 0),
            other => panic!("unknown overlay {other:?}"),
        };
        check("overlay", overlay.to_string(), read, agrees);
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
            // Input, replies, focus and hooks don't touch the grid in replay.
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
    let mut unexpected = Vec::new();
    let mut seen_gaps = Vec::new();
    let (mut casts, mut marks, mut checked) = (0, 0, 0);
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
            for Check {
                key,
                truth: want,
                read: got,
                agrees,
            } in compare(provider, &truth, &verdicts)
            {
                checked += 1;
                let at = format!("{}#{id}", cast.name);
                let gap = EXPECTED_GAPS
                    .iter()
                    .find(|(gap_at, gap_key, _)| *gap_at == at && *gap_key == key);
                match (agrees, gap) {
                    (true, _) => {}
                    (false, Some(gap)) => seen_gaps.push(*gap),
                    (false, None) => {
                        unexpected.push(format!("{at} {key}: truth {want}, read {got}\n{rows}"))
                    }
                }
            }
        }
    }
    let closed: Vec<_> = EXPECTED_GAPS
        .iter()
        .filter(|gap| !seen_gaps.contains(gap))
        .map(|(at, key, finding)| format!("{at} {key} ({finding})"))
        .collect();
    eprintln!(
        "{casts} casts, {marks} marks, {checked} checks: {} agree, {} known gaps",
        checked - seen_gaps.len() - unexpected.len(),
        seen_gaps.len(),
    );
    assert!(
        unexpected.is_empty() && closed.is_empty(),
        "{checked} comparisons.\n\nDisagreements not in EXPECTED_GAPS:\n{}\nGaps that closed (remove them from EXPECTED_GAPS):\n{}",
        unexpected.join("\n"),
        closed.join("\n"),
    );
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

/// Nothing personal reaches the repo: no home directory, email address or
/// real session id in any payload or launch argument. Casts use one fixed
/// placeholder id where a provider prints a session id.
#[test]
fn casts_hold_no_personal_data() {
    for cast in repo_casts() {
        let mut texts = vec![serde_json::to_string(&cast.header["launch"]).unwrap_or_default()];
        // Each stream joined, so nothing hides across a read boundary.
        for stream in ["out", "in", "reply"] {
            let bytes: Vec<u8> = cast
                .events
                .iter()
                .filter(|(_, kind, _)| kind == stream)
                .flat_map(|(_, _, payload)| decode_bytes(payload))
                .collect();
            texts.push(String::from_utf8_lossy(&bytes).into_owned());
        }
        for text in &texts {
            if let Some(found) = personal_detail(text) {
                panic!("{}: personal data: {found}", cast.name);
            }
        }
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

/// Sanitizing must not change a single verdict. Run from a checkout that can
/// see the unsanitized casts: `MUXEL_RAW_CASTS` names a folder whose
/// subfolders hold `raw.cast.jsonl` files; one whose header `name` gives a
/// repo cast's file name is compared with it. Skipped when it isn't set.
#[test]
fn sanitizing_changes_no_verdict() {
    let Some(root) = std::env::var_os("MUXEL_RAW_CASTS") else {
        return;
    };
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
