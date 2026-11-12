//! The leaving question. Closing panes, quitting, restarting and removing a
//! project ask the same way: only when work would be cut off, with one row
//! for each pane or input that would be, and the same keys. The app gathers
//! the facts; [`plan`] decides whether to ask, which rows, the title, the
//! buttons and the colour, and [`answer`] what a key does.

use crate::control::AgentState;
use crate::recoverable_close::{CloseDecision, CloseFacts, decide};
use uuid::Uuid;

/// What is leaving.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leave {
    Close,
    Quit,
    Restart,
    RemoveProject,
}

/// A pane the leave takes, with what the app knows about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pane {
    pub id: Uuid,
    pub kind: PaneKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneKind {
    /// A terminal. `unmarked_shell`: a shell that has never marked its
    /// prompt, so muxel can't tell its prompt from a command running.
    Terminal {
        facts: CloseFacts,
        unmarked_shell: bool,
    },
    Editor {
        dirty: bool,
    },
    /// A diff or browser pane: nothing to lose.
    Other,
}

/// Input or work, besides a pane's own state, that leaving would cut short.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pending {
    CtlSend(Option<Uuid>),
    CtlKeys(Option<Uuid>),
    Upload(Option<Uuid>),
    Dictation,
    WakeSweep,
    /// A preset's startup input still waiting to be typed.
    StartupInput(Uuid),
    Launching(Uuid),
    EditorMove(Uuid),
    /// A close past its Undo, being checked or waiting for Close or Keep.
    CloseUndecided(Uuid),
    FileDialog,
    /// A pane whose state changed during the quit countdown. The countdown
    /// asks instead of going, as an Undo expiry does.
    Changed(Uuid),
}

impl Pending {
    fn row(self) -> (Option<Uuid>, Note) {
        match self {
            Self::CtlSend(pane) => (pane, Note::CtlSend),
            Self::CtlKeys(pane) => (pane, Note::CtlKeys),
            Self::Upload(pane) => (pane, Note::Uploading),
            Self::Dictation => (None, Note::Dictation),
            Self::WakeSweep => (None, Note::WakingPanes),
            Self::StartupInput(pane) => (Some(pane), Note::PromptQueued),
            Self::Launching(pane) => (Some(pane), Note::Launching),
            Self::EditorMove(pane) => (Some(pane), Note::Moving),
            Self::CloseUndecided(pane) => (Some(pane), Note::CloseUndecided),
            Self::FileDialog => (None, Note::FileDialog),
            Self::Changed(pane) => (Some(pane), Note::Changed),
        }
    }
}

/// The words a row adds after its pill and name, only for what the pill
/// can't say. A row without a pane is named by its note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Note {
    /// Its pill says idle, but muxel can't confirm it.
    MaybeBusy,
    /// A shell that doesn't mark its prompt.
    UnmarkedPrompt,
    /// Its client exited, and the tmux session closing kills may still run.
    TmuxOpen,
    /// Another tmux client is attached to its session.
    AttachedElsewhere,
    Unsaved,
    CtlSend,
    CtlKeys,
    Uploading,
    PromptQueued,
    Launching,
    Moving,
    CloseUndecided,
    Dictation,
    WakingPanes,
    FileDialog,
    Changed,
}

