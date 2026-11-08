# Provider background fixtures

Codex 0.167.0 and Muse Code 1.4.14 were captured in real terminal panes in an
isolated muxel build. Each command was a harmless PowerShell sleep. Codex ran
one terminal, then two; `/stop` removed both. Muse ran one background shell task
and its task tree disappeared after completion.

The screen fixtures retain the provider chrome that identifies live work and
enough surrounding rows to locate it. Blank padding, introductory help, usage
information and personal status fields are omitted. Project paths are replaced
with `[project]`. Muse's physical rows were transcribed from the window capture:
the terminal's scrollback reader rejoins soft wraps, including flags left by
full-screen redraws, so that output alone does not preserve the displayed rows.

Codex's terminal count sits immediately above its composer, with both `/ps` and
`/stop` hints. A foreground turn can share that row with the count. `/subagents`
prints a historical list: it remains after a child completes, alongside a new
`Completed` message. Neither that history nor `Started` proves live child work.

Muse's live terminal tree sits below the lower composer rule. Captured task spinner
frames include `◇`, `◆` and `◈`; the explicit `running` column identifies live work. A
past `Backgrounded` message above the composer remains after the tree vanishes.
An additional run ended with the composer free and the last OSC spinner title
unchanged after completion. A Done hook and five seconds of quiet distinguish
that cached title from the live title updates during work.

The child JSONL fixture retains Codex's first metadata record, copied parent
history, the child-history boundary, its own turn start, assistant text, and turn
completion. Opaque IDs are replaced. Unrelated records and metadata are omitted; the history ordinal is
adjusted to the retained rows. A copied parent turn must never count as a child
turn, and assistant text alone must never end one. Only complete JSONL records
are evidence.
