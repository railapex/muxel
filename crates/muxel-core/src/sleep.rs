//! Putting an idle local agent to sleep: its program ends and its pane stays in
//! the layout asleep, and resumes the same conversation when it is woken. These rules decide which panes may sleep; the app gathers
//! the facts and does the stopping.

use crate::{Instance, Settings, is_known_agent_program, should_restore_dormant};

/// How long an agent stays idle before automatic sleep, unless the user picks
/// another value. Long enough that a pane touched this morning is still running
/// after lunch, short enough that the panes left over from yesterday stop
/// holding memory before the next working day's builds start.
pub const DEFAULT_SLEEP_AFTER_HOURS: u32 = 4;

/// How long a pane a machine woke (`ctl wake`) may idle before automatic sleep
/// takes it again, while nobody has looked at it. It woke for one job; holding
/// an agent's memory for the whole threshold after every such wake would cost
/// hours of it each time. A guess: the event log's wake lines show whether a
/// second job tends to arrive later than this.
pub const MACHINE_WAKE_IDLE_MINUTES: u32 = 30;

const MINUTE_MS: i64 = 60 * 1000;
const HOUR_MS: i64 = 60 * MINUTE_MS;

/// Who asked for a sleep. Only automatic sleep waits for the idle threshold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepRequest {
    /// A pane's own Sleep item. The user named this pane, so being in it
    /// doesn't keep it awake.
    Pane,
    /// A project's "Sleep idle agents". It names no pane, so it leaves the one
    /// the user is in.
    Project,
    /// The idle timer, which also leaves the pane the user is in.
    Auto { after_hours: u32, now: i64 },
}

/// A running pane's state, as far as sleeping it is concerned.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SleepStatus {
    /// Launching or reconnecting: there is no settled conversation yet.
    #[default]
    Starting,
    Idle,
    /// Finished a turn and waits for the user.
    Done,
    Working,
    /// Waiting on a permission or choice prompt.
    Asking,
    /// The program ended, failed, or isn't running.
    NotRunning,
}

/// What the app knows about a pane when deciding whether it may sleep.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SleepFacts {
    /// Runs one of muxel's known agent providers (not a shell or a wrapper).
    pub agent: bool,
    /// A plain PTY on this machine: not a remote project, not tmux.
    pub local_pty: bool,
    /// Its saved conversation id will resume it (see [`crate::resume_trusted`]).
    /// Whether the file behind that id still exists is checked after these
    /// rules, by [`ResumeFile::sleep_refusal`].
    pub resume_trusted: bool,
    pub status: SleepStatus,
    /// The provider's screen says work still runs beside the turn (subagents,
    /// background shells), or muxel holds the turn open for it.
    pub background_held: bool,
    /// The pane the user is in: focused, popped out, or shown by a project
    /// window.
    pub focused: bool,
    /// Popped out into its own window, which owns its terminal.
    pub popped_out: bool,
    /// Text typed into the agent's input that hasn't been submitted.
    pub unsent_text: bool,
    pub pinned: bool,
    /// The latest work, completion, question, attendance or wake (Unix ms).
    pub last_active_at: Option<i64>,
    /// Woken by a machine and not looked at since: automatic sleep gives it
    /// [`MACHINE_WAKE_IDLE_MINUTES`] in place of the whole threshold.
    pub machine_woken: bool,
}

/// Why a pane may not sleep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepRefusal {
    NotAgent,
    NotLocal,
    NoResume,
    Pinned,
    NotRunning,
    Starting,
    Working,
    Asking,
    Background,
    UnsentText,
    /// Sleep can't reach a popped-out pane's terminal until it is docked.
    PoppedOut,
    /// The bulk and automatic paths only: the user is in the pane.
    Focused,
    /// Automatic sleep only: active within the threshold, or its age is unknown.
    RecentlyActive,
}

impl SleepRefusal {
    /// Stable machine-readable form, for logs and outside control.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotAgent => "not_agent",
            Self::NotLocal => "not_local",
            Self::NoResume => "no_resume",
            Self::Pinned => "pinned",
            Self::NotRunning => "not_running",
            Self::Starting => "starting",
            Self::Working => "working",
            Self::Asking => "asking",
            Self::Background => "background",
            Self::UnsentText => "unsent_text",
            Self::PoppedOut => "popped_out",
            Self::Focused => "focused",
            Self::RecentlyActive => "recently_active",
        }
    }
}

