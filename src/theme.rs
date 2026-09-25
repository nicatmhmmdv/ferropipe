//! Foundry visual system: palette tokens, fonts, and the egui `Visuals`/`Style` pass.
//!
//! Every color in the UI comes from a [`Palette`]; nothing else in the app should
//! construct a `Color32` literal. `install_fonts` and `apply` are the only two
//! entry points the app calls.

use egui::style::{Selection, WidgetVisuals, Widgets};
use egui::{
    Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, Style, TextStyle, Theme,
    Visuals, vec2,
};
use std::collections::BTreeMap;

mod fonts;

/// Row height for file listings.
pub const ROW_H: f32 = 24.0;
/// Row height for host rows in the sidebar.
pub const HOST_ROW_H: f32 = 26.0;
/// Default sidebar width.
pub const SIDEBAR_W: f32 = 236.0;
/// Width of the transfer gutter between the two panes.
pub const GUTTER_W: f32 = 44.0;
/// Corner radius for buttons and inputs.
pub const RADIUS: u8 = 4;

/// Named color tokens for one theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// Deepest ground: file lists, text fields.
    pub bg0: Color32,
    /// Panels: sidebar, panes.
    pub bg1: Color32,
    /// Bars: top bar, pane headers, drawer, popups.
    pub bg2: Color32,
    /// Raised: buttons, hover, chips.
    pub bg3: Color32,
    /// Hairlines and strokes.
    pub line: Color32,
    /// Zebra stripe on even rows.
    pub zebra: Color32,
    /// Primary text.
    pub text: Color32,
    /// Secondary text.
    pub text2: Color32,
    /// Tertiary text: labels, hints, disabled.
    pub text3: Color32,
    /// The rust accent. Primary action, running transfer, drop target. Nothing else.
    pub accent: Color32,
    /// Text drawn on top of an accent fill.
    pub accent_ink: Color32,
    /// Selection fill (a dim rust, not the accent).
    pub sel: Color32,
    pub ok: Color32,
    pub warn: Color32,
    pub err: Color32,
    pub info: Color32,
}

const fn rgb(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

pub const DARK: Palette = Palette {
    bg0: rgb(0x151413),
    bg1: rgb(0x1C1B19),
    bg2: rgb(0x242220),
    bg3: rgb(0x2D2A27),
    line: rgb(0x353230),
    zebra: rgb(0x191817),
    text: rgb(0xE9E4DD),
    text2: rgb(0xA39C93),
    text3: rgb(0x6F6962),
    accent: rgb(0xE8763F),
    accent_ink: rgb(0x1A1108),
    sel: rgb(0x3B2416),
    ok: rgb(0x5CBF86),
    warn: rgb(0xE1B24B),
    err: rgb(0xE2645C),
    info: rgb(0x6FA3D9),
};

pub const LIGHT: Palette = Palette {
    bg0: rgb(0xF4F2EE),
    bg1: rgb(0xEAE7E2),
    bg2: rgb(0xE1DED8),
    bg3: rgb(0xD6D2CB),
    line: rgb(0xC9C4BC),
    zebra: rgb(0xEFEDE8),
    text: rgb(0x1E1C1A),
    text2: rgb(0x5F5A53),
    text3: rgb(0x8C867E),
    accent: rgb(0xC4531F),
    accent_ink: rgb(0xFFF6F0),
    sel: rgb(0xF0D3C2),
    ok: rgb(0x237A4A),
    warn: rgb(0x9A6B00),
    err: rgb(0xB9382F),
    info: rgb(0x2F6BB0),
};

/// The palette for a theme.
pub fn palette(dark: bool) -> &'static Palette {
    if dark { &DARK } else { &LIGHT }
}

/// The palette matching the theme currently applied to `ui`.
pub fn current(ui: &egui::Ui) -> &'static Palette {
    palette(ui.visuals().dark_mode)
}

/// Condensed display family (headings, brand). Falls back to the proportional face.
pub fn cond() -> FontFamily {
    FontFamily::Name("cond".into())
}

/// Medium-weight proportional family (directory names, host names).
pub fn medium() -> FontFamily {
    FontFamily::Name("medium".into())
}

/// Register fonts: discover the four roles on this machine, keep egui's
/// built-ins and the symbol fallbacks behind them. Safe to call when nothing is found.
pub fn install_fonts(ctx: &egui::Context) {
    fonts::install(ctx);
}

/// Apply the palette, text styles and spacing for `dark` and make it the active theme.
pub fn apply(ctx: &egui::Context, dark: bool) {
    let theme = if dark { Theme::Dark } else { Theme::Light };
    ctx.set_theme(theme);
    ctx.set_visuals_of(theme, base_visuals(palette(dark), dark));
    ctx.all_styles_mut(style_pass);
}

/// Text styles shared by both themes.
pub fn text_styles() -> BTreeMap<TextStyle, FontId> {
    use FontFamily::{Monospace, Proportional};
    [
        (TextStyle::Small, FontId::new(11.5, Proportional)),
        (TextStyle::Body, FontId::new(13.0, Proportional)),
        (TextStyle::Button, FontId::new(13.0, Proportional)),
        (TextStyle::Monospace, FontId::new(12.0, Monospace)),
        (TextStyle::Heading, FontId::new(15.0, cond())),
    ]
    .into()
}

fn hairline(color: Color32) -> Stroke {
    Stroke::new(1.0, color)
}

