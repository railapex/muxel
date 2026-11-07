//! Lifecycle status pills whose labels stay readable in every theme.
use gpui::{
    App, Hsla, IntoElement, ParentElement, RenderOnce, Rgba, SharedString, Styled, Window, div,
    relative,
};
use gpui_component::{ActiveTheme, Theme};
use muxel_terminal::AgentStatus;

/// WCAG AA for normal text. The pill labels are extra-small text.
const MIN_LABEL_CONTRAST: f32 = 4.5;

/// A small status pill for an agent.
pub(super) fn status_tag(status: AgentStatus, label: impl Into<SharedString>) -> StatusPill {
    StatusPill {
        status,
        label: label.into(),
        outline: false,
    }
}

/// Laid out like gpui-component's small `Tag`, with [`pill_colors`]. `Tag` also
/// fades the whole pill to 90% on hover, which would let the row show through
/// the label; this pill stays opaque.
#[derive(IntoElement)]
pub(super) struct StatusPill {
    status: AgentStatus,
    label: SharedString,
    outline: bool,
}

impl StatusPill {
    /// Draw with [`outline_colors`] instead of a fill.
    pub(super) fn outline(mut self) -> Self {
        self.outline = true;
        self
    }
}

impl RenderOnce for StatusPill {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (fill, label, border) = if self.outline {
            outline_colors(self.status, theme)
        } else {
            pill_colors(self.status, theme)
        };
        div()
            .flex()
            .items_center()
            .border_1()
            .line_height(relative(1.0))
            .text_xs()
            .px_1p5()
            .py_0p5()
            .rounded(theme.radius / 2.0)
            .bg(fill)
            .text_color(label)
            .border_color(border)
            .child(self.label)
    }
}

/// A pill's fill, label and border colours. The fill is the theme's status
/// colour, made opaque over the sidebar. The label is the theme's own colour for
/// it when that reads at 4.5:1 against the fill, and otherwise black or white,
/// whichever reads better; one of the two always clears 4.5:1.
pub(super) fn pill_colors(status: AgentStatus, theme: &Theme) -> (Hsla, Hsla, Hsla) {
    let (fill, label, border) = match status {
        AgentStatus::Working => (theme.primary, theme.primary_foreground, theme.primary),
        AgentStatus::Idle => (theme.secondary, theme.secondary_foreground, theme.border),
        AgentStatus::Blocked => (theme.warning, theme.warning_foreground, theme.warning),
        AgentStatus::Done => (theme.success, theme.success_foreground, theme.success),
    };
    let base = if theme.is_dark() {
        gpui::black()
    } else {
        gpui::white()
    };
    let fill = over(fill, over(theme.sidebar, over(theme.background, base)));
    let label = over(label, fill);
    let label = if contrast(fill, label) >= MIN_LABEL_CONTRAST {
        label
    } else if contrast(fill, gpui::black()) >= contrast(fill, gpui::white()) {
        gpui::black()
    } else {
        gpui::white()
    };
    (fill, label, border)
}

/// An outline pill's colours, as `Tag::outline` draws them: no fill, and the
/// status colour as label and border.
pub(super) fn outline_colors(status: AgentStatus, theme: &Theme) -> (Hsla, Hsla, Hsla) {
    let (label, border) = match status {
        AgentStatus::Working => (theme.primary, theme.primary),
        AgentStatus::Idle => (theme.muted_foreground, theme.border),
        AgentStatus::Blocked => (theme.warning, theme.warning),
        AgentStatus::Done => (theme.success, theme.success),
    };
    (gpui::transparent_white(), label, border)
}

/// `top` composited over `bottom`, which is assumed opaque.
fn over(top: Hsla, bottom: Hsla) -> Hsla {
    let (t, b): (Rgba, Rgba) = (top.into(), bottom.into());
    let mix = |t: f32, b: f32| b + (t - b) * top.a;
    Rgba {
        r: mix(t.r, b.r),
        g: mix(t.g, b.g),
        b: mix(t.b, b.b),
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
