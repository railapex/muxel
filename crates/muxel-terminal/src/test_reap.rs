//! Ending a test terminal's program and waiting until it, and what it started,
//! are gone. Killing a fixture's shell leaves its child (`more`) running until
//! the session's monitor thread closes the pseudoconsole after the shell
//! exits, about a millisecond later on an idle machine. One such `more` and
//! its conhost outlived a loaded gate's test binary and stayed until killed by
//! hand. The app's grok fixtures wait the same way with their own copy
//! (`conpty_test_support::ProcessTree`). A process still running when the wait
//! gives up is named on stderr, so the next one that stays says what it is.

use crate::session::TerminalSession;
use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::core::PWSTR;

/// How long a fixture's processes get to exit once its shell is killed.
const PATIENCE: Duration = Duration::from_secs(10);

/// Kill `session`'s program, then wait up to ten seconds for it and every
/// process it had started to exit, and name any that hasn't.
pub(crate) fn end_and_wait(session: &TerminalSession) {
    let tree = session.child_pid().map(ProcessTree::open);
    session.kill();
    let Some(tree) = tree else {
        return;
    };
    for (pid, image) in tree.wait_gone(PATIENCE) {
        report_survivor(pid, &image);
    }
}

/// Names a process the wait gave up on, on stderr. It writes to the stream
/// itself because libtest captures only the print macros, so the line shows
/// even when the test passes.
fn report_survivor(pid: u32, image: &str) {
    let thread = std::thread::current();
    let test = thread.name().unwrap_or("unnamed");
    let _ = writeln!(
        std::io::stderr(),
        "test_reap: {image} pid {pid} still running {} s after {test} killed its fixture",
        PATIENCE.as_secs()
    );
}

/// A process and its descendants, each held by handle so a reused pid can't
/// stand in for one that exited.
struct ProcessTree(Vec<(u32, HANDLE)>);

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
            handles.push((pid, handle));
            pending.extend(
                parents
                    .iter()
                    .filter(|&(&child, &parent)| parent == pid && child != pid)
                    .map(|(&child, _)| (child, started)),
            );
        }
        Self(handles)
    }

    /// Wait up to `timeout` for every process in the tree to exit. Returns the
    /// pid and image name of each one still running.
    fn wait_gone(&self, timeout: Duration) -> Vec<(u32, String)> {
        let deadline = Instant::now() + timeout;
        let mut running = Vec::new();
        for &(pid, handle) in &self.0 {
            let left = deadline.saturating_duration_since(Instant::now());
            let millis = u32::try_from(left.as_millis()).unwrap_or(u32::MAX);
            // SAFETY: this tree owns the handle until Drop.
            if unsafe { WaitForSingleObject(handle, millis) } != WAIT_OBJECT_0 {
                running.push((pid, image_name(handle)));
            }
        }
        running
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        for &(_, handle) in &self.0 {
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

/// Process `handle`'s image file name, such as `more.com`; `unknown` if
/// Windows won't say.
fn image_name(handle: HANDLE) -> String {
    let mut path = [0u16; 1024];
    let mut len = path.len() as u32;
    // SAFETY: `path` holds `len` u16s, and `handle` is open with
    // PROCESS_QUERY_LIMITED_INFORMATION.
    let named = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(path.as_mut_ptr()),
            &mut len,
        )
    };
    if named.is_err() {
        return "unknown".into();
    }
    let path = String::from_utf16_lossy(&path[..len as usize]);
    path.rsplit('\\').next().unwrap_or("unknown").to_string()
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

#[test]
fn a_process_still_running_when_the_wait_gives_up_is_named() {
    // `more` reading a pipe nobody writes to runs until it is killed.
    let mut more = std::process::Command::new("more.com")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("start more on an open pipe");
    let running = ProcessTree::open(more.id()).wait_gone(Duration::from_millis(100));
    let _ = more.kill();
    let _ = more.wait();
    let [(pid, image)] = running.as_slice() else {
        panic!("expected one survivor, got {running:?}");
    };
    assert_eq!(*pid, more.id());
    assert!(image.eq_ignore_ascii_case("more.com"), "image {image}");
}
