# Casts

A cast is one provider session (Claude Code, Codex, Grok) recorded as its PTY reader saw it: every read with its time, what was typed, and marks that say what was true about the provider at chosen moments. `cast_replay_tests.rs` plays every `*.cast.jsonl` in this folder through muxel's own terminal parser and readers and checks each mark. Adding a cast adds coverage with no new code.

## The file

`<provider>-<version>-<os>-<slug>.cast.jsonl`, JSON lines. Line 1 is the header; every other line is `[t, kind, payload]`, with `t` in whole milliseconds since the cast began, never decreasing. Line order breaks ties.

```json
{"muxel_cast":1,"provider":"claude","version":"2.1.322","os":"windows","pty":"conpty 1.25.260930003","tap":"helper","cols":120,"rows":40,"term":"xterm-256color","replies":{"osc7501":true},"launch":["--model","sonnet"],"recorded":"2026-11-12","sanitized":true,"out_sha256":"…"}
[13,"out","\u001b[?25l"]
[15,"reply","\u001b[?6c"]
[1948,"out",{"hex":"e29480e29480e2"}]
[2962,"mark",{"id":"idle-first","by":"hand","truth":{"turn":"none","finished":0,"overlay":"none","composer":"shown","draft":"","takes_prompt":true}}]
[3983,"in","f"]
```

| kind | payload | records |
|---|---|---|
| `out` | bytes | one PTY read, exactly as returned: never merged or re-split |
| `in` | bytes | input that reached the child: keys, a paste, Enter |
| `reply` | bytes | what the terminal answered to a query (device attributes, cursor position, 7501) |
| `resize` | `[cols, rows]` | the grid changed size |
| `focus` | `true` / `false` | the pane gained or lost focus |
| `hook` | object, `name` first | an event from the provider's own hooks or transcript |
| `exit` | `{"code", "eof"}` | the child ended; `eof` says whether the stream ended too |
| `mark` | `{"id", "by", "truth", "note"?}` | what was true here |

**Bytes** are a JSON string when they are valid UTF-8 on their own, and `{"hex": "…"}` when they aren't (a read that ends inside a character). Decoding is exact either way. `out_sha256` hashes every `out` payload decoded and joined.

**Header.** `tap` is `muxel` (recorded through muxel's own session) or `helper` (a standalone ConPTY harness, whose query replies differ from muxel's). The `helper` Claude casts carry no colour: they were recorded with `NO_COLOR` set, which Claude honours, so they can't check a reader that uses cell colour. That harness also answered a cursor-position query wrongly (see `HELPER_CPR` in the test). Record with `NO_COLOR` unset, and on Windows with a `conpty.dll` and `OpenConsole.exe` beside the recorder, or Claude sends no OSC 7501 reports. `no_color: true` says the provider ran with `NO_COLOR` set. Those casts stay as the screen a person who sets `NO_COLOR` gets: a reader that tells a placeholder from a draft by colour must still pass their marks. `replies.osc7501` says whether the provider was told the terminal understands OSC 7501, which changes what it draws. `launch` is the provider's arguments with no paths.

## Marks

A mark sits between events, so always at a read boundary, and describes the settled screen: every read before it processed and no synchronized frame left open. `id` is unique in the file and never renamed. **Truth describes the provider, never muxel's reading of it**, so a cast outlives changes to the readers. `by` says where the truth came from: `script` (the recording script did it: it typed that draft and pressed Home), `hook` (the provider's own hooks said so, outside the terminal stream), `protocol` (the provider said so in the bytes a reader parses, an OSC 7501 report or its title, so agreement shows the parser reads the report, not that the report was right), or `hand` (a person read the screen, so a reader of the same text is checked against a person, not against the provider). Confirm a `hand` mark on a render other than muxel's, or its parse errors become the truth.

Truth keys, version 1. Every key is optional; an absent key claims nothing.

| key | values |
|---|---|
| `turn` | `none`, `running`, `waiting` (running and needs the person), `background` (turn over, background work continues) |
| `finished` | turns completed so far in this cast |
| `overlay` | `none`, `slash_menu`, `picker`, `permission`, `question`, `trust`, `other` |
| `composer` | `shown`, `hidden` (covered), `absent` (starting, or exited) |
| `draft` | the unsent text, exact; `""` is empty. It stands while the composer is hidden: a parked draft |
| `cursor` | character offset of the cursor in `draft` |
| `takes_prompt` | would text plus Enter, now, become exactly one new message and do nothing else |
| `options` | a question or permission card's choices, in order: `{"key", "label", "selected"?}`. `label` is the choice's name as drawn on its row, without a description column or line; `selected` marks the one the cursor is on, and is left out of every option when the screen doesn't show the cursor |