impl Note {
    /// Whether what it names is lost for good, rather than perhaps.
    pub fn certain(self) -> bool {
        matches!(
            self,
            Self::Unsaved
                | Self::CtlSend
                | Self::CtlKeys
                | Self::Uploading
                | Self::PromptQueued
                | Self::Dictation
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The pane whose pill and name lead the row. None for input of the
    /// whole app, such as dictation.
    pub pane: Option<Uuid>,
    pub notes: Vec<Note>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Title {
    /// "Close {name}?"
    CloseOne(Uuid),
    /// "Close {n} sessions?", or panes when some aren't terminals.
    CloseMany {
        n: usize,
        sessions: bool,
    },
    Quit,
    Restart,
    /// "Remove {project}?"
    RemoveProject,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Question {
    pub leave: Leave,
    pub title: Title,
    pub rows: Vec<Row>,
    /// How many panes closing only the idle ones would close. None where
    /// that choice doesn't exist: a single pane, nothing idle, or not a close.
    pub idle: Option<usize>,
    /// The confirm button is red: something would be lost for good.
    pub danger: bool,
}

/// Whether leaving asks, and what. None means it goes ahead: a close holds
/// for Undo and a quit counts down.
///
/// A terminal asks by the rule a pane close uses ([`decide`], with
/// `ask_busy`). Quit, Restart and project removal pass `ask_busy` true; a
/// close passes the setting. A terminal in a tmux session never asks on Quit
/// or Restart: the session goes on running and muxel attaches to it again.
/// Removing a project always asks, because no Undo brings it back; its rows
/// are the panes it would cut off.
pub fn plan(leave: Leave, panes: &[Pane], pending: &[Pending], ask_busy: bool) -> Option<Question> {
    /// One row per pane, holding each note once.
    fn add(rows: &mut Vec<Row>, pane: Option<Uuid>, note: Option<Note>) {
        let at = match rows
            .iter()
            .position(|row| pane.is_some() && row.pane == pane)
        {
            Some(at) => at,
            None => {
                rows.push(Row {
                    pane,
                    notes: Vec::new(),
                });
                rows.len() - 1
            }
        };
        if let Some(note) = note
            && !rows[at].notes.contains(&note)
        {
            rows[at].notes.push(note);
        }
    }
    let mut rows: Vec<Row> = Vec::new();
    let mut danger = leave == Leave::RemoveProject;
    for pane in panes {
        match pane.kind {
            PaneKind::Terminal {
                facts,
                unmarked_shell,
            } => {
                // Quitting leaves a tmux session running, so it cuts off
                // nothing there; a close or a removal kills the session.
                let survives = matches!(leave, Leave::Quit | Leave::Restart);
                if (survives && facts.session_may_outlive)
                    || decide(facts, ask_busy) != CloseDecision::Ask
                {
                    continue;
                }
                let (note, certain) = terminal_note(facts, unmarked_shell);
                danger |= certain;
                add(&mut rows, Some(pane.id), note);
            }
            PaneKind::Editor { dirty: true } => {
                danger = true;
                add(&mut rows, Some(pane.id), Some(Note::Unsaved));
            }
            PaneKind::Editor { dirty: false } | PaneKind::Other => {}
        }
    }
    for item in pending {
        let (pane, note) = item.row();
        danger |= note.certain();
        add(&mut rows, pane, Some(note));
    }
    if rows.is_empty() && leave != Leave::RemoveProject {
        return None;
    }
    // Panes in the order given, then input of the whole app.
    rows.sort_by_key(|row| {
        row.pane
            .and_then(|id| panes.iter().position(|pane| pane.id == id))
            .unwrap_or(usize::MAX)
    });
    let title = match leave {
        Leave::Close => match panes {
            [] => return None,
            [only] => Title::CloseOne(only.id),
            _ => Title::CloseMany {
                n: panes.len(),
                sessions: panes
                    .iter()
                    .all(|pane| matches!(pane.kind, PaneKind::Terminal { .. })),
            },
        },
        Leave::Quit => Title::Quit,
        Leave::Restart => Title::Restart,
        Leave::RemoveProject => Title::RemoveProject,
    };
    let busy = panes
        .iter()
        .filter(|pane| rows.iter().any(|row| row.pane == Some(pane.id)))
        .count();
    let idle = (leave == Leave::Close && panes.len() > 1)
        .then(|| panes.len() - busy)
        .filter(|&idle| idle > 0);
    Some(Question {
        leave,
        title,
        rows,
        idle,
        danger,
    })
}

/// What a quit countdown watches, by pane id: each terminal's close facts,
/// except a tmux session's, which runs on after the quit. If these differ
/// when the countdown ends, it asks instead of going.
pub fn countdown_facts(panes: &[Pane]) -> Vec<(Uuid, CloseFacts)> {
    let mut facts: Vec<(Uuid, CloseFacts)> = panes
        .iter()
        .filter_map(|pane| match pane.kind {
            PaneKind::Terminal { facts, .. } if !facts.session_may_outlive => {
                Some((pane.id, facts))
            }
            PaneKind::Terminal { .. } | PaneKind::Editor { .. } | PaneKind::Other => None,
        })
        .collect();
    facts.sort_by_key(|(iid, _)| *iid);
    facts
}

/// A terminal that asks: the note its row needs beyond the pill, and whether
/// closing it loses work for certain.
fn terminal_note(facts: CloseFacts, unmarked_shell: bool) -> (Option<Note>, bool) {
    match facts.status {
        // A dead client asks only when its tmux session may outlive it.
        AgentState::Exited | AgentState::NotRunning | AgentState::Failed => {
            (Some(Note::TmuxOpen), false)
        }
        AgentState::Working | AgentState::Blocked | AgentState::Starting => (None, true),
        AgentState::Idle | AgentState::Done => {
            let note = if facts.another_tmux_client {
                Note::AttachedElsewhere
            } else if unmarked_shell {
                Note::UnmarkedPrompt
            } else {
                Note::MaybeBusy
            };
            (Some(note), false)
        }
    }
}

/// What a key does in a leaving question or the quit countdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Confirm,
    Cancel,
    IdleOnly,
}

/// The answer a key gives, for an unmodified key. Enter does the thing, Esc
/// backs out, and I closes only the idle ones where that choice exists.
pub fn answer(key: &str, idle_choice: bool) -> Option<Answer> {
    match key {
        "enter" => Some(Answer::Confirm),
        "escape" => Some(Answer::Cancel),
        "i" if idle_choice => Some(Answer::IdleOnly),
        _ => None,
    }
}

#[cfg(test)]
mod leaving_tests {
    use super::{
        Answer, Leave, Note, Pane, PaneKind, Pending, Question, Row, Title, answer,
        countdown_facts, plan,
    };
    use crate::control::AgentState;
    use crate::recoverable_close::{CloseEvidence, CloseFacts};
    use uuid::Uuid;

    fn facts(status: AgentState) -> CloseFacts {
        CloseFacts {
            status,
            evidence: CloseEvidence::Title,
            attributable: true,
            awaiting_reply: false,
            another_tmux_client: false,
            session_may_outlive: false,
            launch_id: None,
            binding_rev: 0,
            input_rev: 0,
            completion_rev: 0,
            shell_marks: 0,
        }
    }

    fn terminal(facts: CloseFacts) -> Pane {
        Pane {
            id: Uuid::new_v4(),
            kind: PaneKind::Terminal {
                facts,
                unmarked_shell: false,
            },
        }
    }

    fn row(pane: &Pane, notes: &[Note]) -> Row {
        Row {
            pane: Some(pane.id),
            notes: notes.to_vec(),
        }
    }

    #[test]
    fn a_close_asks_only_for_panes_it_would_cut_off() {
        let idle = terminal(facts(AgentState::Idle));
        let done = terminal(facts(AgentState::Done));
        let working = terminal(facts(AgentState::Working));
        let asking = terminal(facts(AgentState::Blocked));
        let editor = Pane {
            id: Uuid::new_v4(),
            kind: PaneKind::Other,
        };

        assert_eq!(plan(Leave::Close, &[idle, done, editor], &[], true), None);
        // With the setting off, a busy pane goes to Undo instead.
        assert_eq!(plan(Leave::Close, &[working], &[], false), None);

        assert_eq!(
            plan(Leave::Close, &[working], &[], true),
            Some(Question {
                leave: Leave::Close,
                title: Title::CloseOne(working.id),
                rows: vec![row(&working, &[])],
                idle: None,
                danger: true,
            })
        );
        assert_eq!(
            plan(Leave::Close, &[idle, working, asking, done], &[], true),
            Some(Question {
                leave: Leave::Close,
                title: Title::CloseMany {
                    n: 4,
                    sessions: true
                },
                rows: vec![row(&working, &[]), row(&asking, &[])],
                idle: Some(2),
                danger: true,
            })
        );
        // Nothing idle to close on its own: no idle choice. A batch with a
        // non-terminal counts panes, not sessions.
        let question = plan(Leave::Close, &[working, asking, editor], &[], true).unwrap();
        assert_eq!(question.idle, Some(1));
        assert_eq!(
            question.title,
            Title::CloseMany {
                n: 3,
                sessions: false
            }
        );
        assert_eq!(
            plan(Leave::Close, &[working, asking], &[], true)
                .unwrap()
                .idle,
            None
        );
    }

    #[test]
    fn a_row_says_only_what_its_pill_cannot_and_red_means_certain_loss() {
        let unconfirmed = terminal(CloseFacts {
            evidence: CloseEvidence::Output,
            ..facts(AgentState::Idle)
        });
        let cmd = Pane {
            id: Uuid::new_v4(),
            kind: PaneKind::Terminal {
                facts: CloseFacts {
                    evidence: CloseEvidence::Quiet,
                    ..facts(AgentState::Idle)
                },
                unmarked_shell: true,
            },
        };
        let watched = terminal(CloseFacts {
            another_tmux_client: true,
            ..facts(AgentState::Done)
        });
        let detached = terminal(CloseFacts {
            session_may_outlive: true,
            ..facts(AgentState::Exited)
        });
        let gone = terminal(facts(AgentState::Exited));

        // Every one of these is uncertain, so the button isn't red.
        assert_eq!(
            plan(
                Leave::Close,
                &[unconfirmed, cmd, watched, detached, gone],
                &[],
                true
            ),
            Some(Question {
                leave: Leave::Close,
                title: Title::CloseMany {
                    n: 5,
                    sessions: true
                },
                rows: vec![
                    row(&unconfirmed, &[Note::MaybeBusy]),
                    row(&cmd, &[Note::UnmarkedPrompt]),
                    row(&watched, &[Note::AttachedElsewhere]),
                    row(&detached, &[Note::TmuxOpen]),
                ],
                idle: Some(1),
                danger: false,
            })
        );
        let starting = terminal(facts(AgentState::Starting));
        assert!(
            plan(Leave::Close, &[cmd, starting], &[], true)
                .unwrap()
                .danger
        );
    }

    #[test]
    fn quit_asks_for_busy_panes_and_cut_short_input_and_counts_down_otherwise() {
        let idle = terminal(facts(AgentState::Idle));
        let working = terminal(facts(AgentState::Working));
        let dirty = Pane {
            id: Uuid::new_v4(),
            kind: PaneKind::Editor { dirty: true },
        };
        let clean = Pane {
            id: Uuid::new_v4(),
            kind: PaneKind::Editor { dirty: false },
        };

        assert_eq!(plan(Leave::Quit, &[idle, clean], &[], true), None);
        assert_eq!(plan(Leave::Restart, &[idle, clean], &[], true), None);

        // Input for a pane joins that pane's row, in pane order; input of the
        // whole app is a row of its own, after the panes.
        assert_eq!(
            plan(
                Leave::Quit,
                &[idle, working, dirty, clean],
                &[
                    Pending::Dictation,
                    Pending::CtlSend(Some(working.id)),
                    Pending::CtlSend(Some(working.id)),
                    Pending::Upload(Some(idle.id)),
                ],
                true,
            ),
            Some(Question {
                leave: Leave::Quit,
                title: Title::Quit,
                rows: vec![
                    row(&idle, &[Note::Uploading]),
                    row(&working, &[Note::CtlSend]),
                    row(&dirty, &[Note::Unsaved]),
                    Row {
                        pane: None,
                        notes: vec![Note::Dictation],
                    },
                ],
                idle: None,
                danger: true,
            })
        );

        // What only might be lost asks without red.
        let waking = plan(Leave::Restart, &[idle], &[Pending::WakeSweep], true).unwrap();
        assert_eq!(waking.title, Title::Restart);
        assert!(!waking.danger);
        for (item, certain) in [
            (Pending::CtlKeys(Some(idle.id)), true),
            (Pending::StartupInput(idle.id), true),
            (Pending::Launching(idle.id), false),
            (Pending::EditorMove(clean.id), false),
            (Pending::CloseUndecided(idle.id), false),
            (Pending::FileDialog, false),
            (Pending::Changed(idle.id), false),
        ] {
            let question = plan(Leave::Quit, &[idle, clean], &[item], true).unwrap();
            assert_eq!(question.rows.len(), 1, "{item:?}");
            assert_eq!(question.danger, certain, "{item:?}");
        }
    }

    #[test]
    fn a_tmux_session_asks_on_close_and_removal_but_not_on_quit_or_restart() {
        let tmux = terminal(CloseFacts {
            session_may_outlive: true,
            ..facts(AgentState::Working)
        });
        let detached = terminal(CloseFacts {
            session_may_outlive: true,
            ..facts(AgentState::Exited)
        });
        for leave in [Leave::Close, Leave::RemoveProject] {
            assert_eq!(
                plan(leave, &[tmux, detached], &[], true).unwrap().rows,
                vec![row(&tmux, &[]), row(&detached, &[Note::TmuxOpen])],
                "{leave:?}"
            );
        }
        // Nor does its change during a quit countdown make the countdown ask.
        let idle = terminal(facts(AgentState::Idle));
        let PaneKind::Terminal { facts: quiet, .. } = idle.kind else {
            unreachable!()
        };
        assert_eq!(
            countdown_facts(&[tmux, idle, detached]),
            vec![(idle.id, quiet)]
        );
        for leave in [Leave::Quit, Leave::Restart] {
            assert_eq!(plan(leave, &[tmux, detached], &[], true), None, "{leave:?}");
            // Input on its way to the pane lives in muxel, and quitting drops it.
            assert_eq!(
                plan(leave, &[tmux], &[Pending::CtlSend(Some(tmux.id))], true)
                    .unwrap()
                    .rows,
                vec![row(&tmux, &[Note::CtlSend])],
                "{leave:?}"
            );
        }
    }

    #[test]
    fn removing_a_project_always_asks_and_names_what_it_cuts_off() {
        let idle = terminal(facts(AgentState::Idle));
        let working = terminal(facts(AgentState::Working));
        assert_eq!(
            plan(Leave::RemoveProject, &[idle], &[], true),
            Some(Question {
                leave: Leave::RemoveProject,
                title: Title::RemoveProject,
                rows: Vec::new(),
                idle: None,
                danger: true,
            })
        );
        assert_eq!(
            plan(Leave::RemoveProject, &[idle, working], &[], true)
                .unwrap()
                .rows,
            vec![row(&working, &[])]
        );
        assert!(plan(Leave::RemoveProject, &[], &[], true).is_some());
    }

    #[test]
    fn enter_does_the_thing_esc_backs_out_and_i_only_where_it_exists() {
        assert_eq!(answer("enter", false), Some(Answer::Confirm));
        assert_eq!(answer("escape", true), Some(Answer::Cancel));
        assert_eq!(answer("i", true), Some(Answer::IdleOnly));
        assert_eq!(answer("i", false), None);
        assert_eq!(answer("q", true), None);
        assert_eq!(answer("space", true), None);
    }
}
