//! Windows' process table, for what a terminal's own handles can't tell:
//! which processes a pane's shell started, and which a process descends from.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long one snapshot answers [`recent_process_parent_map`]. A walk costs
/// 10–30 ms on a machine with ~700 processes, and one close asks about each of
/// its panes more than once on the UI thread; this keeps a click to one walk.
pub(crate) const RECENT: Duration = Duration::from_millis(250);

/// Every process's parent pid, shared between the callers of one snapshot.
pub type ParentMap = Arc<HashMap<u32, u32>>;

/// [`process_parent_map`], reusing a snapshot any caller took in the last
/// 250 ms. A process started after it isn't seen, so callers must tolerate a
/// quarter-second-old answer: a pane close does when it starts, since its Undo
/// countdown checks again at expiry with its own walk
/// (`TerminalSession::is_idle_foreground_now`).
pub fn recent_process_parent_map() -> ParentMap {
    static CACHE: Mutex<Option<(Instant, ParentMap)>> = Mutex::new(None);
    let mut cache = CACHE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, parents)) = cache.as_ref()
        && at.elapsed() < RECENT
    {
        return Arc::clone(parents);
    }
    let parents = Arc::new(process_parent_map());
    // A failed walk is empty; don't serve it as a picture of the machine.
    *cache = (!parents.is_empty()).then(|| (Instant::now(), Arc::clone(&parents)));
    parents
}

/// Every process's parent pid, from one Toolhelp snapshot. Windows records a
/// parent's pid when a process starts and never updates it, so an entry can
/// name a parent that has exited, or a pid since given to another process.
/// Empty when the snapshot fails.
pub fn process_parent_map() -> HashMap<u32, u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

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

/// When process `pid` started, as a `FILETIME` tick (100 ns since 1601), or
/// `None` when it can't be opened: it exited, or it's protected.
pub fn process_start_time(pid: u32) -> Option<u64> {
    use windows::Win32::Foundation::{CloseHandle, FILETIME};
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: opening by pid takes no pointers; the handle is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut times = [FILETIME::default(); 4];
    let [created, exited, kernel, user] = &mut times;
    // SAFETY: `process` is live and each pointer is a FILETIME on this stack.
    let read = unsafe { GetProcessTimes(process, created, exited, kernel, user) };
    // SAFETY: `process` is the live handle opened above, closed once.
    let _ = unsafe { CloseHandle(process) };
    read.ok()?;
    Some(u64::from(times[0].dwHighDateTime) << 32 | u64::from(times[0].dwLowDateTime))
}