/// What a bounded look on disk found of the file a resume would read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResumeFile {
    /// The conversation file is where the pane's saved binding puts it.
    Present,
    /// It isn't: the provider deleted it (Claude removes transcripts older than
    /// `cleanupPeriodDays`, 30 days by default) or never wrote it.
    Gone,
    /// The look failed, or couldn't settle it without walking the provider's
    /// whole store.
    Unknown,
    /// muxel can't name this provider's file (Grok, a custom resume program),
    /// so the saved id is all there is to go on.
    Unnamed,
}

impl ResumeFile {
    /// The last sleep rule, asked only once every other rule lets the pane
    /// sleep because it reads the disk: a resume needs the file it would read.
    /// A provider whose file muxel can't name keeps trusting its saved id.
    pub fn sleep_refusal(self) -> Option<SleepRefusal> {
        match self {
            Self::Present | Self::Unnamed => None,
            Self::Gone | Self::Unknown => Some(SleepRefusal::NoResume),
        }
    }

    /// Outside control's `resumes` for a pane whose saved id would resume it:
    /// `None` when muxel can't tell.
    pub fn resumes(self) -> Option<bool> {
        match self {
            Self::Present => Some(true),
            Self::Gone => Some(false),
            Self::Unknown | Self::Unnamed => None,
        }
    }
}

/// How long ago an asleep pane last worked, as its row shows it beside the
/// moon: minutes under an hour (at least 1m), hours under a day, then days.
/// `None` with no recorded work.
pub fn asleep_age(activity: &crate::AgentActivity, now: i64) -> Option<String> {
    const DAY_MS: i64 = 24 * HOUR_MS;
    let age = now.saturating_sub(activity.last_work_at()?).max(0);
    Some(if age < HOUR_MS {
        format!("{}m", (age / MINUTE_MS).max(1))
    } else if age < DAY_MS {
        format!("{}h", age / HOUR_MS)
    } else {
        format!("{}d", age / DAY_MS)
    })
}

/// Whether an agent last active at `last_active_at` has been idle for at least
/// `hours`, or for [`MACHINE_WAKE_IDLE_MINUTES`] when a machine woke it and
/// nobody has looked at it since (the shorter of the two). Zero hours turns
/// automatic sleep off; an unknown age never qualifies.
pub fn sleep_due(last_active_at: Option<i64>, hours: u32, machine_woken: bool, now: i64) -> bool {
    let mut limit = i64::from(hours) * HOUR_MS;
    if machine_woken {
        limit = limit.min(i64::from(MACHINE_WAKE_IDLE_MINUTES) * MINUTE_MS);
    }
    hours > 0 && last_active_at.is_some_and(|at| now.saturating_sub(at) >= limit)
}

/// The first rule that keeps this pane awake, or `None` when it may sleep.
///
/// Pinned panes never sleep: pinning already means "always start" for dormant
/// restore, and toggling a pin on wakes a dormant pane, so a pin keeps a pane
/// running in every path.
pub fn sleep_refusal(facts: &SleepFacts, request: SleepRequest) -> Option<SleepRefusal> {
    let refusal = if !facts.agent {
        SleepRefusal::NotAgent
    } else if !facts.local_pty {
        SleepRefusal::NotLocal
    } else if !facts.resume_trusted {
        SleepRefusal::NoResume
    } else if facts.pinned {
        SleepRefusal::Pinned
    } else {
        match facts.status {
            SleepStatus::NotRunning => SleepRefusal::NotRunning,
            SleepStatus::Starting => SleepRefusal::Starting,
            // A turn held open for its background work reads as working; say why.
            _ if facts.background_held => SleepRefusal::Background,
            SleepStatus::Working => SleepRefusal::Working,
            SleepStatus::Asking => SleepRefusal::Asking,
            SleepStatus::Idle | SleepStatus::Done if facts.unsent_text => SleepRefusal::UnsentText,
            SleepStatus::Idle | SleepStatus::Done if facts.popped_out => SleepRefusal::PoppedOut,
            // Only a pane's own Sleep names it; the others leave the pane the
            // user is in.
            SleepStatus::Idle | SleepStatus::Done
                if facts.focused && request != SleepRequest::Pane =>
            {
                SleepRefusal::Focused
            }
            SleepStatus::Idle | SleepStatus::Done => match request {
                SleepRequest::Auto { after_hours, now }
                    if !sleep_due(facts.last_active_at, after_hours, facts.machine_woken, now) =>
                {
                    SleepRefusal::RecentlyActive
                }
                _ => return None,
            },
        }
    };
    Some(refusal)
}