fn widget(bg: Color32, weak_bg: Color32, border: Color32, fg: Color32) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: weak_bg,
        bg_stroke: hairline(border),
        corner_radius: CornerRadius::same(RADIUS),
        fg_stroke: hairline(fg),
        expansion: 0.0,
    }
}

fn widget_visuals(p: &Palette) -> Widgets {
    Widgets {
        noninteractive: widget(p.bg2, p.bg1, p.line, p.text2),
        inactive: widget(p.bg3, p.bg2, p.line, p.text),
        hovered: widget(p.bg3, p.bg3, p.text3, p.text),
        active: widget(p.sel, p.sel, p.accent, p.text),
        open: widget(p.bg3, p.bg3, p.line, p.text),
    }
}

fn shadow(dark: bool) -> Shadow {
    Shadow {
        offset: [0, 4],
        blur: 8,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 90 } else { 40 }),
    }
}

fn base_visuals(p: &Palette, dark: bool) -> Visuals {
    let base = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    Visuals {
        panel_fill: p.bg1,
        window_fill: p.bg2,
        extreme_bg_color: p.bg0,
        faint_bg_color: p.zebra,
        code_bg_color: p.bg3,
        text_edit_bg_color: Some(p.bg0),
        window_stroke: hairline(p.line),
        window_corner_radius: CornerRadius::same(6),
        menu_corner_radius: CornerRadius::same(6),
        hyperlink_color: p.accent,
        selection: Selection {
            bg_fill: p.sel,
            stroke: hairline(p.accent),
        },
        warn_fg_color: p.warn,
        error_fg_color: p.err,
        weak_text_color: Some(p.text2),
        override_text_color: None,
        window_shadow: shadow(dark),
        popup_shadow: shadow(dark),
        widgets: widget_visuals(p),
        ..base
    }
}

fn style_pass(s: &mut Style) {
    s.text_styles = text_styles();
    s.spacing.item_spacing = vec2(8.0, 6.0);
    s.spacing.button_padding = vec2(10.0, 5.0);
    s.spacing.interact_size.y = 24.0;
    s.spacing.menu_margin = Margin::same(6);
    s.spacing.window_margin = Margin::same(12);
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::TextStyle;

    /// WCAG 2.x relative luminance of an sRGB color, 0.0 (black) to 1.0 (white).
    fn relative_luminance(c: Color32) -> f32 {
        fn linear(channel: u8) -> f32 {
            let s = f32::from(channel) / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * linear(c.r()) + 0.7152 * linear(c.g()) + 0.0722 * linear(c.b())
    }

    /// WCAG contrast ratio between two colors, from 1.0 (identical) to 21.0.
    fn contrast_ratio(a: Color32, b: Color32) -> f32 {
        let (la, lb) = (relative_luminance(a) + 0.05, relative_luminance(b) + 0.05);
        if la > lb { la / lb } else { lb / la }
    }

    #[test]
    fn rgb_splits_hex_into_channels() {
        let c = rgb(0xE8763F);
        assert_eq!((c.r(), c.g(), c.b()), (0xE8, 0x76, 0x3F));
    }

    #[test]
    fn pick_font_returns_first_existing_candidate() {
        let candidates = ["Plex.ttf", "Noto.ttf", "DejaVu.ttf"];
        let exists = |name: &str| name == "Noto.ttf" || name == "DejaVu.ttf";
        assert_eq!(fonts::pick_font(&candidates, exists), Some(1));
    }

    #[test]
    fn pick_font_returns_none_when_nothing_exists() {
        let candidates = ["Plex.ttf", "Noto.ttf"];
        assert_eq!(fonts::pick_font(&candidates, |_| false), None);
    }

    #[test]
    fn text_styles_cover_every_builtin_style() {
        let styles = text_styles();
        for style in [
            TextStyle::Small,
            TextStyle::Body,
            TextStyle::Button,
            TextStyle::Monospace,
            TextStyle::Heading,
        ] {
            assert!(styles.contains_key(&style), "missing {style:?}");
        }
        assert_eq!(styles[&TextStyle::Heading].family, cond());
        assert_eq!(styles[&TextStyle::Monospace].family, FontFamily::Monospace);
    }

    #[test]
    fn relative_luminance_spans_black_to_white() {
        assert!(relative_luminance(Color32::BLACK).abs() < 1e-6);
        assert!((relative_luminance(Color32::WHITE) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn primary_text_meets_wcag_aaa_on_ground_in_both_themes() {
        for p in [&DARK, &LIGHT] {
            let ratio = contrast_ratio(p.text, p.bg0);
            assert!(ratio >= 7.0, "text/bg0 contrast {ratio:.2} < 7.0 for {p:?}");
        }
    }

    #[test]
    fn accent_meets_wcag_graphics_contrast_on_ground_in_both_themes() {
        for p in [&DARK, &LIGHT] {
            let ratio = contrast_ratio(p.accent, p.bg0);
            assert!(
                ratio >= 3.0,
                "accent/bg0 contrast {ratio:.2} < 3.0 for {p:?}"
            );
        }
    }

    #[test]
    fn discovery_only_reports_existing_files_matching_a_candidate() {
        for (role, found) in fonts::discover() {
            println!("{role:?}: {found:?}");
            let Some(path) = found else { continue };
            assert!(
                path.is_file(),
                "{role:?} -> {} is not a file",
                path.display()
            );
            let file = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            assert!(
                role.candidates()
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(file)),
                "{role:?} -> {file} is not a listed candidate"
            );
        }
    }
}
