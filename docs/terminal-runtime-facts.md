# Terminal runtime facts

`TerminalView` owns one immutable runtime snapshot for each live PTY launch.
Consumers read the snapshot; they do not parse terminal rows or advance status.

## Reconciliation edges

The owner reconciles after drained PTY output, process exit, a settled resize,
an explicit title/bell invalidation, a deliberate submission, or the earliest
due quiet/provider debounce deadline. A one-entry runtime trigger coalesces
redundant invalidations. Timers carry a generation and become no-ops after a
newer deadline replaces them.

Output reconciliation captures the latest visible screen when configured markers
or the provider adapter needs it. Active output is coalesced to at most one scan
per 50 ms with one exact trailing deadline; unchanged generations and ordinary
`status()` / `runtime_snapshot()` reads never scan the grid. Render therefore
cannot manufacture a lifecycle transition by polling more often.

## Snapshot identity

`TerminalRuntimeSnapshot` exposes:

- `launch_id`: a fresh UUID owned by the spawned `TerminalSession`;
- `binding_rev`: advances when a different provider conversation is observed
  within that launch, through a UUID title or the app's verified binding-adoption
  path. Without one of those sources, an unchanged revision is not proof that a
  provider cannot switch internally;
- `snapshot_seq`: advances with public lifecycle, binding, and readiness changes;
- `input_rev`: advances before every non-empty keyboard/control write enters the
  PTY writer;
- `draft_latched`: conservative evidence that input has not been deliberately
  submitted;
- status, its strongest visible reason, readiness, and evidence coverage.

These are observation tokens, not execution authorization. A later conditional
operation must validate them again on the serialized input owner immediately
before enqueueing bytes.

## Coverage boundary

The first implementation preserves the existing no-hook classifier. Provider
titles, visible rows, configured markers, bells, recent output, and process exit
are valid evidence for the fields they describe. Quiet visible output never
proves that a provider has no hidden background work. The snapshot reports
`ActivityOnly`, `VisibleMarkers`, or `ProviderVisible` coverage instead of
turning missing hook evidence into `KnownNone`.

Hooks, close policy, Mesh delivery, transcripts, browser facts, and durable
history are outside this component.
