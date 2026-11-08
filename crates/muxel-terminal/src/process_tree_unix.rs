//! A Unix shell is quiet only when it has no child process, including jobs
//! running behind its prompt. Failure to inspect children never proves idle.

pub(crate) fn shell_has_no_children(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }

    #[cfg(target_os = "linux")]
    {
        linux::has_no_children(std::path::Path::new("/proc"), pid).unwrap_or(false)
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: signal 0 only checks the shell's existence; no signal is sent.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        let mut child: libc::pid_t = 0;
        // One slot suffices: any child means busy, even if more did not fit.
        // libproc maps some errors to a zero count, so clear and read errno
        // on this thread around the call to distinguish failure from no child.
        // SAFETY: __error points to this thread's errno; child is a writable,
        // aligned pid_t and the buffer size describes exactly that one slot.
        let (count, error) = unsafe {
            *libc::__error() = 0;
            let count = libc::proc_listchildpids(
                pid,
                (&mut child as *mut libc::pid_t).cast(),
                std::mem::size_of_val(&child) as libc::c_int,
            );
            (count, *libc::__error())
        };
        count == 0 && error == 0
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs;
    use std::io::{self, ErrorKind};
    use std::path::Path;

    pub(super) fn has_no_children(proc: &Path, pid: libc::pid_t) -> io::Result<bool> {
        match task_children(proc, pid) {
            // Kernels without CONFIG_PROC_CHILDREN have no children files.
            // Scan parent pids instead; other read errors remain uncertain.
            Err(error) if error.kind() == ErrorKind::NotFound => no_children_by_parent(proc, pid),
            result => result,
        }
    }

    fn task_children(proc: &Path, pid: libc::pid_t) -> io::Result<bool> {
        let mut saw_task = false;
        for task in fs::read_dir(proc.join(pid.to_string()).join("task"))? {
            let children = fs::read(task?.path().join("children"))?;
            if children.iter().any(|byte| !byte.is_ascii_whitespace()) {
                return Ok(false);
            }
            saw_task = true;
        }
        // An empty task directory does not establish a living shell.
        Ok(saw_task)
    }

    pub(super) fn no_children_by_parent(proc: &Path, pid: libc::pid_t) -> io::Result<bool> {
        let mut saw_shell = false;
        for process in fs::read_dir(proc)? {
            let process = process?;
            let Some(process_pid) = process
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::pid_t>().ok())
            else {
                continue;
            };
            let stat = match fs::read(process.path().join("stat")) {
                Ok(stat) => stat,
                // A process that exited during the walk has no work to lose.
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if parent_pid(&stat)? == pid {
                return Ok(false);
            }
            saw_shell |= process_pid == pid;
        }
        Ok(saw_shell)
    }

    fn parent_pid(stat: &[u8]) -> io::Result<libc::pid_t> {
        // comm is parenthesized and can contain spaces, ')' and non-UTF8.
        // The fields after its final ')' start with state, then parent pid.
        let invalid = || io::Error::new(ErrorKind::InvalidData, "invalid process stat");
        let end = stat
            .iter()
            .rposition(|&byte| byte == b')')
            .ok_or_else(invalid)?;
        let parent = stat[end + 1..]
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty())
            .nth(1)
            .ok_or_else(invalid)?;
        std::str::from_utf8(parent)
            .ok()
            .and_then(|parent| parent.parse().ok())
            .ok_or_else(invalid)
    }

    #[cfg(test)]
    mod tests {
        use super::{has_no_children, parent_pid};
        use std::fs;
        use std::path::PathBuf;

        struct Proc(PathBuf);

        impl Proc {
            fn new() -> Self {
                let root =
                    std::env::temp_dir().join(format!("muxel-proc-{}", uuid::Uuid::new_v4()));
                fs::create_dir_all(root.join("40/task/40")).unwrap();
                fs::write(root.join("40/stat"), b"40 (bash) S 1 40 40").unwrap();
                Self(root)
            }

            fn write(&self, path: &str, content: &[u8]) {
                let path = self.0.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, content).unwrap();
            }
        }

        impl Drop for Proc {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn children_of_every_shell_thread_count() {
            let proc = Proc::new();
            proc.write("40/task/40/children", b"");
            proc.write("40/task/41/children", b"51 ");
            assert!(!has_no_children(&proc.0, 40).unwrap());
        }

        #[test]
        fn missing_children_files_fall_back_to_parent_pids() {
            let proc = Proc::new();
            proc.write("7/stat", b"7 (unrelated) S 1 7 7");
            assert!(has_no_children(&proc.0, 40).unwrap());
            proc.write("51/stat", b"51 (sleep) S 40 51 40");
            assert!(!has_no_children(&proc.0, 40).unwrap());
            fs::remove_dir_all(proc.0.join("51")).unwrap();
            assert!(has_no_children(&proc.0, 40).unwrap());
        }

        #[test]
        fn failed_reads_cannot_prove_idle() {
            let proc = Proc::new();
            // A directory in place of a file causes a read error even as root.
            fs::create_dir(proc.0.join("40/task/40/children")).unwrap();
            assert!(has_no_children(&proc.0, 40).is_err());
            fs::remove_dir(proc.0.join("40/task/40/children")).unwrap();
            proc.write("7/stat", b"unreadable parent fields");
            assert!(has_no_children(&proc.0, 40).is_err());
            fs::remove_dir_all(proc.0.join("7")).unwrap();
            assert!(!has_no_children(&proc.0, 99).unwrap());
        }

        #[test]
        fn stat_names_do_not_shift_parent_fields() {
            assert_eq!(parent_pid(b"51 (a name ) with \xff) S 40 51").unwrap(), 40);
            assert!(parent_pid(b"51 (no parent) S").is_err());
        }
    }
}
