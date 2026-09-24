# App actions and keyboard discovery

The configurable `ShowActionSheet` action opens App actions. Its candidate
default is `Ctrl+Shift+Space`. The sheet lists existing commands with mnemonic
keys, descriptive names, effective shortcuts, and unavailable-target reasons.
Use a listed key, the arrow keys and Enter, or a button. Escape and click-away
cancel; leaving the owning window dismisses it. There is no timeout.

The sheet owns a separate focus scope. Unknown and modified keys are consumed
there before ordinary action dispatch. They never become terminal input, and
cancellation does not replay the prefix. Outside that scope, ordinary terminal
input and the existing terminal passthrough rules remain unchanged.

## One catalogue, existing handlers

`action_catalog` owns stable IDs, labels, categories, target kinds, defaults and
sheet mnemonics. The palette, key sheet and binding settings use those names and
effective bindings. Existing persisted action IDs and 58 of the 59 v0.1.9
defaults are preserved. Close Pane deliberately moves to the platform tab-close
chord: Cmd+W on macOS and Ctrl+W elsewhere, with Ctrl+F4 as a Windows alias.
Terminal Ctrl+W remains PTY input and its existing Ctrl+Shift+W close chord stays
available. Fixed terminal Tab/Shift+Tab, nonterminal Ctrl+P, copy and quit aliases
remain present. Applying bindings replaces the app's prior bindings while
retaining component bindings; invalid input is rejected and overlapping app
bindings are shown before applying.

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
the operation. Executed actions and deactivation do not perform that restoration.

The native Windows bridge supports single chords using letters, digits, Space,
navigation keys or F1–F24. Layout-dependent punctuation and multistroke prefixes
remain available in GPUI chrome, but are not forwarded from native pages.
The native macOS page and Linux's separate browser window have no bridge in
this slice. Their chrome and ordinary terminal/editor paths use GPUI bindings.

## Verification contract

- Retain the v0.1.9 binding fixture and saved-override/passthrough regressions.
- Prove palette/keyboard action identity, including New Tab's active preset.
- Prove unknown sheet keys and cancellation send no terminal bytes; verify
  invocation, rebinding, deactivation, modal ownership and popout routing.
- Reject removed/recreated targets and focus movement before native dispatch.
- Check browser hide, invoke and page-focus return using the native process.
- Record supported-platform, keyboard-layout, IME and provider/TUI evidence
  separately. Windows compilation does not prove those other environments.

Exact Back after attention/resource navigation is a separate change. This slice
adds no layout history, close recovery, Mesh entries or plugin registry.
