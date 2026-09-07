//! A window-owned action sheet. Its focus scope consumes every key until it
//! executes an action or is dismissed; no prefix or partial command is replayed.

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, Keystroke, MouseButton, ParentElement, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, Subscription, Window, WindowId, div, px,
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
    /// Whether, and when, OS focus returns to the native page the sheet was
    /// opened from.
    pub native_focus: NativeFocus,
}

/// Returning OS focus to a native page moves the OS focus, so it waits until
/// the user is in the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeFocus {
    /// Leave it where it is.
    Keep,
    /// Return it now: the user is still in this window.
    Now,
    /// Return it when the window is next active, if the page still owns focus.
    OnReturn,
}

/// Why the sheet closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dismissal {
    /// Escape or a click outside the sheet: the user stays in this window.
    Cancel,
    /// The owning window lost OS activation.
    Deactivated,
    /// Something else in the window took focus.
    FocusMoved,
    /// An action ran; `invoke` focuses its own target.
    Invoked,
}

#[derive(Debug, PartialEq, Eq)]
struct FocusReturn {
    gpui: bool,
    native: NativeFocus,
}

/// Where focus goes when the sheet closes.
///
/// GPUI focus is per window and sends no focus-in while the window is
/// inactive, so returning it on deactivation only decides where keys go once
/// the user comes back; without it they reach no pane until a click. Native
/// focus moves OS focus, so after deactivation it waits for the user's return.
fn focus_return(dismissal: Dismissal, sheet_focused: bool) -> FocusReturn {
    match dismissal {
        Dismissal::Cancel => FocusReturn {
            gpui: true,
            native: NativeFocus::Now,
        },
        Dismissal::Deactivated if sheet_focused => FocusReturn {
            gpui: true,
            native: NativeFocus::OnReturn,
        },
        Dismissal::Deactivated => FocusReturn {
            gpui: false,
            native: NativeFocus::Keep,
        },
        Dismissal::FocusMoved | Dismissal::Invoked => FocusReturn {
            gpui: false,
            native: NativeFocus::Keep,
        },
    }
}

/// Page focus owed to windows whose sheet closed with
/// [`NativeFocus::OnReturn`], keyed by window `W`. `P` names the pane that held
/// the window's focus; `isize` is the page's saved native focus.
#[cfg(any(target_os = "windows", test))]
pub struct NativeReturns<W, P> {
    owed: Vec<(W, P, isize)>,
}

#[cfg(any(target_os = "windows", test))]
impl<W, P> Default for NativeReturns<W, P> {
    fn default() -> Self {
        Self { owed: Vec::new() }
    }
}

#[cfg(any(target_os = "windows", test))]
impl<W: PartialEq, P: PartialEq> NativeReturns<W, P> {
    /// A sheet opened in `window`, which owns its page focus now, or `window`
    /// closed. Other windows keep theirs.
    pub fn forget(&mut self, window: &W) {
        self.owed.retain(|(owner, ..)| owner != window);
    }

    /// `window`'s sheet closed on deactivation while `pane` held its focus.
    pub fn owe(&mut self, window: W, pane: P, focus: isize) {
        self.forget(&window);
        self.owed.push((window, pane, focus));
    }

    /// `window` is active again with `pane` holding its focus. If that is the
    /// pane owed page focus, `restore` gets the saved focus and says whether
    /// it is used up. Only another pane holding the focus drops the return.
    /// With no pane (the page is hidden, or nothing focusable holds focus) or
    /// a page that cannot take it yet, it waits for the next activation.
    pub fn resume(&mut self, window: &W, pane: Option<&P>, restore: impl FnOnce(isize) -> bool) {
        let Some(at) = self.owed.iter().position(|(owner, ..)| owner == window) else {
            return;
        };
        let (_, owed, focus) = &self.owed[at];
        let spent = match pane {
            None => false,
            Some(pane) if pane != owed => true,
            Some(_) => restore(*focus),
        };
        if spent {
            self.owed.swap_remove(at);
        }
    }
}

