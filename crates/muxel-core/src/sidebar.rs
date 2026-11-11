//! Sidebar ordering and unread decisions over persisted activity, without UI or I/O.
use std::cmp::{Ordering, Reverse};
use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Days, NaiveDate, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AgentActivity, AgentActivityState, Instance, InstanceKind, Project, Workspace};

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

/// The clock Recent work's bands are read against: now, local midnight today,
/// and local midnight six days before that, all in Unix milliseconds. Today is
/// since midnight, This week the six days before it, Older the rest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BandClock {
    now: i64,
    today: i64,
    week: i64,
}

impl BandClock {
    /// The bands as of `now`, on the calendar of `now`'s time zone.
    pub fn at<Tz: TimeZone>(now: &DateTime<Tz>) -> Self {
        let today = now.date_naive();
        let midnight = |date: NaiveDate| {
            let zone = now.timezone();
            // A day whose clocks jump over midnight starts at its first hour.
            zone.from_local_datetime(&date.and_time(NaiveTime::MIN))
                .earliest()
                .or_else(|| {
                    zone.from_local_datetime(&date.and_hms_opt(1, 0, 0)?)
                        .earliest()
                })
                .map_or(i64::MIN, |start| start.timestamp_millis())
        };
        let week = today.checked_sub_days(Days::new(6)).unwrap_or(today);
        Self {
            now: now.timestamp_millis(),
            today: midnight(today),
            week: midnight(week),
        }
    }

    pub fn now(self) -> i64 {
        self.now
    }

    /// Local midnight today: the bands change when this does.
    pub fn today(self) -> i64 {
        self.today
    }

    /// The band of something that happened at `at`. Future times count as now.
    pub fn band(self, at: i64) -> ActivityBand {
        let at = at.min(self.now);
        if at >= self.today {
            ActivityBand::Today
        } else if at >= self.week {
            ActivityBand::ThisWeek
        } else {
            ActivityBand::Older
        }
    }
}

/// The groups of rows within each band of Recent work, top to bottom. Whether
/// a row has been read never decides its group or its place in the group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecentGroup {
    /// Asking for something now: newest question first.
    Asking,
    /// Has worked and isn't running a turn: finished, stopped, exited or
    /// interrupted. Newest event first.
    Recent,
    /// Running a turn: oldest start first, so a new turn joins the bottom.
    Working,
    /// Has never worked: newest open first.
    Unused,
}

/// Where a row sits in Recent work: its band, its group within the band, then
/// its time within the group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecentPlace {
    band: ActivityBand,
    group: RecentGroup,
    at: Option<i64>,
}

impl RecentPlace {
    /// `shown` is the state the row shows now. Only a change of work moves a
    /// row: a start, a finish, a question or an answer. The band comes from
    /// the latest of those, so a row that stops working stays in the band its
    /// turn started in. A working row is Today, however long its turn has run.
    /// A row that has never worked goes by when it was last opened (a file a
    /// session opened again counts from then), or with the Older rows when
    /// that isn't known.
    pub fn of(instance: &Instance, shown: Option<AgentActivityState>, clock: BandClock) -> Self {
        let activity = &instance.activity;
        let latest = activity.last_work_at();
        let opened = instance.created_at.max(instance.opened_at);
        let (group, at) = match shown {
            // A turn or question the reducer hasn't recorded yet began now.
            Some(AgentActivityState::Working) => (
                RecentGroup::Working,
                activity.work_started_at.unwrap_or(clock.now),
            ),
            Some(AgentActivityState::Blocked) => (
                RecentGroup::Asking,
                activity.blocked_at.or(latest).unwrap_or(clock.now),
            ),
            _ => match latest {
                Some(at) => (RecentGroup::Recent, at),
                None => {
                    return Self {
                        band: opened.map_or(ActivityBand::Older, |at| clock.band(at)),
                        group: RecentGroup::Unused,
                        at: opened.map(|at| at.min(clock.now)),
                    };
                }
            },
        };
        let band = if group == RecentGroup::Working {
            ActivityBand::Today
        } else {
            clock.band(latest.unwrap_or(at).max(at))
        };
        Self {
            band,
            group,
            at: Some(at.min(clock.now)),
        }
    }

    pub fn band(self) -> ActivityBand {
        self.band
    }

