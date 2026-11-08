//! A per-process memory limit for a pane's local processes.
//!
//! Windows has no per-process memory limit by default and no out-of-memory
//! killer. One runaway process can commit memory until the commit pool (RAM
//! plus page file) runs dry; then whichever program allocates next fails,
//! muxel included, while the runaway lives on. A pane's local process tree
//! therefore runs in its own job object with `JOB_OBJECT_LIMIT_PROCESS_MEMORY`:
//! a process that asks for more than the limit is refused, which usually stops
//! it, and muxel and the pane's other processes carry on.
//!
//! - One job per pane, owned by its [`crate::TerminalSession`]. The child is
//!   created inside it (`PROC_THREAD_ATTRIBUTE_JOB_LIST`, in the vendored
//!   portable-pty), so a process it starts at once cannot escape.
//! - The limit is per process, not per job: a pane legitimately runs many
//!   large processes at once, like cargo's parallel rustc.
//! - No `KILL_ON_JOB_CLOSE`, and muxel never calls `TerminateJobObject`. A GUI
//!   program started from a pane (`start notepad`) outlives the pane as before;
//!   closing a pane still kills its shell by exact PID.
//! - `BREAKAWAY_OK`, not `SILENT_BREAKAWAY_OK`: a child created with
//!   `CREATE_BREAKAWAY_FROM_JOB` leaves the job and runs without the limit.
//!   Without the flag such launches would fail outright.
//! - If the job can't be created, or Windows refuses it at launch (a parent
//!   job's limits can conflict), the pane starts without a limit and the
//!   reason is logged. A pane never fails to start because of this.
//! - Every job reports to one shared completion port and thread. It turns
//!   `JOB_OBJECT_MSG_PROCESS_MEMORY_LIMIT` into [`MemoryLimitHit`]s the pane
//!   reports, and notes when that process exits.
//!
//! Only processes in the pane's local tree are covered: what a tmux server or
//! a remote host runs, WSL's Linux processes, and processes Windows starts on a
//! program's behalf (COM servers, services) are outside it.
//!
//! Unix has no equivalent here. The closest per-process control is
//! `setrlimit(RLIMIT_AS)` in the child before exec, which limits address space
//! rather than commit and breaks programs that reserve large ranges up front
//! (V8, Go, sanitizers). On Linux, a cgroup v2 `memory.max` per pane (for
//! example through `systemd-run --user --scope`) limits the whole group, and
//! the kernel's OOM killer then picks the victim inside it.

use std::collections::HashSet;
use std::time::{Duration, Instant};

/// A process in a pane that asked for more memory than the pane's limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryLimitHit {
    pub pid: u32,
    /// The executable's file name, when Windows could still name the process.
    pub program: Option<String>,
    pub limit_bytes: u64,
    /// The process had exited when the hit was reported. Windows only refuses
    /// the allocation; most programs stop on that, and some carry on.
    pub exited: bool,
}

/// How long a hit waits for its process to exit before it is reported as
/// still running. A program that dies on the refusal usually exits within
/// milliseconds; this only delays the report of one that doesn't.
#[cfg_attr(not(windows), allow(dead_code))]
const SETTLE: Duration = Duration::from_secs(2);

/// Hits held per pane between reports. A pane that keeps starting runaways
/// reports the first few; the rest are logged only.
#[cfg_attr(not(windows), allow(dead_code))]
const MAX_PENDING: usize = 16;

/// The hits of one pane's job, in the order Windows posted them.
#[derive(Default)]
#[cfg_attr(not(windows), allow(dead_code))]
struct HitLog {
    pending: Vec<(MemoryLimitHit, Instant)>,
    /// Processes recorded and not yet exited, so one that keeps asking is
    /// reported once.
    live: HashSet<u32>,
}