/// Whether a saved local PTY pane opens dormant instead of starting: it was put
/// to sleep; or it is an agent idle past the automatic-sleep threshold (or a
/// machine's unattended wake past its shorter limit) whose resume muxel trusts (starting it only for the timer to stop it again would
/// cost a launch and the memory spike sleep exists to avoid); or it is older
/// than the dormant-restore threshold. Pinned panes always start. The pane
/// the user is in is not exempt: selecting a pane shows it, and only a
/// deliberate wake starts it, so the active pane opens asleep like any other.
pub fn opens_dormant(
    instance: &Instance,
    resume_trusted: bool,
    settings: &Settings,
    now: i64,
) -> bool {
    if instance.pinned {
        return false;
    }
    if instance.asleep_since.is_some() {
        return true;
    }
    let idle_agent = resume_trusted
        && is_known_agent_program(instance.program.as_deref())
        && sleep_due(
            instance.activity.last_active_at(),
            settings.sleep_after_hours,
            instance.activity.machine_woken_unattended(),
            now,
        );
    idle_agent
        || should_restore_dormant(
            &instance.activity,
            settings.dormant_after_days,
            now,
            false,
            false,
        )
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_SLEEP_AFTER_HOURS, HOUR_MS, SleepFacts, SleepRefusal, SleepRequest, SleepStatus,
        opens_dormant, sleep_due, sleep_refusal,
    };
    use crate::{AgentActivity, AgentActivityState, Instance, Settings, Waker};
    use uuid::Uuid;

    const MINUTE_MS: i64 = 60 * 1000;

    fn sleepy() -> SleepFacts {
        SleepFacts {
            agent: true,
            local_pty: true,
            resume_trusted: true,
            status: SleepStatus::Idle,
            background_held: false,
            focused: false,
            popped_out: false,
            unsent_text: false,
            pinned: false,
            last_active_at: Some(0),
            machine_woken: false,
        }
    }

    #[test]
    fn an_idle_or_done_local_agent_with_a_trusted_resume_sleeps_by_hand() {
        for status in [SleepStatus::Idle, SleepStatus::Done] {
            let facts = SleepFacts {
                status,
                // By hand ignores age, including an unknown one.
                last_active_at: None,
                ..sleepy()
            };
            for request in [SleepRequest::Pane, SleepRequest::Project] {
                assert_eq!(sleep_refusal(&facts, request), None);
            }
        }
    }

    #[test]
    fn only_its_own_sleep_takes_the_pane_the_user_is_in() {
        let auto = SleepRequest::Auto {
            after_hours: 4,
            now: 9 * HOUR_MS,
        };
        let focused = SleepFacts {
            focused: true,
            ..sleepy()
        };
        assert_eq!(sleep_refusal(&focused, SleepRequest::Pane), None);
        for request in [SleepRequest::Project, auto] {
            assert_eq!(
                sleep_refusal(&focused, request),
                Some(SleepRefusal::Focused)
            );
        }
        // A popped-out pane's window owns its terminal: no path sleeps it.
        let popped = SleepFacts {
            focused: true,
            popped_out: true,
            ..sleepy()
        };
        for request in [SleepRequest::Pane, SleepRequest::Project, auto] {
            assert_eq!(
                sleep_refusal(&popped, request),
                Some(SleepRefusal::PoppedOut)
            );
        }
        // Being named lifts only the focus rule.
        let drafting = SleepFacts {
            unsent_text: true,
            ..focused
        };
        assert_eq!(
            sleep_refusal(&drafting, SleepRequest::Pane),
            Some(SleepRefusal::UnsentText)
        );
    }

    #[test]
    fn every_never_rule_keeps_a_pane_awake_by_hand_and_automatically() {
        let auto = SleepRequest::Auto {
            after_hours: 4,
            now: 9 * HOUR_MS,
        };
        let cases: [(SleepFacts, SleepRefusal); 10] = [
            (
                SleepFacts {
                    agent: false,
                    ..sleepy()
                },
                SleepRefusal::NotAgent,
            ),
            (
                SleepFacts {
                    local_pty: false,
                    ..sleepy()
                },
                SleepRefusal::NotLocal,
            ),
            (
                SleepFacts {
                    resume_trusted: false,
                    ..sleepy()
                },
                SleepRefusal::NoResume,
            ),
            (
                SleepFacts {
                    pinned: true,
                    ..sleepy()
                },
                SleepRefusal::Pinned,
            ),
            (
                SleepFacts {
                    status: SleepStatus::NotRunning,
                    ..sleepy()
                },
                SleepRefusal::NotRunning,
            ),
            (
                SleepFacts {
                    status: SleepStatus::Starting,
                    ..sleepy()
                },
                SleepRefusal::Starting,
            ),
            (
                SleepFacts {
                    status: SleepStatus::Working,
                    ..sleepy()
                },
                SleepRefusal::Working,
            ),
            (
                SleepFacts {
                    status: SleepStatus::Asking,
                    ..sleepy()
                },
                SleepRefusal::Asking,
            ),
            (
                SleepFacts {
                    status: SleepStatus::Done,
                    background_held: true,
                    ..sleepy()
                },
                SleepRefusal::Background,
            ),
            (
                SleepFacts {
                    unsent_text: true,
                    ..sleepy()
                },
                SleepRefusal::UnsentText,
            ),
        ];
        for (facts, refusal) in cases {
            for request in [SleepRequest::Pane, SleepRequest::Project, auto] {
                assert_eq!(
                    sleep_refusal(&facts, request),
                    Some(refusal),
                    "{facts:?} {request:?}"
                );
            }
        }
        assert_eq!(sleep_refusal(&sleepy(), auto), None);
    }

    #[test]
    fn automatic_sleep_waits_for_the_threshold_and_never_guesses_an_age() {
        let at = |now| SleepRequest::Auto {
            after_hours: 4,
            now,
        };
        let facts = SleepFacts {
            last_active_at: Some(HOUR_MS),
            ..sleepy()
        };
        assert_eq!(
            sleep_refusal(&facts, at(5 * HOUR_MS - 1)),
            Some(SleepRefusal::RecentlyActive)
        );
        assert_eq!(sleep_refusal(&facts, at(5 * HOUR_MS)), None);
        let unknown = SleepFacts {
            last_active_at: None,
            ..sleepy()
        };
        assert_eq!(
            sleep_refusal(&unknown, at(99 * HOUR_MS)),
            Some(SleepRefusal::RecentlyActive)
        );
        let off = SleepRequest::Auto {
            after_hours: 0,
            now: 99 * HOUR_MS,
        };
        assert_eq!(
            sleep_refusal(&facts, off),
            Some(SleepRefusal::RecentlyActive)
        );
        assert!(!sleep_due(Some(0), 0, false, 99 * HOUR_MS));
    }

    #[test]
    fn a_machine_woken_pane_sleeps_after_thirty_idle_minutes() {
        let at = |now| SleepRequest::Auto {
            after_hours: 4,
            now,
        };
        let woken = SleepFacts {
            last_active_at: Some(HOUR_MS),
            machine_woken: true,
            ..sleepy()
        };
        assert_eq!(
            sleep_refusal(&woken, at(HOUR_MS + 30 * MINUTE_MS - 1)),
            Some(SleepRefusal::RecentlyActive)
        );
        assert_eq!(sleep_refusal(&woken, at(HOUR_MS + 30 * MINUTE_MS)), None);
        // The short limit only shortens the setting: zero hours is still off,
        // and an unknown age still never qualifies.
        assert!(!sleep_due(Some(0), 0, true, 99 * HOUR_MS));
        assert!(!sleep_due(None, 4, true, 99 * HOUR_MS));
        // By hand ignores age either way.
        assert_eq!(sleep_refusal(&woken, SleepRequest::Pane), None);
    }

    #[test]
    fn automatic_sleep_is_on_at_four_hours_for_new_and_old_configs() {
        assert_eq!(
            Settings::default().sleep_after_hours,
            DEFAULT_SLEEP_AFTER_HOURS
        );
        assert_eq!(DEFAULT_SLEEP_AFTER_HOURS, 4);
        let old: Settings = toml::from_str("").expect("an empty config parses");
        assert_eq!(old.sleep_after_hours, 4);
        let off: Settings = toml::from_str("sleep_after_hours = 0").expect("parse");
        assert_eq!(off.sleep_after_hours, 0);
    }

    fn agent(activity: AgentActivity) -> Instance {
        let mut instance = Instance::shell(Uuid::new_v4());
        instance.program = Some("claude".into());
        instance.activity = activity;
        instance
    }

    #[test]
    fn a_slept_pane_stays_dormant_across_restarts_unless_pinned() {
        let settings = Settings {
            sleep_after_hours: 0,
            dormant_after_days: 0,
            ..Settings::default()
        };
        let mut slept = agent(AgentActivity {
            completed_at: Some(9 * HOUR_MS),
            ..Default::default()
        });
        assert!(!opens_dormant(&slept, true, &settings, 9 * HOUR_MS));
        slept.asleep_since = Some(9 * HOUR_MS);
        // Neither threshold applies: the sleep itself keeps it dormant.
        assert!(opens_dormant(&slept, true, &settings, 9 * HOUR_MS));
        slept.pinned = true;
        assert!(!opens_dormant(&slept, true, &settings, 9 * HOUR_MS));
    }

    #[test]
    fn an_agent_idle_past_the_threshold_opens_dormant_only_with_a_trusted_resume() {
        let settings = Settings::default();
        let old = agent(AgentActivity {
            completed_at: Some(0),
            last_attended_at: Some(HOUR_MS),
            ..Default::default()
        });
        // Attendance counts as activity: idle since hour 1, due at hour 5.
        assert!(!opens_dormant(&old, true, &settings, 5 * HOUR_MS - 1));
        assert!(opens_dormant(&old, true, &settings, 5 * HOUR_MS));
        assert!(!opens_dormant(&old, false, &settings, 5 * HOUR_MS));
        let mut shell = old.clone();
        shell.program = None;
        assert!(!opens_dormant(&shell, true, &settings, 5 * HOUR_MS));
        // Dormant restore still covers shells and untrusted resumes by days.
        let days = Settings {
            dormant_after_days: 1,
            ..Settings::default()
        };
        assert!(opens_dormant(&shell, false, &days, 25 * HOUR_MS));
    }

    #[test]
    fn a_wake_restarts_the_idle_clock_across_restarts_without_reading_a_finish() {
        let settings = Settings::default();
        // Finished at hour 0, woken at hour 5, and nobody has typed since.
        let woken = agent(AgentActivity {
            completed_at: Some(0),
            last_state: Some(AgentActivityState::Done),
            woken_at: Some(5 * HOUR_MS),
            ..Default::default()
        });
        assert_eq!(woken.activity.last_active_at(), Some(5 * HOUR_MS));
        assert!(!opens_dormant(&woken, true, &settings, 9 * HOUR_MS - 1));
        assert!(opens_dormant(&woken, true, &settings, 9 * HOUR_MS));
        // `ctl wake` must not mark the finished turn read.
        assert!(woken.activity.has_unattended_completion());
    }

    #[test]
    fn only_a_machine_wake_nobody_has_looked_at_gets_the_short_limit() {
        let settings = Settings::default();
        // Finished at hour 0, woken by `ctl wake` at hour 5.
        let machine = agent(AgentActivity {
            completed_at: Some(0),
            woken_at: Some(5 * HOUR_MS),
            woken_by: Some(Waker::Machine),
            ..Default::default()
        });
        let half_hour = 5 * HOUR_MS + 30 * MINUTE_MS;
        assert!(!opens_dormant(&machine, true, &settings, half_hour - 1));
        assert!(opens_dormant(&machine, true, &settings, half_hour));
        // Its work counts too: 30 minutes from the later of wake and work.
        let mut worked = machine.clone();
        worked.activity.completed_at = Some(6 * HOUR_MS);
        let after_work = 6 * HOUR_MS + 30 * MINUTE_MS;
        assert!(!opens_dormant(&worked, true, &settings, after_work - 1));
        assert!(opens_dormant(&worked, true, &settings, after_work));
        // A look before the wake isn't a look at it.
        let mut seen_before = machine.clone();
        seen_before.activity.last_attended_at = Some(4 * HOUR_MS);
        assert!(opens_dormant(&seen_before, true, &settings, half_hour));
        // The first look after it makes an ordinary pane: N hours from the look.
        let mut seen = machine.clone();
        seen.activity.last_attended_at = Some(5 * HOUR_MS + MINUTE_MS);
        // Half an hour after the look, where the short limit would end.
        assert!(!opens_dormant(
            &seen,
            true,
            &settings,
            half_hour + MINUTE_MS
        ));
        assert!(!opens_dormant(
            &seen,
            true,
            &settings,
            9 * HOUR_MS + MINUTE_MS - 1
        ));
        assert!(opens_dormant(
            &seen,
            true,
            &settings,
            9 * HOUR_MS + MINUTE_MS
        ));
        // The user's own wake, and an older save that never said who woke it,
        // get the whole threshold.
        for by in [Some(Waker::User), None] {
            let mut user = machine.clone();
            user.activity.woken_by = by;
            assert!(!opens_dormant(&user, true, &settings, 9 * HOUR_MS - 1));
            assert!(opens_dormant(&user, true, &settings, 9 * HOUR_MS));
        }
    }
}
