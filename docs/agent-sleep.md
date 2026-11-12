# Sleeping idle agents

An idle agent's program keeps its memory for as long as the pane is open. A
day's worth of finished panes can hold tens of gigabytes. Sleeping a pane ends
its program and leaves the pane in place asleep, as
[dormant restore](dormant-restore.md) leaves an old one: same name, saved
status, age and last title. Selecting it shows
it; waking it resumes the same conversation through the ordinary restore path.

## Ways in

- **By hand:** a session's sidebar menu has **Sleep**. When the pane can't
  sleep, the item is disabled and says why.
- **A project at once:** the project menu has **Sleep idle agents**, which
  sleeps every pane in the project the rules below allow and reports how many.
- **Automatically:** Settings → Behavior → "Sleep agents idle for N hours". It
  is on by default at 4 hours; the hand-editable `config.toml` key is
  `sleep_after_hours = 4`, and zero turns it off. Once a minute, muxel sleeps
  each agent idle that long. Idle is measured from the latest of the pane's
  work start, completion, question (`activity.work_started_at`,
  `completed_at`, `blocked_at`), the last time the user opened it
  (`last_attended_at`), and the last time it was woken (`woken_at`, see
  below). A pane with no work or attendance recorded is never slept
  automatically.

Four hours keeps everything touched this morning running through the
afternoon, while the panes left from yesterday stop holding memory before the
next day's work starts. The same threshold applies at launch: an agent already
idle that long opens asleep instead of starting only to be put to sleep a
minute later.

## What never sleeps

- a pane that is working, asking a question, or held open for background work
  (subagents or background shells its provider reports, or a loop run muxel
  is still watching);
- a pane with text typed into its input and not yet submitted;
- the pane you're in (the focused pane, or the one a project window shows),
  except by its own Sleep, and a popped-out pane;
- a pane whose resume can't be trusted: it hasn't launched, its conversation
  id hasn't been captured, its preset doesn't resume by id, or the file a
  resume would read isn't there (see below);
- a shell or any program that isn't one of muxel's known agents;
- a remote (SSH) pane or a local tmux pane;
- a pinned pane. Pinning already means "always start" for dormant restore,
  and pinning a dormant pane starts it, so a pin keeps a pane running in every
  path.

These rules apply to every way in, with one exception: a pane's own Sleep
names it, so it sleeps the pane you're in. You stay in it and see its asleep
face, since selecting never wakes a pane; Enter, typing or Wake brings it back.
Sleep idle agents and the timer name no pane and leave the one you're in. Only
automatic sleep waits for the idle threshold.

A saved conversation id is only as good as the file behind it: Claude deletes
transcripts older than `cleanupPeriodDays` (30 by default). So the last rule,
checked only once every other rule lets the pane sleep, looks for the file a
resume would read:

- **Claude:** the saved transcript path, or `~/.claude/projects/<slug>/<id>.jsonl`
  for the pane's folder when none was saved. One stat.
- **Codex:** the saved rollout path and its compression sibling, or the rollout
  in the dated directories its id names when no path was saved.
- **Muse Code:** the session's `session.jsonl` in its dated store.
- **Grok:** not checked. Grok files a session under its own encoding of the
  working directory, which muxel doesn't reproduce, so its saved id is trusted
  as before.

A pane sleeps only when its file is there. A missing file, or a look that
fails or would need a walk of the provider's whole store, keeps it awake with
"Can't sleep: no history". The look never walks a whole store, so it
doesn't stall the UI.

## How a sleep ends the program

The pane turns dormant and its saved state records `asleep_since` (Unix
milliseconds) at once, so nothing can type into it while it stops. muxel then
presses Ctrl+C twice, 300 ms apart, which quits an idle Claude or Codex the
way a user would, saving its conversation. If the program is still running
five seconds later, whatever the provider, muxel ends it by its exact PID, and
closing its pseudo console ends what it left running, as closing a pane does. The event log
records each sleep with the pane's preset, and whether the program quit or was
killed.

A slept pane stays dormant across restarts until it is woken, even when
neither threshold applies, and even when it is the pane you were in. Any launch
that lands for the pane clears the mark. Waking it during the quit grace ends
the old process first, so two copies never run on one conversation.

