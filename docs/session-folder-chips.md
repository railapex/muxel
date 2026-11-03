# Session folder and branch chips

A terminal's sidebar row identifies its stored launch folder. A folder outside
the project root shows its basename and current Git branch. A session at the root
shows only the branch; a root without a known branch has no chip. Branches are
shown before their first commit too. Detached HEADs
and folders outside Git omit the branch. Long labels truncate within the title
column so the status badge keeps its space. Hover for the full path and branch.

Clicking a chip filters that project's sidebar rows to the same full folder **or**
the same known branch. Matching basenames alone do not count, and unknown branches
never match each other. Clicking the same chip again clears the filter. The
project header also has a clear button, including while collapsed or after the
original session closes. Filtering affects only the sidebar, not the pane layout.
Filters are not persisted.

Long project-header branch labels ellipsize so they cannot paint over the clear
button. Their tooltip keeps the full branch name.

Branches are cached per project and distinct launch folder. Local folders refresh
with the existing status/change-count refresh; remote folders refresh with the
existing SSH branch poll using the host's commands connection. Git reads run off
the UI thread with optional locking disabled. Rendering and control requests do
not run Git. Results for removed folders or changed project/host contexts are
discarded. A newly opened session can briefly show only its folder until refreshed.

`muxel ctl panes` and `muxel ctl status` add an optional `branch` field to each
pane's existing `cwd`. It is the cached current branch of that launch folder,
separate from the persisted `worktree_branch` used when creating a worktree.
Older responses without `branch` still deserialize. Unknown branches are omitted.

The folder is `Instance.launch_dir`, then `worktree_path`, then the project root
(the remote root for SSH projects). A running program can change directory without
updating this metadata. These chips do not track those later movements.
