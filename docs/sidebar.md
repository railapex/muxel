# Sidebar order and unread sessions

Settings → Appearance → **Sidebar order** defaults to **Layout**, preserving
worktree grouping and the order of panes on screen. **Recent work** lists each
project's sessions by when each last finished or needed attention, independently
of the pane layout.
Projects retain their saved order. Focus Next and Focus Previous still follow
the layout.

The saved `config.toml` setting is `sidebar_sort = "Layout"` or
`sidebar_sort = "RecentWork"`. Older configurations keep Layout.

A session's place is the later of its persisted `completed_at` and `blocked_at`
timestamps, in Unix milliseconds. `sidebar::recent_work_at` owns that decision.
Starting a turn doesn't move a session, so typing in one leaves the list still;
it moves when that turn finishes or asks for something. Until a session has done
either, it goes by its latest `work_started_at`. Looking at a pane updates only
attendance and can't move it. Future timestamps count as now for ordering and
bands.

Pinned sessions form a block at the top of their project, newest first. The
remaining sessions appear newest first under Today (less than 24 hours), This
week (less than seven days), and Older. A block spanning only one band needs no
band label. A working session stays in the band of its last finish or question
until the turn ends. Sessions with no recorded work follow in layout order, as
do ties.
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
is in another project, the resource is an ordinary row. Layout order ignores
owners, and tab strips don't change.

The owner is `opened_by` (the session's instance id) and `opened_at` (Unix
milliseconds) on the resource's instance in this machine's `workspace.json`.
`sidebar::nests_under` owns the nesting decision. The shared layout document
leaves both fields out and they never count as a layout change, so other
machines and the iOS app don't see them, the same as a pane's activity.

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
