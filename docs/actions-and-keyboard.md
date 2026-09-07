# App actions and keyboard discovery

The configurable `ShowActionSheet` action opens App actions. Its default is
`Ctrl+Shift+Space`. The sheet lists existing commands with mnemonic
keys, descriptive names, effective shortcuts, and unavailable-target reasons.
Use a listed key, the arrow keys and Enter, or a button. In a short window the
list scrolls, and keyboard selection scrolls into view. Escape and click-away
cancel, and the click reaches nothing behind the sheet. Leaving the owning
window dismisses the sheet, and when you come back, muxel's keyboard focus is
where it was before the sheet opened. There is no timeout.

The sheet owns a separate focus scope. Unknown and modified keys are consumed
there before ordinary action dispatch. They never become terminal input, and
cancellation does not replay the prefix. Outside that scope, ordinary terminal
input and the existing terminal passthrough rules remain unchanged.

## One catalogue, existing handlers

`action_catalog` owns stable IDs, labels, categories, target kinds, defaults and
sheet mnemonics. The palette, key sheet and binding settings use those names and
effective bindings. Existing persisted action IDs and all 59 v0.1.9 defaults are
preserved. Fixed terminal Tab/Shift+Tab, nonterminal Ctrl+P, copy and quit
aliases, and the macOS app menu's Cmd+H and Option+Cmd+H remain present.
Applying bindings replaces the app's prior bindings while retaining component
bindings; invalid input is rejected and overlapping app bindings are shown
before applying.

Palette and sheet commands dispatch the same typed actions as direct shortcuts.
In particular, New Tab inherits the active pane's preset through every entry.
Palette discovery includes navigation and view commands that previously appeared
only in binding settings. Hold-to-record remains a physical-key action; the
palette offers toggle recording instead. Runner, snippet and memory commands
retain their existing project-specific paths.

Availability explains the target an operation needs. It does not authorize an
external caller or bypass a handler's close/save policy. This catalogue is not
an externally callable action registry.

## Window and target ownership

A sheet belongs to the window that opened it. It retains the originating focus
handle and rejects execution if the workspace, selection or live pane entity
changes. Action dispatch on that validated focus handle is synchronous. A
removed or recreated pane does not silently become a replacement target.

Existing modals keep their input and decision. The prefix does not replace them.
Native browser surfaces hide under the sheet through the existing overlay owner.
Single-pane popouts expose their applicable local close, clear-scrollback and
fullscreen actions; unrelated workspace commands are visibly unavailable there.
Their close action uses the existing popout confirmation.

On Windows, WebView2's native accelerator callback recognizes the configured
single chord and admits at most one pending request. The owning UI checks the
same native focus, shortcut and a 250 ms validity interval before opening.
Rebinding, focus movement, expiry and view retirement reject delayed requests.
Cancel restores the original page focus only while its exact pane still owns
the operation. After deactivation, page focus returns when the owning window is
next active, and only if the same pane and browser still hold focus there; it
is never restored while another app is in front. Executed actions leave page
focus alone.

The native Windows bridge supports single chords using letters, digits, Space,
navigation keys or F1–F24. Layout-dependent punctuation and multistroke prefixes
remain available in GPUI chrome, but are not forwarded from native pages.
The native macOS page and Linux's separate browser window have no bridge in
this slice. Their chrome and ordinary terminal/editor paths use GPUI bindings.

## Tests

`crates/muxel/tests/fixtures/keybindings-v0.1.9.json` records every v0.1.9
action ID, default chord and context, and a catalogue test holds the current
defaults to it. Unit tests also cover saved overrides and terminal passthrough,
rebinding and the fixed bindings, conflict and invalid-input reporting, palette
and sheet action identity, sheet key handling, where focus returns when the
sheet closes, and, on Windows, the native chord and request checks. Window
activation, scrolling, browser hiding and native page focus need a running
window and have no automated test.
