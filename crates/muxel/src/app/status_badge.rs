//! Shared lifecycle badge colours and visible completion deadlines.
use gpui::{
    AnyElement, App, Hsla, IntoElement, ParentElement, Rgba, Styled, Task, Window, canvas, div, px,
};
use gpui_component::{ActiveTheme, Theme};
use muxel_core::status_badge::{done_age, next_done_fade};
use muxel_terminal::AgentStatus;
use std::{cell::RefCell, rc::Rc, time::Duration};

/// Owned by the window's view. Dropping it cancels its only outstanding wake.
#[derive(Clone, Default)]
pub(super) struct BadgeClock(Rc<RefCell<ClockState>>);

#[derive(Default)]
struct ClockState {
    painted_deadline: Option<i64>,
    armed_deadline: Option<i64>,
    wake: Option<Task<()>>,
}

impl BadgeClock {
    /// Bracket painting, rather than rendering: clipped/offscreen rows must not
    /// keep a completion timer alive. The canvases draw nothing and take no input.
    pub(super) fn frame(&self, content: impl IntoElement) -> AnyElement {
        let start = self.clone();
        let end = self.clone();
        div()
            .size_full()
            .relative()
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, _, _| {
                        start.0.borrow_mut().painted_deadline = None;
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .child(content)
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, _, window, cx| {
                        end.arm(window, cx);
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .into_any_element()
    }

    fn arm(&self, window: &mut Window, cx: &App) {
        let mut state = self.0.borrow_mut();
        let next = state.painted_deadline;
        if next == state.armed_deadline {
            return;
        }
        state.wake = None;
        state.armed_deadline = next;
        let Some(deadline) = next else { return };
        let weak = Rc::downgrade(&self.0);
        let handle = window.window_handle();
        state.wake = Some(cx.spawn(async move |cx| {
            let delay = deadline
                .saturating_sub(chrono::Utc::now().timestamp_millis())
                .max(1) as u64;
            cx.background_executor()
                .timer(Duration::from_millis(delay))
                .await;
            if let Some(clock) = weak.upgrade() {
                clock.borrow_mut().armed_deadline = None;
                let _ = handle.update(cx, |_, window, _| window.refresh());
            }
        }));
    }

    fn visible(&self, content: impl IntoElement, deadline: Option<i64>) -> AnyElement {
        let clock = self.clone();
        div()
            .relative()
            .flex_none()
            .child(content)
            .children(deadline.map(|deadline| {
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        if bounds.intersects(&window.content_mask().bounds) {
                            let mut state = clock.0.borrow_mut();
                            state.painted_deadline = Some(
                                state
                                    .painted_deadline
                                    .map_or(deadline, |at| at.min(deadline)),
                            );
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full()
            }))
            .into_any_element()
    }

    pub(super) fn tag(
        &self,
        status: AgentStatus,
        label: String,
        completed_at: Option<i64>,
        cx: &App,
    ) -> AnyElement {
        let now = chrono::Utc::now().timestamp_millis();
        let strength = if status == AgentStatus::Done {
            done_age(completed_at, now).strength()
        } else {
            1.0
        };
        let (bg, fg, border) = colors(status, strength, cx.theme());
        // Tag's built-in hover lowers the opacity of both foreground and
        // background. Keep these labels opaque even while the row is hovered.
        self.visible(
            div()
                .flex()
                .items_center()
                .border_1()
                .line_height(gpui::relative(1.0))
                .text_xs()
                .px_1p5()
                .py_0p5()
                .rounded(cx.theme().radius / 2.0)
                .bg(bg)
                .text_color(fg)
                .border_color(border)
                .child(label),
            (status == AgentStatus::Done)
                .then(|| next_done_fade(completed_at, now))
                .flatten(),
        )
    }

    pub(super) fn dot(
        &self,
        status: AgentStatus,
        completed_at: Option<i64>,
        cx: &App,
    ) -> AnyElement {
        let now = chrono::Utc::now().timestamp_millis();
        let strength = if status == AgentStatus::Done {
            done_age(completed_at, now).strength()
        } else {
            1.0
        };
        let (color, _, _) = colors(status, strength, cx.theme());
        self.visible(
            div().size(px(7.0)).rounded_full().bg(color),
            (status == AgentStatus::Done)
                .then(|| next_done_fade(completed_at, now))
                .flatten(),
        )
    }

    pub(super) fn icon(
        &self,
        program: Option<&str>,
        size: gpui::Pixels,
        status: AgentStatus,
        completed_at: Option<i64>,
        cx: &App,
    ) -> AnyElement {
        let now = chrono::Utc::now().timestamp_millis();
        let strength = if status == AgentStatus::Done {
            done_age(completed_at, now).strength()
        } else {
            1.0
        };
        let (color, _, _) = colors(status, strength, cx.theme());
        self.visible(
            super::agent_icon(program, size, color),
            (status == AgentStatus::Done)
                .then(|| next_done_fade(completed_at, now))
                .flatten(),
        )
    }
}

pub(super) fn colors(status: AgentStatus, strength: f32, theme: &Theme) -> (Hsla, Hsla, Hsla) {
    let accent = match status {
        AgentStatus::Working => theme.primary,
        AgentStatus::Idle => theme.secondary,
        AgentStatus::Blocked => theme.warning,
        AgentStatus::Done => theme.success,
    };
    let black = gpui::black();
    let white = gpui::white();
    let surface = mix(
        if theme.is_dark() { black } else { white },
        theme.background,
        1.0,
    );
    let quiet = mix(surface, theme.muted, 1.0);
    let strength = if status == AgentStatus::Done {
        strength
    } else {
        1.0
    };
    let background = mix(quiet, accent, strength);
    let preferred = Hsla {
        a: 1.0,
        ..theme.foreground
    };
    let foreground = if contrast(background, preferred) >= 4.5 {
        preferred
    } else if contrast(background, black) >= contrast(background, white) {
        black
    } else {
        white
    };
    // Retain a little green at the edge after the fill has become quiet.
    let border = mix(quiet, accent, strength.max(0.35));
    (background, foreground, border)
}

/// Resolve alpha here, so the same badge colours work over every row/hover fill.
fn mix(base: Hsla, tint: Hsla, strength: f32) -> Hsla {
    let base: Rgba = base.into();
    let tint: Rgba = tint.into();
    let amount = strength * tint.a;
    Rgba {
        r: base.r + (tint.r - base.r) * amount,
        g: base.g + (tint.g - base.g) * amount,
        b: base.b + (tint.b - base.b) * amount,
        a: 1.0,
    }
    .into()
}

fn contrast(a: Hsla, b: Hsla) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn luminance(color: Hsla) -> f32 {
    let Rgba { r, g, b, .. } = color.into();
    let linear = |v: f32| {
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

#[cfg(test)]
mod tests {
    use super::colors;
    use crate::theme::ThemeAssets;
    use gpui_component::{Theme, ThemeSet};
    use muxel_terminal::AgentStatus;
    use std::rc::Rc;

    #[test]
    fn every_bundled_badge_meets_text_contrast_at_every_fade_step() {
        let mut variants = 0;
        let mut failures = Vec::new();
        for file in ThemeAssets::iter() {
            let asset = ThemeAssets::get(&file).unwrap();
            let set: ThemeSet = serde_json::from_slice(&asset.data).unwrap();
            for config in set.themes {
                let name = config.name.clone();
                let mut theme = Theme::default();
                theme.apply_config(&Rc::new(config));
                variants += 1;
                for status in [
                    AgentStatus::Working,
                    AgentStatus::Idle,
                    AgentStatus::Blocked,
                    AgentStatus::Done,
                ] {
                    for band in muxel_core::status_badge::DoneAge::ALL {
                        let strength = band.strength();
                        let (bg, fg, _) = colors(status, strength, &theme);
                        if status != AgentStatus::Done {
                            assert_eq!(
                                colors(status, strength, &theme),
                                colors(status, 1.0, &theme)
                            );
                        }
                        assert_eq!(bg.a, 1.0);
                        assert_eq!(fg.a, 1.0);
                        // Independent WCAG calculation in f64, against the
                        // actual resolved colours handed to the renderer.
                        let luminance = |color: gpui::Hsla| {
                            let c: gpui::Rgba = color.into();
                            [c.r, c.g, c.b]
                                .into_iter()
                                .zip([0.2126, 0.7152, 0.0722])
                                .map(|(channel, weight)| {
                                    let v = f64::from(channel);
                                    weight
                                        * if v <= 0.04045 {
                                            v / 12.92
                                        } else {
                                            ((v + 0.055) / 1.055).powf(2.4)
                                        }
                                })
                                .sum::<f64>()
                        };
                        let (a, b) = (luminance(bg), luminance(fg));
                        let ratio = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                        if ratio < 4.5 {
                            failures.push(format!(
                                "{name}: {status:?} strength {strength}: {ratio:.2}:1"
                            ));
                        }
                    }
                }
            }
        }
        assert!(variants > 0);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        eprintln!("checked {variants} bundled theme variants");
    }

    #[test]
    fn only_painted_visible_completions_arm_the_soonest_wake() {
        use super::BadgeClock;
        use gpui::{IntoElement, ParentElement, Styled, TestAppContext, div, point, px, size};

        let mut app = TestAppContext::single();
        let cx = app.add_empty_window();
        let clock = BadgeClock::default();
        let now = chrono::Utc::now().timestamp_millis();
        let early = now + 60_000;
        let later = now + 120_000;
        let draw = |cx: &mut gpui::VisualTestContext, content: gpui::AnyElement| {
            cx.draw(
                point(px(0.0), px(0.0)),
                size(px(100.0), px(100.0)),
                |_, _| clock.frame(content).into_element(),
            );
        };
        draw(
            cx,
            div()
                .size_full()
                .overflow_hidden()
                .child(clock.visible(div().size(px(10.0)), Some(later)))
                .child(clock.visible(div().size(px(10.0)), Some(early)))
                .child(
                    div()
                        .absolute()
                        .top(px(200.0))
                        .child(clock.visible(div().size(px(10.0)), Some(now + 1))),
                )
                .into_any_element(),
        );
        assert_eq!(clock.0.borrow().armed_deadline, Some(early));
        assert!(clock.0.borrow().wake.is_some());
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_secs(61));
        cx.run_until_parked();
        assert_eq!(
            clock.0.borrow().armed_deadline,
            None,
            "the boundary wake fired"
        );
        draw(cx, clock.visible(div().size(px(10.0)), Some(later)));
        assert_eq!(clock.0.borrow().armed_deadline, Some(later));
        draw(cx, div().size_full().into_any_element());
        assert_eq!(clock.0.borrow().armed_deadline, None);
        assert!(clock.0.borrow().wake.is_none());
    }
}
