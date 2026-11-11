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
  (`last_attended_at`), and the last `ctl wake` (see below). A pane with no
  work or attendance recorded is never slept automatically.

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
- the focused pane, a popped-out pane, and the pane a project window shows;
- a pane whose resume can't be trusted: it hasn't launched, its conversation
  id hasn't been captured, or its preset doesn't resume by id;
- a shell or any program that isn't one of muxel's known agents;
- a remote (SSH) pane or a local tmux pane;
- a pinned pane. Pinning already means "always start" for dormant restore,
  and pinning a dormant pane starts it, so a pin keeps a pane running in every
  path.

These rules apply to every way in; only automatic sleep also waits for the
idle threshold.

## How a sleep ends the program

The pane turns dormant and its saved state records `asleep_since` (Unix
milliseconds) at once, so nothing can type into it while it stops. muxel then
presses Ctrl+C twice, 300 ms apart, which quits an idle Claude or Codex the
way a user would, saving its conversation. If the program is still running
five seconds later, whatever the provider, muxel ends it by its exact PID, and
closing its pseudo console ends what it left running, as closing a pane does. The event log
records each sleep and whether the program quit or was killed.

A slept pane stays dormant across restarts until it is opened or woken, even
when neither threshold applies. Any launch that lands for the pane clears the
mark. Opening it during the quit grace ends the old process first, so two
copies never run on one conversation.

## Outside control

`muxel ctl status AGENT` and `panes` report `status: "not_running"` with
`not_running_reason: "sleeping"` for a pane put to sleep (`"dormant"` remains
the reason for a pane deferred at launch). `show` includes the reason; `screen`,
`send`, `keys` and `answer` refuse with code `not_running` and that reason.

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

Waking isn't user activity: it records no attendance and changes no saved
timestamp. The idle timer also counts from the wake, though, so a woken pane
runs at least N hours before the timer can sleep it again, which leaves the
caller time to deliver what it woke the pane for. A wake never starts a new
conversation; if the provider's saved transcript is gone, restore's ordinary
recovery applies and `session_id` changes, so a caller holding the old id can
tell.

Sleep is for local PTY panes only. SSH and tmux sessions, editors, diffs and
browsers are unchanged.
