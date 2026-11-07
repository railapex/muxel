//! Sidebar ordering and unread decisions over persisted activity, without UI or I/O.
use std::cmp::{Ordering, Reverse};
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AgentActivity, AgentActivityState, Instance, InstanceKind, Workspace};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarSort {
    #[default]
    Layout,
    RecentWork,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
    /// A file, diff or page drawn under the session row before it, which
    /// opened it. Recent work only.
    Nested(Uuid),
}

pub type InstancesById<'a> = HashMap<Uuid, &'a Instance>;

pub fn instances_by_id(workspace: &Workspace) -> InstancesById<'_> {
    workspace.instances.iter().map(|i| (i.id, i)).collect()
}

/// When a session last finished or asked for something: what orders the rows
/// that need you and the rows at rest. Starting a turn doesn't count. A session
/// with neither event yet goes by its latest start.
pub fn recent_work_at(activity: &AgentActivity) -> Option<i64> {
    activity
        .completed_at
        .max(activity.blocked_at)
        .or(activity.work_started_at)
}

/// The three groups of rows within each band of Recent work, top to bottom.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecentTier {
    /// Unread, or asking for something now: newest event first. An agent
    /// that finishes and starts again by itself stays here until it is seen.
    NeedsYou,
    /// Working and seen: in the order each went working, so a session that
    /// starts a turn joins the bottom.
    Working,
    /// Done and read, idle, or never worked: newest finish first.
    Rest,
}

/// Where a row sits in Recent work: its band, its tier within the band, then
/// its time within the tier. Sessions with no recorded work come last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecentPlace {
    band: Option<ActivityBand>,
    tier: RecentTier,
    at: Option<i64>,
}

impl RecentPlace {
    /// `shown` is the state the row shows now. A session with no live terminal
    /// shows idle whatever state it was saved in, so it isn't working here.
    pub fn of(activity: &AgentActivity, shown: Option<AgentActivityState>, now: i64) -> Self {
        let working = shown == Some(AgentActivityState::Working);
        let tier = if is_unread(activity) || shown == Some(AgentActivityState::Blocked) {
            RecentTier::NeedsYou
        } else if working {
            RecentTier::Working
        } else {
            RecentTier::Rest
        };
        Self::placed(activity, tier, working, now)
    }

    fn placed(activity: &AgentActivity, tier: RecentTier, working: bool, now: i64) -> Self {
        let at = if tier == RecentTier::Working {
            // A turn the reducer hasn't recorded yet started now.
            Some(activity.work_started_at.unwrap_or(now).min(now))
        } else {
            recent_work_at(activity).map(|at| at.min(now))
        };
        // A working session is active now, however long ago it last finished.
        let band = if working {
            Some(ActivityBand::Today)
        } else {
            at.map(|at| band_at(at, now))
        };
        Self { band, tier, at }
    }

    pub fn band(self) -> Option<ActivityBand> {
        self.band
    }

    pub fn tier(self) -> RecentTier {
        self.tier
    }
}

impl Ord for RecentPlace {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.band.is_none(), self.band, self.tier)
            .cmp(&(other.band.is_none(), other.band, other.tier))
            .then_with(|| match self.tier {
                RecentTier::Working => self.at.cmp(&other.at),
                RecentTier::NeedsYou | RecentTier::Rest => other.at.cmp(&self.at),
            })
    }
}

impl PartialOrd for RecentPlace {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The focused session's place in Recent work, taken as it gets focus and
/// before it is attended. Reading an unread row would drop it from the rows
/// that need you to the rows at rest under the click, so the row keeps this
/// place until focus leaves. Going working is the exception: from then until
/// focus leaves, it sits with the working rows by the start of its newest turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FocusHold {
    pub id: Uuid,
    place: RecentPlace,
    started: Option<i64>,
}

impl FocusHold {
    pub fn take(
        id: Uuid,
        activity: &AgentActivity,
        shown: Option<AgentActivityState>,
        now: i64,
    ) -> Self {
        Self {
            id,
            place: RecentPlace::of(activity, shown, now),
            started: activity.work_started_at,
        }
    }

