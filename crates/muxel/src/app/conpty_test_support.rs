use muxel_terminal::TerminalSession;
use std::collections::HashSet;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    WaitForSingleObject,
};

/// Keep the fixture's process identity after exit, independently of PTY delivery.
pub(super) struct ChildHandle(HANDLE);

impl ChildHandle {
    pub(super) fn new(session: &TerminalSession) -> Self {
        // SAFETY: retain the process created by the fixture before waiting, so
        // PID reuse cannot satisfy a later liveness assertion.
        Self(unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                session.child_pid().unwrap(),
            )
            .unwrap()
        })
    }

    pub(super) fn running(&self) -> bool {
        // SAFETY: this guard owns the retained handle until Drop.
        let status = unsafe { WaitForSingleObject(self.0, 0) };
        if status == WAIT_TIMEOUT {
            true
        } else {
            assert_eq!(status, WAIT_OBJECT_0);
            false
        }
    }
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        // SAFETY: this guard exclusively owns the retained process handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// A fixture's process and its descendants, each held by handle so a reused
/// PID can't stand in for one that exited.
pub(super) struct ProcessTree(Vec<HANDLE>);

impl ProcessTree {
    /// Process `root` and the descendants it has now. A process counts only if
    /// it started no earlier than its parent: Windows reuses PIDs, so an older
    /// process can still name `root`'s PID as its long-gone parent.
    pub(super) fn open(root: u32) -> Self {
        let parents = super::process_parent_map();
        let mut handles = Vec::new();
        let mut seen = HashSet::new();
        let mut pending = vec![(root, 0)];
        while let Some((pid, parent_started)) = pending.pop() {
            if !seen.insert(pid) {
                continue;
            }
            // SAFETY: a handle this tree owns until Drop, or none.
            let Ok(handle) = (unsafe {
                OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                    false,
                    pid,
                )
            }) else {
                continue;
            };
            let started = started_at(handle);
            if started < parent_started {
                // SAFETY: opened above and not kept.
                let _ = unsafe { CloseHandle(handle) };
                continue;
            }
            handles.push(handle);
            pending.extend(
                parents
                    .iter()
                    .filter(|&(&child, &parent)| parent == pid && child != pid)
                    .map(|(&child, _)| (child, started)),
            );
        }
        Self(handles)
    }

    /// Wait up to `timeout` for every process in the tree to exit; false if
    /// one is still running then.
    pub(super) fn wait_gone(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        self.0.iter().all(|&handle| {
            let left = deadline.saturating_duration_since(Instant::now());
            let millis = u32::try_from(left.as_millis()).unwrap_or(u32::MAX);
            // SAFETY: this tree owns the handle until Drop.
            let waited = unsafe { WaitForSingleObject(handle, millis) };
            waited == WAIT_OBJECT_0
        })
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        for &handle in &self.0 {
            // SAFETY: this tree exclusively owns each handle.
            let _ = unsafe { CloseHandle(handle) };
        }
    }
}

/// When process `handle` started, in 100 ns ticks; 0 if Windows won't say.
fn started_at(handle: HANDLE) -> u64 {
    let mut times = [FILETIME::default(); 4];
    let [created, exited, kernel, user] = &mut times;
    // SAFETY: `handle` is open with PROCESS_QUERY_LIMITED_INFORMATION.
    if unsafe { GetProcessTimes(handle, created, exited, kernel, user) }.is_err() {
        return 0;
    }
    (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime)
}
