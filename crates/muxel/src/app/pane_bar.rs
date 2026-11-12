//! The pane bar (stop-and-return design, § 3): one row at the bottom of a
//! terminal pane for how its program ended and what waits for it, in the
//! order `muxel_core::pane_bar::arrange` gives. It takes the place of the exit
//! banner, and rescue's unsent-prompt offer is one of its lines. It pushes the
//! pane's screen up rather than drawing over it, so browser panes need no
//! overlay flag for it.

use super::*;
use muxel_core::pane_bar::{BarLine, Ending, Urgency, arrange};

impl MuxelApp {
    /// What pane `iid`'s bar shows, most urgent first.
    pub(super) fn pane_bar_lines(&self, iid: Uuid, cx: &App) -> Vec<BarLine> {
        let mut lines = Vec::new();
        if let Some(view) = self.terminals.get(&iid) {
            let view = view.read(cx);
            if view.exited() {
                // A remote pane mid-reconnect hasn't ended: its tmux session is
                // alive on the host and muxel is retrying.
                lines.push(if self.reconnecting.contains_key(&iid) {
                    BarLine::Reconnecting
                } else {
                    BarLine::Ended(Ending::from_exit(
                        view.exit_read_error().is_some(),
                        view.exit_signal(),
                        view.exit_code(),
                    ))
                });
            }
        }
        if let Some(offers) = self.rescue.offers.get(&iid)
            && !offers.is_empty()
        {
            lines.push(BarLine::Unsent {
                count: offers.len(),
            });
        }
        arrange(lines)
    }

    /// Pane `iid`'s bar, or None when it has nothing to say.
    pub(super) fn render_pane_bar(&self, iid: Uuid, cx: &mut Context<Self>) -> Option<AnyElement> {
        let lines = self.pane_bar_lines(iid, cx);
        if lines.is_empty() {
            return None;
        }
        let border = cx.theme().border;
        let mut bar = div()
            .id(format!("pane-bar-{iid}"))
            .debug_selector(move || format!("pane-bar-{}", iid.simple()))
            .flex_none()
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(border);
        let mut first = true;
        for line in &lines {
            let row = match line {
                BarLine::Ended(ending) => Some(self.render_ended_line(iid, ending, cx)),
                BarLine::Reconnecting => Some(Self::render_reconnecting_line(cx)),
                BarLine::Unsent { .. } => self.render_draft_offer(iid, cx),
            };
            if let Some(row) = row {
                bar = bar.child(if first {
                    row
                } else {
                    div()
                        .border_t_1()
                        .border_color(border)
                        .child(row)
                        .into_any_element()
                });
                first = false;
            }
        }
        Some(bar.into_any_element())
    }

    /// Enter in an ended pane starts it again, as its bar's Restart (or Run
    /// again) does. The pane's terminal passes only an ended pane's plain
    /// Enter up to the app, so a running program never loses one to the bar.
    pub(super) fn restart_on_enter(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if keystroke.key != "enter" || keystroke.modifiers.modified() {
            return false;
        }
        let Some(iid) = self.active_instance else {
            return false;
        };
        let Some(view) = self.terminals.get(&iid) else {
            return false;
        };
        let view = view.read(cx);
        if !view.exited()
            || !view.focus_handle(cx).is_focused(window)
            || self.reconnecting.contains_key(&iid)
        {
            return false;
        }
        if self.workspace.instance(iid).is_some_and(|i| i.literal_argv) {
            self.run_literal_again(iid, window, cx);
        } else if self.launch_permitted(iid, muxel_core::TerminalLaunchReason::Restart) {
            self.restart_agent(iid, window, cx);
        }
        true
    }

    /// `■ Ended · exit 1  [Restart]`: the program ended and the pane keeps its
    /// last screen. Restart (or Enter in the pane) starts it again; a literal
    /// command runs again instead. Tinted red when the exit cut work off,
    /// muted for a clean one.
    fn render_ended_line(&self, iid: Uuid, ending: &Ending, cx: &mut Context<Self>) -> AnyElement {
        let label: SharedString = match ending {
            Ending::ReadFailed => t("Ended · terminal read failed"),
            Ending::Signal(signal) => {
                tf("Ended · signal {sig}", &[("sig", signal.as_str())]).into()
            }
            Ending::Clean => t("Ended"),
            Ending::Code(code) => tf("Ended · exit {code}", &[("code", &code.to_string())]).into(),
            Ending::Unknown => t("Ended · exit code unknown"),
        };
        let loss = BarLine::Ended(ending.clone()).urgency() == Urgency::Loss;
        let (bg, fg) = if loss {
            (cx.theme().danger.opacity(0.15), cx.theme().foreground)
        } else {
            (
                cx.theme().muted_foreground.opacity(0.12),
                cx.theme().muted_foreground,
            )
        };
        let action = if self.workspace.instance(iid).is_some_and(|i| i.literal_argv) {
            Button::new(format!("run-again-{iid}"))
                .xsmall()
                .label(t("Run again"))
                .tooltip(t("Enter"))
                .disabled(!self.launch_permitted(iid, muxel_core::TerminalLaunchReason::RunAgain))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.run_literal_again(iid, window, cx)),
                )
        } else {
            Button::new(format!("pane-bar-restart-{iid}"))
                .xsmall()
                .primary()
                .label(t("Restart"))
                .tooltip(t("Enter"))
                .disabled(!self.launch_permitted(iid, muxel_core::TerminalLaunchReason::Restart))
                .on_click(
                    cx.listener(move |this, _, window, cx| this.restart_agent(iid, window, cx)),
                )
        };
        Self::bar_line(bg, fg, "■", label, Some(action.into_any_element()))
    }

    /// A remote pane whose connection dropped, while muxel reconnects it.
    fn render_reconnecting_line(cx: &mut Context<Self>) -> AnyElement {
        let hint = div()
            .text_color(cx.theme().muted_foreground)
            .child(t("Waiting for the host…"))
            .into_any_element();
        Self::bar_line(
            cx.theme().warning.opacity(0.15),
            cx.theme().warning,
            "↻",
            t("Connection lost — reconnecting…"),
            Some(hint),
        )
    }

    /// One bar line: glyph, words, and its action at the end.
    fn bar_line(
        bg: Hsla,
        fg: Hsla,
        glyph: &'static str,
        label: SharedString,
        action: Option<AnyElement>,
    ) -> AnyElement {
        div()
            .flex_none()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .px_2()
            .py_1()
            .text_xs()
            .bg(bg)
            .text_color(fg)
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_none().w(px(12.)).child(glyph))
                    .child(div().min_w_0().truncate().child(label)),
            )
            .children(action)
            .into_any_element()
    }
}