#[cfg_attr(not(windows), allow(dead_code))]
impl HitLog {
    fn hit(&mut self, pid: u32, program: Option<String>, limit_bytes: u64, now: Instant) -> bool {
        if !self.live.insert(pid) || self.pending.len() >= MAX_PENDING {
            return false;
        }
        self.pending.push((
            MemoryLimitHit {
                pid,
                program,
                limit_bytes,
                exited: false,
            },
            now,
        ));
        true
    }

    fn exited(&mut self, pid: u32) {
        if self.live.remove(&pid) {
            for (hit, _) in &mut self.pending {
                if hit.pid == pid {
                    hit.exited = true;
                }
            }
        }
    }

    /// Whether exits still matter: a hit waits to be reported, or a reported
    /// process hasn't exited yet.
    fn busy(&self) -> bool {
        !self.pending.is_empty() || !self.live.is_empty()
    }

    /// Hits whose process has exited, or that have waited [`SETTLE`].
    fn take(&mut self, now: Instant) -> Vec<MemoryLimitHit> {
        let mut ready = Vec::new();
        self.pending.retain(|(hit, at)| {
            let due = hit.exited || now.saturating_duration_since(*at) >= SETTLE;
            if due {
                ready.push(hit.clone());
            }
            !due
        });
        ready
    }
}

#[cfg(windows)]
pub(crate) use imp::PaneJob;

