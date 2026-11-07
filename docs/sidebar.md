# Sidebar order and unread sessions

Settings → Appearance → **Sidebar order** defaults to **Layout**, preserving
worktree grouping and the order of panes on screen. **Recent work** lists each
project's sessions by what each needs from you and when it last worked,
independently of the pane layout.
Projects retain their saved order. Focus Next and Focus Previous still follow
the layout.

The saved `config.toml` setting is `sidebar_sort = "Layout"` or
`sidebar_sort = "RecentWork"`. Older configurations keep Layout.

Pinned sessions form a block at the top of their project, newest first. The
remaining sessions appear under Today (less than 24 hours), This week (less
than seven days), and Older. A block spanning only one band needs no band
label. Within each band, `sidebar::RecentPlace` puts sessions in three tiers:

1. **Needs you**: an unread completion or question, or a session asking for
   something now. Newest event first. An agent that finishes and starts again by
   itself stays here until someone looks at it.
2. **Working**, and seen: in the order each started its turn, newest start at
   the bottom. A session that goes working joins the bottom of this tier; with
   nothing else working, that is right under the sessions that need you.
3. **The rest**: read completions, idle sessions and sessions that never
   worked. Newest first by the later of `completed_at` and `blocked_at`
   (`sidebar::recent_work_at`), or by the latest `work_started_at` until a
   session has done either. Sessions with no recorded work come last.

A working session belongs to Today however long ago it last finished. Tiers go
by the status the row shows, so a session with no live terminal (in a project
not opened since a restart, say) shows idle and sorts as idle, whatever state it
was saved in. Timestamps are Unix milliseconds; future ones count as now. Ties
keep layout order.

Looking at a session doesn't move its row while it has focus. When focus first
attends a session, the app takes a `sidebar::FocusHold` of its place, and the
row keeps that place until focus moves to another pane, or for a popped-out
session until its window loses focus. An unread row you click stays with the
sessions that need you until you look elsewhere. Starting a turn is the
exception: the row moves to the bottom of the working sessions and stays with
them until focus leaves, even if the turn has finished by then. Row ages and
`muxel ctl` still use `last_work_at`, the latest activity of any kind.
Recent work is flat within the project so a worktree cannot hold an older
session above newer work. Worktree color dots remain; detached worktrees retain
their action subheaders. Row-to-row swaps are available in Layout; dragging a
session onto a pane remains available in either order.

Recent work includes live popped-out sessions even though they are outside the
pane layout. Their rows carry an external-window icon; clicking raises that
window, and attending it clears unread through the normal window attendance
path. These rows contribute to the project count and cannot be dragged into
the layout. Layout keeps its existing docked-pane list.

In Recent work, a completion or blocked event newer than `last_attended_at`
shows a dot and bold session label. Missing attendance means the event is
unread. The mark survives subsequent state changes and clears only when the
session is attended. Done and its age remain after attendance. Project headers
show their unread count whether expanded or collapsed.

Agent events are omitted from the Recent work notification feed because the
session itself moves up and carries unread. App messages such as git results,
SSH errors and save failures remain in NOTIFICATIONS. This is a presentation
filter over the notification collection; tray and desktop delivery are
unchanged. Switching back to Layout reveals the ordinary agent feed rows.
