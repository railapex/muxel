# Outside-control names and labels

`muxel ctl new PROJECT --name "build task"` gives the pane a starting label. The
label appears until a useful title arrives from the agent after launch. Blank,
session-ID, and transient executable titles do not replace it. A saved title from
the previous launch is cleared before a labeled terminal starts again.

The label remains available after the display changes. Use it to address the pane
in `ctl` commands, or qualify it as `PROJECT/LABEL` when names repeat. Display
names and manual custom names remain valid too. Matching two panes is an
ambiguity error; an id still identifies one pane. `ctl status` and `ctl panes`
return the retained `label` separately from the current display `name`.

Use `--name-fixed TEXT` for a sticky name. It is mutually exclusive with `--name`
and behaves like an inline rename. Manual names always take precedence. A named
row's tooltip shows the agent's current title and any retained starting label.

Existing saved custom names stay sticky: older data cannot distinguish an inline
rename from the earlier meaning of `--name`. Panes created without either flag
keep their existing naming behavior. The iOS workspace codec preserves labels
when writing layouts back to desktop peers.

On the wire, `name` carries the text and `name_fixed` selects sticky behavior
(default false). This lets older hosts honor `--name-fixed` through their
existing `name` field. A host predating labels still treats `--name` as sticky.
