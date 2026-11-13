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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ActivityBand {
    Today,
    ThisWeek,
    Older,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarEntry {
    Worktree(Uuid),
    /// Recent work's label over the rows asking for something, with how many
    /// sessions in them are asking.
    NeedsYou(usize),
    /// Recent work's label over the working rows, with how many sessions in
    /// them are working.
    Working(usize),
    /// Recent work's label over an open band's rows.
    Band(ActivityBand),
    /// Recent work's label for a folded band, in place of the band's label
    /// and rows.
    FoldedBand(BandLabel),
    Pane(Uuid),
    /// A top-level row whose family is folded: drawn as [`Self::Pane`], with
    /// what its hidden rows hold in place of them. Recent work only.
    Folded(Uuid, FamilyFold),
    /// A row drawn one level in, under the top-level session row before it:
    /// a file, diff or page that session opened, or a session it spawned.
    /// Recent work only.
    Nested(Uuid),
    /// A file, diff or page drawn two levels in, under the nested session row
    /// before it, which opened it. Recent work only.
    NestedTwice(Uuid),
}

/// What can fold in Recent work: a band, or the family under a top-level
/// session (by that session's id).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fold {
    Band(ActivityBand),
    Family(Uuid),
}

/// What a folded family's top row says of the rows it hides: its child
/// sessions by group, the files, diffs and pages under it or them, and how
/// many of all those are unread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FamilyFold {
    pub asking: usize,
    pub working: usize,
    /// Have worked and aren't running a turn.
    pub done: usize,
    /// Have never worked.
    pub idle: usize,
    pub files: usize,
    pub unread: usize,
}

/// What a folded band's label says of the rows it hides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BandLabel {
    pub band: ActivityBand,
    /// The band's top-level rows; a family counts once.
    pub rows: usize,
    /// How many of the band's rows, nested ones included, are unread.
    pub unread: usize,
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

/// The groups of rows in Recent work, top to bottom. Whether a row has been
/// read never decides its group or its place in the group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecentGroup {
    /// Asking for something now: newest question first, whatever day it was
    /// asked.
    Asking,
    /// Running a turn: oldest start first, so a new turn joins the bottom.
    Working,
    /// Has worked and isn't running a turn (finished, stopped, exited,
    /// interrupted or not running): in the band of its latest event, newest
    /// first.
    Done,
    /// Has never worked: in the band it was opened, newest open first, below
    /// that band's done rows.
    Unused,
}

/// The sections of Recent work, top to bottom, each under its own label when
/// a project's rows span more than one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RecentSection {
    /// The asking rows.
    NeedsYou,
    /// The working rows.
    Working,
    /// The done and never-used rows of one band.
    Band(ActivityBand),
}

/// Where a row sits in Recent work: its section, its group within the
/// section, then its time within the group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecentPlace {
    band: ActivityBand,
    group: RecentGroup,
    at: Option<i64>,
}