    pub fn group(self) -> RecentGroup {
        self.group
    }
}

impl Ord for RecentPlace {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.band, self.group)
            .cmp(&(other.band, other.group))
            .then_with(|| match self.group {
                RecentGroup::Working => self.at.cmp(&other.at),
                RecentGroup::Asking | RecentGroup::Recent | RecentGroup::Unused => {
                    other.at.cmp(&self.at)
                }
            })
    }
}

impl PartialOrd for RecentPlace {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
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

/// Whether the sidebar's feed lists a notification. Recent work leaves out a
/// pane's event its row already shows (`row_shows_it`: a finish or a
/// question). A presentation filter only; callers retain the events for
/// desktop/tray use.
pub fn notification_in_feed(sort: SidebarSort, row_shows_it: bool) -> bool {
    sort == SidebarSort::Layout || !row_shows_it
}

/// The rows `project` lists in the sidebar, in the order [`sidebar_order`]
/// breaks ties by. Layout lists the layout's panes in layout order. Recent
/// work adds the project's live popped-out panes (`popped`), which have left
/// the layout, and lists every row in the order its pane was opened (the
/// workspace's order), so popping a row out or docking it back never changes
/// how it ties with its neighbours.
pub fn sidebar_rows(
    project: &Project,
    workspace: &Workspace,
    popped: &dyn Fn(Uuid) -> bool,
    sort: SidebarSort,
) -> Vec<Uuid> {
    sidebar_rows_each([project], workspace, popped, sort)
        .remove(&project.id)
        .unwrap_or_default()
}

/// Each of `projects`' rows ([`sidebar_rows`]), by project id, from one pass
/// over the workspace's instances however many projects there are.
pub fn sidebar_rows_each<'a>(
    projects: impl IntoIterator<Item = &'a Project>,
    workspace: &Workspace,
    popped: &dyn Fn(Uuid) -> bool,
    sort: SidebarSort,
) -> HashMap<Uuid, Vec<Uuid>> {
    let docked: Vec<(Uuid, Vec<Uuid>)> = projects
        .into_iter()
        .map(|project| (project.id, project.instances()))
        .collect();
    if sort == SidebarSort::Layout {
        return docked.into_iter().collect();
    }
    // Each docked pane's projects that haven't listed it yet.
    let mut unlisted: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    let mut rows: HashMap<Uuid, Vec<Uuid>> = HashMap::with_capacity(docked.len());
    for (pid, ids) in &docked {
        rows.insert(*pid, Vec::with_capacity(ids.len()));
        for id in ids {
            let holders = unlisted.entry(*id).or_default();
            if !holders.contains(pid) {
                holders.push(*pid);
            }
        }
    }
    for instance in &workspace.instances {
        let holders = unlisted.remove(&instance.id).unwrap_or_default();
        for pid in &holders {
            if let Some(listed) = rows.get_mut(pid) {
                listed.push(instance.id);
            }
        }
        if !holders.contains(&instance.project_id)
            && let Some(own) = rows.get_mut(&instance.project_id)
            && popped(instance.id)
        {
            own.push(instance.id);
        }
    }
    // A pane the layout holds without an instance still gets its row.
    for (pid, ids) in &docked {
        let Some(listed) = rows.get_mut(pid) else {
            continue;
        };
        let mut seen = HashSet::new();
        listed.extend(ids.iter().copied().filter(|id| {
            unlisted
                .get(id)
                .is_some_and(|holders| holders.contains(pid))
                && seen.insert(*id)
        }));
    }
    rows
}