Every wake (see [Waking an asleep pane](#waking-an-asleep-pane)) records
`activity.woken_at` and who woke it (`activity.woken_by`), and the idle timer
counts from it. A wake by the user's own input in muxel (Enter, typing, Wake,
pinning) gets the whole threshold: the pane runs at least N hours before the timer can
sleep it again, even if nobody types into it, and a restart in that time
starts it rather than leaving it asleep.

Anything through ctl (`ctl wake`, `ctl focus`) is a machine's wake, for one
job, so until someone looks at the pane it sleeps after 30 idle minutes (or N hours, if shorter), counted from the
later of the wake and its last work. Holding an agent's memory for four hours
after every scripted wake would cost hours of it each time. The first time the
user focuses the live pane, it becomes an ordinary pane on the whole
threshold. A look before the wake doesn't count, and a restart applies the
same limit. Thirty minutes is a guess: each wake writes
`wake: "NAME" [ID] by CAUSE (user|machine)` to the event log, which shows how
often a second job arrives after the short limit.

## How an asleep pane looks

A pane put to sleep and one [deferred at launch](dormant-restore.md) look the
same, because waking either does the same thing. Outside control still tells
them apart (`not_running_reason`).

- **Sidebar row:** one line at any sidebar width, like a running row. The name
  is muted unless the pane holds an unread finish, and the agent icon takes the
  saved status color dimmed the way a read finish is (bright only for an unread
  finish), so nothing asleep reads as running. The pill is outlined rather
  than filled, so filled pills mean running panes: a moon and how long ago the
  pane last worked (`5m`, `20h`, `31d`), in its saved status color (green
  finished, amber interrupted, grey idle). Hovering the pill gives the words:
  "Asleep · 20h". Hovering the row turns the pill into a Wake button in the
  same place.
- **Tab:** a moon and the muted title, no age.
- **Pane:** its name, last title, the same words, folder and a **Wake** button,
  whose tooltip says Enter.

The words are spent on exceptions. A pane that will resume its conversation
says nothing about it; an interrupted one adds "interrupted" ("Asleep · 6h ·
interrupted"). When the pane's saved conversation is gone (checked on load,
project open, sleep, wake and once a minute, the same look as the sleep rule),
the pill adds `∅`, the words add "no history" ("Asleep · 31d · no history"),
and the button reads **Start fresh**.

## Waking an asleep pane

Selecting an asleep pane (its sidebar row, its tab, tab cycling, a
notification, the tray, opening its project, or muxel restarting with it
active) shows its face and starts nothing. Each start costs an agent's memory
(about 659 MiB for an idle Claude) and keeps it for at least the sleep
threshold, so clicking through rows to find a session must not start each one.

A deliberate act wakes it:

- Enter, or a key that types a character, while the pane is selected. The key
  itself is dropped: nothing may reach a booting program, which could be
  showing a trust card or an update prompt. Shortcuts (Ctrl, Alt, the platform
  key), arrows, Tab and Escape leave it asleep.
- the pane's **Wake** (or **Start fresh**) button, the Wake button its row's
  pill turns into on hover, or **Wake** in the row menu. These also select it.
- pinning it.
- `ctl focus`, which opens the pane on purpose, and `ctl wake`, which starts it
  in the background.

While the program starts, the pane keeps its face, saying "Waking…", until
the terminal draws its first frame, in place of the startup spinner.

## Outside control

`muxel ctl status AGENT` and `panes` report `status: "not_running"` with
`not_running_reason: "sleeping"` for a pane put to sleep (`"dormant"` remains
the reason for a pane deferred at launch). `show` includes the reason; `screen`,
`send`, `keys` and `answer` refuse with code `not_running` and that reason.

For an asleep agent, `status` and `panes` also report `resumes`,
made from the same look as the sleep rule each time they are asked: `true`
when its file is there, `false` when it is gone or no conversation was ever
captured. It is left out while the pane runs, and when muxel can't tell (Grok,
or a look that failed).

`muxel ctl wake AGENT` wakes an asleep pane in the background,
through the same restore path, without selecting it, raising a window,
changing the active tab or marking its notifications read. Replies:

- `{"woken": true, "agent": ID, "uuid": UUID, "status": "starting"}` when it
  started; `status` then moves from `starting` to idle with a new `launch_id`.
- `{"woken": false, "already_running": true, "status": STATE, "agent": ID,
  "uuid": UUID}` when it already runs or starts, so a retry is harmless.
- Refusals: `not_running` (with reason `stopped` for a stopped command,
  `failed` for a failed launch, none for a pane not started this session),
  `exited`, `closing`, and `control_disabled`.

Waking isn't attendance: it marks nothing read. It is a machine's wake, so it
leaves the caller 30 idle minutes, counted again from each piece of work, to
deliver what it woke the pane for, unless the user looks at the pane in that
time. A wake never mints a new conversation id for a conversation that still
exists.

A pane can sleep for longer than its provider keeps the file, so a wake can
find it gone. Before every resume, muxel's launch path looks for the file
(walking the provider's store when the cheap look misses); when the file is
gone, the launch starts a new conversation in the same folder instead of
resuming, and `session_id` changes. muxel then posts "NAME: started fresh · no
history" to Notifications and the event log. The same notice follows the rarer case where the resume itself reports the
conversation missing. The new launch is not a resume, so it can't fail the same
way again: one wake, one launch.

Sleep is for local PTY panes only. SSH and tmux sessions, editors, diffs and
browsers are unchanged.
