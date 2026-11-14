//! Ending a test terminal's program and waiting until it, and what it started,
//! are gone. Killing a fixture's shell leaves its child (`more`) running until
//! the session's monitor thread closes the pseudoconsole after the shell
//! exits, about a millisecond later on an idle machine. One such `more` and
//! its conhost outlived a loaded gate's test binary and stayed until killed by
//! hand. The app's grok fixtures wait the same way with their own copy
//! (`conpty_test_support::ProcessTree`).

use crate::session::TerminalSession;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    WaitForSingleObject,
};

/// How long a fixture's processes get to exit once its shell is killed.
const PATIENCE: Duration = Duration::from_secs(10);

/// Kill `session`'s program, then wait up to ten seconds for it and every
/// process it had started to exit.
pub(crate) fn end_and_wait(session: &TerminalSession) {
    let tree = session.child_pid().map(ProcessTree::open);
    session.kill();
    if let Some(tree) = tree {
        tree.wait_gone(PATIENCE);
    }
}

/// A process and its descendants, each held by handle so a reused pid can't
/// stand in for one that exited.
struct ProcessTree(Vec<HANDLE>);

impl ProcessTree {
    /// Process `root` and the descendants it has now. A process counts only if
    /// it started no earlier than its parent: Windows reuses pids, so an older
    /// process can still name `root`'s pid as its long-gone parent.
    fn open(root: u32) -> Self {
        let parents = parent_map();
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

    /// Wait up to `timeout` for every process in the tree to exit.
    fn wait_gone(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        for &handle in &self.0 {
            let left = deadline.saturating_duration_since(Instant::now());
            let millis = u32::try_from(left.as_millis()).unwrap_or(u32::MAX);
            // SAFETY: this tree owns the handle until Drop.
            if unsafe { WaitForSingleObject(handle, millis) } != WAIT_OBJECT_0 {
                return;
            }
        }
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

/// Every process's parent pid, from one Toolhelp snapshot; empty when the
/// snapshot fails.
fn parent_map() -> HashMap<u32, u32> {
    // SAFETY: a process snapshot takes no pointers; the handle is closed below.
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return HashMap::new();
    };
    let mut parents = HashMap::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `entry` is a sized PROCESSENTRY32W the walk writes into.
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            parents.insert(entry.th32ProcessID, entry.th32ParentProcessID);
            // SAFETY: as above.
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    // SAFETY: `snapshot` is the live handle opened above, closed once.
    let _ = unsafe { CloseHandle(snapshot) };
    parents
}
