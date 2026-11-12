# Sleeping idle agents

An idle agent's program keeps its memory for as long as the pane is open. A
day's worth of finished panes can hold tens of gigabytes. Sleeping a pane ends
its program and leaves the pane in place as a [dormant](dormant-restore.md)
placeholder: same name, saved status, age and last title. Opening it resumes
the same conversation through the ordinary restore path.

## Ways in

- **By hand:** a session's sidebar menu has **Sleep**. When the pane can't
  sleep, the item is disabled and says why.
- **A project at once:** the project menu has **Sleep idle agents**, which
  sleeps every pane in the project the rules below allow and reports how many.
- **Automatically:** Settings → Behavior → "Put agents to sleep after N hours
  idle". It is on by default at 4 hours; the hand-editable `config.toml` key is
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
idle that long opens dormant instead of starting only to be put to sleep a
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
names it, so it sleeps the pane you're in. Focus then moves the way a close
moves it, to the tab its group shows next or another running pane of the
project (focusing a dormant pane starts it, so focus doesn't stay). Sleep idle
agents and the timer name no pane and leave the one you're in. Only automatic
sleep waits for the idle threshold.

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
"no saved conversation to resume". The look never walks a whole store, so it
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

A slept pane stays dormant across restarts until it is opened or woken, even
when neither threshold applies. Any launch that lands for the pane clears the
mark. Opening it during the quit grace ends the old process first, so two
copies never run on one conversation.

Every wake (opening the pane, pinning it, or `ctl wake`) records
`activity.woken_at`, and the idle timer counts from it. A woken pane therefore
runs at least N hours before the timer can sleep it again, even if nobody
types into it, and a restart in that time starts it rather than leaving it
dormant.

## How an asleep pane looks

A pane put to sleep and one [deferred at launch](dormant-restore.md) look the
same, because opening either does the same thing. Outside control still tells
them apart (`not_running_reason`).

- **Sidebar row:** one line at any sidebar width, like a running row. The name
  is muted unless the pane holds an unread finish. The pill is outlined rather
  than filled, so filled pills mean running panes: a moon and how long ago the
  pane last worked (`5m`, `20h`, `31d`), in its saved status color (green
  finished, amber interrupted, grey idle). Hovering the pill gives the words:
  "Asleep · finished 20h ago".
- **Tab:** a moon and the muted title, no age.
- **Pane:** its name, last title, the same words, folder and a Start button.

When the pane's saved conversation is gone (checked on load, project open,
sleep, wake and once a minute, the same look as the sleep rule), the pill adds
`∅`, the tooltip and pane say "The saved conversation is gone. Waking starts a
new one in the same folder.", and the button reads **Start fresh**.

## Outside control

`muxel ctl status AGENT` and `panes` report `status: "not_running"` with
`not_running_reason: "sleeping"` for a pane put to sleep (`"dormant"` remains
the reason for a pane deferred at launch). `show` includes the reason; `screen`,
`send`, `keys` and `answer` refuse with code `not_running` and that reason.

For a sleeping or dormant agent, `status` and `panes` also report `resumes`,
made from the same look as the sleep rule each time they are asked: `true`
when its file is there, `false` when it is gone or no conversation was ever
captured. It is left out while the pane runs, and when muxel can't tell (Grok,
or a look that failed).

`muxel ctl wake AGENT` starts a sleeping or dormant pane in the background,
through the same restore path, without selecting it, raising a window,
changing the active tab or marking its notifications read. Replies:

- `{"woken": true, "agent": ID, "uuid": UUID, "status": "starting"}` when it
  started; `status` then moves from `starting` to idle with a new `launch_id`.
- `{"woken": false, "already_running": true, "status": STATE, "agent": ID,
  "uuid": UUID}` when it already runs or starts, so a retry is harmless.
- Refusals: `not_running` (with reason `stopped` for a stopped command,
  `failed` for a failed launch, none for a pane not started this session),
  `exited`, `closing`, and `control_disabled`.

Waking isn't attendance: it marks nothing read. Like any wake, it restarts the
idle clock, which leaves the caller N hours to deliver what it woke the pane
for. A wake never mints a new conversation id for a conversation that still
exists.

A pane can sleep for longer than its provider keeps the file, so a wake can
find it gone. Before every resume, muxel's launch path looks for the file
(walking the provider's store when the cheap look misses); when the file is
gone, the launch starts a new conversation in the same folder instead of
resuming, and `session_id` changes. muxel then posts "NAME: saved conversation
is gone · Started a new one in the same folder" to Notifications and the event
log. The same notice follows the rarer case where the resume itself reports the
conversation missing. The new launch is not a resume, so it can't fail the same
way again: one wake, one launch.

Sleep is for local PTY panes only. SSH and tmux sessions, editors, diffs and
browsers are unchanged.
