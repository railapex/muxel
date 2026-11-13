# Session folder and branch chips

A terminal's sidebar row identifies its stored launch folder. A folder outside
the project root shows its basename and current Git branch. A session at the root
shows a branch-only chip when its known branch differs from the project header's
branch. Otherwise it has no chip and stays single-line. Branches are
shown before their first commit too. Detached HEADs
and folders outside Git omit the branch. Long labels truncate within the title
column so the status badge keeps its space. Hover for the full path and branch.

A chip is a label: clicking it selects the pane, like the rest of the row.
Right-click a terminal row and choose **Show only this folder** (or **…folder and
branch**) to filter that project's sidebar rows to the same full folder **or** the
same known branch. The choice fills the sidebar's filter box with
`project:<name> folder:<path> branch:<name>`, so the box is the one place a
filter lives and shows what it holds (see [sidebar.md](sidebar.md), "The filter
box"). Other projects list nothing while it holds a project. Matching basenames
alone do not count, and unknown branches never match each other. **Show all
sessions** on the row whose folder the box names empties it. The project header
also has a clear button, including while collapsed or after the original session
closes. Filtering affects only the sidebar, not the pane layout. The box is not
saved.

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
