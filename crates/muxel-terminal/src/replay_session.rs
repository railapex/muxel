//! A terminal session with no child process, for replaying a recording.
//!
//! [`TerminalSession::replay`] builds the session a PTY launch builds, minus
//! the PTY. A recorded read goes in through [`ReplayFeed`], which hands it on
//! as the reader thread hands on a PTY read: query replies first, then shell
//! prompt marks, then the output queue with the time it was read. The queue is
//! drained like any pane's. What muxel writes to the child (keys, pastes and
//! query replies) collects in [`ReplayWrites`] instead of reaching a pipe.
//! Nothing waits on a process, so a replay runs the same on every OS.

use super::*;

/// Stands in for the PTY reader of a [`TerminalSession::replay`] session.
pub(crate) struct ReplayFeed {
    tx: PtyChunkSender,
    immediate_replies: ImmediateReplies,
    shell_marks: ShellMarks,
    shell_prompt: Arc<ShellPrompt>,
    input_closed: Arc<AtomicBool>,
    input_stop: std::sync::mpsc::Sender<Option<QueuedWrite>>,
}

impl ReplayFeed {
    /// One recorded read, handed on as the reader hands on a PTY read that
    /// returned at `read_at`.
    pub(crate) fn read(&mut self, bytes: &[u8], read_at: SystemTime) {
        deliver_read(
            bytes,
            read_at,
            None,
            &mut self.immediate_replies,
            &mut self.shell_marks,
            &self.shell_prompt,
            &self.tx,
        );
    }

    /// The child's exit, published as the reader publishes it after the last
    /// read: input closes, then Exit ends the stream.
    pub(crate) fn exit(self, code: Option<i32>) {
        self.input_closed.store(true, Ordering::Release);
        let _ = self.input_stop.send(None);
        self.tx.send_exit(code, None, None);
    }
}

/// What a [`TerminalSession::replay`] session wrote to its child.
pub(crate) struct ReplayWrites {
    rx: Mutex<std::sync::mpsc::Receiver<Option<QueuedWrite>>>,
    gate: Arc<InputGate>,
}

impl ReplayWrites {
    /// Everything written since the last call, in order, as the PTY writer
    /// would have passed it on: a write the input gate refuses at delivery is
    /// dropped, as the writer drops it.
    pub(crate) fn take(&self) -> Vec<u8> {
        let rx = self.rx.lock();
        let mut out = Vec::new();
        for write in rx.try_iter().flatten() {
            if write.admitted(&self.gate) {
                out.extend_from_slice(&write.bytes);
            }
        }
        out
    }
}

/// The child's killer, for a session that has no child.
#[derive(Debug)]
struct NoChild;

impl ChildKiller for NoChild {
    fn kill(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(NoChild)
    }
}

impl TerminalSession {
    /// A session at `cols` x `rows` with no child process. The feed stands in
    /// for the PTY reader, the receiver is drained as a pane drains its PTY,
    /// and the writes handle holds what muxel sent the child.
    pub(crate) fn replay(
        cols: u16,
        rows: u16,
    ) -> (Arc<Self>, ReplayFeed, ProfiledPtyReceiver, ReplayWrites) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let (tx, rx) = output_queue(
            PTY_OUTPUT_QUEUE_BYTE_CAPACITY,
            PTY_OUTPUT_QUEUE_CHUNK_CAPACITY,
        );
        let tx = PtyChunkSender::Profiled(tx);
        let (write_tx, write_rx) = std::sync::mpsc::channel::<Option<QueuedWrite>>();
        let input_closed = Arc::new(AtomicBool::new(false));
        let input_gate = Arc::new(InputGate::default());
        let shell_prompt = Arc::new(ShellPrompt::default());
        let channel_writer = |gate: Option<Arc<InputGate>>| -> SharedWriter {
            Arc::new(Mutex::new(Box::new(ChannelWriter {
                tx: write_tx.clone(),
                profile: false,
                closed: input_closed.clone(),
                gate,
            })))
        };
        let writer = channel_writer(Some(input_gate.clone()));
        let replies = Arc::new(Replies::new(channel_writer(None)));
        let palette = Arc::new(Mutex::new(TerminalPalette::default()));
        let term_config = term_config();
        let grid_size = Arc::new(GridSize::new(cols, rows));
        let immediate_replies = ImmediateReplies::new(
            replies.clone(),
            palette.clone(),
            grid_size.clone(),
            &term_config,
        );
        let output_queue = tx.queue_state();
        let feed = ReplayFeed {
            tx,
            immediate_replies,
            shell_marks: ShellMarks::default(),
            shell_prompt: shell_prompt.clone(),
            input_closed: input_closed.clone(),
            input_stop: write_tx.clone(),
        };
        let writes = ReplayWrites {
            rx: Mutex::new(write_rx),
            gate: input_gate.clone(),
        };

