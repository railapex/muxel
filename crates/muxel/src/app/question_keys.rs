//! Keys while a question shows. A question is modal: no key pressed under it
//! reaches a pane behind it, and an unmodified Enter or Esc answers it.
//!
//! Modals take no focus, so a key travels from the window's root towards the
//! focused pane. The root's capture-phase listener sees it first and stops it
//! there. Tab and Shift+Tab are actions (`SendTab`, `SendBackTab`), which
//! dispatch ahead of any key listener, so their handlers check
//! [`MuxelApp::question_shown`] themselves.

use super::*;

/// A question a window can show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shown {
    /// The confirm dialog (a close, a project removal, a deletion).
    Confirm,
    /// The Quit question, in the main window.
    Quit,
}

/// How a key answers a question.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Answer {
    Confirm,
    Cancel,
}

/// The answer an unmodified Enter or Esc gives a question. Any other key,
/// or one with a modifier, answers nothing.
pub(super) fn answer(keystroke: &Keystroke) -> Option<Answer> {
    let keys = &keystroke.modifiers;
    if keys.control || keys.alt || keys.platform || keys.function || keys.shift {
        return None;
    }
    match keystroke.key.as_str() {
        "enter" => Some(Answer::Confirm),
        "escape" => Some(Answer::Cancel),
        _ => None,
    }
}

impl MuxelApp {
    /// The question showing in window `here`, if any: the confirm dialog in
    /// the window that draws it (a pop-out answers its own), then the Quit
    /// question in the main window.
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
        } else if in_main && self.show_quit_confirm {
            Some(Shown::Quit)
        } else {
            None
        }
    }

    /// A key travels from the window's root to the focused element, so with
    /// nothing on screen focused it never reaches the root's handlers: no
    /// focus at all, or a pane no longer drawn. A question that shows then
    /// focuses the root (`root`, its focus target), so Enter and Esc still
    /// answer it. A pane on screen keeps its focus, and the root's capture
    /// keeps keys from it.
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

    /// Keys for the question showing in `window`, ahead of the pane under it:
    /// none reaches the pane, and Enter or Esc answers. Bindings such as
    /// Ctrl+Q run before this, as they always do.
    pub(super) fn question_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(shown) = self.question_shown(window.window_handle()) else {
            return;
        };
        cx.stop_propagation();
        let Some(answer) = answer(&event.keystroke) else {
            return;
        };
        match (shown, answer) {
            (Shown::Confirm, Answer::Confirm) => self.run_confirm(window, cx),
            (Shown::Confirm, Answer::Cancel) => self.cancel_confirm(cx),
            (Shown::Quit, Answer::Confirm) => self.confirm_quit_question(cx),
            (Shown::Quit, Answer::Cancel) => self.cancel_quit_question(cx),
        }
    }

    /// The Quit question's Quit: kill the tmux sessions it was told to, and
    /// quit once the panes have closed.
    pub(super) fn confirm_quit_question(&mut self, cx: &mut Context<Self>) {
        self.kill_checked_tmux_sessions();
        self.confirm_quit = true;
        self.quit_after_closes(cx);
    }

    /// The Quit question's Cancel.
    pub(super) fn cancel_quit_question(&mut self, cx: &mut Context<Self>) {
        self.show_quit_confirm = false;
        cx.notify();
    }
}
