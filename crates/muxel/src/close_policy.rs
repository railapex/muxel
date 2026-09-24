//! Pure, evidence-bound close decisions. The app supplies facts; this module
//! never reads a view, process, persisted confirmation preference, or layout.

use muxel_terminal::{
    AgentStatus, TerminalRuntimeSnapshot, TerminalStatusCoverage, TerminalStatusReason,
};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseEvidence {
    Terminal {
        snapshot: TerminalEvidence,
        plain_shell: bool,
        idle_foreground: bool,
    },
    Launching(u64),
    UnknownTerminal,
    Editor {
        dirty_text: Option<String>,
    },
    Other,
}

/// Fields that can change what work would be destroyed. `snapshot_seq` records
/// output progress, and `completion_ready` is a launch/notification hint; neither
/// grants permission to close or invalidates an otherwise identical decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalEvidence {
    pub status: AgentStatus,
    pub reason: TerminalStatusReason,
    pub coverage: TerminalStatusCoverage,
    pub launch_id: Uuid,
    pub binding_rev: u64,
    pub input_rev: u64,
    pub draft_latched: bool,
}

impl From<TerminalRuntimeSnapshot> for TerminalEvidence {
    fn from(s: TerminalRuntimeSnapshot) -> Self {
        Self {
            status: s.status,
            reason: s.reason,
            coverage: s.coverage,
            launch_id: s.launch_id,
            binding_rev: s.binding_rev,
            input_rev: s.input_rev,
            draft_latched: s.draft_latched,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseTarget {
    pub id: Uuid,
    pub evidence: CloseEvidence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseReason {
    Working,
    Blocked,
    UncoveredProvider,
    UnprovenShell,
    Draft,
    Launching,
    UnknownTerminal,
    DirtyEditor,
}

impl CloseReason {
    pub fn description(self) -> &'static str {
        match self {
            Self::Working => "agent is working",
            Self::Blocked => "agent is awaiting input",
            Self::UncoveredProvider => "agent work may be hidden",
            Self::UnprovenShell => "shell activity is uncertain",
            Self::Draft => "terminal has unsubmitted input",
            Self::Launching => "terminal is still launching",
            Self::UnknownTerminal => "terminal state is unavailable",
            Self::DirtyEditor => "editor has unsaved changes",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseConcern {
    pub id: Uuid,
    pub reason: CloseReason,
}

/// Exact membership and work identity/reasons at the point of approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloseToken {
    pub targets: Vec<CloseTarget>,
    pub concerns: Vec<CloseConcern>,
}

impl CloseToken {
    pub fn needs_confirmation(&self) -> bool {
        !self.concerns.is_empty()
    }

    pub fn has_dirty_editors(&self) -> bool {
        self.concerns
            .iter()
            .any(|c| c.reason == CloseReason::DirtyEditor)
    }

    pub fn after_editor_save(&self) -> Self {
        let mut targets = self.targets.clone();
        for target in &mut targets {
            if matches!(
                target.evidence,
                CloseEvidence::Editor {
                    dirty_text: Some(_)
                }
            ) {
                target.evidence = CloseEvidence::Editor { dirty_text: None };
            }
        }
        assess(targets)
    }

    pub fn evidence_for(&self, id: Uuid) -> Option<&CloseEvidence> {
        self.targets
            .iter()
            .find(|target| target.id == id)
            .map(|t| &t.evidence)
    }
}

pub fn assess(mut targets: Vec<CloseTarget>) -> CloseToken {
    targets.sort_by_key(|target| target.id);
    targets.dedup_by_key(|target| target.id);
    let mut concerns = Vec::new();
    for target in &targets {
        let mut add = |reason| {
            concerns.push(CloseConcern {
                id: target.id,
                reason,
            })
        };
        match &target.evidence {
            CloseEvidence::Terminal {
                snapshot,
                plain_shell,
                idle_foreground,
            } => {
                if snapshot.reason != TerminalStatusReason::ProcessExit {
                    if *plain_shell {
                        // Direct foreground-group proof wins over a stale activity
                        // badge. False also means "cannot inspect" on Windows.
                        if !idle_foreground {
                            add(CloseReason::UnprovenShell);
                        }
                    } else {
                        match snapshot.status {
                            AgentStatus::Working => add(CloseReason::Working),
                            AgentStatus::Blocked => add(CloseReason::Blocked),
                            // A1 has no hidden-work coverage. Visible Idle/Done
                            // is never proof that provider work stopped.
                            AgentStatus::Idle | AgentStatus::Done => {
                                add(CloseReason::UncoveredProvider)
                            }
                        }
                    }
                    if snapshot.draft_latched {
                        add(CloseReason::Draft);
                    }
                }
            }
            CloseEvidence::Launching(_) => add(CloseReason::Launching),
            CloseEvidence::UnknownTerminal => add(CloseReason::UnknownTerminal),
            CloseEvidence::Editor {
                dirty_text: Some(_),
            } => add(CloseReason::DirtyEditor),
            CloseEvidence::Editor { dirty_text: None } | CloseEvidence::Other => {}
        }
    }
    CloseToken { targets, concerns }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime(status: AgentStatus) -> TerminalRuntimeSnapshot {
        TerminalRuntimeSnapshot {
            status,
            reason: TerminalStatusReason::Quiet,
            coverage: TerminalStatusCoverage::ProviderVisible,
            launch_id: Uuid::new_v4(),
            binding_rev: 1,
            snapshot_seq: 1,
            input_rev: 1,
            draft_latched: false,
            completion_ready: false,
        }
    }

    fn terminal(
        s: TerminalRuntimeSnapshot,
        plain_shell: bool,
        idle_foreground: bool,
    ) -> CloseToken {
        assess(vec![CloseTarget {
            id: Uuid::new_v4(),
            evidence: CloseEvidence::Terminal {
                snapshot: s.into(),
                plain_shell,
                idle_foreground,
            },
        }])
    }

    #[test]
    fn active_blocked_and_uncovered_provider_confirm_even_when_done() {
        assert_eq!(
            terminal(runtime(AgentStatus::Working), false, false).concerns[0].reason,
            CloseReason::Working
        );
        assert_eq!(
            terminal(runtime(AgentStatus::Blocked), false, false).concerns[0].reason,
            CloseReason::Blocked
        );
        for status in [AgentStatus::Idle, AgentStatus::Done] {
            for coverage in [
                TerminalStatusCoverage::ActivityOnly,
                TerminalStatusCoverage::VisibleMarkers,
                TerminalStatusCoverage::ProviderVisible,
            ] {
                let mut s = runtime(status);
                s.coverage = coverage;
                s.completion_ready = true;
                assert_eq!(
                    terminal(s, false, false).concerns[0].reason,
                    CloseReason::UncoveredProvider
                );
            }
        }
    }

    #[test]
    fn proven_shell_and_exited_process_are_quiet_but_draft_is_not() {
        let s = runtime(AgentStatus::Idle);
        assert!(!terminal(s, true, true).needs_confirmation());
        assert!(!terminal(runtime(AgentStatus::Working), true, true).needs_confirmation());
        assert_eq!(
            terminal(s, true, false).concerns[0].reason,
            CloseReason::UnprovenShell
        );
        let mut s = s;
        s.draft_latched = true;
        assert_eq!(
            terminal(s, true, true).concerns[0].reason,
            CloseReason::Draft
        );
        s.reason = TerminalStatusReason::ProcessExit;
        assert!(!terminal(s, false, false).needs_confirmation());
    }

    #[test]
    fn launching_unknown_and_dirty_editor_aggregate() {
        let ids = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
        let token = assess(vec![
            CloseTarget {
                id: ids[0],
                evidence: CloseEvidence::Launching(1),
            },
            CloseTarget {
                id: ids[1],
                evidence: CloseEvidence::UnknownTerminal,
            },
            CloseTarget {
                id: ids[2],
                evidence: CloseEvidence::Editor {
                    dirty_text: Some("draft".into()),
                },
            },
        ]);
        assert_eq!(token.concerns.len(), 3);
        assert!(
            token
                .concerns
                .iter()
                .any(|c| c.reason == CloseReason::DirtyEditor)
        );
    }

    #[test]
    fn mixed_batch_and_clean_quit_use_the_same_aggregate_decision() {
        let terminal = CloseTarget {
            id: Uuid::new_v4(),
            evidence: CloseEvidence::Terminal {
                snapshot: runtime(AgentStatus::Blocked).into(),
                plain_shell: false,
                idle_foreground: false,
            },
        };
        let editor = CloseTarget {
            id: Uuid::new_v4(),
            evidence: CloseEvidence::Editor {
                dirty_text: Some("unsaved".into()),
            },
        };
        let grouped = assess(vec![editor.clone(), terminal.clone()]);
        assert_eq!(grouped, assess(vec![terminal, editor]));
        assert_eq!(grouped.concerns.len(), 2);
        assert!(
            grouped
                .concerns
                .iter()
                .any(|c| c.reason == CloseReason::Blocked)
        );
        assert!(
            grouped
                .concerns
                .iter()
                .any(|c| c.reason == CloseReason::DirtyEditor)
        );
        let saved = grouped.after_editor_save();
        assert_eq!(saved.targets.len(), grouped.targets.len());
        assert_eq!(saved.concerns.len(), 1);
        assert_eq!(saved.concerns[0].reason, CloseReason::Blocked);
        assert!(!assess(Vec::new()).needs_confirmation());
        assert!(
            !assess(vec![CloseTarget {
                id: Uuid::new_v4(),
                evidence: CloseEvidence::Other
            }])
            .needs_confirmation()
        );
    }

    #[test]
    fn token_tracks_work_evidence_but_not_output_progress_or_readiness() {
        let id = Uuid::new_v4();
        let s = runtime(AgentStatus::Idle);
        let make = |s: TerminalRuntimeSnapshot| {
            assess(vec![CloseTarget {
                id,
                evidence: CloseEvidence::Terminal {
                    snapshot: s.into(),
                    plain_shell: false,
                    idle_foreground: false,
                },
            }])
        };
        let baseline = make(s);
        let mut changed = s;
        changed.snapshot_seq += 1;
        changed.completion_ready = true;
        assert_eq!(baseline, make(changed));
        changed.launch_id = Uuid::new_v4();
        assert_ne!(baseline, make(changed));
        changed = s;
        changed.binding_rev += 1;
        assert_ne!(baseline, make(changed));
        changed = s;
        changed.input_rev += 1;
        assert_ne!(baseline, make(changed));
        changed = s;
        changed.status = AgentStatus::Working;
        assert_ne!(baseline, make(changed));
        changed = s;
        changed.draft_latched = true;
        assert_ne!(baseline, make(changed));
        changed = s;
        changed.coverage = TerminalStatusCoverage::ActivityOnly;
        assert_ne!(baseline, make(changed));
        let extra = CloseTarget {
            id: Uuid::new_v4(),
            evidence: CloseEvidence::Other,
        };
        let mut members = baseline.targets.clone();
        members.push(extra);
        assert_ne!(baseline, assess(members));
        let dirty = CloseTarget {
            id,
            evidence: CloseEvidence::Editor {
                dirty_text: Some("one".into()),
            },
        };
        let modified = CloseTarget {
            id,
            evidence: CloseEvidence::Editor {
                dirty_text: Some("two".into()),
            },
        };
        assert_ne!(assess(vec![dirty]), assess(vec![modified]));
    }
}
