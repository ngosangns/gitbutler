// Hallmark v2.0.0 · GitButler · theme: GitButler brand, mint #A9F0EC on tinted near-black read from the installed icon · shape: app screen · scope: app · redesign: full · nav: 44px header with page tabs · footer: 24px status bar · signature: the mint lane rail that carries each branch band · mono: material (git ledger)
// Hallmark · pre-emit critique: P5 H4 E5 S4 R5 V4 · weakest: V, a native GPUI window cannot run the CSS render tier; swept the workspace, diff, composer, branches, history, settings, keyboard and menus at 1440x900 over the dev socket and walked the judged gates by hand · signature: the lane rail also holds the selected lane, so state and identity share one 2px mark
// Hallmark · waive 64: every label is mono, because the material is a git ledger and a diff; the chrome follows suit so one family carries the window
// Hallmark · answered F4: one filled primary action per view (Commit in the composer); fetch, undo, settings and refresh are ghost icon buttons

//! The GitButler window palette. ns-ui has no GitButler tint, so the window
//! builds its own `ns_ui::Theme` and points Ely's widgets at it at startup.
//! Every surface is tinted toward the mint accent; no pure black or white.

use ely_gpui_component::theme::{Mode, Palette, Theme as ElyTheme};
use gpui::{App, Rgba, rgb, rgba};

/// The canvas. Greener than grey, darker than the panels.
const BACKGROUND: u32 = 0x0A0C0B;
/// Window chrome: the project bar and status bar.
const PANEL: u32 = 0x101312;
/// The three working columns and the diff paper.
const EDITOR: u32 = 0x141816;
/// Raised blocks: the composer, menus, the keyboard card.
const RAISED: u32 = 0x1A1F1D;
const HOVER: u32 = 0x212924;
/// Selection and the included lane band.
const SELECTED: u32 = 0x14332D;
const BORDER: u32 = 0x2C3531;
const TEXT: u32 = 0xEDF3EF;
const MUTED: u32 = 0x9AA7A0;
/// The installed GitButler icon's mint.
const ACCENT: u32 = 0xA9F0EC;
const ACCENT_HOVER: u32 = 0xC9F8F4;
/// Ink on mint, so a filled mint button stays readable.
const ACCENT_INK: u32 = 0x06110F;
const OK: u32 = 0x7FE3AE;
const OK_BG: u32 = 0x122F23;
const ERR: u32 = 0xFF8E96;
const ERR_BG: u32 = 0x35191D;
const WARN: u32 = 0xF2CC79;
const WARN_BG: u32 = 0x33290F;

pub fn current() -> ns_ui::Theme {
    ns_ui::Theme {
        background: rgb(BACKGROUND),
        panel: rgb(PANEL),
        editor: rgb(EDITOR),
        raised: rgb(RAISED),
        hover: rgb(HOVER),
        selected: rgb(SELECTED),
        border: rgb(BORDER),
        text: rgb(TEXT),
        muted: rgb(MUTED),
        accent: rgb(ACCENT),
        accent_hover: rgb(ACCENT_HOVER),
        accent_ink: rgb(ACCENT_INK),
        status: rgb(BACKGROUND),
        ok: rgb(OK),
        ok_bg: rgb(OK_BG),
        err: rgb(ERR),
        err_bg: rgb(ERR_BG),
        warn: rgb(WARN),
        warn_bg: rgb(WARN_BG),
    }
}

/// A modal's ground: the canvas at 72%, tinted, never pure black.
pub fn scrim() -> Rgba {
    rgba(0x080A09B8)
}

/// Points Ely's buttons, fields, scrollbars and tooltips at the brand palette.
/// ns-ui installs its own palette before the window opens; this replaces it in
/// the same window, before anything is drawn.
pub fn adopt_ely_theme(cx: &mut App) {
    let palette = current();
    let ink = |color: Rgba| -> gpui::Hsla { color.into() };
    let mut colors = Palette::dark(false);
    colors.bg = ink(palette.background);
    colors.surface = ink(palette.raised);
    colors.sunken = ink(palette.panel);
    colors.overlay = ink(palette.panel);
    colors.hover = ink(palette.hover);
    colors.active = ink(palette.selected);
    colors.border = ink(palette.border);
    colors.border_strong = ink(palette.border);
    colors.fg = ink(palette.text);
    colors.fg_muted = ink(palette.muted);
    colors.fg_subtle = ink(palette.muted);
    colors.fg_disabled = ink(palette.muted);
    colors.accent = ink(palette.accent);
    colors.accent_hover = ink(palette.accent_hover);
    colors.on_accent = ink(palette.accent_ink);
    colors.on_media = ink(palette.text);
    colors.focus = ink(palette.accent);
    colors.link = ink(palette.accent);
    colors.selection = ink(palette.selected);
    colors.success = ink(palette.ok);
    colors.warning = ink(palette.warn);
    colors.danger = ink(palette.err);
    colors.info = ink(palette.accent);
    colors.success_subtle = ink(palette.ok_bg);
    colors.warning_subtle = ink(palette.warn_bg);
    colors.danger_subtle = ink(palette.err_bg);
    colors.info_subtle = ink(palette.selected);
    colors.paper = ink(palette.editor);
    colors.ink = ink(palette.text);
    colors.tooltip_bg = ink(palette.panel);
    colors.tooltip_fg = ink(palette.text);
    ElyTheme::set_palette(Mode::Dark, Some(colors), cx);
    // set_palette starts a cross-fade from the light default. Snap to dark so
    // the first frame is already the brand palette.
    ElyTheme::set_mode_now(Mode::Dark, cx);
    ElyTheme::update(cx, |theme| {
        theme.font_family = ns_ui::MONO_FONT.into();
        theme.mono_family = ns_ui::MONO_FONT.into();
    });
}
