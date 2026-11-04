//! Sidebar ordering and unread decisions over persisted activity, without UI or I/O.
use std::cmp::Reverse;
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AgentActivity, Instance, Workspace};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarSort {
    #[default]
    Layout,
    RecentWork,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityBand {
    Today,
    ThisWeek,
    Older,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarEntry {
    Worktree(Uuid),
    Band(ActivityBand),
    Pane(Uuid),
}

pub type InstancesById<'a> = HashMap<Uuid, &'a Instance>;

pub fn instances_by_id(workspace: &Workspace) -> InstancesById<'_> {
    workspace.instances.iter().map(|i| (i.id, i)).collect()
}

/// A session's place in Recent work: when it last finished or asked for something.
/// Starting a turn doesn't count, so typing in a session leaves the list where it
/// is. A session with neither event yet goes by its latest start.
pub fn recent_work_at(activity: &AgentActivity) -> Option<i64> {
    activity
        .completed_at
        .max(activity.blocked_at)
        .or(activity.work_started_at)
}

pub fn activity_band(activity: &AgentActivity, now: i64) -> Option<ActivityBand> {
    recent_work_at(activity).map(|at| band_at(at, now))
}

fn band_at(at: i64, now: i64) -> ActivityBand {
    let age = now.saturating_sub(at);
    if age < crate::DAY_MS {
        ActivityBand::Today
    } else if age < 7 * crate::DAY_MS {
        ActivityBand::ThisWeek
    } else {
        ActivityBand::Older
    }
}

/// An unseen completion or question remains unread even after another turn starts.
pub fn is_unread(activity: &AgentActivity) -> bool {
    [activity.completed_at, activity.blocked_at]
        .into_iter()
        .flatten()
        .max()
        .is_some_and(|event| activity.last_attended_at.is_none_or(|seen| event > seen))
}

/// A presentation filter only; callers retain the events for desktop/tray use.
pub fn notification_in_feed(sort: SidebarSort, agent_event: bool) -> bool {
    sort == SidebarSort::Layout || !agent_event
}

/// The caller supplies this project's visible pane ids, including live popouts
/// in recent mode. Layout preserves existing worktree groups. Recent work is flat,
/// so worktree membership cannot hold older sessions above newer work.
/// Both sorts are stable; only recent work pulls pins into a leading block.
pub fn sidebar_order(
    pane_ids: &[Uuid],
    instances: &InstancesById,
    now: i64,
    sort: SidebarSort,
) -> Vec<SidebarEntry> {
    if sort == SidebarSort::Layout {
        let mut plain = Vec::new();
        let mut groups: Vec<(Uuid, Vec<Uuid>)> = Vec::new();
        let mut group_index = HashMap::new();
        for &id in pane_ids {
            if let Some(wid) = instances.get(&id).and_then(|i| i.worktree_id) {
                let index = *group_index.entry(wid).or_insert_with(|| {
                    groups.push((wid, Vec::new()));
                    groups.len() - 1
                });
                groups[index].1.push(id);
            } else {
                plain.push(SidebarEntry::Pane(id));
            }
        }
        for (wid, panes) in groups {
            plain.push(SidebarEntry::Worktree(wid));
            plain.extend(panes.into_iter().map(SidebarEntry::Pane));
        }
        return plain;
    }

    let mut panes: Vec<_> = pane_ids
        .iter()
        .copied()
        .map(|id| {
            let instance = instances.get(&id);
            (
                id,
                instance
                    .and_then(|i| recent_work_at(&i.activity))
                    .map(|at| at.min(now)),
                instance.is_some_and(|i| i.pinned),
            )
        })
        .collect();
    panes.sort_by_key(|(_, at, pinned)| (!pinned, Reverse(*at)));
    let pinned = panes.iter().take_while(|(_, _, pinned)| *pinned).count();
    let mut out: Vec<_> = panes[..pinned]
        .iter()
        .map(|(id, _, _)| SidebarEntry::Pane(*id))
        .collect();
    // Pinned rows have a pin glyph instead of an age heading; their ages must
    // not cause Older → Today headings to repeat across the pinned boundary.
    let mut bands = panes[pinned..]
        .iter()
        .filter_map(|(_, at, _)| at.map(|at| band_at(at, now)));
    let first_band = bands.next();
    let label_bands = bands.any(|band| Some(band) != first_band);
    let mut current = None;
    for &(id, at, _) in &panes[pinned..] {
        let band = at.map(|at| band_at(at, now));
        if label_bands
            && let Some(band) = band
            && current != Some(band)
        {
            out.push(SidebarEntry::Band(band));
        }
        current = band;
        out.push(SidebarEntry::Pane(id));
    }
    out
}

pub fn unread_count(pane_ids: &[Uuid], instances: &InstancesById) -> usize {
    pane_ids
        .iter()
        .filter_map(|id| instances.get(id))
        .filter(|i| is_unread(&i.activity))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentActivityState, LeafData, PaneNode, Project};

    const DAY: i64 = 86_400_000;
    const NOW: i64 = 400 * DAY;

    fn worked(at: i64) -> AgentActivity {
        AgentActivity {
            work_started_at: Some(at - 100),
            completed_at: Some(at),
            last_attended_at: Some(at),
            last_state: Some(AgentActivityState::Done),
            ..Default::default()
        }
    }

    fn fixture(activities: Vec<AgentActivity>) -> (Project, Workspace, Vec<Uuid>) {
        let mut project = Project::new("repo", "/repo");
        let mut workspace = Workspace::default();
        let mut ids = Vec::new();
        for activity in activities {
            let mut instance = Instance::shell(project.id);
            instance.activity = activity;
            ids.push(instance.id);
            workspace.add_instance(instance);
        }
        project.layout = Some(PaneNode::Leaf(LeafData {
            pane_id: Uuid::new_v4(),
            tabs: ids.clone(),
            active: 0,
        }));
        workspace.projects.push(project.clone());
        (project, workspace, ids)
    }

    fn pane_order(project: &Project, workspace: &Workspace) -> Vec<Uuid> {
        sidebar_order(
            &project.instances(),
            &instances_by_id(workspace),
            NOW,
            SidebarSort::RecentWork,
        )
        .into_iter()
        .filter_map(|entry| match entry {
            SidebarEntry::Pane(id) => Some(id),
            _ => None,
        })
        .collect()
    }

    #[test]
    fn starting_a_turn_keeps_the_row_until_it_finishes_or_asks() {
        for outcome in [AgentActivityState::Done, AgentActivityState::Blocked] {
            let (project, mut workspace, ids) =
                fixture(vec![worked(NOW - 1_000), worked(NOW - 2_000)]);
            let older = &mut workspace.instance_mut(ids[1]).unwrap().activity;
            older.observe(
                Some(AgentActivityState::Done),
                AgentActivityState::Working,
                NOW - 500,
            );
            assert_eq!(pane_order(&project, &workspace), vec![ids[0], ids[1]]);

            let older = &mut workspace.instance_mut(ids[1]).unwrap().activity;
            older.observe(Some(AgentActivityState::Working), outcome, NOW - 100);
            assert_eq!(pane_order(&project, &workspace), vec![ids[1], ids[0]]);
        }
    }

    #[test]
    fn a_new_turn_on_old_work_stays_in_its_band() {
        let (project, mut workspace, ids) =
            fixture(vec![worked(NOW - 1_000), worked(NOW - 40 * DAY)]);
        workspace.instance_mut(ids[1]).unwrap().activity.observe(
            Some(AgentActivityState::Done),
            AgentActivityState::Working,
            NOW - 500,
        );
        assert_eq!(
            sidebar_order(
                &project.instances(),
                &instances_by_id(&workspace),
                NOW,
                SidebarSort::RecentWork
            ),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                SidebarEntry::Pane(ids[0]),
                SidebarEntry::Band(ActivityBand::Older),
                SidebarEntry::Pane(ids[1]),
            ]
        );
    }

    #[test]
    fn a_session_with_no_outcome_yet_sorts_by_its_start() {
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - 2_000),
            AgentActivity::default(),
            AgentActivity::default(),
        ]);
        workspace.instance_mut(ids[2]).unwrap().activity.observe(
            None,
            AgentActivityState::Working,
            NOW - 1_000,
        );
        assert_eq!(
            pane_order(&project, &workspace),
            vec![ids[2], ids[0], ids[1]]
        );
    }

    #[test]
    fn sidebar_orders_by_last_work_not_layout_order() {
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - 2_000),
            worked(NOW - 35 * DAY),
            worked(NOW - 1_000),
        ]);
        workspace
            .instance_mut(ids[1])
            .unwrap()
            .activity
            .last_attended_at = Some(NOW);
        workspace
            .instance_mut(ids[2])
            .unwrap()
            .activity
            .last_attended_at = None;
        let index = instances_by_id(&workspace);
        assert_eq!(
            sidebar_order(&project.instances(), &index, NOW, SidebarSort::RecentWork),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                SidebarEntry::Pane(ids[2]),
                SidebarEntry::Pane(ids[0]),
                SidebarEntry::Band(ActivityBand::Older),
                SidebarEntry::Pane(ids[1]),
            ]
        );
        assert_eq!(unread_count(&project.instances(), &index), 1);
        assert_eq!(
            activity_band(&index[&ids[1]].activity, NOW),
            Some(ActivityBand::Older)
        );
    }

    #[test]
    fn last_work_and_bands_use_all_lifecycle_events_never_attendance() {
        let mut activity = worked(NOW - 35 * DAY);
        activity.blocked_at = Some(NOW - DAY);
        activity.last_attended_at = Some(NOW);
        assert_eq!(activity.last_work_at(), activity.blocked_at);
        for (age, band) in [
            (DAY - 1, ActivityBand::Today),
            (DAY, ActivityBand::ThisWeek),
            (7 * DAY - 1, ActivityBand::ThisWeek),
            (7 * DAY, ActivityBand::Older),
            (-DAY, ActivityBand::Today),
        ] {
            assert_eq!(activity_band(&worked(NOW - age), NOW), Some(band));
        }
        assert_eq!(
            activity_band(
                &AgentActivity {
                    last_attended_at: Some(NOW),
                    ..Default::default()
                },
                NOW
            ),
            None
        );
    }

    #[test]
    fn pinned_first_across_worktrees_then_newest_and_stable_ties() {
        let (project, mut workspace, ids) =
            fixture(vec![worked(NOW), worked(NOW - 40 * DAY), worked(NOW)]);
        workspace.instance_mut(ids[1]).unwrap().pinned = true;
        workspace.instance_mut(ids[1]).unwrap().worktree_id = Some(Uuid::new_v4());
        assert_eq!(
            sidebar_order(
                &project.instances(),
                &instances_by_id(&workspace),
                NOW,
                SidebarSort::RecentWork
            ),
            vec![
                SidebarEntry::Pane(ids[1]),
                SidebarEntry::Pane(ids[0]),
                SidebarEntry::Pane(ids[2]),
            ]
        );
    }

    #[test]
    fn layout_keeps_worktree_groups_and_recent_keeps_never_worked_last() {
        let (project, mut workspace, ids) =
            fixture(vec![AgentActivity::default(), worked(NOW - 1), worked(NOW)]);
        let wid = Uuid::new_v4();
        workspace.instance_mut(ids[1]).unwrap().worktree_id = Some(wid);
        assert_eq!(
            sidebar_order(
                &project.instances(),
                &instances_by_id(&workspace),
                NOW,
                SidebarSort::Layout
            ),
            vec![
                SidebarEntry::Pane(ids[0]),
                SidebarEntry::Pane(ids[2]),
                SidebarEntry::Worktree(wid),
                SidebarEntry::Pane(ids[1]),
            ]
        );
        assert_eq!(
            sidebar_order(
                &project.instances(),
                &instances_by_id(&workspace),
                NOW,
                SidebarSort::RecentWork
            ),
            vec![
                SidebarEntry::Pane(ids[2]),
                SidebarEntry::Pane(ids[1]),
                SidebarEntry::Pane(ids[0]),
            ]
        );
    }

    #[test]
    fn completed_and_blocked_stay_unread_until_attendance_even_on_next_turn() {
        for state in [AgentActivityState::Done, AgentActivityState::Blocked] {
            let mut activity = AgentActivity::default();
            activity.observe(None, AgentActivityState::Working, 100);
            activity.observe(Some(AgentActivityState::Working), state, 200);
            assert!(is_unread(&activity));
            activity.observe(Some(state), AgentActivityState::Working, 300);
            assert!(is_unread(&activity));
            activity.attend(400);
            assert!(!is_unread(&activity));
            assert_eq!(activity.last_work_at(), Some(300));
        }
    }

    #[test]
    fn recent_work_folds_agent_events_only() {
        assert!(!notification_in_feed(SidebarSort::RecentWork, true));
        assert!(notification_in_feed(SidebarSort::RecentWork, false));
        assert!(notification_in_feed(SidebarSort::Layout, true));
        assert!(notification_in_feed(SidebarSort::Layout, false));
    }

    #[test]
    fn duplicate_has_no_work_or_unread_of_its_own() {
        let mut instance = Instance::shell(Uuid::new_v4());
        instance.activity = worked(NOW);
        instance.activity.last_attended_at = None;
        instance.reset_conversation_for_duplicate();
        assert_eq!(instance.activity, AgentActivity::default());
    }

    #[test]
    fn new_peer_pane_does_not_import_another_machines_attendance() {
        let mut peer = Instance::shell(Uuid::new_v4());
        peer.activity = worked(NOW);
        let local_pid = Uuid::new_v4();
        let merged = crate::merge_peer_instances(&[], vec![peer], local_pid);
        assert_eq!(merged[0].activity, AgentActivity::default());
    }

    #[test]
    fn old_settings_keep_layout_and_recent_choice_round_trips() {
        let mut settings: crate::Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.sidebar_sort, SidebarSort::Layout);
        assert_eq!(crate::Settings::default().sidebar_sort, SidebarSort::Layout);
        settings.sidebar_sort = SidebarSort::RecentWork;
        let restored: crate::Settings =
            serde_json::from_value(serde_json::to_value(settings).unwrap()).unwrap();
        assert_eq!(restored.sidebar_sort, SidebarSort::RecentWork);
    }

    #[test]
    fn blocked_history_survives_idle_and_new_blocked_turn_gets_new_time() {
        let mut activity = AgentActivity::default();
        activity.observe(None, AgentActivityState::Working, 100);
        activity.observe(
            Some(AgentActivityState::Working),
            AgentActivityState::Blocked,
            200,
        );
        activity.observe(
            Some(AgentActivityState::Blocked),
            AgentActivityState::Idle,
            300,
        );
        assert_eq!(activity.blocked_at, Some(200));
        assert_eq!(activity.current_state(), Some(AgentActivityState::Idle));
        assert!(is_unread(&activity));
        activity.attend(400);
        // First live sample after restore is a NEW block after the prior idle.
        activity.observe(None, AgentActivityState::Blocked, 500);
        assert_eq!(activity.blocked_at, Some(500));
        assert!(is_unread(&activity));
        activity.attend(500);
        assert!(!is_unread(&activity));
    }

    #[test]
    fn future_work_clamps_to_now_and_ties_keep_layout_order() {
        let (project, workspace, ids) =
            fixture(vec![worked(NOW), worked(NOW + DAY), worked(NOW - 1)]);
        assert_eq!(
            sidebar_order(
                &project.instances(),
                &instances_by_id(&workspace),
                NOW,
                SidebarSort::RecentWork
            ),
            ids.into_iter().map(SidebarEntry::Pane).collect::<Vec<_>>()
        );
    }
}
