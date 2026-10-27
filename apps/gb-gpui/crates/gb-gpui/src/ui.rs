//! Small chrome pieces shared by the three columns, the lane ledger and the
//! header. Type scale and spacing stay at the call site; only shapes live here.

use ely_gpui_component::buttons::{ButtonVariant, IconButton};
use ely_gpui_component::primitives::IconName;
use ely_gpui_component::theme::ControlSize;
use gpui::{
    App, ClickEvent, IntoElement, ParentElement, SharedString, Styled, Window, div, prelude::*, px,
    relative,
};
use ns_ui::Theme;

/// The 2px lane mark at a header or row's leading edge: mint when the lane,
/// column or row is the current one, a border hairline otherwise.
pub fn rail(theme: &Theme, on: bool) -> impl IntoElement {
    div()
        .w(px(2.))
        .h(px(14.))
        .flex_none()
        .bg(if on { theme.accent } else { theme.border })
}

/// The same rail, full height, for a band that is a flex row with default
/// stretch: it runs the whole lane, the window's signature.
pub fn lane_rail(theme: &Theme, on: bool) -> impl IntoElement {
    div()
        .w(px(2.))
        .flex_none()
        .bg(if on { theme.accent } else { theme.border })
}

/// The product wordmark. A wordmark is the one place tracked caps are legal;
/// this one stays sentence case and untracked to match the mono chrome.
pub fn wordmark(theme: &Theme) -> impl IntoElement {
    div()
        .flex_none()
        .text_sm()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text)
        .child("GitButler")
}

/// The thin chevron between the wordmark and the project name. A glyph, not a
/// middle dot, so the pair reads as one breadcrumb.
pub fn crumb(theme: &Theme) -> impl IntoElement {
    div()
        .flex_none()
        .text_sm()
        .text_color(theme.muted)
        .child("›")
}

/// A ghost icon control at the header's 32px size. Every one carries its name
/// in a tooltip, because the bar shows no text.
pub fn icon_action(
    id: &'static str,
    icon: IconName,
    tip: impl Into<SharedString>,
    disabled: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    IconButton::new(id, icon)
        .variant(ButtonVariant::Ghost)
        .size(ControlSize::Lg)
        .disabled(disabled)
        .tooltip(tip)
        .on_click(on_click)
}

/// A pane that is loading: the mint rail, the work in progress, and three
/// quiet bars where the rows will be. Static, so it never asks for motion.
pub fn loading_pane(
    id: &'static str,
    label: impl Into<SharedString>,
    theme: &Theme,
) -> impl IntoElement {
    div()
        .id(id)
        .w_full()
        .p_3()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .h(px(16.))
                .child(rail(theme, true))
                .child(div().text_xs().text_color(theme.muted).child(label.into())),
        )
        .child(bar(theme, 0.42))
        .child(bar(theme, 0.78))
        .child(bar(theme, 0.6))
}

fn bar(theme: &Theme, width: f32) -> impl IntoElement {
    div()
        .h(px(8.))
        .w(relative(width))
        .rounded(px(2.))
        .bg(theme.raised)
}
