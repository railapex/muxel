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
the unread mark and the Done pill. A row moves only when work in its project
changes (a turn starts, finishes or stops, including when its process exits; a
question is asked or answered) or when the clock crosses into another band.
Mostly that is the row's own work; another session's turn can move a finished
row across the working rows, as below. A file, diff or page also moves when a
session opens it, as described below.

Bands are calendar days on this machine's clock: Today since local midnight,
This week the six days before that, Older the rest. A row's band comes from its
latest event of any kind, a start, a finish or a question
(`AgentActivity::last_work_at`), so a row that stops working stays in the band
its turn started in. One exception: a working row is Today however long its
turn has run, so a turn started yesterday and still running sits with today's
work, not in the band of its start. A row that has never worked goes by when
it was opened (`created_at` on its instance); a pane saved before muxel
recorded that sits with Older. The sidebar
redraws at midnight, when rows change band. A block spanning only one band
needs no band label. Within each band, `sidebar::RecentPlace` puts rows in five
groups:

1. **Asking**: sessions showing a question now. Newest question first, whether
   or not you have seen it.
2. **Newly done**: sessions that have worked and aren't running a turn
   (finished, idle after work, interrupted, exited, or not running since a
   restart) whose latest event is at or after the start of the oldest turn
   still running in the project. Read or not. Newest event first.
3. **Working**: sessions running a turn, in the order each started it, so a
   session that starts a turn joins the bottom.
4. **Old done**: sessions that have worked and aren't running a turn whose
   latest event came before that oldest running turn began. Newest event
   first.
5. **Never used**: newest open first.

A finish older than every turn still running is older news than that work, so
it goes below it. With nothing running, every finished row is newly done, and
the order is newest event first. While anything runs, the oldest start only
moves forward (a turn starts now; a question takes its turn out of the running
until it is answered, which starts a new one), so a row that dropped comes back
up once the work it fell behind ends. That is a change of work, like any other
move. The oldest start comes from each row's recorded state
(`AgentActivity::current_state`, `work_started_at`), not the status it shows,
so a status the reducer hasn't recorded, such as a flicker or a turn the tick
hasn't sampled yet, drops nothing. It covers the project's rows, the pinned
block included.

Pinned sessions form a block at the top of their project, ordered by the same
rule, without band labels.

Groups go by the status the row shows. A dormant session
([dormant-restore.md](dormant-restore.md)) shows the state it was saved in and
sorts by it. Any other session with no live terminal (in a project not opened
since a restart, say) shows idle, or Done if its last turn finished, and sorts
with the done rows by its latest event. Timestamps are
Unix milliseconds; future ones count as now. Ties keep the order the panes were
opened in, so popping a row out into its own window or docking it back never
moves it.

An event is stamped when it happened, not when muxel noticed it. The terminal
reads the wall clock as each event happens (its status changing, its provider
reporting a completion, its provider switching conversations) and keeps that
time, so a second turn that ends at the same Done badge gets its own time. The
app records those times however late it reads them, as for a pane held for
Undo. Two kinds of time are bounds, not readings: a status the last sample
couldn't show (held back while a restored terminal settled, or before any turn
was submitted) is stamped no earlier than that sample, and a new conversation's
first status no earlier than the switch. A row sits where the work its terminal
shows puts it before the status tick has recorded that work, and focusing the
pane records exactly that, so clicking a row that has just finished moves
nothing. The tick then samples that same work again; what the focus read (the
conversation switch, the completion by its revision) isn't recorded twice or
notified, so the finish the click read stays read. That holds when the click
left the stored values as they were, too (a completion the floor dates to the
stored time after a clock correction). Auto-read and the accomplishment log
still take it. A click while the restore guard holds a restored pane's sample
records nothing, and leaves the tick to record that sample as it would without
the click.

A clock correction rewrites nothing already recorded. A time the clock reads as
in the future after it steps back is kept as stamped; only ages and the order
count it as now. Looking at a pane covers every event recorded so far, whatever
the clock reads, and never moves back, and an event after that look is stamped
after it even when the clock has stepped back behind it, so it stays unread.
Times are still wall-clock times: across a clock step, two panes' events order
by what the clock read, not by which came first.

