//! Window ownership must not change a live terminal's background lifecycle.

use super::{LoopCleanup, LoopRun, PostRunAction, terminal_view_in};
use std::collections::HashMap;
use uuid::Uuid;

pub(super) fn loop_heartbeat<'a, T, P>(
    runs: &mut HashMap<Uuid, LoopRun>,
    docked: &'a HashMap<Uuid, T>,
    popouts: &'a HashMap<Uuid, P>,
    terminal: impl Fn(&'a P) -> Option<&'a T>,
    cleanup: impl Fn(&T, &mut LoopRun) -> LoopCleanup,
) -> Vec<Uuid> {
    let mut close = Vec::new();
    runs.retain(|iid, run| {
        let Some(view) = terminal_view_in(*iid, docked, popouts, &terminal) else {
            return false;
        };
        match cleanup(view, run) {
            LoopCleanup::Continue => true,
            LoopCleanup::Abandon => false,
            LoopCleanup::Finish => {
                if run.post_run == PostRunAction::Exit {
                    close.push(*iid);
                }
                false
            }
        }
    });
    close
}

#[cfg(test)]
mod tests {
    use super::loop_heartbeat;
    use crate::app::{AgentStatus, LoopRun, PostRunAction, launch_initial_binding};
    use std::cell::Cell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    #[test]
    fn minted_claude_loop_without_hook_or_uuid_title_finishes_and_closes_exit_pane() {
        use muxel_terminal::{
            CommandSpec, TerminalRuntimeSnapshot, TerminalSession, TerminalStatusCoverage,
            TerminalStatusReason,
        };
        let pane = Uuid::new_v4();
        let id = Uuid::new_v4();
        #[cfg(windows)]
        let mut spec = CommandSpec::program(
            "cmd.exe",
            vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
        );
        #[cfg(unix)]
        let mut spec = CommandSpec::program("/bin/cat", vec![]);
        let launch_args = vec!["--session-id".to_string(), id.to_string()];
        spec.status_program = Some("claude".into());
        spec.initial_binding = launch_initial_binding(Some("claude"), Some(&launch_args), false);
        let (session, _receiver) = TerminalSession::spawn(spec, 80, 24).unwrap();
        session.mark_turn_submitted();
        assert_eq!(session.binding_revision(), 1);
        assert_eq!(session.first_submission_binding(), Some(1));
        let make_snapshot = |status| TerminalRuntimeSnapshot {
            status,
            attributable: true,
            reason: TerminalStatusReason::Quiet,
            coverage: TerminalStatusCoverage::ProviderVisible,
            launch_id: session.id,
            binding_rev: session.binding_revision(),
            snapshot_seq: 1,
            input_rev: 0,
            draft_latched: false,
            completion_ready: status == AgentStatus::Done,
            completion_revision: 0,
        };
        let terminal = Rc::new(Cell::new(make_snapshot(AgentStatus::Working)));
        let docked = HashMap::from([(pane, terminal.clone())]);
        let mut runs = HashMap::from([(
            pane,
            LoopRun {
                loop_id: Uuid::new_v4(),
                launch: Some(session.id),
                seen_working: false,
                started: Instant::now(),
                post_run: PostRunAction::Exit,
            },
        )]);
        let beat = |runs: &mut HashMap<_, _>| {
            loop_heartbeat(
                runs,
                &docked,
                &HashMap::<Uuid, Option<Rc<Cell<TerminalRuntimeSnapshot>>>>::new(),
                Option::as_ref,
                |view, run| {
                    run.cleanup(
                        view.get(),
                        session.binding_revision(),
                        session.first_submission_binding(),
                        Duration::ZERO,
                    )
                },
            )
        };
        assert!(beat(&mut runs).is_empty());
        terminal.set(make_snapshot(AgentStatus::Done));
        assert_eq!(beat(&mut runs), vec![pane]);
        assert!(runs.is_empty());
        session.kill();
    }

    #[test]
    fn popout_loop_grok_heartbeat_retains_owned_run_through_redock_and_exit_cleanup() {
        use muxel_terminal::{
            TerminalRuntimeSnapshot, TerminalStatusCoverage, TerminalStatusReason,
        };
        let pane = Uuid::new_v4();
        let loop_id = Uuid::new_v4();
        let launch = Uuid::new_v4();
        let runtime = TerminalRuntimeSnapshot {
            status: AgentStatus::Working,
            attributable: true,
            reason: TerminalStatusReason::Quiet,
            coverage: TerminalStatusCoverage::ProviderVisible,
            launch_id: launch,
            binding_rev: 1,
            snapshot_seq: 1,
            input_rev: 0,
            draft_latched: false,
            completion_ready: false,
            completion_revision: 0,
        };
        let terminal = Rc::new(Cell::new(runtime));
        let mut docked = HashMap::from([(pane, terminal.clone())]);
        let mut popouts = HashMap::new();
        let mut runs = HashMap::from([(
            pane,
            LoopRun {
                loop_id,
                launch: Some(launch),
                seen_working: false,
                started: Instant::now(),
                post_run: PostRunAction::Exit,
            },
        )]);
        let beat = |runs: &mut HashMap<_, _>, docked: &HashMap<_, _>, popouts: &HashMap<_, _>| {
            loop_heartbeat(
                runs,
                docked,
                popouts,
                Option::as_ref,
                |view: &Rc<Cell<TerminalRuntimeSnapshot>>, run| {
                    run.cleanup(view.get(), 1, Some(1), Duration::ZERO)
                },
            )
        };
        assert!(beat(&mut runs, &docked, &popouts).is_empty());
        popouts.insert(pane, Some(docked.remove(&pane).unwrap()));
        assert!(beat(&mut runs, &docked, &popouts).is_empty());
        assert!(
            runs.contains_key(&pane),
            "a live popout is not a closed pane"
        );
        assert!(runs.values().any(|run| run.loop_id == loop_id));
        docked.insert(pane, popouts.remove(&pane).unwrap().unwrap());
        assert!(Rc::ptr_eq(docked.get(&pane).unwrap(), &terminal));
        assert!(beat(&mut runs, &docked, &popouts).is_empty());
        terminal.set(TerminalRuntimeSnapshot {
            status: AgentStatus::Done,
            completion_ready: true,
            ..runtime
        });
        assert_eq!(beat(&mut runs, &docked, &popouts), vec![pane]);
        assert!(runs.is_empty());
    }
}
