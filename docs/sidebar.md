# Sidebar order and unread sessions

Settings → Appearance → **Sidebar order** defaults to **Layout**, preserving
worktree grouping and the order of panes on screen. **Recent work** lists each
project's sessions by their most recent work, independently of the pane layout.
Projects retain their saved order. Focus Next and Focus Previous still follow
the layout.

The saved `config.toml` setting is `sidebar_sort = "Layout"` or
`sidebar_sort = "RecentWork"`. Older configurations keep Layout.

Last work is the latest of the persisted `work_started_at`, `completed_at`, and
`blocked_at` timestamps, in Unix milliseconds. `AgentActivity::last_work_at` owns
that decision. Looking at a pane updates only attendance; it cannot refresh its
work age. Future work timestamps count as now for ordering and bands.

Pinned sessions form a block at the top of their project, newest first. The
remaining sessions appear newest first under Today (less than 24 hours), This
week (less than seven days), and Older. A block spanning only one band needs no
band label. Sessions with no recorded work follow in layout order, as do ties.
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
