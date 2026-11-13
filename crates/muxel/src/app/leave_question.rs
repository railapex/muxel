//! The leaving question as the app shows it. [`muxel_core::leaving::plan`]
//! decides whether closing, quitting, restarting or removing a project asks,
//! and what; this gathers its facts, names its rows, renders them in any
//! window, and answers its keys.

use super::pending_input::PendingKind;
use super::*;
use muxel_core::leaving::{self, Answer, Note, Pane, PaneKind, Pending, Question, Title};
use std::rc::Rc;
use std::time::Duration;

/// One row as shown: the pane's pill and name, then what the pill can't say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct LeaveRow {
    pub(super) pill: Option<(AgentStatus, SharedString)>,
    pub(super) name: SharedString,
    pub(super) note: Option<SharedString>,
    /// The sidebar's line for the session: what it is doing, or the question
    /// it asked. The sidebar design's S6 computes it; until then this is None
    /// and the row is its pill and name.
    pub(super) live_line: Option<SharedString>,
}

/// A leaving question's rows and buttons as shown, in any window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct LeaveView {
    pub(super) rows: Vec<LeaveRow>,
    /// "Close 2 idle", where that choice exists.
    pub(super) idle: Option<SharedString>,
    /// The confirm button is red: something would be lost for good.
    pub(super) danger: bool,
}

/// Which question a window shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shown {
    Confirm,
    Countdown,
    Quit,
}

fn note_label(note: Note) -> SharedString {
    match note {
        Note::MaybeBusy => t("maybe busy"),
        Note::UnmarkedPrompt => t("unmarked prompt"),
        Note::TmuxOpen => t("tmux open"),
        Note::AttachedElsewhere => t("attached elsewhere"),
        Note::Unsaved => t("unsaved"),
        Note::CtlSend => t("ctl send"),
        Note::CtlKeys => t("ctl keys"),
        Note::Uploading => t("uploading"),
        Note::PromptQueued => t("prompt queued"),
        Note::Launching => t("launching"),
        Note::Moving => t("moving"),
        Note::CloseUndecided => t("close undecided"),
        Note::Dictation => t("Dictation"),
        Note::WakingPanes => t("Waking panes"),
        Note::FileDialog => t("File dialog"),
        Note::Changed => t("just changed"),
    }
}

fn notes_label(notes: &[Note]) -> Option<SharedString> {
    (!notes.is_empty()).then(|| {
        notes
            .iter()
            .map(|&note| note_label(note).to_string())
            .collect::<Vec<_>>()
            .join(" · ")
            .into()
    })
}

/// The quiet word beside a key a button answers to.
pub(super) fn key_hint(key: &'static str) -> impl IntoElement {
    div().ml_1p5().text_xs().opacity(0.6).child(key)
}

/// A leaving question: its title, its rows, anything the question adds
/// (Quit's tmux choices), and its buttons. A click outside backs out.
pub(super) fn render_leave_dialog(
    prefix: &'static str,
    title: SharedString,
    view: &LeaveView,
    extra: Option<AnyElement>,
    confirm: SharedString,
    on_answer: impl Fn(Answer, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let on_answer = Rc::new(on_answer);
    let outside = on_answer.clone();
    modal_backdrop()
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            outside(Answer::Cancel, window, cx)
        })
        .child(
            div()
                .w(px(if view.rows.is_empty() { 360.0 } else { 480.0 }))
                .max_w(relative(0.95))
                .flex()
                .flex_col()
                .gap_3()
                .p_5()
                .bg(cx.theme().background)
                .border_1()
                .border_color(cx.theme().border)
                .rounded(cx.theme().radius_lg)
                .shadow_lg()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(div().text_lg().font_semibold().child(title))
                .children(render_leave_rows(view, cx))
                .children(extra)
                .child(leave_buttons(
                    prefix,
                    confirm,
                    view.idle.clone(),
                    view.danger,
                    move |answer, window, cx| on_answer(answer, window, cx),
                )),
        )
        .into_any_element()
}