#[cfg(windows)]
mod imp {
    use super::{HitLog, MemoryLimitHit};
    use parking_lot::Mutex;
    use std::collections::BTreeMap;
    use std::os::windows::io::RawHandle;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, OnceLock, Weak};
    use std::time::Instant;
    use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows::Win32::System::IO::{
        CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
    };
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_ASSOCIATE_COMPLETION_PORT,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectAssociateCompletionPortInformation,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows::Win32::System::Threading::{
        INFINITE, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::{PCWSTR, PWSTR};

    // `JOB_OBJECT_MSG_*` (winnt.h). For these, the completion packet's
    // overlapped pointer carries the process id.
    const MSG_EXIT_PROCESS: u32 = 7;
    const MSG_ABNORMAL_EXIT_PROCESS: u32 = 8;
    const MSG_PROCESS_MEMORY_LIMIT: u32 = 9;

    /// Where one job's messages land.
    struct Sink {
        limit_bytes: u64,
        log: Mutex<HitLog>,
        /// [`HitLog::busy`], written under the lock and read without it, so
        /// the per-tick poll and the stream of exit messages from a busy pane
        /// (cargo's rustc processes) skip the lock while there are no hits.
        busy: AtomicBool,
    }

    /// Completion key → sink. Keys are never reused, so a message from a job
    /// whose pane has closed (a GUI program still running in it) finds nothing.
    static SINKS: std::sync::Mutex<BTreeMap<usize, Weak<Sink>>> =
        std::sync::Mutex::new(BTreeMap::new());
    static NEXT_KEY: AtomicUsize = AtomicUsize::new(1);
    /// The shared completion port, `None` when it couldn't be set up (jobs
    /// still limit memory; their hits just go unreported).
    static PORT: OnceLock<Option<HANDLE>> = OnceLock::new();

    fn port() -> Option<HANDLE> {
        *PORT.get_or_init(|| {
            let port = match unsafe {
                CreateIoCompletionPort(INVALID_HANDLE_VALUE, HANDLE::default(), 0, 1)
            } {
                Ok(port) => port,
                Err(err) => {
                    log::warn!(
                        "pane memory limit: no completion port, hits won't be reported: {err}"
                    );
                    return None;
                }
            };
            let watcher = std::thread::Builder::new()
                .name("muxel-pane-jobs".to_string())
                .spawn(move || watch(port));
            match watcher {
                Ok(_) => Some(port),
                Err(err) => {
                    log::warn!(
                        "pane memory limit: no watcher thread, hits won't be reported: {err}"
                    );
                    unsafe {
                        let _ = CloseHandle(port);
                    }
                    None
                }
            }
        })
    }

    fn sink(key: usize) -> Option<Arc<Sink>> {
        SINKS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&key)
            .and_then(Weak::upgrade)
    }

    /// The watcher thread: idle in the kernel until a job posts a message.
    fn watch(port: HANDLE) {
        loop {
            let mut message = 0u32;
            let mut key = 0usize;
            let mut overlapped: *mut OVERLAPPED = std::ptr::null_mut();
            let got = unsafe {
                GetQueuedCompletionStatus(port, &mut message, &mut key, &mut overlapped, INFINITE)
            };
            if let Err(err) = got {
                if overlapped.is_null() {
                    log::warn!("pane memory limit: watcher stopped: {err}");
                    return;
                }
                continue;
            }
            let pid = overlapped as usize as u32;
            match message {
                MSG_PROCESS_MEMORY_LIMIT => {
                    let Some(sink) = sink(key) else { continue };
                    let program = image_name(pid);
                    log::warn!(
                        "pane memory limit: {} (pid {pid}) reached the {} byte limit",
                        program.as_deref().unwrap_or("a process"),
                        sink.limit_bytes
                    );
                    let mut log = sink.log.lock();
                    log.hit(pid, program, sink.limit_bytes, Instant::now());
                    sink.busy.store(log.busy(), Ordering::Release);
                }
                MSG_EXIT_PROCESS | MSG_ABNORMAL_EXIT_PROCESS => {
                    if let Some(sink) = sink(key)
                        && sink.busy.load(Ordering::Acquire)
                    {
                        let mut log = sink.log.lock();
                        log.exited(pid);
                        sink.busy.store(log.busy(), Ordering::Release);
                    }
                }
                _ => {}
            }
        }
    }

    fn image_name(pid: u32) -> Option<String> {
        let process =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, BOOL(0), pid) }.ok()?;
        let mut buffer = [0u16; 1024];
        let mut len = buffer.len() as u32;
        let named = unsafe {
            QueryFullProcessImageNameW(
                process,
                PROCESS_NAME_WIN32,
                PWSTR(buffer.as_mut_ptr()),
                &mut len,
            )
        };
        unsafe {
            let _ = CloseHandle(process);
        }
        named.ok()?;
        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        std::path::Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }

    /// One pane's job object. Dropping it closes muxel's handle; processes
    /// still in the job keep running, with the limit still applied.
    pub(crate) struct PaneJob {
        handle: HANDLE,
        /// What the spawn hands to Windows. The job itself, except in tests
        /// that make Windows refuse it.
        spawn_handle: HANDLE,
        key: Option<usize>,
        sink: Arc<Sink>,
    }

    impl PaneJob {
        /// A job whose processes may each commit at most `limit_bytes`.
        pub(crate) fn create(limit_bytes: u64) -> std::io::Result<Self> {
            #[cfg(test)]
            if tests::refusal() == Some(tests::Refusal::Create) {
                return Err(std::io::Error::other("job creation refused by the test"));
            }
            let handle =
                unsafe { CreateJobObjectW(None, PCWSTR::null()) }.map_err(std::io::Error::from)?;
            let mut job = Self {
                handle,
                spawn_handle: handle,
                key: None,
                sink: Arc::new(Sink {
                    limit_bytes,
                    log: Mutex::new(HitLog::default()),
                    busy: AtomicBool::new(false),
                }),
            };
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_PROCESS_MEMORY | JOB_OBJECT_LIMIT_BREAKAWAY_OK;
            limits.ProcessMemoryLimit = usize::try_from(limit_bytes).unwrap_or(usize::MAX);
            let limited = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const core::ffi::c_void,
                    std::mem::size_of_val(&limits) as u32,
                )
            };
            limited.map_err(std::io::Error::from)?;
            job.report_hits();
            #[cfg(test)]
            if tests::refusal() == Some(tests::Refusal::Assign) {
                job.spawn_handle = tests::query_only(handle);
            }
            Ok(job)
        }

        /// Route this job's messages to its sink. Failure leaves the limit in
        /// force and only loses the report.
        fn report_hits(&mut self) {
            let Some(port) = port() else { return };
            let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
            SINKS
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key, Arc::downgrade(&self.sink));
            let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                CompletionKey: key as *mut core::ffi::c_void,
                CompletionPort: port,
            };
            let associated = unsafe {
                SetInformationJobObject(
                    self.handle,
                    JobObjectAssociateCompletionPortInformation,
                    &association as *const _ as *const core::ffi::c_void,
                    std::mem::size_of_val(&association) as u32,
                )
            };
            match associated {
                Ok(()) => self.key = Some(key),
                Err(err) => {
                    log::warn!("pane memory limit: hits won't be reported for this pane: {err}");
                    SINKS
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&key);
                }
            }
        }

        /// The handle portable-pty creates the child inside.
        pub(crate) fn spawn_handle(&self) -> RawHandle {
            self.spawn_handle.0 as RawHandle
        }

        /// Whether `process` (a handle with query access) runs in this job.
        pub(crate) fn contains(&self, process: RawHandle) -> bool {
            let mut inside = BOOL(0);
            let asked =
                unsafe { IsProcessInJob(HANDLE(process as isize), self.handle, &mut inside) };
            asked.is_ok() && inside.as_bool()
        }

        pub(crate) fn limit_bytes(&self) -> u64 {
            self.sink.limit_bytes
        }

        /// Hits ready to report: their process exited, or it outlived the
        /// settle time. Cheap when there are none.
        pub(crate) fn take_hits(&self, now: Instant) -> Vec<MemoryLimitHit> {
            if !self.sink.busy.load(Ordering::Acquire) {
                return Vec::new();
            }
            let mut log = self.sink.log.lock();
            let hits = log.take(now);
            self.sink.busy.store(log.busy(), Ordering::Release);
            hits
        }
    }

    impl Drop for PaneJob {
        fn drop(&mut self) {
            if let Some(key) = self.key {
                SINKS
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&key);
            }
            unsafe {
                if self.spawn_handle != self.handle {
                    let _ = CloseHandle(self.spawn_handle);
                }
                let _ = CloseHandle(self.handle);
            }
        }
    }

    #[cfg(test)]
    pub(crate) mod tests {
        use std::cell::Cell;
        use windows::Win32::Foundation::{BOOL, DUPLICATE_HANDLE_OPTIONS, DuplicateHandle, HANDLE};
        use windows::Win32::System::Threading::GetCurrentProcess;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum Refusal {
            /// `CreateJobObjectW` fails.
            Create,
            /// The job exists, but the spawn gets a handle Windows can't
            /// assign processes with, as when a parent job's rules refuse it.
            Assign,
        }

        thread_local! {
            static REFUSAL: Cell<Option<Refusal>> = const { Cell::new(None) };
        }

        pub(crate) fn refusal() -> Option<Refusal> {
            REFUSAL.with(Cell::get)
        }

        pub(crate) struct RefusalGuard;

        impl Drop for RefusalGuard {
            fn drop(&mut self) {
                REFUSAL.with(|cell| cell.set(None));
            }
        }

        /// Make job setup on this thread fail as `refusal` says until the guard drops.
        pub(crate) fn refuse(refusal: Refusal) -> RefusalGuard {
            REFUSAL.with(|cell| cell.set(Some(refusal)));
            RefusalGuard
        }

        /// A duplicate of `job` that can only query it.
        pub(crate) fn query_only(job: HANDLE) -> HANDLE {
            const JOB_OBJECT_QUERY: u32 = 0x0004;
            let mut duplicate = HANDLE::default();
            let duplicated = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    job,
                    GetCurrentProcess(),
                    &mut duplicate,
                    JOB_OBJECT_QUERY,
                    BOOL(0),
                    DUPLICATE_HANDLE_OPTIONS(0),
                )
            };
            duplicated.expect("duplicate the job handle with query access only");
            duplicate
        }
    }
}

