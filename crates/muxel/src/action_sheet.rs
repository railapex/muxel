//! A window-owned action sheet. Its focus scope consumes every key until it
//! executes an action or is dismissed; no prefix or partial command is replayed.

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, Keystroke, MouseButton, ParentElement, Render, StatefulInteractiveElement, Styled,
    Subscription, Window, WindowId, div, px,
};
use gpui_component::{
    ActiveTheme, Disableable, Selectable, StyledExt,
    button::{Button, ButtonVariants},
    v_flex,
};

pub struct Row {
    pub id: &'static str,
    pub label: String,
    pub category: &'static str,
    pub key: &'static str,
    pub binding: String,
    pub unavailable: Option<&'static str>,
    pub action: Box<dyn gpui::Action>,
}

pub struct Dismissed {
    pub restore_focus: bool,
}

pub struct ActionSheet {
    owner: WindowId,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    rows: Vec<Row>,
    selected: usize,
    message: Option<String>,
    closed: bool,
    origin_is_current: Box<dyn Fn(&App) -> bool>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<Dismissed> for ActionSheet {}

impl ActionSheet {
    pub fn open(
        rows: Vec<Row>,
        origin_is_current: impl Fn(&App) -> bool + 'static,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx: &mut Context<Self>| {
            let focus = cx.focus_handle();
            let previous_focus = window.focused(cx);
            let owner = window.window_handle().window_id();
            let weak = cx.weak_entity();
            let keys = cx.intercept_keystrokes(move |event, window, cx| {
                let _ = weak.update(cx, |sheet, cx| {
                    if !sheet.closed
                        && sheet.owner == window.window_handle().window_id()
                        && sheet.focus.contains_focused(window, cx)
                    {
                        cx.stop_propagation();
                        window.prevent_default();
                        sheet.key(&event.keystroke, window, cx);
                    }
                });
            });
            let activation = cx.observe_window_activation(window, |sheet, window, cx| {
                if !window.is_window_active() {
                    sheet.dismiss(false, window, cx);
                }
            });
            let blur = cx.on_focus_out(&focus, window, |sheet, _, window, cx| {
                sheet.dismiss(false, window, cx);
            });
            window.focus(&focus, cx);
            let selected = rows
                .iter()
                .position(|row| row.unavailable.is_none())
                .unwrap_or(0);
            Self {
                owner,
                focus,
                previous_focus,
                rows,
                selected,
                message: None,
                closed: false,
                origin_is_current: Box::new(origin_is_current),
                _subscriptions: vec![keys, activation, blur],
            }
        })
    }

    pub fn owner(&self) -> WindowId {
        self.owner
    }

    fn dismiss(&mut self, restore_focus: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.closed = true;
        if restore_focus && let Some(previous) = &self.previous_focus {
            window.focus(previous, cx);
        }
        cx.emit(Dismissed { restore_focus });
        cx.notify();
    }

    fn invoke(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(index) else {
            return;
        };
        if let Some(reason) = row.unavailable {
            self.message = Some(reason.into());
            cx.notify();
            return;
        }
        // Revalidate the actual route. A closed/recreated target must not turn
        // the captured command into an action on another window or focus owner.
        if !(self.origin_is_current)(cx)
            || !self
                .previous_focus
                .as_ref()
                .is_some_and(|focus| window.is_action_available_in(row.action.as_ref(), focus))
        {
            self.message = Some("The original action target is no longer available.".into());
            cx.notify();
            return;
        }
        let action = row.action.boxed_clone();
        let target = self.previous_focus.clone();
        self.dismiss(false, window, cx);
        if let Some(target) = target {
            window.focus(&target, cx);
            target.dispatch_action(action.as_ref(), window, cx);
        }
    }

    fn key(&mut self, key: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        match key_intent(key, self.rows.iter().map(|row| row.key)) {
            Intent::Cancel => self.dismiss(true, window, cx),
            Intent::Move(delta) => {
                if !self.rows.is_empty() {
                    self.selected = (self.selected as isize + delta)
                        .rem_euclid(self.rows.len() as isize)
                        as usize;
                    self.message = None;
                    cx.notify();
                }
            }
            Intent::Confirm => self.invoke(self.selected, window, cx),
            Intent::Invoke(index) => self.invoke(index, window, cx),
            Intent::NoMatch => {
                self.message =
                    Some("No matching action. Choose a listed key, or press Escape.".into());
                cx.notify();
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Intent {
    Cancel,
    Move(isize),
    Confirm,
    Invoke(usize),
    NoMatch,
}

fn key_intent<'a>(key: &Keystroke, mut row_keys: impl Iterator<Item = &'a str>) -> Intent {
    if key.modifiers.control
        || key.modifiers.alt
        || key.modifiers.platform
        || key.modifiers.function
    {
        return Intent::NoMatch;
    }
    match key.key.as_str() {
        "escape" => Intent::Cancel,
        "up" => Intent::Move(-1),
        "down" => Intent::Move(1),
        "tab" => Intent::Move(if key.modifiers.shift { -1 } else { 1 }),
        "enter" => Intent::Confirm,
        _ => row_keys
            .position(|candidate| {
                candidate.eq_ignore_ascii_case(key.key_char.as_deref().unwrap_or(&key.key))
            })
            .map(Intent::Invoke)
            .unwrap_or(Intent::NoMatch),
    }
}

impl Focusable for ActionSheet {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for ActionSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut list = v_flex().gap_1();
        for (index, row) in self.rows.iter().enumerate() {
            let help = row
                .unavailable
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} · {}", row.category, row.binding));
            list = list.child(
                Button::new(gpui::SharedString::from(format!("action-sheet-{}", row.id)))
                    .ghost()
                    .w_full()
                    .label(format!("{}   {}", row.key.to_uppercase(), row.label))
                    .tooltip(help)
                    .selected(index == self.selected)
                    .disabled(row.unavailable.is_some())
                    .on_click(
                        cx.listener(move |sheet, _, window, cx| sheet.invoke(index, window, cx)),
                    ),
            );
        }
        div()
            .id("action-sheet-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(56.0))
            .bg(gpui::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|sheet, _, window, cx| {
                    sheet.dismiss(true, window, cx);
                }),
            )
            .child(
                div()
                    .id("action-sheet")
                    .track_focus(&self.focus)
                    .key_context("ActionSheet")
                    .w((window.viewport_size().width - px(32.0)).min(px(460.0)))
                    .max_h((window.viewport_size().height - px(80.0)).min(px(620.0)))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .bg(cx.theme().background)
                    .text_color(cx.theme().foreground)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_lg().font_semibold().child("App actions"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Choose a key or use ↑ ↓ and Enter. Escape cancels."),
                    )
                    .child(
                        div()
                            .id("action-sheet-list")
                            .overflow_y_scroll()
                            .min_h_0()
                            .child(list),
                    )
                    .children(self.rows.get(self.selected).map(|row| {
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                row.unavailable.map(str::to_owned).unwrap_or_else(|| {
                                    format!("{} · {}", row.category, row.binding)
                                }),
                            )
                    }))
                    .children(
                        self.message
                            .clone()
                            .map(|message| div().text_sm().child(message)),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Intent, key_intent};
    use gpui::Keystroke;

    #[test]
    fn modified_and_unknown_sheet_keys_never_become_actions() {
        for key in ["ctrl-t", "ctrl-shift-p", "alt-n", "z", "space"] {
            assert_eq!(
                key_intent(&Keystroke::parse(key).unwrap(), ["t", "n"].into_iter()),
                Intent::NoMatch
            );
        }
        assert_eq!(
            key_intent(&Keystroke::parse("t").unwrap(), ["t", "n"].into_iter()),
            Intent::Invoke(0)
        );
        assert_eq!(
            key_intent(&Keystroke::parse("escape").unwrap(), [].into_iter()),
            Intent::Cancel
        );
        assert_eq!(
            key_intent(&Keystroke::parse("shift-tab").unwrap(), [].into_iter()),
            Intent::Move(-1)
        );
    }
}
