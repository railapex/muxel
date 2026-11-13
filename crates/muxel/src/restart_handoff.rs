//! A restarted muxel waits for the one it replaces to let go of its workspace.
//!
//! A Restart starts the new version before the old one has exited, and the old
//! one keeps its workspace's lock until it does. The old process therefore puts
//! `--after-restart <workspace id>` first on the new one's command line. Before
//! it opens a window or starts outside control, the new process waits, for at
//! most [`WAIT`], until that workspace's lock is free. It doesn't take the lock
//! then: the workspace is locked when it's opened, as on any launch.
//!
//! The lock is the one thing the two processes contend for. Outside control
//! publishes a fresh loopback port and token, and a server removes the endpoint
//! file only while the file is still its own (`control.rs`). Nothing forwards a
//! launch to a running instance. Waiting on the lock itself needs no
//! per-platform process identity.

use std::ffi::OsString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use uuid::Uuid;

/// The flag, always first on the new version's command line.
pub(crate) const FLAG: &str = "--after-restart";
/// The longest the new version waits.
const WAIT: Duration = Duration::from_secs(30);

static STILL_HELD: AtomicBool = AtomicBool::new(false);

/// The new version's arguments for a Restart from `workspace`.
pub(crate) fn args_for(workspace: Option<Uuid>) -> Vec<String> {
    workspace
        .map(|id| vec![FLAG.to_string(), id.to_string()])
        .unwrap_or_default()
}

/// Split a leading `--after-restart <id>` off `args`. Only the leading position
/// counts, so arguments to `muxel ctl …` are never taken. A value that isn't a
/// workspace id goes with the flag: a restart never stops the new version from
/// starting.
pub(crate) fn take(mut args: Vec<OsString>) -> (Vec<OsString>, Option<Uuid>) {
    if args.first().is_none_or(|arg| arg != FLAG) {
        return (args, None);
    }
    let value = (args.len() > 1).then(|| args.remove(1));
    args.remove(0);
    let id = value.and_then(|value| value.to_str().and_then(|v| Uuid::parse_str(v).ok()));
    (args, id)
}

/// Wait for the replaced version to release `workspace`, for at most [`WAIT`].
pub(crate) fn wait(workspace: Uuid) {
    if !muxel_store::wait_for_workspace_lock(workspace, WAIT) {
        STILL_HELD.store(true, Ordering::Relaxed);
    }
}

/// The replaced version still held its workspace when the wait gave up.
pub(crate) fn still_held() -> bool {
    STILL_HELD.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::{FLAG, args_for, take};
    use std::ffi::OsString;
    use uuid::Uuid;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    /// What the old version passes, the new one reads back. The flag is taken
    /// only in front, and a bad value never blocks the launch.
    #[test]
    fn the_new_version_reads_the_workspace_the_old_one_passed() {
        let id = Uuid::new_v4();
        let passed: Vec<OsString> = args_for(Some(id)).into_iter().map(OsString::from).collect();
        assert_eq!(take(passed), (vec![], Some(id)));
        assert!(args_for(None).is_empty());

        let ctl = os(&["ctl", "send", "pane", FLAG, &id.to_string()]);
        assert_eq!(take(ctl.clone()), (ctl, None));
        assert_eq!(take(os(&[FLAG, "not-an-id", "--x"])), (os(&["--x"]), None));
        assert_eq!(take(os(&[FLAG])), (vec![], None));
    }
}
