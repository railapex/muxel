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
turn starts or finishes, a question is asked or answered) or when the clock
crosses into another band. A file, diff or page also moves when a session opens
it, as described below.

Bands are calendar days on this machine's clock: Today since local midnight,
This week the six days before that, Older the rest. A row's band comes from its
latest event of any kind, a start, a finish or a question
(`AgentActivity::last_work_at`), so a row that stops working stays in the band
its turn started in. A working row is Today however long its turn has run. A
row that has never worked goes by when it was opened (`created_at` on its
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
Unix milliseconds; future ones count as now. Ties keep layout order.

An event is stamped when it happened, not when muxel noticed it. The terminal
records when its status changed, and the app records that time even when it
reads the status late: a pane held for Undo, or a finish the status tick hadn't
recorded when you clicked the row. Three things are not work and never move a
row: a process exiting (the row keeps the state it had, so a turn cut short
reads as idle and nothing is marked unread), a relaunched agent reporting
Working before any turn is submitted, and startup output while a restored
terminal settles. Row ages and `muxel ctl` use `last_work_at` too.

Read state stays on each machine, as the rest of a pane's activity does. Since
reading never moves a row, two machines that saw the same work order it the
same way; only their unread marks differ.

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
is in another project, the resource is an ordinary row. It has no work of its
own, so it sits with the never-used rows by the later of when it was opened and
when a session last opened it. A pinned resource
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
