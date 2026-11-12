# Dormant session restore

Settings → Behavior offers “Start sessions not worked on in N days only when
opened”. It is off by default. Enabling it selects three days; the adjacent
buttons adjust the threshold. The hand-editable `config.toml` setting is
`dormant_after_days = 3`; zero disables it. Changes apply on the next project
open or app launch and do not stop processes that already run.

The age is measured from the latest of `activity.work_started_at`,
`completed_at`, and `blocked_at` (Unix milliseconds). Opening a pane does not
change that work timestamp. At exactly N elapsed days, an old pane becomes
dormant. Panes without these timestamps start normally. Pinned panes always
start, as does the active pane of the active project at launch.

A dormant pane owns no PTY or child process. It looks exactly like a pane put
to sleep, since opening either does the same thing (see
[Sleeping idle agents](agent-sleep.md#how-an-asleep-pane-looks)): one sidebar
line with a muted name and an outlined moon-and-age pill, and a moon on its tab.
The pane body shows its name, last saved title, saved status and folder with a
Start button.
Selecting it through the sidebar, tab, notification, tray or `ctl focus`
starts it using the ordinary restore path, including the saved provider session
id and existing session discovery. Repeated focus during launch does not start
another process. Closing a dormant pane never starts it.
Opening a particular pane in another project selects that target directly;
it does not also start the project's previously focused dormant session.

A running agent can also be made dormant on purpose: see
[Sleeping idle agents](agent-sleep.md).

This setting applies to local PTY terminals. SSH and local tmux restoration
are unchanged; it does not disconnect or suspend an existing remote session.
Editors, diffs and browsers are unchanged.

## Outside control

`muxel ctl status AGENT` and `panes` include `status: "not_running"` and
`not_running_reason: "dormant"` for dormant terminals. Their saved lifecycle
timestamps remain available as `status_since`; no `launch_id` exists yet.
`show` returns this same agent metadata with no screen reply. `screen` refuses
with code `not_running` and reason `dormant`.

`send`, `keys` and `answer` also refuse with code `not_running` and reason
`dormant`, including with `--force`. These commands never start a pane.
The new `muxel ctl focus AGENT` command deliberately selects and raises it,
changing keyboard focus, after which normal startup and
input rules apply. `muxel ctl skill` includes this contract.
An approved `ctl new --focus` opens the project without focusing its previous
preferred pane, then creates and focuses the new pane. The old dormant session
stays asleep; project reconciliation still precedes creation.
