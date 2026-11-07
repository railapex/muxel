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

In Recent work, a file, diff or web page that a session opened (a link
clicked in its terminal, its diff button) shows directly under that session's
row, indented, and moves with it. A session's resources go newest open first.
Opening a resource that is already open counts as opening it again: the last
session to open it owns it, so reopening a file from a second session moves it
there. An open no session asked for (the file browser, the palette, the
sidebar) leaves the owner as it was. A page opened from a file, such as an HTML
preview or a link in the file, belongs to the file's session, so nesting is one
level deep. When the owning session has closed, is counting down to close, or
is in another project, the resource is an ordinary row. A pinned resource
nests only under a pinned session, so its pin keeps it in the pinned block.
Layout order ignores owners, and tab strips don't change.

The owner is `opened_by` (the session's instance id) and `opened_at` (Unix
milliseconds) on the resource's instance in this machine's `workspace.json`.
`sidebar::nests_under` owns the nesting decision. The shared layout document
leaves both fields out and they never count as a layout change, so other
machines and the iOS app don't see them, the same as a pane's activity.

Closing a session yourself in Recent work (its tab, its pane, Close
terminals, or a worktree's disposal choice) closes the resources in its
project that it owns at that moment, chosen by `sidebar::closes_with`. They
join the session's recoverable close as followers: they have no close evidence
of their own, finish when the session's close does, and return with it on Undo
or Keep. A session that closes at once, such as one whose process has exited,
takes them at once too. An unsaved editor gets a Save / Don't Save / Cancel
question before anything closes, naming the file and the session taking it,
since the user clicked the session rather than the file. Cancel keeps that
editor, clears its owner so it shows as an ordinary row, and lets the session
close. The same close request goes on, so a worktree answer already given for
the session still stands. Close other tabs and Close tabs to the side keep the tab they were
chosen from: if its owner is among the tabs closing, the owner is cleared
first. Popped-out resources stay open, since their views belong to their own
windows. An agent exiting on its own, Layout order, and closes from other
machines leave resources alone.

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

`sidebar::is_unread` is the one read rule. The mark, the tiers above, and the
outline pill of a read Done all use it: the pill's test,
`AgentActivity::has_unattended_completion`, is `is_unread` restricted to a Done
session, so a Done row's pill and mark cannot disagree. The pill can only say
read or unread while the row shows Done. The mark also flags a finish whose
agent has started again, and tells an unseen question from a seen one, which
the blocked pill shows alike.

Agent events are omitted from the Recent work notification feed because the
session itself moves up and carries unread. App messages such as git results,
SSH errors and save failures remain in NOTIFICATIONS. This is a presentation
filter over the notification collection; tray and desktop delivery are
unchanged. Switching back to Layout reveals the ordinary agent feed rows.
