//! Colors and metrics for RV.
//!
//! Address-book chrome follows the active GPUI Component theme so light and
//! dark mode both look intentional; the session window keeps a fixed dark
//! chrome because it frames a remote desktop of unknown brightness.

use gpui::{App, Hsla, Pixels, Window, hsla, px, rgb};
use gpui_component::{ActiveTheme as _, Theme, ThemeMode};
use rv_core::ThemePref;

pub fn accent(cx: &App) -> Hsla {
    cx.theme().primary
}

pub fn accent_fg(cx: &App) -> Hsla {
    cx.theme().primary_foreground
}

/// Window background behind cards and lists. Cards must read as raised, so
/// the pairing flips with the mode: light gray under white cards in light
/// mode, near-black under dark-gray cards in dark mode.
pub fn surface(cx: &App) -> Hsla {
    if is_dark(cx) {
        cx.theme().background
    } else {
        cx.theme().secondary
    }
}

/// Card, dialog, and toolbar background.
pub fn card(cx: &App) -> Hsla {
    if is_dark(cx) {
        cx.theme().secondary
    } else {
        cx.theme().background
    }
}

pub fn ink(cx: &App) -> Hsla {
    cx.theme().foreground
}

pub fn muted(cx: &App) -> Hsla {
    cx.theme().muted_foreground
}

pub fn line(cx: &App) -> Hsla {
    cx.theme().border
}

pub fn danger(cx: &App) -> Hsla {
    cx.theme().danger
}

pub fn sidebar(cx: &App) -> Hsla {
    cx.theme().sidebar
}

/// Background of a selected list row or card.
pub fn selected(cx: &App) -> Hsla {
    cx.theme().accent
}

pub fn hover(cx: &App) -> Hsla {
    cx.theme().list_hover
}

/// Placeholder behind connection previews.
pub fn thumb_bg(cx: &App) -> Hsla {
    cx.theme().muted
}

pub fn is_dark(cx: &App) -> bool {
    cx.theme().mode.is_dark()
}

// Session window chrome: fixed, dark.

pub fn desktop() -> Hsla {
    rgb(0x111418).into()
}

pub fn toolbar() -> Hsla {
    rgb(0x1E252E).into()
}

pub fn toolbar_fg() -> Hsla {
    rgb(0xE8EEF4).into()
}

pub fn toolbar_muted() -> Hsla {
    hsla(0.58, 0.12, 0.7, 1.0)
}

pub fn toolbar_line() -> Hsla {
    hsla(0., 0., 1., 0.12)
}

pub fn scrim() -> Hsla {
    hsla(0., 0., 0., 0.55)
}

pub fn sidebar_width() -> Pixels {
    px(220.)
}

pub fn toolbar_height() -> Pixels {
    px(40.)
}

/// Apply the user's theme preference to the whole app. `System` follows the
/// window appearance; the caller re-invokes this when that changes.
pub fn apply(pref: ThemePref, window: &mut Window, cx: &mut App) {
    let mode = match pref {
        ThemePref::System => ThemeMode::from(window.appearance()),
        ThemePref::Light => ThemeMode::Light,
        ThemePref::Dark => ThemeMode::Dark,
    };
    if cx.theme().mode == mode {
        return;
    }
    Theme::change(mode, Some(window), cx);
    // Other windows (open sessions) share the global theme; nudge them.
    for handle in cx.windows() {
        let _ = handle.update(cx, |_, window, _| window.refresh());
    }
}