A process exiting is a finish only for a pane started to run a command
(`muxel_core::LaunchPurpose::Job`): a literal command, a custom command preset,
or an agent's one-shot run (`claude -p`, `codex exec`). Its exit marks it
unread, notifies Done, and moves the row to the top of the done rows like any
finish. A non-zero exit counts too. Its notification gives the code ("exited
with code 3"), since a code isn't always a failure (`grep` exits 1 when nothing
matches), in place of the unexpected-exit report, and the row shows Done, as
there is no failed badge. That notification is the job's first exit's, whatever
the pane notified before and whether or not you're watching it: a job that
announced a finish and then exited has that notice replaced.

Only the command's own exit counts. Behind tmux the pane's process is a client
whose exit a lost server or connection, a detach and the session ending all
cause alike, so a job there finishes only when its provider reports it. ssh
alone exits with the command's code, or 255 for its own failures, which count
as no exit. ssh run through `sshpass` exits 0 only when the command did, which
finishes the job; any other code may be sshpass's own (1 to 6) or ssh's, so it
counts as no exit, and a failing job there finishes only by its provider.

An interactive session (an agent conversation or a shell) finishes turns while
it runs, so its exit (`/exit`, a kill, a crash, `exit` typed in a shell) records
no finish, marks nothing unread and doesn't notify: a working row stops and
joins the done rows by when its turn started. A provider that reported its own
finish before exiting is finished all the same, notifies, and keeps its chime
through the exit until you look at the pane. A finish that only a bell or the
title showed is lost if the process exits before the status tick reads it.

The purpose comes from the program and arguments the pane runs, so a pane
saved before this rule gets the same answer as a new one. The provider comes
from the program (a built-in agent preset's: `claude`, `codex`, `grok`, `muse`,
`opencode`, `amp`, `pi`, `ollama`, `hermes`) and whether it runs one-shot from
its arguments, so a literal `claude` is still a session. A one-shot subcommand
is the agent's first operand (`codex exec`, `codex review`, `opencode run`), so
a prompt after another subcommand (`codex resume <id> e`) isn't one. A shell is
a session unless it was started to run a command or a script and exit: `-c`,
`cmd /c`, or a script's path (`bash build.sh`). A shell's options end where its
command begins, so `cmd /k` keeps the prompt only before the command; after it
it is the command's. PowerShell (`pwsh`, `powershell.exe`) is always a session,
as on `master`, so a pane running `pwsh -Command make` doesn't finish when it
exits. Its two hosts take different options, prefixes and values, and `-File -`
reads commands from the terminal; reading those lines kept finding cases a
session was read as a job, and a session read as a job finishes falsely when you
quit it. fish, nu, xonsh and csh are read by their
command flag alone, so a script they run by path stays a session. Any other
program, an agent muxel doesn't know included, is a command, and so is a
wrapper (`npx`, `uvx`, `wsl`, `ssh host`): the program it starts isn't looked
at. That has a known limit: quitting an interactive program started as a
literal command or a custom preset (`lazygit`, `vim`, `htop`, a `python` or
`node` REPL, an agent behind `npx` or `wsl`, `ssh host`) counts as its finish,
with an unread mark and a Done notification, as every exit did before this
rule.

Some other states are not work either. A relaunched agent reporting Working
before any turn is submitted, and startup output while a restored terminal
settles, record nothing. Row ages and `muxel ctl` use `last_work_at` too.

Read state stays on each machine, as the rest of a pane's activity does. Since
reading never moves a row, two machines that saw the same work and keep the
same calendar order it the same way; only their unread marks differ. Machines
in different time zones can put one event in different bands, and so order
rows differently.

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

A session's finishes and questions are omitted from the Recent work
notification feed because the session's own row carries the event and its
unread mark. What a row can't show stays: a job's "exited with code N", whose
row shows Done. App messages such as git results, SSH errors and save failures
remain in NOTIFICATIONS. This is a presentation
filter over the notification collection; tray and desktop delivery are
unchanged. Switching back to Layout reveals the ordinary agent feed rows.
