//! Real shell close decisions, with the test owning every command's lifetime.

use super::{CommandSpec, PtyChunk, PtyReceiver, TerminalSession};
use muxel_core::control::AgentState;
use muxel_core::recoverable_close::{CloseDecision, CloseFacts, decide};
use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PROMPT: &str = "MUXEL_READY> ";

struct Shell {
    session: Arc<TerminalSession>,
    rx: PtyReceiver,
    job: Option<libc::pid_t>,
    marked: bool,
    pending: RefCell<String>,
}

impl Shell {
    fn start(marked: bool) -> Self {
        let mut spec = CommandSpec::program(
            "/bin/bash",
            vec![
                "--noprofile".into(),
                "--norc".into(),
                "-i".into(),
                "+H".into(),
            ],
        );
        spec.env_remove = vec!["PROMPT_COMMAND".into(), "BASH_ENV".into(), "ENV".into()];
        let prompt = if marked {
            format!("\\[\x1b]133;A\x07\\]{PROMPT}\\[\x1b]133;B\x07\\]")
        } else {
            PROMPT.into()
        };
        spec.env = vec![
            ("PS1".into(), prompt),
            ("HISTFILE".into(), "/dev/null".into()),
        ];
        let (session, rx) = TerminalSession::spawn(spec, 120, 24).expect("spawn bash");
        let shell = Self {
            session,
            rx,
            job: None,
            marked,
            pending: RefCell::new(String::new()),
        };
        shell.read_prompt();
        assert_eq!(shell.session.shell_at_prompt(), marked);
        shell
    }

    fn read_until(&self, marker: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut pending = self.pending.borrow_mut();
            if let Some(end) = pending.find(marker) {
                let rest = pending.split_off(end + marker.len());
                return std::mem::replace(&mut *pending, rest);
            }
            match self.rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    self.session.process_output(&bytes);
                    pending.push_str(&String::from_utf8_lossy(&bytes));
                }
                Ok(PtyChunk::Exit { .. }) => panic!("shell exited before {marker}"),
                Err(async_channel::TryRecvError::Closed) => panic!("shell stream closed"),
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "no {marker} in {:?}", *pending);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    fn read_prompt(&self) -> String {
        self.read_until(if self.marked {
            "\x1b]133;B\x07"
        } else {
            PROMPT
        })
    }

    fn close_decision(&self) -> CloseDecision {
        decide(
            CloseFacts {
                status: AgentState::Idle,
                evidence: self.session.shell_close_evidence(None, false),
                attributable: true,
                awaiting_reply: false,
                another_tmux_client: false,
                session_may_outlive: false,
                launch_id: None,
                binding_rev: 0,
                input_rev: 0,
                completion_rev: 0,
            },
            true,
        )
    }

    fn start_job(&mut self, foreground: bool) {
        // $! names exactly the process this shell started. No wait for sleep
        // to finish: the test kills it and the shell explicitly reaps it.
        self.session
            .write_input(b"sleep 30 & muxel_job=$!; printf '\\nCHILD:%s\\n' \"$muxel_job\"\n");
        // Wait for the command's PID output before accepting its next prompt.
        self.read_until("\nCHILD:");
        let pid_line = self.read_until("\n");
        self.job = Some(
            pid_line
                .trim()
                .parse()
                .expect("the shell reported its child's pid"),
        );
        self.read_prompt();
        if foreground {
            self.session.write_input(b"fg\n");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let group = self
                    .session
                    .master
                    .inner
                    .lock()
                    .as_ref()
                    .and_then(|master| master.process_group_leader());
                if group == self.job {
                    break;
                }
                assert!(Instant::now() < deadline, "job never took the foreground");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn end_job(&mut self) {
        let pid = self.job.expect("test job");
        // SAFETY: this is the exact live child reported by this shell. The
        // shell waits below before another command or assertion can run.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        self.job = None;
        self.session
            .write_input(b"wait \"$muxel_job\"; printf '\\nJOB_%s\\n' REAPED\n");
        self.read_until("JOB_REAPED");
        self.read_prompt();
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        if let Some(pid) = self.job.take() {
            // SAFETY: cleanup of this test's child if an assertion failed.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        self.session.kill();
    }
}

#[test]
fn an_idle_prompt_is_idle() {
    for marked in [false, true] {
        let shell = Shell::start(marked);
        assert!(shell.session.is_idle_foreground());
        assert_eq!(
            shell.close_decision(),
            CloseDecision::Hold { forced: false }
        );
    }
}

#[test]
fn a_background_job_asks_before_close_until_that_pid_is_killed() {
    for marked in [false, true] {
        let mut shell = Shell::start(marked);
        shell.start_job(false);
        assert_eq!(shell.session.shell_at_prompt(), marked);
        assert_eq!(
            shell.close_decision(),
            CloseDecision::Ask,
            "a background job behind the prompt must ask (OSC 133: {marked})"
        );
        assert!(!shell.session.is_idle_foreground());
        shell.end_job();
        assert!(shell.session.is_idle_foreground());
        assert_eq!(
            shell.close_decision(),
            CloseDecision::Hold { forced: false }
        );
    }
}

#[test]
fn a_foreground_command_is_not_idle() {
    let mut shell = Shell::start(false);
    shell.start_job(true);
    assert!(!shell.session.is_idle_foreground());
    assert_eq!(shell.close_decision(), CloseDecision::Ask);
    shell.end_job();
    assert!(shell.session.is_idle_foreground());
}