impl RecentPlace {
    /// `shown` is the state the row shows now. Only a change of the row's own
    /// work moves it: a start, a finish, a question or an answer. A done
    /// row's band comes from the latest of those, so a row that stops working
    /// stays in the band its turn started in. A row that has never worked goes
    /// by when it was last opened (a file a session opened again counts from
    /// then), or with the Older rows when that isn't known. Asking and working
    /// rows have their own sections above the bands. `running` says whether
    /// the row's process is running (`placed_state`).
    pub fn of(
        instance: &Instance,
        shown: Option<AgentActivityState>,
        running: bool,
        clock: BandClock,
    ) -> Self {
        let activity = &instance.activity;
        let latest = activity.last_work_at();
        let opened = instance.created_at.max(instance.opened_at);
        let (group, at) = match placed_state(shown, running) {
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
                Some(at) => (RecentGroup::Done, at),
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

    /// The section this place puts its row in.
    pub fn section(self) -> RecentSection {
        match self.group {
            RecentGroup::Asking => RecentSection::NeedsYou,
            RecentGroup::Working => RecentSection::Working,
            RecentGroup::Done | RecentGroup::Unused => RecentSection::Band(self.band),
        }
    }
}

/// The state a row is placed and counted by: the one it shows, except that a
/// row whose process isn't running can't be running a turn. A session put to
/// sleep mid-turn shows the turn it was saved in; it goes with the done rows by
/// its latest event, and neither Working's count nor a family's working tag
/// counts it.
fn placed_state(shown: Option<AgentActivityState>, running: bool) -> Option<AgentActivityState> {
    match shown {
        Some(AgentActivityState::Working) if !running => Some(AgentActivityState::Idle),
        state => state,
    }
}

impl Ord for RecentPlace {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.section(), self.group)
            .cmp(&(other.section(), other.group))
            .then_with(|| match self.group {
                RecentGroup::Working => self.at.cmp(&other.at),
                RecentGroup::Asking | RecentGroup::Done | RecentGroup::Unused => {
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

/// The session row `id` nests under in Recent work. A file, diff or page nests
/// under the session that last opened it. A session nests one level under the
/// session that spawned it (`muxel ctl new` from its pane) or that the user
/// dragged it onto, and a session spawned by a nested one nests under the same
/// top-level session, so there are at most two levels: a session, its child
/// sessions, and each one's files. Only a session in `listed` (the project's
/// rows) takes nested rows: one that has closed, is held for Undo, or sits in
/// another project leaves its rows as ordinary ones. A pinned row nests only
/// under a pinned session, so its pin keeps it in the pinned block. Spawners
/// that loop back on each other nest nothing.
pub fn nests_under(id: Uuid, instances: &InstancesById, listed: &HashSet<Uuid>) -> Option<Uuid> {
    let instance = instances.get(&id)?;
    let parent = listed_parent(instance, instances, listed)?;
    if instance.kind != InstanceKind::Terminal {
        return Some(parent);
    }
    let mut seen = HashSet::from([id]);
    let mut top = parent;
    loop {
        if !seen.insert(top) {
            return None;
        }
        match instances
            .get(&top)
            .and_then(|session| listed_parent(session, instances, listed))
        {
            Some(up) => top = up,
            None => return Some(top),
        }
    }
}

/// `instance`'s opener or spawner when it is a listed session it may nest
/// under.
fn listed_parent(
    instance: &Instance,
    instances: &InstancesById,
    listed: &HashSet<Uuid>,
) -> Option<Uuid> {
    instance.opened_by.filter(|parent| {
        *parent != instance.id
            && listed.contains(parent)
            && instances.get(parent).is_some_and(|parent| {
                parent.kind == InstanceKind::Terminal && (parent.pinned || !instance.pinned)
            })
    })
}

/// The `opened_by` that dropping session `dragged` on session row `onto` gives
/// it in Recent work: `onto`'s top-level session, so nesting stays one level
/// deep. `None` when the drop changes nothing: `dragged` or `onto` isn't a
/// session among the project's rows (`listed`), `onto` is `dragged` or nests
/// under it, `dragged` already nests there, or its pin would keep it out (a
/// pinned session nests only under a pinned one).
pub fn nest_on_drop(
    dragged: Uuid,
    onto: Uuid,
    instances: &InstancesById,
    listed: &HashSet<Uuid>,
) -> Option<Uuid> {
    let session = instances
        .get(&dragged)
        .filter(|i| i.kind == InstanceKind::Terminal && listed.contains(&dragged))?;
    instances
        .get(&onto)
        .filter(|i| i.kind == InstanceKind::Terminal && listed.contains(&onto))?;
    let top = nests_under(onto, instances, listed).unwrap_or(onto);
    let parent = instances.get(&top)?;
    (top != dragged
        && nests_under(dragged, instances, listed) != Some(top)
        && (parent.pinned || !session.pinned))
        .then_some(top)
}

/// The sessions nested under session `top` in Recent work, in `listed` order:
/// its child sessions, including the ones they spawned, which nest under `top`
/// too. A nested session has none under it. Closing `top` with everything
/// under it closes these, and the files each of them would take
/// ([`closes_with`]).
pub fn sessions_under(top: Uuid, listed: &[Uuid], instances: &InstancesById) -> Vec<Uuid> {
    let set: HashSet<Uuid> = listed.iter().copied().collect();
    listed
        .iter()
        .copied()
        .filter(|&id| {
            id != top
                && instances
                    .get(&id)
                    .is_some_and(|i| i.kind == InstanceKind::Terminal)
                && nests_under(id, instances, &set) == Some(top)
        })
        .collect()
}

/// How a click with a modifier changes the sidebar's row selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectClick {
    /// Ctrl (Cmd on macOS): add the row, or take it out if it's in.
    Toggle,
    /// Shift: select every row from the anchor to this one.
    Range,
}

/// The selection and its anchor after a modified click on row `clicked`.
/// `order` is the rows as the sidebar draws them. A toggle makes `clicked` the
/// anchor; a range keeps the anchor and replaces the selection with the rows
/// from it to `clicked`, either way along `order`, or just `clicked` when the
/// anchor isn't among them (another project's row, or none yet).
pub fn select_rows(
    selected: &[Uuid],
    anchor: Option<Uuid>,
    clicked: Uuid,
    click: SelectClick,
    order: &[Uuid],
) -> (Vec<Uuid>, Option<Uuid>) {
    match click {
        SelectClick::Toggle => {
            let mut next: Vec<Uuid> = selected
                .iter()
                .copied()
                .filter(|&id| id != clicked)
                .collect();
            if next.len() == selected.len() {
                next.push(clicked);
            }
            (next, Some(clicked))
        }
        SelectClick::Range => {
            let at = |id: Uuid| order.iter().position(|&row| row == id);
            match (anchor.and_then(at), at(clicked)) {
                (Some(from), Some(to)) => (order[from.min(to)..=from.max(to)].to_vec(), anchor),
                _ => (vec![clicked], Some(clicked)),
            }
        }
    }
}

/// What a family's top row tells of the sessions nested under it, whose own
/// rows sit below it, where you may not look.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FamilyTags {
    /// How many show a question: the family sorts as Asking meanwhile.
    pub asking: usize,
    /// How many are working while the top session isn't, whose own badge
    /// would say so: the family sorts as Working meanwhile, unless one asks.
    pub working: usize,
}

/// The tags of the top row at `entries[at]` ([`FamilyTags`]). Any other
/// row has none. `running` says whether a row's process is running
/// (`placed_state`).
pub fn family_tags(
    entries: &[SidebarEntry],
    at: usize,
    instances: &InstancesById,
    shown: &dyn Fn(Uuid) -> Option<AgentActivityState>,
    running: &dyn Fn(Uuid) -> bool,
) -> FamilyTags {
    let placed = |id: Uuid| placed_state(shown(id), running(id));
    let mut tags = FamilyTags::default();
    let SidebarEntry::Pane(top) = entries[at] else {
        return tags;
    };
    let children = entries
        .iter()
        .skip(at + 1)
        .take_while(|entry| {
            matches!(
                entry,
                SidebarEntry::Nested(_) | SidebarEntry::NestedTwice(_)
            )
        })
        .filter_map(|entry| match entry {
            SidebarEntry::Nested(id)
                if instances
                    .get(id)
                    .is_some_and(|i| i.kind == InstanceKind::Terminal) =>
            {
                Some(placed(*id))
            }
            _ => None,
        });
    for state in children {
        match state {
            Some(AgentActivityState::Blocked) => tags.asking += 1,
            Some(AgentActivityState::Working) => tags.working += 1,
            _ => {}
        }
    }
    if placed(top) == Some(AgentActivityState::Working) {
        tags.working = 0;
    }
    tags
}

/// The files, diffs and pages that close with `sessions` when the user closes
/// them in Recent work: each one in a session's project whose last opener is
/// that session. A resource another session opened again since belongs to that
/// session and stays. Sessions nested under them never close with them: they
/// are work of their own. Returns `(resource, session)` pairs in workspace
/// order.
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
/// diff or page follows the session that opened it, newest open first, and a
/// session follows the one that spawned it ([`nests_under`]). A family moves
/// as one row: Asking while any of its sessions asks, else Working while any
/// works, else by its newest event ([`family_tags`] tells the top row).
/// Both sorts are stable; only recent work pulls pins into a leading block.
/// Recent work places every row, pinned or not, by [`RecentPlace`], from the
/// state each row shows (`shown`) and whether its process is running
/// (`running`). It labels Needs you and Working when the unpinned rows span
/// more than one section, and every band always, since a band can fold: a band
/// `folded` says is folded has a [`SidebarEntry::FoldedBand`] label in place
/// of its label and rows, and a top-level row with rows under it whose family
/// `folded` says is folded is a [`SidebarEntry::Folded`] row without them.
/// Nothing about reading or focus is an input.
pub fn sidebar_order(
    pane_ids: &[Uuid],
    instances: &InstancesById,
    shown: &dyn Fn(Uuid) -> Option<AgentActivityState>,
    running: &dyn Fn(Uuid) -> bool,
    folded: &dyn Fn(Fold) -> bool,
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

    // A row with no instance has no work and no open time.
    let unknown = RecentPlace {
        band: ActivityBand::Older,
        group: RecentGroup::Unused,
        at: None,
    };
    let places: HashMap<Uuid, RecentPlace> = pane_ids
        .iter()
        .map(|&id| {
            let place = instances.get(&id).map_or(unknown, |i| {
                RecentPlace::of(i, shown(id), running(id), clock)
            });
            (id, place)
        })
        .collect();
    let place = |id: &Uuid| places.get(id).copied().unwrap_or(unknown);

    let listed: HashSet<Uuid> = pane_ids.iter().copied().collect();
    let mut top = Vec::new();
    let mut files: HashMap<Uuid, Vec<(Uuid, Option<i64>)>> = HashMap::new();
    let mut sessions: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for &id in pane_ids {
        let Some(parent) = nests_under(id, instances, &listed) else {
            top.push(id);
            continue;
        };
        let instance = instances.get(&id);
        if instance.is_some_and(|i| i.kind == InstanceKind::Terminal) {
            sessions.entry(parent).or_default().push(id);
        } else {
            let opened = instance.and_then(|i| i.opened_at);
            files.entry(parent).or_default().push((id, opened));
        }
    }
    // Files newest open first, child sessions by the same rule as any row; both
    // sorts are stable, so ties keep layout order.
    for children in files.values_mut() {
        children.sort_by_key(|(_, at)| Reverse(*at));
    }
    for children in sessions.values_mut() {
        children.sort_by_key(place);
    }
    // A family's sessions: its top one and those nested under it.
    let members =
        |id: Uuid| std::iter::once(id).chain(sessions.get(&id).into_iter().flatten().copied());
    // While any of a family asks, it sorts as Asking by its newest question;
    // else while any of it works, as Working by its earliest start, so a
    // second turn starting doesn't move it; else by its newest event, so a
    // child's finish brings the family up.
    let family = |id: Uuid| {
        let all = || members(id).map(|member| place(&member));
        let of = |group| move |member: &RecentPlace| member.group == group;
        all()
            .filter(of(RecentGroup::Asking))
            .max_by_key(|asking| asking.at)
            .or_else(|| {
                all()
                    .filter(of(RecentGroup::Working))
                    .min_by_key(|working| working.at)
            })
            .or_else(|| {
                all()
                    .filter(of(RecentGroup::Done))
                    .max_by_key(|done| done.at)
            })
            .unwrap_or_else(|| place(&id))
    };
    let files_of = |session: &Uuid| files.get(session).into_iter().flatten().map(|(f, _)| *f);
    let unread_id = |id: &Uuid| {
        instances
            .get(id)
            .is_some_and(|instance| is_unread(&instance.activity))
    };
    let row = |out: &mut Vec<SidebarEntry>, id: Uuid| {
        let children = || sessions.get(&id).into_iter().flatten().copied();
        let under =
            files_of(&id).count() + children().map(|c| 1 + files_of(&c).count()).sum::<usize>();
        if under > 0 && folded(Fold::Family(id)) {
            let mut fold = FamilyFold::default();
            for child in children() {
                match place(&child).group {
                    RecentGroup::Asking => fold.asking += 1,
                    RecentGroup::Working => fold.working += 1,
                    RecentGroup::Done => fold.done += 1,
                    RecentGroup::Unused => fold.idle += 1,
                }
            }
            let hidden = files_of(&id)
                .chain(children().flat_map(|c| std::iter::once(c).chain(files_of(&c))));
            for hidden in hidden {
                if instances
                    .get(&hidden)
                    .is_some_and(|i| i.kind != InstanceKind::Terminal)
                {
                    fold.files += 1;
                }
                if unread_id(&hidden) {
                    fold.unread += 1;
                }
            }
            out.push(SidebarEntry::Folded(id, fold));
            return;
        }
        out.push(SidebarEntry::Pane(id));
        out.extend(files_of(&id).map(SidebarEntry::Nested));
        for child in children() {
            out.push(SidebarEntry::Nested(child));
            out.extend(files_of(&child).map(SidebarEntry::NestedTwice));
        }
    };
    let placed = |ids: Vec<Uuid>| {
        let mut rows: Vec<_> = ids.into_iter().map(|id| (id, family(id))).collect();
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
    // Pinned rows have a pin glyph instead of a section label; their places
    // must not cause labels to repeat across the pinned boundary.
    let rest = placed(rest);
    let labelled = rest.first().is_some_and(|(_, first)| {
        rest.iter()
            .any(|(_, place)| place.section() != first.section())
    });
    // The asking sessions under the Needs you label and the working ones under
    // the Working label, nested ones included.
    let in_section = |section: RecentSection, group: RecentGroup| {
        rest.iter()
            .filter(|(_, place)| place.section() == section)
            .flat_map(|&(id, _)| members(id))
            .filter(|member| place(member).group == group)
            .count()
    };
    let asking = in_section(RecentSection::NeedsYou, RecentGroup::Asking);
    let working = in_section(RecentSection::Working, RecentGroup::Working);
    // Each section's rows, with how many top-level rows it has; the rows are
    // sorted by section, so each section is one run.
    let mut sections: Vec<(RecentSection, usize, Vec<SidebarEntry>)> = Vec::new();
    for (id, place) in rest {
        let section = place.section();
        if sections.last().is_none_or(|(last, _, _)| *last != section) {
            sections.push((section, 0, Vec::new()));
        }
        if let Some((_, count, rows)) = sections.last_mut() {
            *count += 1;
            row(rows, id);
        }
    }
    // A row's own unread mark, and a folded family's hidden ones.
    let unread = |entry: &SidebarEntry| {
        let own = entry_id(entry).is_some_and(|id| unread_id(&id)) as usize;
        match entry {
            SidebarEntry::Folded(_, fold) => own + fold.unread,
            _ => own,
        }
    };
    for (section, count, rows) in sections {
        match section {
            RecentSection::NeedsYou | RecentSection::Working => {
                if labelled {
                    out.push(match section {
                        RecentSection::NeedsYou => SidebarEntry::NeedsYou(asking),
                        _ => SidebarEntry::Working(working),
                    });
                }
                out.extend(rows);
            }
            RecentSection::Band(band) if folded(Fold::Band(band)) => {
                out.push(SidebarEntry::FoldedBand(BandLabel {
                    band,
                    rows: count,
                    unread: rows.iter().map(unread).sum(),
                }));
            }
            RecentSection::Band(band) => {
                out.push(SidebarEntry::Band(band));
                out.extend(rows);
            }
        }
    }
    out
}

/// The row `entry` draws, if it is a row rather than a label.
pub fn entry_id(entry: &SidebarEntry) -> Option<Uuid> {
    match entry {
        SidebarEntry::Pane(id)
        | SidebarEntry::Folded(id, _)
        | SidebarEntry::Nested(id)
        | SidebarEntry::NestedTwice(id) => Some(*id),
        SidebarEntry::Worktree(_)
        | SidebarEntry::NeedsYou(_)
        | SidebarEntry::Working(_)
        | SidebarEntry::Band(_)
        | SidebarEntry::FoldedBand(_) => None,
    }
}

/// How many top-level rows the label at `at` heads in `entries`: the rows up
/// to the next label, a family counting once. A folded band's label says its
/// own count.
pub fn label_rows(entries: &[SidebarEntry], at: usize) -> usize {
    if let Some(SidebarEntry::FoldedBand(label)) = entries.get(at) {
        return label.rows;
    }
    entries
        .iter()
        .skip(at + 1)
        .take_while(|entry| entry_id(entry).is_some())
        .filter(|entry| matches!(entry, SidebarEntry::Pane(_) | SidebarEntry::Folded(..)))
        .count()
}

pub fn unread_count(pane_ids: &[Uuid], instances: &InstancesById) -> usize {
    pane_ids
        .iter()
        .filter_map(|id| instances.get(id))
        .filter(|i| is_unread(&i.activity))
        .count()
}

/// What rows add up to, for a project header and the workspace line: how many
/// ask, how many work and how many are unread. Nested rows count like any
/// other, so a folded project or band can't hide a question.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowCounts {
    pub asking: usize,
    pub working: usize,
    pub unread: usize,
}

impl std::ops::Add for RowCounts {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            asking: self.asking + other.asking,
            working: self.working + other.working,
            unread: self.unread + other.unread,
        }
    }
}

impl std::iter::Sum for RowCounts {
    fn sum<I: Iterator<Item = Self>>(rows: I) -> Self {
        rows.fold(Self::default(), std::ops::Add::add)
    }
}

/// Whether row `id` asks or works, by the state it is placed by
/// ([`sidebar_order`]): `shown` and whether its process is `running`.
pub fn row_state(
    id: Uuid,
    shown: &dyn Fn(Uuid) -> Option<AgentActivityState>,
    running: &dyn Fn(Uuid) -> bool,
) -> Option<AgentActivityState> {
    placed_state(shown(id), running(id))
}

/// [`RowCounts`] over `pane_ids`, placed the way [`sidebar_order`] places them,
/// so the counts agree with the Needs you and Working sections.
pub fn row_counts(
    pane_ids: &[Uuid],
    instances: &InstancesById,
    shown: &dyn Fn(Uuid) -> Option<AgentActivityState>,
    running: &dyn Fn(Uuid) -> bool,
) -> RowCounts {
    let mut counts = RowCounts::default();
    for &id in pane_ids {
        match row_state(id, shown, running) {
            Some(AgentActivityState::Blocked) => counts.asking += 1,
            Some(AgentActivityState::Working) => counts.working += 1,
            _ => {}
        }
        if instances.get(&id).is_some_and(|i| is_unread(&i.activity)) {
            counts.unread += 1;
        }
    }
    counts
}

/// The first row of `order` after `current` that `matches`, wrapping round;
/// `current` itself comes last, so a lone match is found again. With no
/// `current`, or one not in `order`, the search starts at the top.
pub fn next_matching(
    order: &[Uuid],
    current: Option<Uuid>,
    matches: &dyn Fn(Uuid) -> bool,
) -> Option<Uuid> {
    let start = current
        .and_then(|current| order.iter().position(|&id| id == current))
        .map_or(0, |at| at + 1);
    order
        .iter()
        .cycle()
        .skip(start)
        .take(order.len())
        .copied()
        .find(|&id| matches(id))
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
        sidebar_order(
            rows,
            &index,
            &shown,
            &|_| true,
            &|_| false,
            clock,
            SidebarSort::RecentWork,
        )
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

    /// A session in `project`'s single tab group with `activity`, spawned an
    /// hour ago by `spawner`, or by no session (`ctl new --top-level`, or the
    /// user opened it).
    fn session(
        workspace: &mut Workspace,
        project: &mut Project,
        spawner: Option<Uuid>,
        activity: AgentActivity,
    ) -> Uuid {
        let mut instance = Instance::shell(project.id);
        instance.activity = activity;
        instance.record_opener(spawner, NOW - HOUR);
        let id = instance.id;
        workspace.add_instance(instance);
        if let Some(PaneNode::Leaf(leaf)) = &mut project.layout {
            leaf.tabs.push(id);
        }
        id
    }

    /// Close `ids` in Recent work's eyes: they leave the project's rows.
    fn unlist(project: &mut Project, ids: &[Uuid]) {
        if let Some(PaneNode::Leaf(leaf)) = &mut project.layout {
            leaf.tabs.retain(|id| !ids.contains(id));
            leaf.active = 0;
        }
    }

    #[test]
    fn a_spawned_session_nests_under_its_spawner_and_the_family_moves_by_its_newest_event() {
        use SidebarEntry::{Nested, NestedTwice, Pane};
        let (mut project, mut workspace, ids) =
            fixture(vec![worked(NOW - 1_000), worked(NOW - 2_000)]);
        let (a, b) = (ids[0], ids[1]);
        // B spawns two sessions that both finished before A did; a third
        // session opted out with `--top-level`.
        let older = session(&mut workspace, &mut project, Some(b), worked(NOW - 3_000));
        let newer = session(&mut workspace, &mut project, Some(b), worked(NOW - 2_500));
        let alone = session(&mut workspace, &mut project, None, worked(NOW - 5_000));
        let page = file(&mut workspace, &mut project, Some(b), 10);
        let notes = file(&mut workspace, &mut project, Some(older), 20);
        // B's own files come first, then each child, newest event first, with
        // its files a level further in.
        assert_eq!(
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                Pane(a),
                Pane(b),
                Nested(page),
                Nested(newer),
                Nested(older),
                NestedTwice(notes),
                Pane(alone)
            ]
        );

        // The older child finishes a turn: the whole family moves above A,
        // and that child leads it.
        workspace.instance_mut(older).unwrap().activity.observe(
            Some(AgentActivityState::Working),
            AgentActivityState::Done,
            NOW - 100,
        );
        assert_eq!(
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                Pane(b),
                Nested(page),
                Nested(older),
                NestedTwice(notes),
                Nested(newer),
                Pane(a),
                Pane(alone)
            ]
        );
    }

    /// The tags the top row at `entries[at]` shows, every row showing its
    /// saved state.
    fn tags(workspace: &Workspace, entries: &[SidebarEntry], at: usize) -> FamilyTags {
        let index = instances_by_id(workspace);
        family_tags(
            entries,
            at,
            &index,
            &|id| index.get(&id).and_then(|i| live(i)),
            &|_| true,
        )
    }

    #[test]
    fn an_asking_child_lifts_its_family_and_its_parent_row_says_so() {
        use AgentActivityState::{Blocked, Done, Working};
        use SidebarEntry::{Band, NeedsYou, Nested, Pane};
        let (mut project, mut workspace, ids) =
            fixture(vec![worked(NOW - 1_000), worked(NOW - 40 * DAY)]);
        let (a, b) = (ids[0], ids[1]);
        let child = session(
            &mut workspace,
            &mut project,
            Some(b),
            worked(NOW - 40 * DAY + 1_000),
        );
        let settled = vec![
            Band(ActivityBand::Today),
            Pane(a),
            Band(ActivityBand::Older),
            Pane(b),
            Nested(child),
        ];
        assert_eq!(recent(&project, &workspace), settled);
        assert_eq!(tags(&workspace, &settled, 3), FamilyTags::default());

        // The child asks: its old parent leads the list with it, and says so.
        change(&mut workspace, child, Blocked, NOW - 50);
        let lifted = recent(&project, &workspace);
        assert_eq!(
            lifted,
            vec![
                NeedsYou(1),
                Pane(b),
                Nested(child),
                Band(ActivityBand::Today),
                Pane(a)
            ]
        );
        assert_eq!(tags(&workspace, &lifted, 1).asking, 1);
        assert_eq!(tags(&workspace, &lifted, 4), FamilyTags::default());
        // Looking at the question moves nothing.
        attend(&mut workspace, child, NOW - 10);
        assert_eq!(recent(&project, &workspace), lifted);

        // Answered, the child works on, and the family with it.
        change(&mut workspace, child, Working, NOW - 5);
        let working = recent(&project, &workspace);
        assert_eq!(
            working,
            vec![
                SidebarEntry::Working(1),
                Pane(b),
                Nested(child),
                Band(ActivityBand::Today),
                Pane(a)
            ]
        );
        assert_eq!(
            tags(&workspace, &working, 1),
            FamilyTags {
                asking: 0,
                working: 1
            }
        );

        // Its finish is the family's newest event.
        change(&mut workspace, child, Done, NOW - 1);
        assert_eq!(
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                Pane(b),
                Nested(child),
                Pane(a)
            ]
        );
    }

    #[test]
    fn an_old_question_lifts_its_family_into_needs_you() {
        use SidebarEntry::{Band, NeedsYou, Nested, Pane};
        let (mut project, mut workspace, ids) =
            fixture(vec![worked(NOW - 1_000), worked(NOW - 500)]);
        let (a, b) = (ids[0], ids[1]);
        // A's child asked nine days ago and is still waiting.
        let child = session(
            &mut workspace,
            &mut project,
            Some(a),
            worked(NOW - 9 * DAY - 1_000),
        );
        change(
            &mut workspace,
            child,
            AgentActivityState::Blocked,
            NOW - 9 * DAY,
        );
        assert_eq!(
            recent(&project, &workspace),
            vec![
                NeedsYou(1),
                Pane(a),
                Nested(child),
                Band(ActivityBand::Today),
                Pane(b)
            ]
        );
    }

    /// The busy day in the order design: a spawner that finished an hour ago
    /// with one child working and one done sits among the working rows by
    /// its child's start, and says how many of its children work.
    #[test]
    fn a_family_sorts_as_working_while_any_child_works() {
        use AgentActivityState::{Done, Working};
        use SidebarEntry::{Band, Nested, Pane};
        let (mut project, mut workspace, ids) = fixture(vec![
            worked(NOW - 3 * HOUR),
            worked(NOW - 3 * HOUR),
            worked(NOW - 65 * MINUTE),
            worked(NOW - 3 * HOUR),
            worked(NOW - 2 * MINUTE),
        ]);
        let (early, middle, release, late, finished) = (ids[0], ids[1], ids[2], ids[3], ids[4]);
        let first = session(
            &mut workspace,
            &mut project,
            Some(release),
            worked(NOW - 2 * HOUR),
        );
        let second = session(
            &mut workspace,
            &mut project,
            Some(release),
            worked(NOW - 12 * MINUTE),
        );
        for (id, started) in [
            (early, NOW - 130 * MINUTE),
            (middle, NOW - 48 * MINUTE),
            (first, NOW - 35 * MINUTE),
            (late, NOW - 21 * MINUTE),
        ] {
            change(&mut workspace, id, Working, started);
        }
        let order = recent(&project, &workspace);
        assert_eq!(
            order,
            vec![
                SidebarEntry::Working(4),
                Pane(early),
                Pane(middle),
                Pane(release),
                Nested(first),
                Nested(second),
                Pane(late),
                Band(ActivityBand::Today),
                Pane(finished),
            ]
        );
        assert_eq!(tags(&workspace, &order, 3).working, 1);

        // The second child starts a turn: the family keeps its place, by its
        // earliest start, and that child goes under the first.
        change(&mut workspace, second, Working, NOW - MINUTE);
        let order = recent(&project, &workspace);
        assert_eq!(order[3..6], [Pane(release), Nested(first), Nested(second)]);
        assert_eq!(order[0], SidebarEntry::Working(5));
        assert_eq!(tags(&workspace, &order, 3).working, 2);
        // The first finishes: the family moves down the block once, by the
        // second's start.
        change(&mut workspace, first, Done, NOW);
        assert_eq!(
            panes(recent(&project, &workspace)),
            vec![early, middle, late, release, finished]
        );
        // The parent's own turn needs no tag: its own badge says working.
        change(&mut workspace, release, Working, NOW);
        let order = recent(&project, &workspace);
        let at = order.iter().position(|e| *e == Pane(release)).unwrap();
        assert_eq!(tags(&workspace, &order, at), FamilyTags::default());
    }

    #[test]
    fn a_spawned_session_never_closes_with_its_spawner_and_outlives_it() {
        use SidebarEntry::{Nested, NestedTwice, Pane};
        let (mut project, mut workspace, ids) = fixture(vec![worked(NOW - 1_000)]);
        let parent = ids[0];
        let child = session(
            &mut workspace,
            &mut project,
            Some(parent),
            worked(NOW - 2_000),
        );
        let plan = file(&mut workspace, &mut project, Some(parent), 10);
        let notes = file(&mut workspace, &mut project, Some(child), 20);
        assert_eq!(
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                Pane(parent),
                Nested(plan),
                Nested(child),
                NestedTwice(notes)
            ]
        );
        // Closing the parent takes its own files, not its child or the
        // child's files.
        assert_eq!(
            closes_with(SidebarSort::RecentWork, &[parent], &workspace.instances),
            vec![(plan, parent)]
        );
        // Once it has closed, or while it is held for Undo, the child is an
        // ordinary row with its files one level in.
        unlist(&mut project, &[parent, plan]);
        assert_eq!(
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                Pane(child),
                Nested(notes)
            ]
        );
    }

    #[test]
    fn a_grandchild_session_nests_under_the_top_session_so_there_are_two_levels() {
        use SidebarEntry::{Nested, NestedTwice, Pane};
        let (mut project, mut workspace, ids) = fixture(vec![worked(NOW - 1_000)]);
        let top = ids[0];
        let child = session(&mut workspace, &mut project, Some(top), worked(NOW - 2_000));
        let grandchild = session(
            &mut workspace,
            &mut project,
            Some(child),
            worked(NOW - 3_000),
        );
        let notes = file(&mut workspace, &mut project, Some(grandchild), 10);
        assert_eq!(
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                Pane(top),
                Nested(child),
                Nested(grandchild),
                NestedTwice(notes)
            ]
        );

        // The top session closes: its child takes the grandchild.
        unlist(&mut project, &[top]);
        assert_eq!(
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                Pane(child),
                Nested(grandchild),
                NestedTwice(notes)
            ]
        );

        // Spawners that loop back on each other nest no session.
        workspace.instance_mut(child).unwrap().opened_by = Some(grandchild);
        assert_eq!(
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                Pane(child),
                Pane(grandchild),
                Nested(notes)
            ]
        );
    }

    #[test]
    fn dropping_a_session_on_a_row_nests_it_under_that_rows_top_session() {
        use SidebarEntry::{Nested, Pane};
        let (mut project, mut workspace, ids) =
            fixture(vec![worked(NOW - 1_000), worked(NOW - 2_000)]);
        let (a, x) = (ids[0], ids[1]);
        let child = session(&mut workspace, &mut project, Some(a), worked(NOW - 3_000));
        let mine = session(&mut workspace, &mut project, Some(x), worked(NOW - 4_000));
        let plan = file(&mut workspace, &mut project, Some(a), 10);
        let gone = Instance::shell(project.id);
        let closed = gone.id;
        workspace.add_instance(gone);
        {
            let index = instances_by_id(&workspace);
            let listed: HashSet<Uuid> = project.instances().into_iter().collect();
            let nest = |dragged, onto| nest_on_drop(dragged, onto, &index, &listed);
            // Onto a top session or a session nested under it: one level under
            // the top session either way.
            assert_eq!(nest(x, a), Some(a));
            assert_eq!(nest(x, child), Some(a));
            // Onto itself or its own child, where it already nests, a file
            // either way, or a session that has closed: nothing changes.
            assert_eq!(nest(a, a), None);
            assert_eq!(nest(a, child), None);
            assert_eq!(nest(child, a), None);
            assert_eq!(nest(plan, x), None);
            assert_eq!(nest(x, plan), None);
            assert_eq!(nest(x, closed), None);
            // A session that isn't one of the project's rows (closed, held
            // for Undo, in another project) can't be nested by a drop.
            assert_eq!(nest(closed, a), None);
        }

        // X joins A and brings its own child along, still two levels.
        workspace.instance_mut(x).unwrap().opened_by = Some(a);
        assert_eq!(
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                Pane(a),
                Nested(plan),
                Nested(x),
                Nested(child),
                Nested(mine)
            ]
        );

        // Dropped on the project's heading, a session leaves its family.
        workspace.instance_mut(child).unwrap().opened_by = None;
        assert_eq!(
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                Pane(a),
                Nested(plan),
                Nested(x),
                Nested(mine),
                Pane(child)
            ]
        );
    }

    #[test]
    fn a_pinned_session_nests_only_under_a_pinned_spawner() {
        use SidebarEntry::{Nested, Pane};
        let (mut project, mut workspace, ids) =
            fixture(vec![worked(NOW - 1_000), worked(NOW - 2_000)]);
        let (a, b) = (ids[0], ids[1]);
        let child = session(&mut workspace, &mut project, Some(b), worked(NOW - 3_000));
        workspace.instance_mut(child).unwrap().pinned = true;
        // B isn't pinned, so the child leads the list on its own pin, and
        // dropping it back on B changes nothing.
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Pane(child),
                SidebarEntry::Band(ActivityBand::Today),
                Pane(a),
                Pane(b)
            ]
        );
        let listed: HashSet<Uuid> = project.instances().into_iter().collect();
        assert_eq!(
            nest_on_drop(child, b, &instances_by_id(&workspace), &listed),
            None
        );
        // Pin B as well and the child nests under it inside the pinned block.
        workspace.instance_mut(b).unwrap().pinned = true;
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Pane(b),
                Nested(child),
                SidebarEntry::Band(ActivityBand::Today),
                Pane(a)
            ]
        );
    }

    /// Everything under a session is exactly what Recent work draws under
    /// its row: the sessions it spawned and theirs, never its files (they
    /// close by `closes_with`), another family's sessions, or one whose
    /// spawner has closed.
    #[test]
    fn the_sessions_under_a_session_are_the_ones_its_row_draws_under_it() {
        let (mut project, mut workspace, ids) =
            fixture(vec![worked(NOW - 1_000), worked(NOW - 2_000)]);
        let (a, b) = (ids[0], ids[1]);
        let child = session(&mut workspace, &mut project, Some(a), worked(NOW - 3_000));
        let grandchild = session(
            &mut workspace,
            &mut project,
            Some(child),
            worked(NOW - 4_000),
        );
        let theirs = session(&mut workspace, &mut project, Some(b), worked(NOW - 5_000));
        let _plan = file(&mut workspace, &mut project, Some(a), 10);
        let gone = Instance::shell(project.id);
        let orphan = session(
            &mut workspace,
            &mut project,
            Some(gone.id),
            worked(NOW - 6_000),
        );
        workspace.add_instance(gone);

        let index = instances_by_id(&workspace);
        let listed = project.instances();
        let under = |top| sessions_under(top, &listed, &index);
        assert_eq!(under(a), vec![child, grandchild]);
        assert_eq!(under(b), vec![theirs]);
        // A nested session and a top session with nothing under it: none.
        assert_eq!(under(child), Vec::<Uuid>::new());
        assert_eq!(under(orphan), Vec::<Uuid>::new());
        // The same rows the sidebar draws nested under each top row.
        let drawn = recent(&project, &workspace);
        for top in [a, b] {
            let at = drawn
                .iter()
                .position(|e| *e == SidebarEntry::Pane(top))
                .unwrap();
            let nested_sessions: Vec<Uuid> = drawn[at + 1..]
                .iter()
                .map_while(|entry| match entry {
                    SidebarEntry::Nested(id) | SidebarEntry::NestedTwice(id) => Some(*id),
                    _ => None,
                })
                .filter(|id| index[id].kind == InstanceKind::Terminal)
                .collect();
            let mut expected = under(top);
            expected.sort();
            let mut nested_sessions = nested_sessions;
            nested_sessions.sort();
            assert_eq!(nested_sessions, expected, "{drawn:?}");
        }
    }

    #[test]
    fn ctrl_toggles_a_row_and_shift_selects_from_the_anchor() {
        use SelectClick::{Range, Toggle};
        let rows: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        let r = |i: usize| rows[i];

        // Ctrl adds a row and anchors there; again takes it out.
        let (selected, anchor) = select_rows(&[], None, r(1), Toggle, &rows);
        assert_eq!((selected.clone(), anchor), (vec![r(1)], Some(r(1))));
        let (selected, anchor) = select_rows(&selected, anchor, r(3), Toggle, &rows);
        assert_eq!(selected, vec![r(1), r(3)]);
        let (selected, anchor) = select_rows(&selected, anchor, r(1), Toggle, &rows);
        assert_eq!((selected.clone(), anchor), (vec![r(3)], Some(r(1))));

        // Shift selects from the anchor to the row, either way, keeping it.
        let (selected, anchor) = select_rows(&selected, Some(r(1)), r(4), Range, &rows);
        assert_eq!((selected, anchor), (rows[1..=4].to_vec(), Some(r(1))));
        let (selected, anchor) = select_rows(&[], Some(r(3)), r(0), Range, &rows);
        assert_eq!((selected, anchor), (rows[0..=3].to_vec(), Some(r(3))));

        // With no anchor among the rows, Shift selects just the row.
        let elsewhere = Uuid::new_v4();
        for anchor in [None, Some(elsewhere)] {
            assert_eq!(
                select_rows(&[r(0)], anchor, r(2), Range, &rows),
                (vec![r(2)], Some(r(2)))
            );
        }
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
                SidebarEntry::Band(ActivityBand::Today),
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
                SidebarEntry::Band(ActivityBand::Today),
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
                SidebarEntry::Band(ActivityBand::Today),
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
        // Under Today's label and B's row.
        assert_eq!(recent(&project, &workspace)[2], Nested(plan));
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
                &|_| true,
                &|_| false,
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
            vec![
                Pane(old),
                Nested(page),
                SidebarEntry::Band(ActivityBand::Today),
                Pane(recent_session)
            ]
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
            vec![
                Pane(plan),
                SidebarEntry::Band(ActivityBand::Today),
                Pane(a),
                Pane(b)
            ]
        );
        // Pin B as well and the plan nests under it inside the pinned block.
        workspace.instance_mut(b).unwrap().pinned = true;
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Pane(b),
                Nested(plan),
                SidebarEntry::Band(ActivityBand::Today),
                Pane(a)
            ]
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
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                SidebarEntry::Pane(ids[0]),
                SidebarEntry::Pane(orphan)
            ]
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
        // Every third row spawns the next, which spawns the one after, so the
        // families read take every shape: an asking parent, child or grandchild.
        for (row, pair) in ids.windows(2).enumerate() {
            if row % 3 != 2 {
                workspace.instance_mut(pair[1]).unwrap().opened_by = Some(pair[0]);
            }
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
            assert!(unseen.iter().any(|e| matches!(e, SidebarEntry::Nested(_))));
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
            unread(NOW - 90 * MINUTE),
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
            vec![first, second, read, unseen]
        );
        // An unseen finish doesn't hold it up: it keeps only its dot.
        change(&mut workspace, unseen, AgentActivityState::Working, NOW);
        assert_eq!(
            pane_order(&project, &workspace),
            vec![first, second, unseen, read]
        );
        assert!(is_unread(&workspace.instance(unseen).unwrap().activity));
    }

    #[test]
    fn finishing_moves_a_row_to_the_top_of_the_done_rows_watched_or_not() {
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
            // Just under the working row, whose turn began before it.
            assert_eq!(
                pane_order(&project, &workspace),
                vec![ids[2], ids[3], ids[0], ids[1]],
                "watched: {watched}"
            );
        }
    }

    #[test]
    fn a_question_leads_the_working_rows_and_newer_finishes() {
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
            vec![asking, working, finished]
        );
        // Seen but not yet answered, it is still asking.
        attend(&mut workspace, asking, NOW);
        assert_eq!(
            pane_order(&project, &workspace),
            vec![asking, working, finished]
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
            vec![asking, working, read]
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
            vec![working, asking, read]
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
        // Working rows lead.
        assert_eq!(pane_order(&project, &workspace), vec![flicker, read]);
        // Idle for a moment: still today's work, the newest event in the band.
        change(
            &mut workspace,
            flicker,
            AgentActivityState::Idle,
            NOW - 9 * MINUTE,
        );
        assert_eq!(
            recent(&project, &workspace),
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                SidebarEntry::Pane(flicker),
                SidebarEntry::Pane(read)
            ]
        );
        change(
            &mut workspace,
            flicker,
            AgentActivityState::Working,
            NOW - 8 * MINUTE,
        );
        assert_eq!(pane_order(&project, &workspace), vec![flicker, read]);
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
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                SidebarEntry::Pane(ids[1]),
                SidebarEntry::Pane(ids[0])
            ]
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
            vec![
                SidebarEntry::Band(ActivityBand::Today),
                SidebarEntry::Pane(saved),
                SidebarEntry::Pane(read)
            ]
        );
        // Its terminal comes up still working: it leads as a working row.
        assert_eq!(pane_order(&project, &workspace), vec![saved, read]);
    }

    /// A session put to sleep mid-turn shows the turn it was saved in, but no
    /// process runs it: it sorts with the done rows by its latest event, and
    /// neither Working's count nor its family's tag counts it.
    #[test]
    fn a_working_row_whose_process_is_not_running_sits_with_the_done_rows() {
        use SidebarEntry::{Band, Nested, Pane};
        let (mut project, mut workspace, ids) = fixture(vec![
            worked(NOW - 3 * HOUR),
            worked(NOW - 3 * DAY),
            worked(NOW - HOUR),
            worked(NOW - 5 * HOUR),
        ]);
        let (busy, asleep, finished, parent) = (ids[0], ids[1], ids[2], ids[3]);
        let child = session(
            &mut workspace,
            &mut project,
            Some(parent),
            worked(NOW - 4 * DAY),
        );
        for (id, started) in [
            (busy, NOW - 30 * MINUTE),
            (asleep, NOW - 2 * HOUR),
            (child, NOW - 90 * MINUTE),
        ] {
            change(&mut workspace, id, AgentActivityState::Working, started);
        }
        let index = instances_by_id(&workspace);
        let rows = sidebar_rows(&project, &workspace, &|_| false, SidebarSort::RecentWork);
        let shown = |id: Uuid| index.get(&id).and_then(|i| live(i));
        let order = |running: &dyn Fn(Uuid) -> bool| {
            let entries = sidebar_order(
                &rows,
                &index,
                &shown,
                running,
                &|_| false,
                clock(),
                SidebarSort::RecentWork,
            );
            let at = entries.iter().position(|e| *e == Pane(parent)).unwrap();
            let working_tag = family_tags(&entries, at, &index, &shown, running).working;
            (entries, working_tag)
        };

        // Every process running: all three lead as working rows.
        assert_eq!(
            order(&|_| true),
            (
                vec![
                    SidebarEntry::Working(3),
                    Pane(asleep),
                    Pane(parent),
                    Nested(child),
                    Pane(busy),
                    Band(ActivityBand::Today),
                    Pane(finished),
                ],
                1
            )
        );
        // The sleeping two run nothing: each sits with the done rows by its
        // start, the latest thing it did, and only the live turn is counted.
        let awake = |id: Uuid| id != asleep && id != child;
        assert_eq!(
            order(&awake),
            (
                vec![
                    SidebarEntry::Working(1),
                    Pane(busy),
                    Band(ActivityBand::Today),
                    Pane(finished),
                    Pane(parent),
                    Nested(child),
                    Pane(asleep),
                ],
                0
            )
        );
    }

    /// Recent work over `project`'s rows, each showing its saved state, with
    /// the bands `folded` says are folded.
    fn order_folded(
        project: &Project,
        workspace: &Workspace,
        folded: &dyn Fn(ActivityBand) -> bool,
    ) -> Vec<SidebarEntry> {
        let rows = sidebar_rows(project, workspace, &|_| false, SidebarSort::RecentWork);
        let index = instances_by_id(workspace);
        let shown = |id: Uuid| index.get(&id).and_then(|i| live(i));
        let folded = |fold| matches!(fold, Fold::Band(band) if folded(band));
        sidebar_order(
            &rows,
            &index,
            &shown,
            &|_| true,
            &folded,
            clock(),
            SidebarSort::RecentWork,
        )
    }

    #[test]
    fn a_folded_family_is_one_row_that_says_what_it_hides() {
        use AgentActivityState::{Blocked, Working};
        use SidebarEntry::{Band, Folded, NeedsYou, Pane};
        let (mut project, mut workspace, ids) =
            fixture(vec![worked(NOW - HOUR), worked(NOW - 2 * HOUR)]);
        let (top, other) = (ids[0], ids[1]);
        let mut child = |activity| session(&mut workspace, &mut project, Some(top), activity);
        let asker = child(worked(NOW - 3 * HOUR));
        let worker = child(worked(NOW - 4 * HOUR));
        let finisher = child(unread(NOW - 30 * MINUTE));
        change(&mut workspace, asker, Blocked, NOW - 10 * MINUTE);
        change(&mut workspace, worker, Working, NOW - 5 * MINUTE);
        file(&mut workspace, &mut project, Some(top), NOW - 20 * MINUTE);
        file(
            &mut workspace,
            &mut project,
            Some(finisher),
            NOW - 25 * MINUTE,
        );
        let rows = sidebar_rows(&project, &workspace, &|_| false, SidebarSort::RecentWork);
        let index = instances_by_id(&workspace);
        let shown = |id: Uuid| index.get(&id).and_then(|i| live(i));
        let order = |folded: &dyn Fn(Fold) -> bool| {
            sidebar_order(
                &rows,
                &index,
                &shown,
                &|_| true,
                folded,
                clock(),
                SidebarSort::RecentWork,
            )
        };
        // Two labels; top, its three children and two files; and the other row.
        assert_eq!(
            order(&|_| false).len(),
            2 + 6 + 1,
            "two labels and every row"
        );
        let folded = order(&|fold| fold == Fold::Family(top));
        assert_eq!(
            folded,
            vec![
                NeedsYou(1),
                Folded(
                    top,
                    FamilyFold {
                        asking: 1,
                        working: 1,
                        done: 1,
                        idle: 0,
                        files: 2,
                        unread: 2,
                    }
                ),
                Band(ActivityBand::Today),
                Pane(other),
            ],
            "the family keeps its place, and its asking child its count"
        );
        assert_eq!(label_rows(&folded, 0), 1);
        // A family with nothing under it has nothing to fold.
        assert_eq!(
            order(&|fold| fold == Fold::Family(other))[7..],
            [Band(ActivityBand::Today), Pane(other)]
        );
    }

    #[test]
    fn a_folded_band_lists_none_of_its_rows_and_says_what_it_hides() {
        use ActivityBand::{Older, Today};
        use SidebarEntry::{Band, FoldedBand, Nested, Pane};
        // Today: a read finish. Older: a read finish, and an unread one whose
        // spawned session also finished unread.
        let (mut project, mut workspace, ids) = fixture(vec![
            worked(NOW - HOUR),
            worked(NOW - 9 * DAY),
            unread(NOW - 10 * DAY),
        ]);
        let child = session(
            &mut workspace,
            &mut project,
            Some(ids[2]),
            unread(NOW - 11 * DAY),
        );
        let open = order_folded(&project, &workspace, &|_| false);
        assert_eq!(
            open,
            vec![
                Band(Today),
                Pane(ids[0]),
                Band(Older),
                Pane(ids[1]),
                Pane(ids[2]),
                Nested(child),
            ]
        );
        assert_eq!(label_rows(&open, 2), 2, "a family counts once");
        let folded = order_folded(&project, &workspace, &|band| band == Older);
        assert_eq!(
            folded,
            vec![
                Band(Today),
                Pane(ids[0]),
                FoldedBand(BandLabel {
                    band: Older,
                    rows: 2,
                    unread: 2,
                }),
            ],
            "the unread count takes in the nested session"
        );
        assert_eq!(label_rows(&folded, 0), 1);
        assert_eq!(label_rows(&folded, 2), 2);
    }

    #[test]
    fn a_band_has_its_label_even_alone_so_it_can_fold() {
        use ActivityBand::Today;
        use SidebarEntry::{Band, Pane};
        let (project, workspace, ids) = fixture(vec![worked(NOW - HOUR), worked(NOW - 2 * HOUR)]);
        assert_eq!(
            recent(&project, &workspace),
            vec![Band(Today), Pane(ids[0]), Pane(ids[1])]
        );
        // Asking rows alone need no label: Needs you never folds.
        let asked = |at: i64| {
            let mut activity = AgentActivity::default();
            activity.observe(None, AgentActivityState::Working, at - MINUTE);
            activity.observe(
                Some(AgentActivityState::Working),
                AgentActivityState::Blocked,
                at,
            );
            activity
        };
        let (project, workspace, ids) = fixture(vec![asked(NOW - 2 * MINUTE), asked(NOW - MINUTE)]);
        assert_eq!(
            recent(&project, &workspace),
            vec![Pane(ids[1]), Pane(ids[0])]
        );
    }

    /// Folding hides no count: with every band folded, every asking and
    /// working row is still drawn and every unread row is drawn or counted on
    /// a folded band's label, and Needs you's label counts every asking row,
    /// over every lifecycle a row can be in, running or not, in families of
    /// every shape.
    #[test]
    fn folding_hides_no_count_and_needs_you_counts_every_question() {
        let rows = population();
        let (project, mut workspace, ids) =
            fixture(rows.iter().map(|(activity, _)| activity.clone()).collect());
        for (row, (&id, (_, opened))) in ids.iter().zip(&rows).enumerate() {
            let instance = workspace.instance_mut(id).unwrap();
            instance.created_at = *opened;
            // Every fourth row has been seen since its last event.
            if row % 4 == 0 {
                instance.activity.last_attended_at = Some(NOW);
            }
        }
        for (row, pair) in ids.windows(2).enumerate() {
            if row % 3 != 2 {
                workspace.instance_mut(pair[1]).unwrap().opened_by = Some(pair[0]);
            }
        }
        let listed = sidebar_rows(&project, &workspace, &|_| false, SidebarSort::RecentWork);
        let index = instances_by_id(&workspace);
        let shown = |id: Uuid| index.get(&id).and_then(|i| live(i));
        // Every third row's process has stopped.
        let stopped: HashSet<Uuid> = ids.iter().copied().step_by(3).collect();
        let running = |id: Uuid| !stopped.contains(&id);
        let order = |folded: &dyn Fn(Fold) -> bool| {
            sidebar_order(
                &listed,
                &index,
                &shown,
                &running,
                folded,
                clock(),
                SidebarSort::RecentWork,
            )
        };
        let counts = row_counts(&listed, &index, &shown, &running);
        assert!(counts.asking > 0 && counts.working > 0 && counts.unread > 0);

        let needs_you = order(&|_| false).into_iter().find_map(|entry| match entry {
            SidebarEntry::NeedsYou(n) => Some(n),
            _ => None,
        });
        assert_eq!(needs_you, Some(counts.asking));

        // What the rows drawn show plus what the folded labels and families
        // say they hide, as (asking, working, unread).
        let shown_and_said = |entries: &[SidebarEntry]| {
            let drawn: Vec<Uuid> = entries.iter().filter_map(entry_id).collect();
            let drawn_in = |state| {
                drawn
                    .iter()
                    .filter(|&&id| row_state(id, &shown, &running) == Some(state))
                    .count()
            };
            let said = |pick: fn(&FamilyFold) -> usize| -> usize {
                entries
                    .iter()
                    .map(|entry| match entry {
                        SidebarEntry::Folded(_, fold) => pick(fold),
                        _ => 0,
                    })
                    .sum()
            };
            let unread_drawn = drawn
                .iter()
                .filter(|&id| is_unread(&index[id].activity))
                .count();
            let unread_bands: usize = entries
                .iter()
                .map(|entry| match entry {
                    SidebarEntry::FoldedBand(label) => label.unread,
                    _ => 0,
                })
                .sum();
            (
                drawn_in(AgentActivityState::Blocked) + said(|fold| fold.asking),
                drawn_in(AgentActivityState::Working) + said(|fold| fold.working),
                unread_drawn + unread_bands + said(|fold| fold.unread),
            )
        };
        let all = (counts.asking, counts.working, counts.unread);
        let bands_folded = order(&|fold| matches!(fold, Fold::Band(_)));
        assert_eq!(shown_and_said(&bands_folded), all, "bands folded");
        let everything_folded = order(&|_| true);
        assert!(
            everything_folded
                .iter()
                .any(|entry| matches!(entry, SidebarEntry::Folded(..))),
            "some families fold"
        );
        assert_eq!(
            shown_and_said(&everything_folded),
            all,
            "bands and families folded"
        );
    }

    #[test]
    fn the_next_match_comes_after_the_current_row_and_wraps() {
        let [a, b, c, d] = [(); 4].map(|_| Uuid::new_v4());
        let order = [a, b, c, d];
        let even = |id: Uuid| id == b || id == d;
        assert_eq!(next_matching(&order, None, &even), Some(b));
        assert_eq!(next_matching(&order, Some(b), &even), Some(d));
        assert_eq!(next_matching(&order, Some(c), &even), Some(d));
        assert_eq!(next_matching(&order, Some(d), &even), Some(b), "wraps");
        assert_eq!(
            next_matching(&order, Some(Uuid::new_v4()), &even),
            Some(b),
            "an unlisted current row starts at the top"
        );
        assert_eq!(
            next_matching(&order, Some(a), &|id| id == a),
            Some(a),
            "a lone match is found again"
        );
        assert_eq!(next_matching(&order, Some(a), &|_| false), None);
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
            vec![asking, working, read, unpinned]
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
    fn working_rows_sit_together_in_start_order_above_every_finish() {
        // Chris's screenshot: three working sessions, two read finishes from
        // 11 h ago, and a session that last finished 13 h ago, yesterday, and
        // has been working for 2 h.
        let (mut project, mut workspace, ids) = fixture(vec![
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
        use SidebarEntry::{Band, Pane, Working};
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Working(4),
                Pane(ids[2]),
                Pane(ids[1]),
                Pane(ids[5]),
                Pane(ids[0]),
                Band(ActivityBand::Today),
                Pane(ids[3]),
                Pane(ids[4]),
            ]
        );
        // A session that finished 20 minutes ago, after session 3's turn
        // began, sits directly under the working rows: a finish is never
        // above work, however long that work has run.
        let fresh = session(
            &mut workspace,
            &mut project,
            None,
            worked(NOW - 20 * MINUTE),
        );
        assert_eq!(
            panes(recent(&project, &workspace)),
            vec![ids[2], ids[1], ids[5], ids[0], fresh, ids[3], ids[4]]
        );
    }

    #[test]
    fn a_working_row_leads_however_old_its_last_finish() {
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
        use SidebarEntry::{Band, Pane, Working};
        assert_eq!(
            recent(&project, &workspace),
            vec![
                Working(1),
                Pane(ids[1]),
                Band(ActivityBand::Today),
                Pane(ids[0]),
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
            RecentPlace::of(instance, Some(AgentActivityState::Idle), true, clock())
        };
        assert_eq!(place(&instance).band(), ActivityBand::ThisWeek);
        instance.activity.work_started_at = Some(NOW - HOUR);
        assert_eq!(place(&instance).band(), ActivityBand::Today);
        assert_eq!(place(&instance).group(), RecentGroup::Done);
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
                &|_| true,
                &|_| false,
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