pub struct ActionSheet {
    owner: WindowId,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    rows: Vec<Row>,
    selected: usize,
    scroll: ScrollHandle,
    /// The selected row still has to be scrolled into view.
    reveal: bool,
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
                    sheet.dismiss(Dismissal::Deactivated, window, cx);
                }
            });
            // GPUI reports focus out of an inactive window too, so blur while
            // inactive is the same deactivation seen from the focus side.
            let blur = cx.on_focus_out(&focus, window, |sheet, _, window, cx| {
                let dismissal = if window.is_window_active() {
                    Dismissal::FocusMoved
                } else {
                    Dismissal::Deactivated
                };
                sheet.dismiss(dismissal, window, cx);
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
                scroll: ScrollHandle::new(),
                // The first available row can sit below a short window's fold.
                reveal: true,
                message: None,
                closed: false,
                origin_is_current: Box::new(origin_is_current),
                _subscriptions: vec![keys, activation, blur],
            }
        })
    }

    /// Select a row and scroll it into view on the next render. The list
    /// scrolls inside a sheet capped to the window, and Enter must not run a
    /// row the user can't see.
    fn select(&mut self, index: usize) {
        self.selected = index;
        self.reveal = true;
    }

    pub fn owner(&self) -> WindowId {
        self.owner
    }

    fn dismiss(&mut self, dismissal: Dismissal, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.closed = true;
        let focus = focus_return(dismissal, self.focus.is_focused(window));
        if focus.gpui
            && let Some(previous) = &self.previous_focus
        {
            window.focus(previous, cx);
        }
        cx.emit(Dismissed {
            native_focus: focus.native,
        });
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
        self.dismiss(Dismissal::Invoked, window, cx);
        if let Some(target) = target {
            window.focus(&target, cx);
            target.dispatch_action(action.as_ref(), window, cx);
        }
    }

    fn key(&mut self, key: &Keystroke, window: &mut Window, cx: &mut Context<Self>) {
        match key_intent(key, self.rows.iter().map(|row| row.key)) {
            Intent::Cancel => self.dismiss(Dismissal::Cancel, window, cx),
            Intent::Move(delta) => {
                if !self.rows.is_empty() {
                    self.select(
                        (self.selected as isize + delta).rem_euclid(self.rows.len() as isize)
                            as usize,
                    );
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
        if self.reveal {
            self.scroll.scroll_to_item(self.selected);
            // GPUI learns the list scrolls only while laying it out, and drops
            // a request made before that first layout. Repeat it next frame.
            if self.rows.is_empty() || self.scroll.bounds_for_item(self.selected).is_some() {
                self.reveal = false;
            } else {
                cx.on_next_frame(window, |_, _, cx| cx.notify());
            }
        }
        // The rows are the scroll container's direct children, so
        // `scroll_to_item` can find the selected one by index.
        let mut list = v_flex()
            .id("action-sheet-list")
            .gap_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll);
        for (index, row) in self.rows.iter().enumerate() {
            let help = row
                .unavailable
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{} · {}", row.category, row.binding));
            list = list.child(
                Button::new(gpui::SharedString::from(format!("action-sheet-{}", row.id)))
                    .ghost()
                    .w_full()
                    .flex_shrink_0()
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
            // Nothing behind the backdrop sees the mouse: no click, scroll or
            // hover reaches a pane, and the focusable app root under it can't
            // take focus on mouse down after a click-away has returned it.
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(56.0))
            .bg(gpui::black().opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|sheet, _, window, cx| {
                    cx.stop_propagation();
                    sheet.dismiss(Dismissal::Cancel, window, cx);
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
                    .child(list)
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
    use super::{
        Dismissal, FocusReturn, Intent, NativeFocus, NativeReturns, focus_return, key_intent,
    };
    use gpui::Keystroke;

    /// Activates `window` with `pane` holding its focus, while the owed page
    /// is `shown` or hidden. Returns the focus the page was handed.
    fn activate(
        returns: &mut NativeReturns<&'static str, &'static str>,
        window: &'static str,
        pane: Option<&'static str>,
        shown: bool,
    ) -> Option<isize> {
        let mut handed = None;
        returns.resume(&window, pane.as_ref(), |focus| {
            if shown {
                handed = Some(focus);
            }
            shown
        });
        handed
    }

    #[test]
    fn a_window_keeps_its_page_focus_through_work_in_another_window() {
        let (a, b) = ("window A", "window B");
        let (page, other) = ("A's browser pane", "another pane");
        let mut returns = NativeReturns::default();
        // A's sheet, opened from its page, closes as the user switches to B.
        returns.owe(a, page, 7);
        // In B: activation, a sheet of its own, then another pane selected.
        // A's pane is read from A's window, so the selection in B leaves it.
        assert_eq!(activate(&mut returns, b, Some(other), true), None);
        returns.forget(&b);
        assert_eq!(activate(&mut returns, b, Some(other), true), None);
        // B shows main's dashboard over A's page: back in A, no pane holds
        // A's focus, so the return waits.
        assert_eq!(activate(&mut returns, a, None, true), None);
        // An overlay hides A's page: it holds A's focus but cannot take it yet.
        assert_eq!(activate(&mut returns, a, Some(page), false), None);
        // Next time in A, with the page shown: the page gets it, once.
        assert_eq!(activate(&mut returns, a, Some(page), true), Some(7));
        assert_eq!(activate(&mut returns, a, Some(page), true), None);

        // Another pane holds A's focus: the return is dropped.
        returns.owe(a, page, 7);
        assert_eq!(activate(&mut returns, a, Some(other), true), None);
        assert_eq!(activate(&mut returns, a, Some(page), true), None);
        // A new sheet in A owns A's page focus; a closed A owes nothing.
        returns.owe(a, page, 7);
        returns.forget(&a);
        assert_eq!(activate(&mut returns, a, Some(page), true), None);
    }

    #[test]
    fn deactivation_returns_keys_to_the_origin_without_taking_os_focus() {
        // Escape or click-away: back to the pane, native page included.
        assert_eq!(
            focus_return(Dismissal::Cancel, true),
            FocusReturn {
                gpui: true,
                native: NativeFocus::Now
            }
        );
        // Alt+Tab away: typing must reach the pane again on return. A native
        // page gets OS focus back only once the user returns, never while
        // another app is in front.
        assert_eq!(
            focus_return(Dismissal::Deactivated, true),
            FocusReturn {
                gpui: true,
                native: NativeFocus::OnReturn
            }
        );
        // Something else already took focus: leave it there.
        for (dismissal, sheet_focused) in [
            (Dismissal::Deactivated, false),
            (Dismissal::FocusMoved, false),
            (Dismissal::Invoked, true),
        ] {
            assert_eq!(
                focus_return(dismissal, sheet_focused),
                FocusReturn {
                    gpui: false,
                    native: NativeFocus::Keep
                },
                "{dismissal:?}"
            );
        }
    }

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
