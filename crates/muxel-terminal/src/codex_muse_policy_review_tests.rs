use super::{
    AgentStatus, RuntimeStatusState, TerminalRuntimeSnapshot, TerminalStatusCoverage, TitleProvider,
};
use crate::session::{CommandSpec, LaunchBinding, PtyReceiver, TerminalSession};
use muxel_core::control::{AgentState, BackgroundWork, WaitOutcome, awaiting_reply, wait_outcome};
use muxel_core::{AgentActivity, AgentActivityState};
use std::sync::Arc;
use std::time::{Duration, Instant};

const CAP: Duration = Duration::from_secs(3 * 60 * 60);
const WORKING: &[u8] = b"\x1b]7501;state=working\x07";
const DONE: &[u8] = b"\x1b]7501;state=done\x07";

struct Probe {
    session: Arc<TerminalSession>,
    _receiver: PtyReceiver,
    runtime: RuntimeStatusState,
    provider: TitleProvider,
    start: Instant,
}

impl Probe {
    fn new(provider: TitleProvider) -> Self {
        #[cfg(windows)]
        let command = CommandSpec::program(
            "cmd.exe",
            vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
        );
        #[cfg(unix)]
        let command = CommandSpec::program("/bin/cat", vec![]);
        let (session, receiver) =
            TerminalSession::spawn(command.with_binding(LaunchBinding::Known(None)), 160, 40)
                .expect("spawn inert Codex/Muse policy fixture");
        let mut runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        if provider == TitleProvider::Muse {
            runtime.muse_cwd_label = Some("project".into());
        }
        Self {
            session,
            _receiver: receiver,
            runtime,
            provider,
            start: Instant::now(),
        }
    }

    fn child(&mut self, state: Option<bool>) {
        self.runtime.codex_subagents = Some((self.session.binding_revision(), state));
    }

    fn sample(&mut self, at: Duration, screen: &str, reports: &[u8]) -> TerminalRuntimeSnapshot {
        let title = if self.provider == TitleProvider::Muse {
            "project"
        } else {
            "Review | Ready"
        };
        self.sample_title(at, screen, reports, title)
    }

    fn sample_title(
        &mut self,
        at: Duration,
        screen: &str,
        reports: &[u8],
        title: &str,
    ) -> TerminalRuntimeSnapshot {
        let mut wire = format!("\x1b[2J\x1b[H\x1b]0;{title}\x07\x1b[?2004h");
        let mut composer = 1;
        for (index, row) in screen.lines().enumerate() {
            wire.push_str(&format!("\x1b[{};1H{row}", index + 1));
            if row.starts_with('›') || row == "❯" {
                composer = index + 1;
            }
        }
        wire.push_str(&format!("\x1b[{composer};3H"));
        self.session.test_program_status(wire.as_bytes());
        if !reports.is_empty() {
            self.session.test_program_status(reports);
        }
        self.runtime.reconcile(
            &self.session,
            self.provider,
            &[],
            &[],
            false,
            self.start + at,
        );
        self.runtime.snapshot(&self.session)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.session.kill();
    }
}

fn screen(provider: &str, state: &str) -> String {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/provider-background-screens.json"))
            .expect("parse real provider screen fixtures");
    fixtures[provider][state]
        .as_str()
        .expect("named screen fixture")
        .to_string()
}

fn shells(count: u32) -> Option<BackgroundWork> {
    (count > 0).then_some(BackgroundWork {
        shells: count,
        ..Default::default()
    })
}

fn assert_plain_wait_finished(snapshot: &TerminalRuntimeSnapshot) {
    let (state, activity_state) = match snapshot.status {
        AgentStatus::Idle => (AgentState::Idle, AgentActivityState::Idle),
        AgentStatus::Working => (AgentState::Working, AgentActivityState::Working),
        AgentStatus::Blocked => (AgentState::Blocked, AgentActivityState::Blocked),
        AgentStatus::Done => (AgentState::Done, AgentActivityState::Done),
    };
    let mut activity = AgentActivity::default();
    activity.observe(None, AgentActivityState::Working, 100);
    activity.observe(Some(AgentActivityState::Working), activity_state, 1_100);
    let awaiting = awaiting_reply(
        Some(0),
        muxel_core::control::REPLY_GRACE_MS,
        Some(state),
        &activity,
        1_100,
    );
    assert_eq!(
        wait_outcome(Some(state), awaiting),
        Some(WaitOutcome::Finished),
        "plain wait finishes the observed foreground turn while terminal counts remain metadata"
    );
}