/// Real ConPTY panes running this test executable as their probes: a hog that
/// commits past the limit, a well-behaved sibling, and a detached lingerer.
#[cfg(all(test, windows))]
mod pty_tests {
    use super::imp::PaneJob;
    use super::imp::tests::{Refusal, refuse};
    use crate::session::{CommandSpec, PtyChunk, PtyReceiver, TerminalSession};
    use std::os::windows::io::RawHandle;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Console::FreeConsole;
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
    };

    const MB: u64 = 1 << 20;
    const TEST_LIMIT: u64 = 256 * MB;
    const PROBE: &str = "pane_job::pty_tests::memory_probe";

    /// Run by the tests below as a pane's program; a no-op in a normal run.
    #[test]
    fn memory_probe() {
        let args: Vec<String> = std::env::args()
            .skip_while(|arg| arg != "--")
            .skip(1)
            .collect();
        match args.as_slice() {
            [mode, mb, file] if mode == "probe-hog" => hog(mb.parse().unwrap(), Path::new(file)),
            [mode, mb, file] if mode == "probe-sibling" => {
                linger(mb.parse().unwrap(), Path::new(file), false)
            }
            [mode, file] if mode == "probe-linger" => linger(0, Path::new(file), true),
            _ => {}
        }
    }

    /// Commit `mb` MB. Past the pane's limit the allocation fails and Rust's
    /// allocation-error handler prints and aborts, so "done" is never written.
    fn hog(mb: usize, file: &Path) {
        std::fs::write(file, format!("start {}\n", std::process::id())).unwrap();
        // Time for the test to open a handle before the outcome.
        std::thread::sleep(Duration::from_millis(500));
        let mut block: Vec<u8> = Vec::with_capacity(mb << 20);
        block.resize(mb << 20, 1);
        let report = format!("start {}\ndone {}\n", std::process::id(), block.len());
        std::fs::write(file, report).unwrap();
    }

    /// Hold `mb` MB and stay alive until killed (two minutes at most).
    /// `detach` leaves the console first, as a GUI program never joins it.
    fn linger(mb: usize, file: &Path, detach: bool) {
        if detach {
            unsafe {
                let _ = FreeConsole();
            }
        }
        let block = vec![1u8; mb << 20];
        std::fs::write(file, format!("ready {}\n", std::process::id())).unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        std::hint::black_box(&block);
    }

    /// The cmd line that runs the probe in `args` mode.
    fn probe_line(args: &[&str]) -> String {
        let exe = std::env::current_exe().unwrap();
        let args: Vec<String> = args.iter().map(|arg| format!("\"{arg}\"")).collect();
        format!(
            "\"{}\" --exact {PROBE} --nocapture -- {}",
            exe.display(),
            args.join(" ")
        )
    }

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("muxel-pane-job-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn shell(limit: Option<u64>) -> Screen {
        let spec = CommandSpec::program("cmd.exe", vec!["/D".into(), "/Q".into(), "/K".into()])
            .with_memory_limit(limit);
        Screen::new(spec)
    }

    /// A pane and everything it has printed.
    struct Screen {
        session: Arc<TerminalSession>,
        rx: PtyReceiver,
        raw: Vec<u8>,
        exited: bool,
    }

    impl Screen {
        fn new(spec: CommandSpec) -> Self {
            let (session, rx) = TerminalSession::spawn(spec, 250, 40).expect("spawn pane");
            Self {
                session,
                rx,
                raw: Vec::new(),
                exited: false,
            }
        }

        fn pump(&mut self) {
            while let Ok(chunk) = self.rx.try_recv() {
                match chunk {
                    PtyChunk::Output(bytes) => {
                        if bytes.windows(4).any(|window| window == b"\x1b[6n") {
                            self.session.write_input(b"\x1b[1;1R");
                        }
                        self.raw.extend(bytes);
                    }
                    PtyChunk::Exit { .. } => self.exited = true,
                }
            }
        }

        fn text(&self) -> String {
            strip_controls(&self.raw)
        }

        fn type_line(&self, line: &str) {
            self.session.write_input(format!("{line}\r").as_bytes());
        }

        /// Pump until `done` holds or `timeout` passes.
        fn until(&mut self, timeout: Duration, mut done: impl FnMut(&mut Self) -> bool) -> bool {
            let deadline = Instant::now() + timeout;
            loop {
                self.pump();
                if done(self) {
                    return true;
                }
                if Instant::now() >= deadline || self.exited {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn shows(&mut self, needle: &str, timeout: Duration) -> bool {
            self.until(timeout, |screen| screen.text().contains(needle))
        }

        /// The pid a probe wrote to `file` after `word`.
        fn probe_pid(&mut self, file: &Path, word: &str) -> u32 {
            let mut pid = None;
            let found = self.until(Duration::from_secs(30), |_| {
                pid = std::fs::read_to_string(file).ok().and_then(|text| {
                    text.lines()
                        .find_map(|line| line.strip_prefix(word)?.trim().parse().ok())
                });
                pid.is_some()
            });
            assert!(
                found,
                "probe never wrote {word:?}; pane shows:\n{}",
                self.text()
            );
            pid.unwrap()
        }

        /// The pane's shell runs a command and prints its result.
        fn shell_answers(&mut self) -> bool {
            // %OS% expands to text the typed line doesn't contain.
            self.type_line("echo SHELL_%OS%_OK");
            self.shows("SHELL_Windows_NT_OK", Duration::from_secs(15))
        }
    }

    /// Remove CSI and OSC sequences so text checks see what the user reads.
    fn strip_controls(raw: &[u8]) -> String {
        let text = String::from_utf8_lossy(raw);
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// A process opened by exact pid. Dropping it ends the process if it is
    /// still running.
    struct Process(HANDLE);

    impl Process {
        fn open(pid: u32) -> Self {
            let access =
                PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE;
            Self(unsafe { OpenProcess(access, BOOL(0), pid) }.expect("open probe process"))
        }

        fn alive(&self) -> bool {
            let waited = unsafe { WaitForSingleObject(self.0, 0) };
            waited == WAIT_TIMEOUT
        }

        /// Its exit code, once it exits within `timeout`.
        fn exit_code(&self, screen: &mut Screen, timeout: Duration) -> Option<u32> {
            let exited = screen.until(timeout, |_| {
                let waited = unsafe { WaitForSingleObject(self.0, 0) };
                waited == WAIT_OBJECT_0
            });
            exited.then(|| {
                let mut code = 0u32;
                let read = unsafe { GetExitCodeProcess(self.0, &mut code) };
                read.expect("exit code");
                code
            })
        }

        fn in_job(&self, job: &PaneJob) -> bool {
            job.contains(self.0.0 as RawHandle)
        }
    }

    impl Drop for Process {
        fn drop(&mut self) {
            unsafe {
                if self.alive() {
                    let _ = TerminateProcess(self.0, 1);
                }
                let _ = CloseHandle(self.0);
            }
        }
    }

    #[test]
    fn a_process_past_the_limit_dies_and_the_pane_carries_on() {
        let dir = scratch();
        let mut screen = shell(Some(TEST_LIMIT));
        let sibling_file = dir.join("sibling");
        screen.type_line(&format!(
            "start \"\" /b {}",
            probe_line(&["probe-sibling", "64", &sibling_file.display().to_string()])
        ));
        let sibling = Process::open(screen.probe_pid(&sibling_file, "ready"));

        let hog_file = dir.join("hog");
        screen.type_line(&probe_line(&[
            "probe-hog",
            "512",
            &hog_file.display().to_string(),
        ]));
        let hog_pid = screen.probe_pid(&hog_file, "start");
        let hog = Process::open(hog_pid);
        let code = hog.exit_code(&mut screen, Duration::from_secs(30));
        let report = std::fs::read_to_string(&hog_file).unwrap();
        assert!(
            !report.contains("done"),
            "512 MB was committed under a 256 MB limit: {report}"
        );
        assert!(
            code.is_some_and(|code| code != 0),
            "the hog didn't stop: {code:?}"
        );
        assert!(
            screen.shows(
                "memory allocation of 536870912 bytes failed",
                Duration::from_secs(10)
            ),
            "pane shows:\n{}",
            screen.text()
        );

        // The rest of the pane carries on.
        assert!(screen.shell_answers(), "pane shows:\n{}", screen.text());
        assert!(sibling.alive(), "the hog's sibling died with it");

        // muxel's own process is outside the job and can still allocate.
        let job = screen.session.pane_job().expect("the pane runs in a job");
        let muxel = unsafe { GetCurrentProcess() };
        assert!(!job.contains(muxel.0 as RawHandle));
        let block = vec![1u8; 512 << 20];
        assert_eq!(std::hint::black_box(&block).len(), 512 << 20);

        // The pane reports the hit once the hog has exited.
        let mut hits = Vec::new();
        screen.until(Duration::from_secs(5), |screen| {
            hits.extend(screen.session.take_memory_limit_hits());
            !hits.is_empty()
        });
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].pid, hog_pid);
        assert_eq!(hits[0].limit_bytes, TEST_LIMIT);
        assert!(hits[0].exited);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_process_the_shell_starts_at_once_is_limited_too() {
        let dir = scratch();
        let hog_file = dir.join("hog");
        let batch = dir.join("first.cmd");
        std::fs::write(
            &batch,
            format!(
                "@start \"\" /b {}\r\n",
                probe_line(&["probe-hog", "512", &hog_file.display().to_string()])
            ),
        )
        .unwrap();
        let spec = CommandSpec::program(
            "cmd.exe",
            vec![
                "/D".into(),
                "/Q".into(),
                "/K".into(),
                batch.display().to_string(),
            ],
        )
        .with_memory_limit(Some(TEST_LIMIT));
        let mut screen = Screen::new(spec);
        let hog = Process::open(screen.probe_pid(&hog_file, "start"));
        let in_job = screen.session.pane_job().map(|job| hog.in_job(job));
        hog.exit_code(&mut screen, Duration::from_secs(30))
            .expect("the hog exits");
        let report = std::fs::read_to_string(&hog_file).unwrap();
        assert!(
            !report.contains("done"),
            "a process started by the shell's first command escaped the limit: {report}"
        );
        assert_eq!(in_job, Some(true));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_detached_program_outlives_its_pane() {
        let dir = scratch();
        let mut screen = shell(Some(TEST_LIMIT));
        let file = dir.join("linger");
        screen.type_line(&format!(
            "start \"\" /b {}",
            probe_line(&["probe-linger", &file.display().to_string()])
        ));
        let lingerer = Process::open(screen.probe_pid(&file, "ready"));
        let job = screen.session.pane_job().expect("the pane runs in a job");
        assert!(
            lingerer.in_job(job),
            "the program never joined the pane's job"
        );

        // Close the pane as muxel does: kill its shell by pid, then drop the
        // session, which closes the console and muxel's job handle.
        screen.session.kill();
        drop(screen);
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            lingerer.alive(),
            "closing the pane ended a program detached from it"
        );
        drop(lingerer);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn a_pane_starts_without_a_limit_when_the_job_cannot_be_created() {
        let _refused = refuse(Refusal::Create);
        let mut screen = shell(Some(TEST_LIMIT));
        assert_eq!(screen.session.memory_limit(), None);
        assert!(screen.shell_answers(), "pane shows:\n{}", screen.text());
    }

    #[test]
    fn a_pane_starts_without_a_limit_when_windows_refuses_the_job() {
        let _refused = refuse(Refusal::Assign);
        let mut screen = shell(Some(TEST_LIMIT));
        assert_eq!(screen.session.memory_limit(), None);
        assert!(screen.shell_answers(), "pane shows:\n{}", screen.text());
    }

    /// `cargo test -p muxel-terminal --lib launch_overhead -- --ignored --nocapture`
    #[test]
    #[ignore = "measurement, run by hand"]
    fn launch_overhead() {
        const JOBS: u32 = 500;
        let started = Instant::now();
        for _ in 0..JOBS {
            drop(PaneJob::create(TEST_LIMIT).expect("create job"));
        }
        println!(
            "job create+limit+port+close: {:?} per pane ({JOBS} runs)",
            started.elapsed() / JOBS
        );

        const SPAWNS: usize = 30;
        let mut with = Vec::new();
        let mut without = Vec::new();
        for _ in 0..SPAWNS {
            for (limit, samples) in [(Some(TEST_LIMIT), &mut with), (None, &mut without)] {
                let spec =
                    CommandSpec::program("cmd.exe", vec!["/D".into(), "/Q".into(), "/K".into()])
                        .with_memory_limit(limit);
                let started = Instant::now();
                let (session, rx) = TerminalSession::spawn(spec, 120, 40).expect("spawn");
                samples.push(started.elapsed());
                assert_eq!(session.memory_limit(), limit);
                session.kill();
                drop(rx);
                drop(session);
            }
        }
        with.sort();
        without.sort();
        println!(
            "TerminalSession::spawn median: with limit {:?}, without {:?} ({SPAWNS} each, interleaved)",
            with[SPAWNS / 2],
            without[SPAWNS / 2]
        );
    }
}

#[cfg(test)]
mod hit_log_tests {
    use super::{HitLog, MemoryLimitHit, SETTLE};
    use std::time::{Duration, Instant};

    fn hit(pid: u32, exited: bool) -> MemoryLimitHit {
        MemoryLimitHit {
            pid,
            program: Some("awk.exe".into()),
            limit_bytes: 1 << 30,
            exited,
        }
    }

    #[test]
    fn a_hit_waits_for_its_process_to_exit_then_reports_once() {
        let start = Instant::now();
        let mut log = HitLog::default();
        assert!(log.hit(7, Some("awk.exe".into()), 1 << 30, start));
        assert!(!log.hit(7, Some("awk.exe".into()), 1 << 30, start));
        assert!(log.take(start).is_empty());
        log.exited(7);
        assert_eq!(log.take(start), [hit(7, true)]);
        assert!(log.take(start + SETTLE).is_empty());
    }

    #[test]
    fn a_process_that_survives_the_refusal_is_reported_still_running() {
        let start = Instant::now();
        let mut log = HitLog::default();
        log.hit(7, Some("awk.exe".into()), 1 << 30, start);
        assert!(
            log.take(start + SETTLE - Duration::from_millis(1))
                .is_empty()
        );
        assert_eq!(log.take(start + SETTLE), [hit(7, false)]);
        // Reported, but its exit still has to be seen.
        assert!(log.busy());
        // Still the same live process: asking again isn't a new hit.
        assert!(!log.hit(7, Some("awk.exe".into()), 1 << 30, start + SETTLE));
        log.exited(7);
        assert!(!log.busy());
        // A later process that reuses the id is.
        assert!(log.hit(7, Some("awk.exe".into()), 1 << 30, start + SETTLE));
    }

    #[test]
    fn exits_of_other_processes_change_nothing() {
        let start = Instant::now();
        let mut log = HitLog::default();
        log.hit(7, None, 1 << 30, start);
        log.exited(8);
        assert!(log.take(start).is_empty());
    }
}