    fn place(&self, activity: &AgentActivity, now: i64) -> RecentPlace {
        if activity.work_started_at != self.started {
            RecentPlace::placed(activity, RecentTier::Working, true, now)
        } else {
            self.place
        }
    }
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

/// The session row resource `id` nests under in Recent work: the session that
/// last opened it, when that session is one of `listed` (the project's rows).
/// A session that has closed, is held for Undo, or sits in another project
/// leaves the resource as an ordinary row. Sessions never nest and only
/// sessions take nested rows, so nesting is one level deep. A pinned resource
/// nests only under a pinned session, so its pin keeps it in the pinned block.
pub fn nests_under(id: Uuid, instances: &InstancesById, listed: &HashSet<Uuid>) -> Option<Uuid> {
    let instance = instances.get(&id)?;
    if instance.kind == InstanceKind::Terminal {
        return None;
    }
    instance.opened_by.filter(|session| {
        listed.contains(session)
            && instances.get(session).is_some_and(|session| {
                session.kind == InstanceKind::Terminal && (session.pinned || !instance.pinned)
            })
    })
}

/// The files, diffs and pages that close with `sessions` when the user closes
/// them in Recent work: each one in a session's project whose last opener is
/// that session. A resource another session opened again since belongs to that
/// session and stays. Returns `(resource, session)` pairs in workspace order.
pub fn closes_with(
    sort: SidebarSort,
    sessions: &[Uuid],
    instances: &[Instance],
) -> Vec<(Uuid, Uuid)> {
    if sort != SidebarSort::RecentWork {
        return Vec::new();
    }
    let projects: HashMap<Uuid, Uuid> = instances
        .iter()
        .filter(|i| i.kind == InstanceKind::Terminal && sessions.contains(&i.id))
        .map(|i| (i.id, i.project_id))
        .collect();
    instances
        .iter()
        .filter(|instance| instance.kind != InstanceKind::Terminal)
        .filter_map(|instance| {
            let session = instance.opened_by?;
            (projects.get(&session) == Some(&instance.project_id)).then_some((instance.id, session))
        })
        .collect()
}

/// A presentation filter only; callers retain the events for desktop/tray use.
pub fn notification_in_feed(sort: SidebarSort, agent_event: bool) -> bool {
    sort == SidebarSort::Layout || !agent_event
}

/// The caller supplies this project's visible pane ids, including live popouts
/// in recent mode. Layout preserves existing worktree groups. Recent work is flat,
/// so worktree membership cannot hold older sessions above newer work, except
/// that a file, diff or page follows the session that opened it
/// ([`nests_under`]), newest open first.
/// Both sorts are stable; only recent work pulls pins into a leading block.
/// Recent work places the other rows by [`RecentPlace`], from the state each
/// row shows (`shown`), and the focused row by its [`FocusHold`].
pub fn sidebar_order(
    pane_ids: &[Uuid],
    instances: &InstancesById,
    shown: &dyn Fn(Uuid) -> Option<AgentActivityState>,
    hold: Option<&FocusHold>,
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

    let listed: HashSet<Uuid> = pane_ids.iter().copied().collect();
    let mut top = Vec::new();
    let mut nested: HashMap<Uuid, Vec<(Uuid, Option<i64>)>> = HashMap::new();
    for &id in pane_ids {
        match nests_under(id, instances, &listed) {
            Some(session) => nested
                .entry(session)
                .or_default()
                .push((id, instances.get(&id).and_then(|i| i.opened_at))),
            None => top.push(id),
        }
    }
    // Newest open first; the sort is stable, so ties keep layout order.
    for children in nested.values_mut() {
        children.sort_by_key(|(_, at)| Reverse(*at));
    }
    let row = |out: &mut Vec<SidebarEntry>, id: Uuid| {
        out.push(SidebarEntry::Pane(id));
        if let Some(children) = nested.get(&id) {
            out.extend(
                children
                    .iter()
                    .map(|(child, _)| SidebarEntry::Nested(*child)),
            );
        }
    };

    let (mut pinned, rest): (Vec<_>, Vec<_>) = top
        .into_iter()
        .partition(|id| instances.get(id).is_some_and(|i| i.pinned));
    pinned.sort_by_key(|id| {
        Reverse(
            instances
                .get(id)
                .and_then(|i| recent_work_at(&i.activity))
                .map(|at| at.min(now)),
        )
    });
    // A row with no instance has no work either: it goes with the never-worked.
    let never = RecentPlace {
        band: None,
        tier: RecentTier::Rest,
        at: None,
    };
    let mut rest: Vec<_> = rest
        .into_iter()
        .map(|id| {
            let place = instances.get(&id).map_or(never, |i| match hold {
                Some(hold) if hold.id == id => hold.place(&i.activity, now),
                _ => RecentPlace::of(&i.activity, shown(id), now),
            });
            (id, place)
        })
        .collect();
    rest.sort_by_key(|&(_, place)| place);
    let mut out = Vec::new();
    for id in pinned {
        row(&mut out, id);
    }
    // Pinned rows have a pin glyph instead of an age heading; their ages must
    // not cause Older → Today headings to repeat across the pinned boundary.
    let mut bands = rest.iter().filter_map(|(_, place)| place.band);
    let first_band = bands.next();
    let label_bands = bands.any(|band| Some(band) != first_band);
    let mut current = None;
    for &(id, place) in &rest {
        let band = place.band;
        if label_bands
            && let Some(band) = band
            && current != Some(band)
        {
            out.push(SidebarEntry::Band(band));
        }
        current = band;
        row(&mut out, id);
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
    use crate::{AgentActivityState, PaneNode, Project};

    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 86_400_000;
    const NOW: i64 = 400 * DAY;

    /// A turn that finished at `at` and has been read.
    fn worked(at: i64) -> AgentActivity {
        AgentActivity {
            work_started_at: Some(at - 100),
            completed_at: Some(at),
            last_attended_at: Some(at),
            last_state: Some(AgentActivityState::Done),
            ..Default::default()
        }
    }

    /// A turn that finished at `at` and hasn't been seen.
    fn unread(at: i64) -> AgentActivity {
        AgentActivity {
            last_attended_at: None,
            ..worked(at)
        }
    }

    fn change(workspace: &mut Workspace, id: Uuid, state: AgentActivityState, at: i64) {
        let activity = &mut workspace.instance_mut(id).unwrap().activity;
        activity.observe(activity.current_state(), state, at);
    }

    fn band(activity: &AgentActivity) -> Option<ActivityBand> {
        RecentPlace::of(activity, activity.current_state(), NOW).band()
    }

    /// Every row shows the state it was saved in, as when each has a live terminal.
    fn order_with(
        project: &Project,
        workspace: &Workspace,
        hold: Option<&FocusHold>,
        sort: SidebarSort,
    ) -> Vec<SidebarEntry> {
        let index = instances_by_id(workspace);
        let shown = |id: Uuid| index.get(&id).and_then(|i| i.activity.current_state());
        sidebar_order(&project.instances(), &index, &shown, hold, NOW, sort)
    }

    fn recent(project: &Project, workspace: &Workspace) -> Vec<SidebarEntry> {
        order_with(project, workspace, None, SidebarSort::RecentWork)
    }

    fn panes(entries: Vec<SidebarEntry>) -> Vec<Uuid> {
        entries
            .into_iter()
            .filter_map(|entry| match entry {
                SidebarEntry::Pane(id) => Some(id),
                _ => None,
            })
            .collect()
    }

    /// Takes the hold as the app does when `id` gets focus, then reads it.
    fn focus(workspace: &mut Workspace, id: Uuid, at: i64) -> FocusHold {
        let activity = &mut workspace.instance_mut(id).unwrap().activity;
        let hold = FocusHold::take(id, activity, activity.current_state(), at);
        activity.attend(at);
        hold
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
        let mut layout = PaneNode::leaf(ids.first().copied().unwrap_or_else(Uuid::new_v4));
        if let PaneNode::Leaf(leaf) = &mut layout {
            leaf.tabs = ids.clone();
        }
        project.layout = Some(layout);
        workspace.projects.push(project.clone());
        (project, workspace, ids)
    }

    fn pane_order(project: &Project, workspace: &Workspace) -> Vec<Uuid> {
        panes(recent(project, workspace))
    }

    /// A file in `project`'s single tab group, opened by `session` at `at`
    /// (or by no session).
    fn file(
        workspace: &mut Workspace,
        project: &mut Project,
        session: Option<Uuid>,
        at: i64,
    ) -> Uuid {
        let mut instance = Instance::editor(project.id, None);
        instance.record_opener(session, at);
        let id = instance.id;
        workspace.add_instance(instance);
        if let Some(PaneNode::Leaf(leaf)) = &mut project.layout {
            leaf.tabs.push(id);
        }
        id
    }

    #[test]
    fn files_nest_newest_first_under_the_session_that_last_opened_them() {
        use SidebarEntry::{Nested, Pane};
        let (mut project, mut workspace, ids) =
            fixture(vec![worked(NOW - 1_000), worked(NOW - 2_000)]);
        let (a, b) = (ids[0], ids[1]);
        let plan = file(&mut workspace, &mut project, Some(a), 10);
        let notes = file(&mut workspace, &mut project, Some(a), 20);
        let page = file(&mut workspace, &mut project, Some(b), 5);
        let loose = file(&mut workspace, &mut project, None, 0);
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Pane(a),
                Nested(notes),
                Nested(plan),
                Pane(b),
                Nested(page),
                Pane(loose)
            ]
        );

        // B finishes a turn: it moves above A and takes its page along.
        workspace.instance_mut(b).unwrap().activity.observe(
            Some(AgentActivityState::Working),
            AgentActivityState::Done,
            NOW - 500,
        );
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Pane(b),
                Nested(page),
                Pane(a),
                Nested(notes),
                Nested(plan),
                Pane(loose)
            ]
        );

        // B opens the plan again: the last opener wins, and it is B's newest.
        workspace
            .instance_mut(plan)
            .unwrap()
            .record_opener(Some(b), 30);
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Pane(b),
                Nested(plan),
                Nested(page),
                Pane(a),
                Nested(notes),
                Pane(loose)
            ]
        );
        // Reopening it from the file browser names no session and moves nothing.
        workspace
            .instance_mut(plan)
            .unwrap()
            .record_opener(None, 40);
        assert_eq!(recent(&project, &workspace)[1], Nested(plan));
        // Closing A takes only what A opened last; B took the plan.
        assert_eq!(
            closes_with(SidebarSort::RecentWork, &[a], &workspace.instances),
            vec![(notes, a)]
        );

        // Layout order ignores openers, and closes nothing with a session.
        assert_eq!(
            order_with(&project, &workspace, None, SidebarSort::Layout),
            [a, b, plan, notes, page, loose].map(Pane).to_vec()
        );
        assert!(closes_with(SidebarSort::Layout, &[a, b], &workspace.instances).is_empty());
    }

    #[test]
    fn nested_rows_ride_in_the_pinned_block_without_band_labels() {
        use SidebarEntry::{Band, Nested, Pane};
        let (mut project, mut workspace, ids) =
            fixture(vec![worked(NOW - 1_000), worked(NOW - 40 * DAY)]);
        let (recent_session, old) = (ids[0], ids[1]);
        workspace.instance_mut(old).unwrap().pinned = true;
        let page = file(&mut workspace, &mut project, Some(old), 10);
        assert_eq!(
            recent(&project, &workspace),
            vec![Pane(old), Nested(page), Pane(recent_session)]
        );
        workspace.instance_mut(old).unwrap().pinned = false;
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Band(ActivityBand::Today),
                Pane(recent_session),
                Band(ActivityBand::Older),
                Pane(old),
                Nested(page)
            ]
        );
    }

    #[test]
    fn a_pinned_file_stays_in_the_pinned_block_under_an_unpinned_session() {
        use SidebarEntry::{Nested, Pane};
        let (mut project, mut workspace, ids) =
            fixture(vec![worked(NOW - 1_000), worked(NOW - 2_000)]);
        let (a, b) = (ids[0], ids[1]);
        let plan = file(&mut workspace, &mut project, Some(b), 10);
        workspace.instance_mut(plan).unwrap().pinned = true;
        // B isn't pinned, so the plan leads the list on its own pin.
        assert_eq!(
            recent(&project, &workspace),
            vec![Pane(plan), Pane(a), Pane(b)]
        );
        // Pin B as well and the plan nests under it inside the pinned block.
        workspace.instance_mut(b).unwrap().pinned = true;
        assert_eq!(
            recent(&project, &workspace),
            vec![Pane(b), Nested(plan), Pane(a)]
        );
    }

    #[test]
    fn a_file_in_another_project_never_closes_with_the_session() {
        let (mut project, mut workspace, ids) = fixture(vec![worked(NOW - 1_000)]);
        let session = ids[0];
        let here = file(&mut workspace, &mut project, Some(session), 10);
        // Opened from this session while another project was active.
        let other = Project::new("other", "/other");
        let mut away = Instance::editor(other.id, None);
        away.record_opener(Some(session), 20);
        workspace.projects.push(other);
        workspace.add_instance(away);
        assert_eq!(
            closes_with(SidebarSort::RecentWork, &[session], &workspace.instances),
            vec![(here, session)]
        );
    }

    #[test]
    fn a_file_whose_session_is_not_listed_shows_as_an_ordinary_row() {
        // Closed, held for Undo, or in another project: the session is in the
        // workspace but not among this project's rows.
        let (mut project, mut workspace, ids) = fixture(vec![worked(NOW - 1_000)]);
        let gone = Instance::shell(project.id);
        let session = gone.id;
        workspace.add_instance(gone);
        let orphan = file(&mut workspace, &mut project, Some(session), 10);
        assert_eq!(
            recent(&project, &workspace),
            vec![SidebarEntry::Pane(ids[0]), SidebarEntry::Pane(orphan)]
        );
    }

    #[test]
    fn a_page_opened_from_a_file_belongs_to_the_files_session() {
        let (mut project, mut workspace, ids) = fixture(vec![worked(NOW - 1_000)]);
        let session = ids[0];
        let plan = file(&mut workspace, &mut project, Some(session), 10);
        let loose = file(&mut workspace, &mut project, None, 0);
        assert_eq!(workspace.opener_session(session), Some(session));
        assert_eq!(workspace.opener_session(plan), Some(session));
        assert_eq!(workspace.opener_session(loose), None);
        // Once the session has gone, its file speaks for no one.
        workspace.remove_instance_meta(session);
        assert_eq!(workspace.opener_session(plan), None);
    }

    #[test]
    fn the_opener_stays_on_this_machine() {
        let (mut project, mut workspace, ids) = fixture(vec![worked(NOW - 1_000)]);
        let plan = file(&mut workspace, &mut project, Some(ids[0]), 10);
        workspace.projects[0] = project.clone();
        let opened = crate::RemoteLayout::capture(&project, &workspace, 1);
        assert!(!opened.to_json().contains("opened_"));

        // Opening it is not a layout change, and a peer's copy keeps ours.
        let mut unopened = workspace.clone();
        unopened.instance_mut(plan).unwrap().opened_by = None;
        unopened.instance_mut(plan).unwrap().opened_at = None;
        let before = crate::RemoteLayout::capture(&project, &unopened, 1);
        assert_eq!(opened.content_key(), before.content_key());
        let peer = crate::RemoteLayout::parse(&opened.to_json(), &opened.remote_root).unwrap();
        let merged = crate::merge_peer_instances(&workspace.instances, peer.instances, project.id);
        let mine = merged.iter().find(|i| i.id == plan).unwrap();
        assert_eq!((mine.opened_by, mine.opened_at), (Some(ids[0]), Some(10)));

        // This machine's own save keeps it.
        let saved: Workspace =
            serde_json::from_str(&serde_json::to_string(&workspace).unwrap()).unwrap();
        assert_eq!(saved.instance(plan).unwrap().opened_by, Some(ids[0]));
    }

    #[test]
    fn a_session_working_for_hours_joins_the_working_rows_above_older_finishes() {
        // Chris's screenshot: three working sessions, two read finishes from
        // 11 h ago, and a session that last finished 13 h ago and has been
        // working for 2 h. It sat under the 11 h rows.
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - HOUR),
            worked(NOW - 3 * HOUR),
            worked(NOW - 4 * HOUR),
            worked(NOW - 11 * HOUR),
            worked(NOW - 11 * HOUR - MINUTE),
            worked(NOW - 13 * HOUR),
        ]);
        for (row, started) in [
            (0, NOW - 50 * MINUTE),
            (1, NOW - 150 * MINUTE),
            (2, NOW - 210 * MINUTE),
            (5, NOW - 2 * HOUR),
        ] {
            change(
                &mut workspace,
                ids[row],
                AgentActivityState::Working,
                started,
            );
        }
        // The working rows in the order each went working, then the 11 h rows.
        assert_eq!(
            recent(&project, &workspace),
            [ids[2], ids[1], ids[5], ids[0], ids[3], ids[4]]
                .map(SidebarEntry::Pane)
                .to_vec()
        );
    }

    #[test]
    fn going_working_joins_the_bottom_of_the_working_rows() {
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - 10 * MINUTE),
            worked(NOW - 20 * MINUTE),
            worked(NOW - 2 * HOUR),
            worked(NOW - 3 * HOUR),
        ]);
        change(
            &mut workspace,
            ids[2],
            AgentActivityState::Working,
            NOW - HOUR,
        );
        change(
            &mut workspace,
            ids[3],
            AgentActivityState::Working,
            NOW - 30 * MINUTE,
        );
        assert_eq!(
            pane_order(&project, &workspace),
            vec![ids[2], ids[3], ids[0], ids[1]]
        );
        // The newest start: under the working rows, above the newer finish.
        change(&mut workspace, ids[1], AgentActivityState::Working, NOW);
        assert_eq!(
            pane_order(&project, &workspace),
            vec![ids[2], ids[3], ids[1], ids[0]]
        );
    }

    #[test]
    fn with_nothing_working_a_new_turn_lands_under_the_rows_that_need_you() {
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - 10 * MINUTE),
            worked(NOW - 20 * MINUTE),
            unread(NOW - 2 * HOUR),
        ]);
        assert_eq!(
            pane_order(&project, &workspace),
            vec![ids[2], ids[0], ids[1]]
        );
        change(&mut workspace, ids[1], AgentActivityState::Working, NOW);
        assert_eq!(
            pane_order(&project, &workspace),
            vec![ids[2], ids[1], ids[0]]
        );
    }

    #[test]
    fn unread_and_asking_rows_stay_above_working_ones() {
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - HOUR),
            unread(NOW - 3 * HOUR),
            worked(NOW - 5 * HOUR),
        ]);
        change(
            &mut workspace,
            ids[0],
            AgentActivityState::Working,
            NOW - 30 * MINUTE,
        );
        // Asked 4 h ago and seen, but still waiting for an answer.
        change(
            &mut workspace,
            ids[2],
            AgentActivityState::Blocked,
            NOW - 4 * HOUR,
        );
        workspace
            .instance_mut(ids[2])
            .unwrap()
            .activity
            .attend(NOW - 4 * HOUR + 1);
        assert_eq!(
            pane_order(&project, &workspace),
            vec![ids[1], ids[2], ids[0]]
        );
    }

    #[test]
    fn an_unseen_finish_keeps_its_row_up_when_the_agent_starts_again() {
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - 30 * MINUTE),
            worked(NOW - 2 * HOUR),
            unread(NOW - 3 * HOUR),
        ]);
        change(
            &mut workspace,
            ids[1],
            AgentActivityState::Working,
            NOW - 90 * MINUTE,
        );
        // The agent starts its next turn by itself; nobody has seen the last.
        change(
            &mut workspace,
            ids[2],
            AgentActivityState::Working,
            NOW - HOUR,
        );
        assert!(is_unread(&workspace.instance(ids[2]).unwrap().activity));
        assert_eq!(
            pane_order(&project, &workspace),
            vec![ids[2], ids[1], ids[0]]
        );

        // Clicking it reads it, but it was already working: it stays put.
        let hold = focus(&mut workspace, ids[2], NOW - 5);
        assert_eq!(
            panes(order_with(
                &project,
                &workspace,
                Some(&hold),
                SidebarSort::RecentWork
            )),
            vec![ids[2], ids[1], ids[0]]
        );
        // Once focus leaves, it joins the working rows by when it started.
        assert_eq!(
            pane_order(&project, &workspace),
            vec![ids[1], ids[2], ids[0]]
        );
    }

    #[test]
    fn the_focused_row_keeps_its_place_until_focus_leaves_unless_it_goes_working() {
        let (project, mut workspace, ids) = fixture(vec![
            unread(NOW - 3 * HOUR),
            worked(NOW - 2 * HOUR),
            worked(NOW - 10 * MINUTE),
        ]);
        let (u, w, d) = (ids[0], ids[1], ids[2]);
        change(&mut workspace, w, AgentActivityState::Working, NOW - HOUR);
        assert_eq!(pane_order(&project, &workspace), vec![u, w, d]);
        let held = |workspace: &Workspace, hold: &FocusHold| {
            panes(order_with(
                &project,
                workspace,
                Some(hold),
                SidebarSort::RecentWork,
            ))
        };

        // Reading it doesn't drop it out from under the click.
        let hold = focus(&mut workspace, u, NOW - 6);
        assert!(!is_unread(&workspace.instance(u).unwrap().activity));
        assert_eq!(held(&workspace, &hold), vec![u, w, d]);
        // Focus leaves: it is a read finish from 3 h ago.
        assert_eq!(pane_order(&project, &workspace), vec![w, d, u]);

        // Focus it again and start a turn: it goes under the working rows.
        let hold = focus(&mut workspace, u, NOW - 5);
        assert_eq!(held(&workspace, &hold), vec![w, d, u]);
        change(&mut workspace, u, AgentActivityState::Working, NOW - 4);
        assert_eq!(held(&workspace, &hold), vec![w, u, d]);
        change(&mut workspace, d, AgentActivityState::Working, NOW - 3);
        assert_eq!(held(&workspace, &hold), vec![w, u, d]);
        // It finishes while focused, so it is read at once. It stays with the
        // working rows until focus leaves, then leads the read finishes.
        change(&mut workspace, u, AgentActivityState::Done, NOW - 2);
        workspace.instance_mut(u).unwrap().activity.attend(NOW - 2);
        assert_eq!(held(&workspace, &hold), vec![w, u, d]);
        assert_eq!(pane_order(&project, &workspace), vec![w, d, u]);
    }

    #[test]
    fn a_working_row_is_today_however_old_its_last_finish() {
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - 1_000),
            worked(NOW - 40 * DAY),
            worked(NOW - 40 * DAY - 1),
        ]);
        change(
            &mut workspace,
            ids[1],
            AgentActivityState::Working,
            NOW - 500,
        );
        assert_eq!(
            band(&workspace.instance(ids[1]).unwrap().activity),
            Some(ActivityBand::Today)
        );
        use SidebarEntry::{Band, Pane};
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Band(ActivityBand::Today),
                Pane(ids[1]),
                Pane(ids[0]),
                Band(ActivityBand::Older),
                Pane(ids[2]),
            ]
        );

        // Saved as working, but with no live terminal it shows idle, and it
        // sorts as what it shows: by its last finish.
        let index = instances_by_id(&workspace);
        let shown = |id: Uuid| {
            (id != ids[1])
                .then(|| index[&id].activity.current_state())
                .flatten()
        };
        assert_eq!(
            sidebar_order(
                &project.instances(),
                &index,
                &shown,
                None,
                NOW,
                SidebarSort::RecentWork
            ),
            vec![
                Band(ActivityBand::Today),
                Pane(ids[0]),
                Band(ActivityBand::Older),
                Pane(ids[1]),
                Pane(ids[2]),
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
        // Its first turn was interrupted: never finished, not working now.
        change(
            &mut workspace,
            ids[2],
            AgentActivityState::Working,
            NOW - 1_000,
        );
        change(&mut workspace, ids[2], AgentActivityState::Idle, NOW - 900);
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
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                SidebarEntry::Pane(ids[2]),
                SidebarEntry::Pane(ids[0]),
                SidebarEntry::Band(ActivityBand::Older),
                SidebarEntry::Pane(ids[1]),
            ]
        );
        assert_eq!(unread_count(&project.instances(), &index), 1);
        assert_eq!(band(&index[&ids[1]].activity), Some(ActivityBand::Older));
    }

    #[test]
    fn last_work_and_bands_use_all_lifecycle_events_never_attendance() {
        let mut activity = worked(NOW - 35 * DAY);
        activity.blocked_at = Some(NOW - DAY);
        activity.last_attended_at = Some(NOW);
        assert_eq!(activity.last_work_at(), activity.blocked_at);
        for (age, expected) in [
            (DAY - 1, ActivityBand::Today),
            (DAY, ActivityBand::ThisWeek),
            (7 * DAY - 1, ActivityBand::ThisWeek),
            (7 * DAY, ActivityBand::Older),
            (-DAY, ActivityBand::Today),
        ] {
            assert_eq!(band(&worked(NOW - age)), Some(expected));
        }
        assert_eq!(
            band(&AgentActivity {
                last_attended_at: Some(NOW),
                ..Default::default()
            }),
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
            order_with(&project, &workspace, None, SidebarSort::RecentWork),
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
            order_with(&project, &workspace, None, SidebarSort::Layout),
            vec![
                SidebarEntry::Pane(ids[0]),
                SidebarEntry::Pane(ids[2]),
                SidebarEntry::Worktree(wid),
                SidebarEntry::Pane(ids[1]),
            ]
        );
        assert_eq!(
            order_with(&project, &workspace, None, SidebarSort::RecentWork),
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
            order_with(&project, &workspace, None, SidebarSort::RecentWork),
            ids.into_iter().map(SidebarEntry::Pane).collect::<Vec<_>>()
        );
    }
}