#[test]
fn bghold_b_terminal_only_population_finishes_current_and_later_turns() {
    for (provider, name, fixture, count) in [
        (TitleProvider::Codex, "codex", "one", 1),
        (TitleProvider::Codex, "codex", "two", 2),
        (TitleProvider::Muse, "muse", "running", 1),
    ] {
        let mut probe = Probe::new(provider);
        let listed = screen(name, fixture);
        for turn in 0..2 {
            let at = Duration::from_secs(turn * 10);
            probe
                .session
                .write_submission(b"\r", crate::Submission::Turn);
            assert_eq!(
                probe.sample(at, &listed, WORKING).status,
                AgentStatus::Working
            );
            let done = probe.sample(at + Duration::from_secs(1), &listed, DONE);
            assert_eq!(
                done.background,
                shells(count),
                "real terminal count remains listed"
            );
            assert_plain_wait_finished(&done);
            assert_eq!(
                done.status,
                AgentStatus::Done,
                "terminal-only work cannot mask a real Done report"
            );
            assert!(done.attributable);
            assert_eq!(
                done.completion_revision,
                turn + 1,
                "each real turn publishes one finish"
            );
            let stable = probe.sample(at + Duration::from_secs(2), &listed, b"");
            assert_eq!(stable.status, AgentStatus::Done);
            assert_eq!(
                stable.completion_revision, done.completion_revision,
                "an unchanged server listing cannot republish completion"
            );
        }
    }
}

#[test]
fn bghold_b_codex_title_and_muse_hook_finish_with_terminal_metadata() {
    let mut codex = Probe::new(TitleProvider::Codex);
    let listed = screen("codex", "one");
    codex.sample(Duration::ZERO, &listed, b"");
    codex
        .session
        .write_submission(b"\r", crate::Submission::Turn);
    assert_eq!(
        codex
            .sample_title(Duration::from_secs(1), &listed, b"", "Review | Working")
            .status,
        AgentStatus::Working
    );
    let done = codex.sample(Duration::from_secs(2), &listed, b"");
    assert_eq!(
        done.status,
        AgentStatus::Done,
        "a ready title completes actual submitted work despite shell badge"
    );
    assert_eq!(done.background, shells(1));
    let mut muse = Probe::new(TitleProvider::Muse);
    let listed = screen("muse", "running");
    muse.runtime.muse_hook_state = Some(muxel_core::muse::MuseState::Working);
    muse.sample(Duration::ZERO, &listed, b"");
    muse.runtime.muse_hook_state = Some(muxel_core::muse::MuseState::Done);
    let done = muse.sample(Duration::from_secs(1), &listed, b"");
    assert_eq!(
        done.status,
        AgentStatus::Done,
        "a Done hook with idle title is not overridden by a shell tree"
    );
    assert_eq!(done.background, shells(1));
}

#[test]
fn bghold_b_unreadable_shell_chrome_cannot_hold_a_finished_turn() {
    for fixture in ["one", "two"] {
        let mut probe = Probe::new(TitleProvider::Codex);
        let listed = screen("codex", fixture);
        probe.sample(Duration::ZERO, &listed, WORKING);
        let done = probe.sample(Duration::from_secs(1), &listed, DONE);
        let truncated = listed.replace(" · /stop to close", "");
        for (offset, unreadable) in ["", truncated.as_str()].into_iter().enumerate() {
            let observed = probe.sample(Duration::from_secs(10 + offset as u64), unreadable, b"");
            assert_eq!(
                observed.status,
                AgentStatus::Done,
                "unreadable terminal chrome retains counts, not a foreground hold"
            );
            assert_eq!(
                observed.background,
                shells(if fixture == "one" { 1 } else { 2 })
            );
            assert!(observed.attributable);
            assert_eq!(
                observed.completion_revision, done.completion_revision,
                "unreadable chrome is not a new foreground completion"
            );
        }
        let before_clear = probe.runtime.snapshot(&probe.session).completion_revision;
        let cleared = probe.sample(Duration::from_secs(12), &screen("codex", "stopped"), b"");
        assert_eq!(cleared.status, AgentStatus::Done);
        assert_eq!(cleared.background, None);
        assert_eq!(
            cleared.completion_revision, before_clear,
            "terminal disappearance is not another foreground completion"
        );
    }
}

#[test]
fn bghold_b_known_children_and_mixed_shells_hold_three_hours_then_release_once() {
    for (fixture, count) in [("none", 0), ("one", 1), ("two", 2)] {
        let mut probe = Probe::new(TitleProvider::Codex);
        let listed = screen("codex", fixture);
        probe.child(Some(true));
        probe.sample(Duration::ZERO, &listed, WORKING);
        let end = Duration::from_secs(1);
        let held = probe.sample(end, &listed, DONE);
        let background = Some(BackgroundWork {
            shells: count,
            monitors: 0,
            subagents: true,
        });
        assert_eq!(held.status, AgentStatus::Working);
        assert_eq!(held.background, background);
        assert!(held.attributable);
        assert_eq!(
            probe
                .sample(end + CAP - Duration::from_secs(1), &listed, b"")
                .status,
            AgentStatus::Working,
            "known children hold beyond B's former thirty-minute cap"
        );
        let released = probe.sample(end + CAP, &listed, b"");
        assert_eq!(released.status, AgentStatus::Done);
        assert_eq!(released.background, background);
        let revision = released.completion_revision;
        for (offset, child) in [Some(false), None, Some(true)].into_iter().enumerate() {
            probe.child(child);
            let stable = probe.sample(
                end + CAP + Duration::from_secs(offset as u64 + 1),
                &listed,
                b"",
            );
            assert_eq!(
                stable.status,
                AgentStatus::Done,
                "child rescan after a released turn cannot rearm the hold"
            );
            assert_eq!(
                stable.completion_revision, revision,
                "release/rescan cannot manufacture another completion notification"
            );
        }
        let next = end + CAP + Duration::from_secs(10);
        probe
            .session
            .write_submission(b"\r", crate::Submission::Turn);
        assert_eq!(
            probe.sample(next, &listed, WORKING).status,
            AgentStatus::Working
        );
        let ended_again = next + Duration::from_secs(1);
        assert_eq!(
            probe.sample(ended_again, &listed, DONE).status,
            AgentStatus::Working
        );
        assert_eq!(
            probe
                .sample(ended_again + CAP - Duration::from_secs(1), &listed, b"")
                .status,
            AgentStatus::Working,
            "a real new foreground turn owns a fresh three-hour clock"
        );
        assert_eq!(
            probe.sample(ended_again + CAP, &listed, b"").status,
            AgentStatus::Done
        );
    }
}