        let title = Arc::new(Mutex::new(None));
        let evidence = Arc::new(Mutex::new(LifecycleEvidence::for_launch(
            &LaunchBinding::Unknown,
        )));
        let title_generation = Arc::new(AtomicU64::new(0));
        let title_changed_at = Arc::new(Mutex::new(None));
        let session_id_hint = Arc::new(Mutex::new(None));
        let bell = Arc::new(AtomicBool::new(false));
        let clipboard_store = Arc::new(Mutex::new(Vec::new()));
        let listener = MuxelListener {
            replies,
            title: title.clone(),
            evidence: evidence.clone(),
            title_generation: title_generation.clone(),
            title_changed_at: title_changed_at.clone(),
            session_id_hint: session_id_hint.clone(),
            bell: bell.clone(),
            clipboard_store: clipboard_store.clone(),
        };
        let term = Term::new(
            term_config,
            &TermSize::new(cols as usize, rows as usize),
            listener,
        );
        let (runtime_trigger, runtime_events) = async_channel::bounded(1);

        let session = Arc::new(Self {
            id: Uuid::new_v4(),
            launched_at: SystemTime::now(),
            launch_resumed: false,
            term: Arc::new(Mutex::new(term)),
            processor: Mutex::new(Processor::new()),
            program_status_observer: Mutex::new(Observer::default()),
            program_status_generation: AtomicU64::new(0),
            observe_program_status_for_bench: AtomicBool::new(true),
            read_clock: Mutex::new(OutputReadClock::default()),
            writer,
            master: Arc::new(SessionMaster {
                inner: Mutex::new(None),
            }),
            output_queue,
            killer: Arc::new(Mutex::new(Box::new(NoChild))),
            input_closed,
            input_gate,
            shell_prompt,
            input_stop: write_tx,
            child_pid: None,
            #[cfg(unix)]
            child_identity: None,
            title,
            evidence,
            program_status: Arc::new(Mutex::new(ProgramStatusSnapshot::default())),
            title_generation,
            title_changed_at,
            session_id_hint,
            bell,
            clipboard_store,
            palette,
            selecting: AtomicBool::new(false),
            mouse_pressed_button: AtomicU8::new(NO_MOUSE_PRESS),
            scroll_accum: Mutex::new(0.0),
            scrollbar_drag: Mutex::new(None),
            grid_size,
            resize: Mutex::new(ResizeState {
                applied: (cols, rows),
                target: (cols, rows),
                since: Instant::now(),
            }),
            resize_revision: AtomicU64::new(0),
            search: Mutex::new(Vec::new()),
            local_paste: Mutex::new(None),
            path_paste_error: Mutex::new(None),
            text_paste_error: Mutex::new(None),
            paste_shell: PasteShell::default(),
            control_paste: crate::control_paste::ControlPaste::Terminal,
            cwd: None,
            hovered_link: Mutex::new(None),
            pointer_hit: Mutex::new(None),
            last_output: Mutex::new(Instant::now()),
            output_seen: AtomicBool::new(false),
            focused: AtomicBool::new(false),
            turn_submitted: AtomicBool::new(false),
            last_user_input: Mutex::new(None),
            submission_pending: AtomicBool::new(false),
            input_state: AtomicU64::new(0),
            runtime_trigger,
            runtime_events,
            content_gen: AtomicU64::new(1),
            paint_list: Mutex::new(None),
            pending_damage: Mutex::new(ContentDamage::Full),
            interactive_until: Mutex::new(None),
            tmux_input: None,
            #[cfg(windows)]
            pane_job: None,
            // No reader or child monitor runs: the feed does the reader's
            // work. Finished threads fill the handle fields.
            _reader: std::thread::spawn(|| {}),
            #[cfg(windows)]
            _monitor: std::thread::spawn(|| {}),
        });
        (session, feed, rx, writes)
    }
}
