# Sidebar order and unread sessions

Settings → Appearance → **Sidebar order** defaults to **Layout**, preserving
worktree grouping and the order of panes on screen. **Recent work** lists each
project's sessions by what each needs from you and when it last worked,
independently of the pane layout.
Projects retain their saved order. Focus Next and Focus Previous still follow
the layout.

The saved `config.toml` setting is `sidebar_sort = "Layout"` or
`sidebar_sort = "RecentWork"`. Older configurations keep Layout.

Recent work follows two rules. Looking at a session never moves its row:
reading, focusing and clicking change no position, and read state drives only
the unread mark and the Done pill. A row moves only when its work changes (a
turn starts, finishes or stops, including when its process exits; a question is
asked or answered) or when the clock crosses into another band.

Bands are calendar days on this machine's clock: Today since local midnight,
This week the six days before that, Older the rest. A row's band comes from its
latest event of any kind, a start, a finish or a question
(`AgentActivity::last_work_at`), so a row that stops working stays in the band
its turn started in. One exception: a working row is Today however long its
turn has run, so a turn started yesterday and still running sits with today's
work, not in the band of its start. A row that has never worked goes by when it was opened (`created_at` on its
instance); a pane saved before muxel recorded that sits with Older. The sidebar
redraws at midnight, when rows change band. A block spanning only one band
needs no band label. Within each band, `sidebar::RecentPlace` puts rows in four
groups:

1. **Asking**: sessions showing a question now. Newest question first, whether
   or not you have seen it.
2. **Recent**: sessions that have worked and aren't running a turn: finished,
   idle after work, interrupted, exited, or not running since a restart. Read
   or not. Newest event first.
3. **Working**: sessions running a turn, in the order each started it, so a
   session that starts a turn joins the bottom.
4. **Never used**: newest open first.

Pinned sessions form a block at the top of their project, ordered by the same
rule, without band labels.

Groups go by the status the row shows. A session with no live terminal (in a
project not opened since a restart, say) shows idle, or Done if its last turn
finished, and sorts with the recent rows by its latest event. Timestamps are
Unix milliseconds; future ones count as now. Ties keep the order the panes were
opened in, so popping a row out into its own window or docking it back never
moves it.

An event is stamped when it happened, not when muxel noticed it. The terminal
reads the wall clock as each event happens (its status changing, its provider
reporting a completion, its provider switching conversations) and keeps that
time, so a later clock correction moves nothing, and a second turn that ends at
the same Done badge gets its own time. The app records those times however late
it reads them, as for a pane held for Undo. A row sits where the work its
terminal shows puts it before the status tick has recorded that work, and
focusing the pane records exactly that, so clicking a row that has just
finished moves nothing.

Some states are not work. A process exiting records no finish, marks nothing
unread and doesn't notify: a working row stops and joins the recent rows by
when its turn started, so a turn cut short reads as idle. A provider that
reported its own finish before exiting is finished all the same, and notifies.
A relaunched agent reporting Working before any turn is submitted, and startup
output while a restored terminal settles, record nothing. Row ages and
`muxel ctl` use `last_work_at` too.

Read state stays on each machine, as the rest of a pane's activity does. Since
reading never moves a row, two machines that saw the same work and keep the
same calendar order it the same way; only their unread marks differ. Machines
in different time zones can put one event in different bands, and so order
rows differently.

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

`sidebar::is_unread` is the one read rule. The mark and the outline pill of a
read Done both use it, and Recent work's order never does: the pill's test,
`AgentActivity::has_unattended_completion`, is `is_unread` restricted to a Done
session, so a Done row's pill and mark cannot disagree. The pill can only say
read or unread while the row shows Done. The mark also flags a finish whose
agent has started again, and tells an unseen question from a seen one, which
the blocked pill shows alike.

Agent events are omitted from the Recent work notification feed because the
session's own row carries the event and its unread mark. App messages such as git results,
SSH errors and save failures remain in NOTIFICATIONS. This is a presentation
filter over the notification collection; tray and desktop delivery are
unchanged. Switching back to Layout reveals the ordinary agent feed rows.