## Replay

Replay runs on a `TerminalSession` with no child process (`TerminalSession::replay`), so it runs the same on every OS. Each `out` goes in through the session's replay feed at its recorded time and takes the reader's path: muxel answers the queries in it, then the read is queued and drained into the parser as a pane's is. What muxel writes back lands in a buffer the test can read. Status is reconciled after each read the way the view does, including the runtime's own timers. At a mark the replay ends any open synchronized frame and asks the readers:

| truth key | reader |
|---|---|
| `turn` | the runtime status (`Idle`, `Working`, `Blocked`, `Done`) |
| `finished` | muxel's completion count for whatever owns the status, exactly (`finished/count`): while a 7501 report owns it, the count the completion pulse reads (`program`); otherwise the launch's count of finishes from titles, markers and hooks, which the held close reads (`launch`). Between turns, the Done badge too (`finished/done`) |
| `overlay` | the startup blocker `ctl new` reports; the numbered options `ctl show` and `ctl answer` read while Blocked |
| `options` | those options, each key, label and (when the truth records it) cursor |
| `composer` | the provider's input box as status finds it (Claude, Codex) |
| `draft` | startup input's empty-composer reading, and the text in the rows the box reader chose |
| `takes_prompt` | `startup_ready` (would startup input type now: its composer gate, not its wait for quiet output), and `takes_prompt/send_state_copy`, a copy of the state half of `ctl send`'s admission. ctl send's own decision lives in the app crate, which these tests can't call, so agreement on the copy proves nothing about ctl send |

A disagreement fails the test unless it is listed in `EXPECTED_GAPS` with the reader's wrong answer, exactly, and the finding it belongs to. A different wrong answer fails too, since another fault may stand behind the listed one. A listed gap that closes fails, so a fix shows up as an entry to delete, and a listed gap no check reaches fails as lost coverage, not as a fix. Failures name `file#mark`, the key, the truth, the verdict, and the screen rows the reader used.

Status scans the screen at most every 50 ms; at a mark the replay takes a fresh scan, as the view would moments later. A pane in the background parses its reads in batches every 250 ms, and a synchronized frame left open ends only when its deadline passes on the real clock. Replay parses every read, and a mark closes any open frame, so replay can see a screen a background pane hasn't drawn yet. Verdicts that depend on age (a title going stale, the quiet and settling fallbacks) read the real clock inside the session and aren't reproducible by replay yet, so no mark's truth depends on a wait. Each `in` is typed into the session at its time, as a pane's keyboard types it (an Enter on its own announces a turn first), so readers that track what was typed since the last Enter see it. `reply`, `focus` and `hook` are carried but not replayed. A `resize` stops the replay: the session settles a resize on the real clock, so replay can't apply one yet.

## Adding a cast

Record through the PTY at a fixed size, in a neutral folder, with scripted input only. `cast-record` (`src/bin/cast-record.rs`) does that through muxel's own session from a step file; its doc comment lists the steps. Launch the provider with the arguments muxel adds for it (for Codex, the `tui.terminal_title` override in `resolve_launch_for_session`), or its title won't carry the state a pane's does. Mark each moment worth checking, from what the script did or what the provider reported, and confirm each mark against the replayed screen. A cast in the repo never contains a path outside its fixture project, a user, account, organisation or host name, an email address, a real conversation or session id (casts use `00000000-0000-7000-8000-000000000001`), a token, key or environment value, text from real work, or a person's own typing. Sanitize by replacing text with text of the same width, so no wrap or cursor position moves; if a width can't be kept, record again. `cast-record` keeps only a hook's `name`, `source`, `reason`, `tool_name` and `notification_type`, never a prompt or tool input. `casts_hold_no_personal_data` reads every string in the header and in every event, and each byte stream joined across reads. It reads bytes, not the drawn screen: text a program assembles with cursor moves isn't seen whole, so look at the replayed screens too. `sanitizing_changes_no_verdict` replays each unsanitized recording beside its repo cast and needs the recordings, so it is ignored unless run with `--include-ignored` and `MUXEL_RAW_CASTS` set.

A Windows cast and a Unix cast of the same session differ (ConPTY re-renders the child's output), so each OS gets its own file. A cast is one provider version. A cast belongs here only when a test needs it, that is when it has a mark no other cast covers.