#[test]
fn bghold_b_pending_child_with_or_without_shells_cannot_fabricate_activity() {
    for fixture in ["none", "one", "two"] {
        let mut probe = Probe::new(TitleProvider::Codex);
        let listed = screen("codex", fixture);
        probe.child(None);
        probe
            .session
            .write_submission(b"\r", crate::Submission::Turn);
        probe.sample(Duration::ZERO, &listed, WORKING);
        let pending = probe.sample(Duration::from_secs(1), &listed, DONE);
        assert_eq!(
            pending.status,
            AgentStatus::Working,
            "actual child catch-up defers the real completed turn"
        );
        assert!(
            !pending.attributable,
            "shell counts must not confirm unknown child work or publish provisional activity"
        );
        assert_eq!(
            pending.completion_revision, 0,
            "catch-up cannot publish an early finish"
        );
        let launch_completions = probe.runtime.launch_completion_revision;
        probe.child(Some(false));
        probe.sample(Duration::from_secs(2), &listed, b"");
        let resolved = probe.sample(Duration::from_secs(8), &listed, b"");
        assert_eq!(resolved.status, AgentStatus::Done);
        assert!(resolved.attributable);
        assert_eq!(
            probe.runtime.launch_completion_revision, launch_completions,
            "the temporary catch-up display cannot invent another real turn"
        );
    }
}

#[test]
fn bghold_b_pending_child_uncertainty_expires_once_after_three_hours() {
    for initially_known in [false, true] {
        let mut probe = Probe::new(TitleProvider::Codex);
        let listed = screen("codex", "none");
        probe.child(if initially_known { Some(true) } else { None });
        probe.sample(Duration::ZERO, &listed, WORKING);
        let end = Duration::from_secs(1);
        probe.sample(end, &listed, DONE);
        probe.child(None);
        assert_eq!(
            probe
                .sample(end + CAP - Duration::from_secs(1), &listed, b"")
                .status,
            AgentStatus::Working,
            "unresolved child evidence shares the three-hour safety bound"
        );
        let expired = probe.sample(end + CAP, &listed, b"");
        assert_eq!(expired.status, AgentStatus::Done);
        let launch_completions = probe.runtime.launch_completion_revision;
        for (offset, child) in [None, Some(false), None, Some(true)]
            .into_iter()
            .enumerate()
        {
            probe.child(child);
            let stable = probe.sample(
                end + CAP + Duration::from_secs(offset as u64 + 1),
                &listed,
                b"",
            );
            assert_eq!(
                stable.status,
                AgentStatus::Done,
                "stale uncertainty must stay released"
            );
            assert_eq!(stable.completion_revision, expired.completion_revision);
            assert_eq!(probe.runtime.launch_completion_revision, launch_completions);
        }
    }
}

#[test]
fn bghold_b_late_pending_after_published_finish_cannot_reopen_the_turn() {
    let mut probe = Probe::new(TitleProvider::Codex);
    let listed = screen("codex", "none");
    probe.child(Some(false));
    probe.sample(Duration::ZERO, &listed, WORKING);
    let finished = probe.sample(Duration::from_secs(1), &listed, DONE);
    assert_eq!(finished.status, AgentStatus::Done);
    probe.child(None);
    let late = probe.sample(Duration::from_secs(2), &listed, b"");
    assert_eq!(
        late.status,
        AgentStatus::Done,
        "a background index rescan is not a new foreground turn"
    );
    assert_eq!(late.completion_revision, finished.completion_revision);
}

#[test]
fn bghold_b_child_evidence_from_another_binding_cannot_hold_or_count() {
    for stale in [None, Some(true)] {
        let mut probe = Probe::new(TitleProvider::Codex);
        let listed = screen("codex", "none");
        probe.runtime.codex_subagents =
            Some((probe.session.binding_revision().wrapping_add(1), stale));
        probe.sample(Duration::ZERO, &listed, WORKING);
        let done = probe.sample(Duration::from_secs(1), &listed, DONE);
        assert_eq!(
            done.status,
            AgentStatus::Done,
            "only the current binding may provide child evidence"
        );
        assert_eq!(done.background, None);
        assert!(done.attributable);
    }
}