/// The quit countdown's line of time, emptying over `duration` from its
/// first frame. `serial` tells countdowns apart, so each starts full.
pub(super) fn time_line(serial: u64, duration: Duration, cx: &App) -> impl IntoElement {
    div()
        .h(px(2.0))
        .w_full()
        .rounded_full()
        .bg(cx.theme().border)
        .child(
            div()
                .h_full()
                .w_full()
                .rounded_full()
                .bg(cx.theme().primary)
                .with_animation(
                    ElementId::NamedInteger("exit-countdown-line".into(), serial),
                    Animation::new(duration),
                    |line, delta| line.w(relative(1.0 - delta)),
                ),
        )
}

/// The question's rows, as every window draws them. A long list scrolls.
pub(super) fn render_leave_rows(view: &LeaveView, cx: &App) -> Option<AnyElement> {
    if view.rows.is_empty() {
        return None;
    }
    let warning = cx.theme().warning;
    let muted = cx.theme().muted_foreground;
    Some(
        div()
            .id("leave-rows")
            .flex()
            .flex_col()
            .gap_1p5()
            .max_h(px(260.0))
            .overflow_y_scroll()
            .children(view.rows.iter().map(|row| {
                div()
                    .debug_selector(|| "leave-row".into())
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .children(row.pill.clone().map(|(status, label)| {
                        div()
                            .debug_selector(|| format!("leave-pill-{status:?}"))
                            .flex_none()
                            .child(status_tag(status, label.to_string()))
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .child(row.name.clone()),
                    )
                    .children(
                        row.note.clone().map(|note| {
                            div().flex_none().text_xs().text_color(warning).child(note)
                        }),
                    )
                    .children(row.live_line.clone().map(|line| {
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_xs()
                            .text_color(muted)
                            .child(line)
                    }))
            }))
            .into_any_element(),
    )
}

/// The buttons every leaving question ends with: Cancel, the idle choice
/// where it exists, and the confirm button, each with the key it answers to.
pub(super) fn leave_buttons(
    prefix: &'static str,
    confirm: SharedString,
    idle: Option<SharedString>,
    danger: bool,
    on_answer: impl Fn(Answer, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let on_answer = Rc::new(on_answer);
    let click = move |answer: Answer| {
        let on_answer = on_answer.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| on_answer(answer, window, cx)
    };
    let ok = Button::new(SharedString::from(format!("{prefix}-ok")))
        .label(confirm)
        .child(key_hint("Enter"));
    let ok = if danger { ok.danger() } else { ok.primary() };
    div()
        .flex()
        .flex_wrap()
        .justify_end()
        .gap_2()
        .pt_2()
        .child(
            Button::new(SharedString::from(format!("{prefix}-cancel")))
                .ghost()
                .label(t("Cancel"))
                .child(key_hint("Esc"))
                .on_click(click(Answer::Cancel)),
        )
        .children(idle.map(|label| {
            Button::new(SharedString::from(format!("{prefix}-idle")))
                .ghost()
                .label(label)
                .child(key_hint("I"))
                .on_click(click(Answer::IdleOnly))
        }))
        .child(ok.on_click(click(Answer::Confirm)))
}

impl MuxelApp {
    /// The core's view of a pane a leave takes.
    pub(super) fn leave_pane(&self, iid: Uuid, cx: &App) -> Option<Pane> {
        let inst = self.workspace.instance(iid)?;
        let kind = if inst.kind == InstanceKind::Terminal {
            PaneKind::Terminal {
                facts: self.close_facts(iid, cx),
                unmarked_shell: self.unmarked_shell(iid, cx),
            }
        } else if let Some(editor) = self.close_editor(iid) {
            PaneKind::Editor {
                dirty: editor.read(cx).is_dirty(cx),
            }
        } else {
            PaneKind::Other
        };
        Some(Pane { id: iid, kind })
    }

    /// A shell that has never marked its prompt, so muxel can't tell its
    /// prompt from a command running. Only Windows reads a shell by its marks
    /// alone; elsewhere the foreground process says.
    fn unmarked_shell(&self, iid: Uuid, cx: &App) -> bool {
        cfg!(windows)
            && self
                .workspace
                .instance(iid)
                .is_some_and(|inst| !muxel_core::is_known_agent_program(inst.program.as_deref()))
            && self
                .terminal_view(iid)
                .is_some_and(|view| view.read(cx).session().shell_reading().marks == 0)
    }

    /// The input and work outside the panes' own state that quitting or
    /// restarting would cut short.
    pub(super) fn leave_pending(&self, cx: &App) -> Vec<Pending> {
        let mut pending: Vec<Pending> = self
            .pending_inputs
            .pending()
            .into_iter()
            .map(|entry| match entry.kind {
                PendingKind::CtlSend => Pending::CtlSend(entry.pane),
                PendingKind::CtlKeys => Pending::CtlKeys(entry.pane),
                PendingKind::Upload => Pending::Upload(entry.pane),
                PendingKind::Dictation => Pending::Dictation,
                PendingKind::WakeSweep => Pending::WakeSweep,
            })
            .collect();
        for (iid, session) in self.live_sessions(cx) {
            if session.pending_input_ops() != 0 {
                pending.push(Pending::StartupInput(iid));
            }
        }
        pending.extend(
            self.terminal_launching
                .keys()
                .map(|&iid| Pending::Launching(iid)),
        );
        pending.extend(
            self.pending_editor_rebuild
                .iter()
                .map(|(iid, _)| *iid)
                .chain(self.pending_editor_redock.iter().map(|(iid, _, _)| *iid))
                .map(Pending::EditorMove),
        );
        for group in &self.close_groups {
            if !muxel_core::recoverable_close::departure_finalizes(group.stage()) {
                pending.extend(
                    group
                        .members
                        .iter()
                        .map(|&iid| Pending::CloseUndecided(iid)),
                );
            }
        }
        if self.native_dialogs.get() > 0 {
            pending.push(Pending::FileDialog);
        }
        pending
    }

    /// A pane's pill, as the sidebar shows it: the same status, kept Done
    /// through a restore, and the same label.
    pub(super) fn leave_pill(&self, iid: Uuid, cx: &App) -> Option<(AgentStatus, SharedString)> {
        let inst = self.workspace.instance(iid)?;
        let view = self.terminal_view(iid)?.read(cx);
        let restoring = self.restoring_activity.contains(&iid);
        let status = persisted_status(
            shown_terminal_status(view, Some(inst), restoring),
            Some(inst),
            restoring,
        );
        let label = localized_activity_label(
            status,
            &inst.activity,
            view.background_work(),
            chrono::Utc::now().timestamp_millis(),
        );
        Some((status, label.into()))
    }

    /// A pane's name, or "A pane" for one outside the workspace, such as a
    /// runner's terminal.
    fn instance_name(&self, iid: Uuid) -> SharedString {
        self.workspace
            .instance(iid)
            .map(|inst| SharedString::from(inst.display_name().to_string()))
            .unwrap_or_else(|| t("A pane"))
    }

    /// `question`'s rows and buttons, named and with their pills.
    pub(super) fn leave_view(&self, question: &Question, cx: &App) -> LeaveView {
        let rows = question
            .rows
            .iter()
            .map(|row| match row.pane {
                Some(iid) => LeaveRow {
                    pill: self.leave_pill(iid, cx),
                    name: self.instance_name(iid),
                    note: notes_label(&row.notes),
                    live_line: None,
                },
                None => LeaveRow {
                    pill: None,
                    name: notes_label(&row.notes).unwrap_or_default(),
                    note: None,
                    live_line: None,
                },
            })
            .collect();
        LeaveView {
            rows,
            idle: question
                .idle
                .map(|n| tf("Close {n} idle", &[("n", n.to_string().as_str())]).into()),
            danger: question.danger,
        }
    }

    /// The question's title. `project` names the project a removal takes.
    pub(super) fn leave_title(&self, question: &Question, project: &str) -> SharedString {
        match question.title {
            Title::CloseOne(iid) => tf(
                "Close {name}?",
                &[("name", self.instance_name(iid).as_ref())],
            )
            .into(),
            Title::CloseMany { n, sessions: true } => {
                tf("Close {n} sessions?", &[("n", n.to_string().as_str())]).into()
            }
            Title::CloseMany { n, sessions: false } => {
                tf("Close {n} panes?", &[("n", n.to_string().as_str())]).into()
            }
            Title::Quit => t("Quit muxel?"),
            Title::Restart => t("Restart muxel?"),
            Title::RemoveProject => tf("Remove {project}?", &[("project", project)]).into(),
        }
    }

    /// The question showing in window `here`, if any: the confirm dialog
    /// where it renders (a pop-out answers its own), then, in the main
    /// window, the quit countdown and the Quit question.
    pub(super) fn question_shown(&self, here: AnyWindowHandle) -> Option<Shown> {
        let in_main = self.main_window.is_none_or(|main| main == here);
        let confirm_here = self.confirm.is_some()
            && self.confirm_popout_iid().is_none()
            && match self.confirm_window_pid() {
                Some(pid) => self
                    .secondary_windows
                    .iter()
                    .any(|sec| sec.pid == pid && sec.handle == here),
                None => in_main,
            };
        if confirm_here {
            Some(Shown::Confirm)
        } else if in_main && self.exit_countdown.is_some() {
            Some(Shown::Countdown)
        } else if in_main && self.show_quit_confirm {
            Some(Shown::Quit)
        } else {
            None
        }
    }

    /// A key travels from the window's root to the focused element, so with
    /// nothing on screen focused it never reaches the root's handlers:
    /// nothing focused at all, or a pane no longer drawn, such as one held
    /// for Undo. A question that shows then focuses the root (`root`, its
    /// focus target), and Enter and Esc still answer it. A pane on screen
    /// keeps its focus: the root's capture keeps keys from it.
    pub(super) fn focus_root_for_question(
        &self,
        root: &FocusHandle,
        window: &mut Window,
        cx: &mut App,
    ) {
        if !root.contains_focused(window, cx)
            && self.question_shown(window.window_handle()).is_some()
        {
            window.focus(root, cx);
        }
    }

    /// Keys for the question showing in `window`, ahead of the terminal under
    /// it. Modals take no focus, so the key comes here from the window's root.
    /// The question is modal: no key reaches a pane behind it, and an
    /// unmodified Enter, Esc or I answers it. Bindings such as Ctrl+Q run
    /// before this, as they always do.
    pub(super) fn dialog_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(shown) = self.question_shown(window.window_handle()) else {
            return;
        };
        cx.stop_propagation();
        let keys = &event.keystroke.modifiers;
        if keys.control || keys.alt || keys.platform || keys.function || keys.shift {
            return;
        }
        let idle = shown == Shown::Confirm
            && self
                .confirm
                .as_ref()
                .and_then(|pending| pending.leave.as_ref())
                .is_some_and(|leave| leave.idle.is_some());
        let Some(answer) = leaving::answer(event.keystroke.key.as_str(), idle) else {
            return;
        };
        match shown {
            Shown::Confirm => self.answer_confirm(answer, window, cx),
            Shown::Countdown => self.answer_countdown(answer, cx),
            Shown::Quit => self.answer_quit(answer, cx),
        }
    }

    /// An answer to the confirm dialog, by key or button.
    pub(super) fn answer_confirm(
        &mut self,
        answer: Answer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match answer {
            Answer::Confirm => self.run_confirm(window, cx),
            Answer::Cancel => self.cancel_confirm(cx),
            Answer::IdleOnly => self.run_close_alternative(cx),
        }
    }

    /// The confirm button's label.
    pub(super) fn leave_confirm_label(question: &Question) -> SharedString {
        match question.title {
            Title::CloseOne(_) => t("Close"),
            Title::CloseMany { n, .. } => {
                tf("Close all {n}", &[("n", n.to_string().as_str())]).into()
            }
            Title::Quit => t("Quit"),
            Title::Restart => t("Restart"),
            Title::RemoveProject => t("Remove"),
        }
    }
}