/// The caller supplies this project's rows ([`sidebar_rows`]). Layout
/// preserves existing worktree groups. Recent work is flat, so worktree
/// membership cannot hold older sessions above newer work, except that a file,
/// diff or page follows the session that opened it ([`nests_under`]), newest
/// open first.
/// Both sorts are stable; only recent work pulls pins into a leading block.
/// Recent work places every row, pinned or not, by [`RecentPlace`], from the
/// state each row shows (`shown`). Nothing about reading or focus is an input.
pub fn sidebar_order(
    pane_ids: &[Uuid],
    instances: &InstancesById,
    shown: &dyn Fn(Uuid) -> Option<AgentActivityState>,
    clock: BandClock,
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

    // A row with no instance has no work and no open time.
    let unknown = RecentPlace {
        band: ActivityBand::Older,
        group: RecentGroup::Unused,
        at: None,
    };
    let placed = |ids: Vec<Uuid>| {
        let mut rows: Vec<_> = ids
            .into_iter()
            .map(|id| {
                let place = instances
                    .get(&id)
                    .map_or(unknown, |i| RecentPlace::of(i, shown(id), clock));
                (id, place)
            })
            .collect();
        rows.sort_by_key(|&(_, place)| place);
        rows
    };
    let (pinned, rest): (Vec<_>, Vec<_>) = top
        .into_iter()
        .partition(|id| instances.get(id).is_some_and(|i| i.pinned));
    let mut out = Vec::new();
    for (id, _) in placed(pinned) {
        row(&mut out, id);
    }
    // Pinned rows have a pin glyph instead of an age heading; their ages must
    // not cause Older → Today headings to repeat across the pinned boundary.
    let rest = placed(rest);
    let label_bands = rest
        .first()
        .is_some_and(|(_, first)| rest.iter().any(|(_, place)| place.band != first.band));
    let mut current = None;
    for (id, place) in rest {
        if label_bands && current != Some(place.band) {
            out.push(SidebarEntry::Band(place.band));
        }
        current = Some(place.band);
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
    use chrono::{FixedOffset, Utc};

    const MINUTE: i64 = 60_000;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 86_400_000;
    /// Noon UTC, so the tests' Today runs from twelve hours ago.
    const NOW: i64 = 400 * DAY + 12 * HOUR;

    fn clock() -> BandClock {
        BandClock::at(&Utc.timestamp_millis_opt(NOW).unwrap())
    }

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

    fn attend(workspace: &mut Workspace, id: Uuid, at: i64) {
        workspace.instance_mut(id).unwrap().activity.attend(at);
    }

    /// Recent work as each row's `shown` state and `clock` give it, over the
    /// rows the sidebar lists for `project`.
    fn order_at(
        project: &Project,
        workspace: &Workspace,
        shown: &dyn Fn(&Instance) -> Option<AgentActivityState>,
        clock: BandClock,
    ) -> Vec<SidebarEntry> {
        let rows = sidebar_rows(project, workspace, &|_| false, SidebarSort::RecentWork);
        order_rows(&rows, workspace, shown, clock)
    }

    /// Recent work over `rows`.
    fn order_rows(
        rows: &[Uuid],
        workspace: &Workspace,
        shown: &dyn Fn(&Instance) -> Option<AgentActivityState>,
        clock: BandClock,
    ) -> Vec<SidebarEntry> {
        let index = instances_by_id(workspace);
        let shown = |id: Uuid| index.get(&id).and_then(|i| shown(i));
        sidebar_order(rows, &index, &shown, clock, SidebarSort::RecentWork)
    }

    /// Every row shows the state it was saved in, as when each has a live terminal.
    fn live(instance: &Instance) -> Option<AgentActivityState> {
        instance.activity.current_state()
    }

    /// What a row with no live terminal shows: Done once it has finished a
    /// turn, else idle.
    fn no_terminal(instance: &Instance) -> Option<AgentActivityState> {
        Some(match instance.activity.current_state() {
            Some(AgentActivityState::Done) => AgentActivityState::Done,
            _ => AgentActivityState::Idle,
        })
    }

    fn recent(project: &Project, workspace: &Workspace) -> Vec<SidebarEntry> {
        order_at(project, workspace, &live, clock())
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

    fn pane_order(project: &Project, workspace: &Workspace) -> Vec<Uuid> {
        panes(recent(project, workspace))
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

    /// Every activity up to three lifecycle steps can leave, starting at ages
    /// that span every band, plus never-worked rows with and without an open
    /// time.
    fn population() -> Vec<(AgentActivity, Option<i64>)> {
        use AgentActivityState::{Blocked, Done, Idle, Working};
        let mut sequences: Vec<Vec<AgentActivityState>> = vec![Vec::new()];
        let mut frontier = sequences.clone();
        for _ in 0..3 {
            frontier = frontier
                .iter()
                .flat_map(|seq| {
                    [Working, Done, Blocked, Idle].map(|state| {
                        let mut next = seq.clone();
                        next.push(state);
                        next
                    })
                })
                .collect();
            sequences.extend(frontier.iter().cloned());
        }
        let mut rows = Vec::new();
        for base in [
            NOW - 10 * MINUTE,
            NOW - 5 * HOUR,
            NOW - 13 * HOUR,
            NOW - 3 * DAY,
            NOW - 9 * DAY,
        ] {
            for sequence in &sequences {
                let mut activity = AgentActivity::default();
                let mut previous = None;
                for (step, &state) in sequence.iter().enumerate() {
                    activity.observe(previous, state, base + step as i64 * 2 * MINUTE);
                    previous = Some(state);
                }
                rows.push((activity, Some(base - MINUTE)));
            }
        }
        rows.push((AgentActivity::default(), None));
        rows
    }

    /// A file in `project`'s single tab group, opened an hour ago, last by
    /// `session` at `at` (or by no session).
    fn file(
        workspace: &mut Workspace,
        project: &mut Project,
        session: Option<Uuid>,
        at: i64,
    ) -> Uuid {
        let mut instance = Instance::editor(project.id, None);
        instance.created_at = Some(NOW - HOUR);
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
            sidebar_order(
                &project.instances(),
                &instances_by_id(&workspace),
                &|_| None,
                clock(),
                SidebarSort::Layout
            ),
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
    fn an_ordinary_file_row_goes_by_the_last_time_it_was_opened() {
        use SidebarEntry::{Band, Pane};
        let (mut project, mut workspace, ids) = fixture(vec![worked(NOW - 1_000)]);
        let gone = Instance::shell(project.id);
        let session = gone.id;
        workspace.add_instance(gone);
        let plan = file(&mut workspace, &mut project, None, 0);
        workspace.instance_mut(plan).unwrap().created_at = Some(NOW - 3 * DAY);
        // Opened nine days ago, and again by a session that has since closed.
        let notes = file(
            &mut workspace,
            &mut project,
            Some(session),
            NOW - 10 * MINUTE,
        );
        workspace.instance_mut(notes).unwrap().created_at = Some(NOW - 9 * DAY);
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Band(ActivityBand::Today),
                Pane(ids[0]),
                Pane(notes),
                Band(ActivityBand::ThisWeek),
                Pane(plan)
            ]
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
    fn reading_never_changes_the_order() {
        let rows = population();
        let (project, mut workspace, ids) =
            fixture(rows.iter().map(|(activity, _)| activity.clone()).collect());
        for (&id, (_, opened)) in ids.iter().zip(&rows) {
            workspace.instance_mut(id).unwrap().created_at = *opened;
        }
        // Each row can be unseen, seen up to just before any of its events, or
        // seen now.
        let choices: Vec<Vec<Option<i64>>> = rows
            .iter()
            .map(|(activity, _)| {
                let mut seen = vec![None, Some(NOW)];
                seen.extend(
                    [
                        activity.work_started_at,
                        activity.completed_at,
                        activity.blocked_at,
                    ]
                    .into_iter()
                    .flatten()
                    .map(|at| Some(at - 1)),
                );
                seen
            })
            .collect();
        for shown in [
            &live as &dyn Fn(&Instance) -> Option<AgentActivityState>,
            &no_terminal,
        ] {
            let read = |workspace: &mut Workspace, pick: &dyn Fn(usize, usize) -> usize| {
                for (row, &id) in ids.iter().enumerate() {
                    let seen = choices[row][pick(row, choices[row].len())];
                    workspace
                        .instance_mut(id)
                        .unwrap()
                        .activity
                        .last_attended_at = seen;
                }
                order_at(&project, workspace, shown, clock())
            };
            let unseen = read(&mut workspace, &|_, _| 0);
            for round in 0..16 {
                let order = read(&mut workspace, &|row, len| (row * 7 + round * 13) % len);
                assert_eq!(order, unseen, "attendance round {round}");
            }
            assert_eq!(read(&mut workspace, &|_, _| 1), unseen, "all read now");
        }
    }

    #[test]
    fn starting_a_turn_joins_the_bottom_of_the_working_rows() {
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - 30 * MINUTE),
            unread(NOW - 3 * HOUR),
            worked(NOW - 5 * HOUR),
            worked(NOW - 4 * HOUR),
        ]);
        let (read, unseen, first, second) = (ids[0], ids[1], ids[2], ids[3]);
        change(
            &mut workspace,
            first,
            AgentActivityState::Working,
            NOW - 2 * HOUR,
        );
        change(
            &mut workspace,
            second,
            AgentActivityState::Working,
            NOW - HOUR,
        );
        assert_eq!(
            pane_order(&project, &workspace),
            vec![read, unseen, first, second]
        );
        // An unseen finish doesn't hold it up: it keeps only its dot.
        change(&mut workspace, unseen, AgentActivityState::Working, NOW);
        assert_eq!(
            pane_order(&project, &workspace),
            vec![read, first, second, unseen]
        );
        assert!(is_unread(&workspace.instance(unseen).unwrap().activity));
    }

    #[test]
    fn finishing_moves_a_row_to_the_top_of_recent_watched_or_not() {
        for watched in [false, true] {
            let (project, mut workspace, ids) = fixture(vec![
                worked(NOW - 20 * MINUTE),
                unread(NOW - 40 * MINUTE),
                worked(NOW - 5 * HOUR),
                worked(NOW - 4 * HOUR),
            ]);
            change(
                &mut workspace,
                ids[2],
                AgentActivityState::Working,
                NOW - 2 * HOUR,
            );
            change(
                &mut workspace,
                ids[3],
                AgentActivityState::Working,
                NOW - HOUR,
            );
            change(
                &mut workspace,
                ids[3],
                AgentActivityState::Done,
                NOW - MINUTE,
            );
            if watched {
                attend(&mut workspace, ids[3], NOW - MINUTE);
            }
            assert_eq!(
                pane_order(&project, &workspace),
                vec![ids[3], ids[0], ids[1], ids[2]],
                "watched: {watched}"
            );
        }
    }

    #[test]
    fn a_question_leads_its_band_above_newer_finishes() {
        let (project, mut workspace, ids) = fixture(vec![
            unread(NOW - MINUTE),
            worked(NOW - 3 * HOUR),
            worked(NOW - 5 * HOUR),
        ]);
        let (finished, asking, working) = (ids[0], ids[1], ids[2]);
        change(
            &mut workspace,
            asking,
            AgentActivityState::Working,
            NOW - HOUR,
        );
        change(
            &mut workspace,
            asking,
            AgentActivityState::Blocked,
            NOW - 10 * MINUTE,
        );
        change(
            &mut workspace,
            working,
            AgentActivityState::Working,
            NOW - 2 * HOUR,
        );
        assert_eq!(
            pane_order(&project, &workspace),
            vec![asking, finished, working]
        );
        // Seen but not yet answered, it is still asking.
        attend(&mut workspace, asking, NOW);
        assert_eq!(
            pane_order(&project, &workspace),
            vec![asking, finished, working]
        );
    }

    #[test]
    fn an_answered_question_joins_the_working_rows() {
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - 3 * HOUR),
            worked(NOW - 5 * MINUTE),
            worked(NOW - 5 * HOUR),
        ]);
        let (asking, read, working) = (ids[0], ids[1], ids[2]);
        change(
            &mut workspace,
            asking,
            AgentActivityState::Working,
            NOW - HOUR,
        );
        change(
            &mut workspace,
            asking,
            AgentActivityState::Blocked,
            NOW - 30 * MINUTE,
        );
        change(
            &mut workspace,
            working,
            AgentActivityState::Working,
            NOW - 2 * HOUR,
        );
        assert_eq!(
            pane_order(&project, &workspace),
            vec![asking, read, working]
        );
        // Answered through `muxel ctl send`, without anyone looking: the
        // question stays unread, and only the dot says so.
        change(
            &mut workspace,
            asking,
            AgentActivityState::Working,
            NOW - MINUTE,
        );
        assert_eq!(
            pane_order(&project, &workspace),
            vec![read, working, asking]
        );
        assert!(is_unread(&workspace.instance(asking).unwrap().activity));
    }

    #[test]
    fn a_working_idle_flicker_keeps_the_row_in_the_band_of_its_turn() {
        let (project, mut workspace, ids) =
            fixture(vec![worked(NOW - 3 * DAY), worked(NOW - 3 * HOUR)]);
        let (flicker, read) = (ids[0], ids[1]);
        change(
            &mut workspace,
            flicker,
            AgentActivityState::Working,
            NOW - 10 * MINUTE,
        );
        assert_eq!(pane_order(&project, &workspace), vec![read, flicker]);
        // Idle for a moment: still today's work, the newest event in the band.
        change(
            &mut workspace,
            flicker,
            AgentActivityState::Idle,
            NOW - 9 * MINUTE,
        );
        assert_eq!(
            recent(&project, &workspace),
            vec![SidebarEntry::Pane(flicker), SidebarEntry::Pane(read)]
        );
        change(
            &mut workspace,
            flicker,
            AgentActivityState::Working,
            NOW - 8 * MINUTE,
        );
        assert_eq!(pane_order(&project, &workspace), vec![read, flicker]);
    }

    #[test]
    fn an_interrupted_turn_sorts_by_its_start_not_an_older_finish() {
        // A shell that rang the bell five days ago, then ran a command today.
        let mut shell = worked(NOW - 5 * DAY);
        shell.observe(
            Some(AgentActivityState::Done),
            AgentActivityState::Working,
            NOW - 2 * HOUR,
        );
        shell.observe(
            Some(AgentActivityState::Working),
            AgentActivityState::Idle,
            NOW - 2 * HOUR + MINUTE,
        );
        let (project, workspace, ids) = fixture(vec![worked(NOW - 3 * HOUR), shell]);
        assert_eq!(
            recent(&project, &workspace),
            vec![SidebarEntry::Pane(ids[1]), SidebarEntry::Pane(ids[0])]
        );
    }

    #[test]
    fn a_restored_terminal_reporting_its_saved_state_does_not_move_its_row() {
        let (project, mut workspace, ids) =
            fixture(vec![worked(NOW - HOUR), worked(NOW - 3 * HOUR)]);
        let before = recent(&project, &workspace);
        // The first live sample after a restore or a tmux reattach.
        let activity = &mut workspace.instance_mut(ids[1]).unwrap().activity;
        activity.observe(None, AgentActivityState::Done, NOW);
        assert_eq!(recent(&project, &workspace), before);
    }

    #[test]
    fn a_session_without_a_live_terminal_sorts_by_its_latest_event() {
        // Saved mid-turn two hours ago; its last finish was three days ago.
        let (project, mut workspace, ids) =
            fixture(vec![worked(NOW - 3 * HOUR), worked(NOW - 3 * DAY)]);
        let (read, saved) = (ids[0], ids[1]);
        change(
            &mut workspace,
            saved,
            AgentActivityState::Working,
            NOW - 2 * HOUR,
        );
        assert_eq!(
            order_at(&project, &workspace, &no_terminal, clock()),
            vec![SidebarEntry::Pane(saved), SidebarEntry::Pane(read)]
        );
        // Its terminal comes up still working.
        assert_eq!(pane_order(&project, &workspace), vec![read, saved]);
    }

    #[test]
    fn bands_follow_the_local_calendar() {
        let zone = FixedOffset::west_opt(7 * 3600).unwrap();
        let local = |day: u32, hour: u32, minute: u32| {
            zone.with_ymd_and_hms(2026, 11, day, hour, minute, 0)
                .unwrap()
        };
        let at = |day, hour, minute| local(day, hour, minute).timestamp_millis();
        let (project, workspace, ids) = fixture(vec![
            worked(at(7, 23, 50)),
            worked(at(1, 0, 0)),
            worked(at(1, 0, 0) - 1),
        ]);
        use ActivityBand::{Older, ThisWeek, Today};
        use SidebarEntry::{Band, Pane};
        let before_midnight = BandClock::at(&local(7, 23, 59));
        assert_eq!(
            order_at(&project, &workspace, &live, before_midnight),
            vec![
                Band(Today),
                Pane(ids[0]),
                Band(ThisWeek),
                Pane(ids[1]),
                Band(Older),
                Pane(ids[2]),
            ]
        );
        // Two minutes later it is the 8th: the 23:50 finish is yesterday's, and
        // the 1st has left the six days before today.
        let after_midnight = BandClock::at(&local(8, 0, 1));
        assert_eq!(
            order_at(&project, &workspace, &live, after_midnight),
            vec![
                Band(ThisWeek),
                Pane(ids[0]),
                Band(Older),
                Pane(ids[1]),
                Pane(ids[2]),
            ]
        );
    }

    #[test]
    fn pinned_rows_follow_the_same_rule() {
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - 5 * HOUR),
            worked(NOW - 30 * MINUTE),
            worked(NOW - 4 * HOUR),
            worked(NOW - 10 * MINUTE),
        ]);
        let (working, read, asking, unpinned) = (ids[0], ids[1], ids[2], ids[3]);
        for id in [working, read, asking] {
            workspace.instance_mut(id).unwrap().pinned = true;
        }
        change(
            &mut workspace,
            working,
            AgentActivityState::Working,
            NOW - HOUR,
        );
        change(
            &mut workspace,
            asking,
            AgentActivityState::Working,
            NOW - 3 * HOUR,
        );
        change(
            &mut workspace,
            asking,
            AgentActivityState::Blocked,
            NOW - 2 * HOUR,
        );
        assert_eq!(
            pane_order(&project, &workspace),
            vec![asking, read, working, unpinned]
        );
    }

    #[test]
    fn a_never_used_row_sorts_by_when_it_was_opened() {
        let (project, mut workspace, ids) = fixture(vec![
            worked(NOW - 3 * HOUR),
            AgentActivity::default(),
            AgentActivity::default(),
            AgentActivity::default(),
            AgentActivity::default(),
        ]);
        for (row, opened) in [
            (1, Some(NOW - HOUR)),
            (2, Some(NOW - 2 * DAY)),
            (3, None),
            (4, Some(NOW - 20 * MINUTE)),
        ] {
            workspace.instance_mut(ids[row]).unwrap().created_at = opened;
        }
        use ActivityBand::{Older, ThisWeek, Today};
        use SidebarEntry::{Band, Pane};
        // Opened before muxel recorded open times: with the Older rows.
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Band(Today),
                Pane(ids[0]),
                Pane(ids[4]),
                Pane(ids[1]),
                Band(ThisWeek),
                Pane(ids[2]),
                Band(Older),
                Pane(ids[3]),
            ]
        );
    }

    #[test]
    fn working_rows_sit_together_in_start_order_under_the_finishes() {
        // Chris's screenshot: three working sessions, two read finishes from
        // 11 h ago, and a session that last finished 13 h ago, yesterday, and
        // has been working for 2 h.
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
        assert_eq!(
            recent(&project, &workspace),
            [ids[3], ids[4], ids[2], ids[1], ids[5], ids[0]]
                .map(SidebarEntry::Pane)
                .to_vec()
        );
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
        use SidebarEntry::{Band, Pane};
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Band(ActivityBand::Today),
                Pane(ids[0]),
                Pane(ids[1]),
                Band(ActivityBand::Older),
                Pane(ids[2]),
            ]
        );
    }

    #[test]
    fn bands_go_by_the_latest_event_of_any_kind_never_attendance() {
        let mut activity = worked(NOW - 35 * DAY);
        activity.blocked_at = Some(NOW - 2 * DAY);
        activity.last_attended_at = Some(NOW);
        let mut instance = Instance::shell(Uuid::new_v4());
        instance.activity = activity;
        let place = |instance: &Instance| {
            RecentPlace::of(instance, Some(AgentActivityState::Idle), clock())
        };
        assert_eq!(place(&instance).band(), ActivityBand::ThisWeek);
        instance.activity.work_started_at = Some(NOW - HOUR);
        assert_eq!(place(&instance).band(), ActivityBand::Today);
        assert_eq!(place(&instance).group(), RecentGroup::Recent);
        for (at, expected) in [
            (NOW - 12 * HOUR, ActivityBand::Today),
            (NOW - 12 * HOUR - 1, ActivityBand::ThisWeek),
            (NOW - 12 * HOUR - 6 * DAY, ActivityBand::ThisWeek),
            (NOW - 12 * HOUR - 6 * DAY - 1, ActivityBand::Older),
            (NOW + DAY, ActivityBand::Today),
        ] {
            assert_eq!(clock().band(at), expected, "{at}");
        }
    }

    #[test]
    fn layout_keeps_worktree_groups_and_recent_keeps_never_worked_last() {
        let (project, mut workspace, ids) =
            fixture(vec![AgentActivity::default(), worked(NOW - 1), worked(NOW)]);
        let wid = Uuid::new_v4();
        workspace.instance_mut(ids[1]).unwrap().worktree_id = Some(wid);
        let index = instances_by_id(&workspace);
        let shown = |id: Uuid| index.get(&id).and_then(|i| live(i));
        assert_eq!(
            sidebar_order(
                &project.instances(),
                &index,
                &shown,
                clock(),
                SidebarSort::Layout
            ),
            vec![
                SidebarEntry::Pane(ids[0]),
                SidebarEntry::Pane(ids[2]),
                SidebarEntry::Worktree(wid),
                SidebarEntry::Pane(ids[1]),
            ]
        );
        workspace.instance_mut(ids[0]).unwrap().created_at = Some(NOW);
        assert_eq!(
            pane_order(&project, &workspace),
            vec![ids[2], ids[1], ids[0]]
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
    fn recent_work_folds_what_rows_show_only() {
        assert!(!notification_in_feed(SidebarSort::RecentWork, true));
        assert!(notification_in_feed(SidebarSort::RecentWork, false));
        assert!(notification_in_feed(SidebarSort::Layout, true));
        assert!(notification_in_feed(SidebarSort::Layout, false));
    }

    #[test]
    fn duplicate_has_no_work_unread_or_open_time_of_its_own() {
        let mut instance = Instance::shell(Uuid::new_v4());
        instance.activity = worked(NOW);
        instance.activity.last_attended_at = None;
        instance.created_at = Some(NOW - DAY);
        instance.reset_conversation_for_duplicate();
        assert_eq!(instance.activity, AgentActivity::default());
        assert_eq!(instance.created_at, None);
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
    fn a_known_pane_keeps_its_open_time_when_a_peer_leaves_it_out() {
        let mut mine = Instance::shell(Uuid::new_v4());
        mine.created_at = Some(NOW - DAY);
        let mut peer = mine.clone();
        peer.created_at = None;
        let merged = crate::merge_peer_instances(&[mine], vec![peer], Uuid::new_v4());
        assert_eq!(merged[0].created_at, Some(NOW - DAY));
    }

    #[test]
    fn an_open_time_is_not_a_layout_change() {
        let (project, mut workspace, ids) = fixture(vec![worked(NOW)]);
        let before = crate::RemoteLayout::capture(&project, &workspace, 1);
        workspace.instance_mut(ids[0]).unwrap().created_at = Some(NOW);
        let after = crate::RemoteLayout::capture(&project, &workspace, 1);
        assert!(after.to_json().contains("created_at"));
        assert_eq!(after.content_key(), before.content_key());
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
    fn future_work_clamps_to_now_and_ties_keep_opening_order() {
        let (project, workspace, ids) =
            fixture(vec![worked(NOW), worked(NOW + DAY), worked(NOW - 1)]);
        assert_eq!(pane_order(&project, &workspace), ids);
    }

    #[test]
    fn popping_a_tied_row_out_or_docking_it_back_never_moves_it() {
        // Panes saved before muxel recorded open times all tie.
        let (mut project, workspace, ids) = fixture(vec![AgentActivity::default(); 3]);
        let first = ids[0];
        let recent_with = |project: &Project, popped: Option<Uuid>| {
            let rows = sidebar_rows(
                project,
                &workspace,
                &|id| Some(id) == popped,
                SidebarSort::RecentWork,
            );
            panes(order_rows(&rows, &workspace, &live, clock()))
        };
        assert_eq!(recent_with(&project, None), ids);
        // Popped out, the first pane leaves the layout but keeps its place.
        let Some(PaneNode::Leaf(leaf)) = &mut project.layout else {
            unreachable!()
        };
        leaf.tabs.retain(|&id| id != first);
        leaf.active = 0;
        assert_eq!(recent_with(&project, Some(first)), ids);
        // Docked back at the end of its group, still.
        let Some(PaneNode::Leaf(leaf)) = &mut project.layout else {
            unreachable!()
        };
        leaf.tabs.push(first);
        assert_eq!(recent_with(&project, None), ids);
        // Layout follows the layout.
        assert_eq!(
            sidebar_rows(&project, &workspace, &|_| false, SidebarSort::Layout),
            [ids[1], ids[2], first]
        );
    }
}
