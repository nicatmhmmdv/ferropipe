//! The Commander function-key bar along the bottom of the window.
//!
//! Midnight Commander style: `F2 Rename  F4 Edit  F5 Copy  F7 Mkdir  F8 Delete
//! F9 Console  F10 Hosts  Tab Switch pane`. F3 View and F6 Move are
//! deliberately absent: the app has no viewer and no move. The pure parts
//! (key press → action, action → enabled) are unit-tested on their own;
//! [`fkey_bar`] only paints the slots and reports the one clicked.

use egui::{
    Align, CursorIcon, Frame, Key, Layout, Margin, Modifiers, Rect, RichText, Sense, Stroke, Ui,
    vec2,
};

use super::{mono, plain_label};
use crate::theme;

/// Height of the bar, margins included.
pub const FKEY_BAR_H: f32 = 24.0;
/// Horizontal inner margin of the bar.
const BAR_MARGIN_X: i8 = 10;
/// Vertical inner margin of the bar.
const BAR_MARGIN_Y: i8 = 3;
/// Height left for the slots once the vertical margin is taken out.
const SLOT_ROW_H: f32 = FKEY_BAR_H - 2.0 * BAR_MARGIN_Y as f32;
/// Font size of the key caption ("F5", "Tab").
const KEY_FONT_SIZE: f32 = 11.0;
/// Gap between a key caption and its label.
const KEY_LABEL_GAP: f32 = 4.0;
/// Gap between two slots.
const SLOT_GAP: f32 = 14.0;

/// What a function key does in the file panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FKeyAction {
    Rename,
    Edit,
    Copy,
    Mkdir,
    Delete,
    Console,
    Hosts,
    SwitchPane,
}

impl FKeyAction {
    /// Human-readable name, shown as the slot's tooltip.
    pub fn name(self) -> &'static str {
        match self {
            Self::Rename => "Rename",
            Self::Edit => "Edit",
            Self::Copy => "Copy to the other pane",
            Self::Mkdir => "New directory",
            Self::Delete => "Delete",
            Self::Console => "Console",
            Self::Hosts => "Hosts",
            Self::SwitchPane => "Switch pane",
        }
    }
}

/// One slot of the bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FKeyItem {
    /// The key caption ("F5", "Tab"); also a valid `egui::Key` name.
    pub key: &'static str,
    /// The short action label next to it.
    pub label: &'static str,
    /// What pressing the key (or clicking the slot) does.
    pub action: FKeyAction,
}

const fn slot(key: &'static str, label: &'static str, action: FKeyAction) -> FKeyItem {
    FKeyItem { key, label, action }
}

/// The bar's slots, in display order.
pub const FKEY_ITEMS: [FKeyItem; 8] = [
    slot("F2", "Rename", FKeyAction::Rename),
    slot("F4", "Edit", FKeyAction::Edit),
    slot("F5", "Copy", FKeyAction::Copy),
    slot("F7", "Mkdir", FKeyAction::Mkdir),
    slot("F8", "Delete", FKeyAction::Delete),
    slot("F9", "Console", FKeyAction::Console),
    slot("F10", "Hosts", FKeyAction::Hosts),
    slot("Tab", "Switch pane", FKeyAction::SwitchPane),
];

/// The action bound to a raw key press, if any.
///
/// Function keys only fire bare. `Tab` and `Shift+Tab` both switch panes, so
/// the reflex of "going back" lands on the other pane too.
pub fn fkey_action(key: Key, modifiers: Modifiers) -> Option<FKeyAction> {
    let bare = modifiers.is_none();
    if key == Key::Tab {
        return (bare || modifiers == Modifiers::SHIFT).then_some(FKeyAction::SwitchPane);
    }
    if !bare {
        return None;
    }
    match key {
        Key::F2 => Some(FKeyAction::Rename),
        Key::F4 => Some(FKeyAction::Edit),
        Key::F5 => Some(FKeyAction::Copy),
        Key::F7 => Some(FKeyAction::Mkdir),
        Key::F8 => Some(FKeyAction::Delete),
        Key::F9 => Some(FKeyAction::Console),
        Key::F10 => Some(FKeyAction::Hosts),
        _ => None,
    }
}

/// Which slots are enabled given the current context.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FKeyContext {
    /// At least one entry is selected in the focused pane.
    pub has_selection: bool,
    /// Exactly one entry is selected in the focused pane.
    pub single_selection: bool,
    /// The focused pane shows a remote host rather than the local disk.
    pub focused_is_remote: bool,
    /// The focused pane has a live listing to act on.
    pub focused_connected: bool,
    /// The other pane can receive files.
    pub other_usable: bool,
}

/// Whether `action` can run right now.
pub fn fkey_enabled(action: FKeyAction, ctx: FKeyContext) -> bool {
    match action {
        FKeyAction::Rename => ctx.single_selection && ctx.focused_connected,
        FKeyAction::Edit => ctx.single_selection && ctx.focused_is_remote && ctx.focused_connected,
        FKeyAction::Copy => ctx.has_selection && ctx.focused_connected && ctx.other_usable,
        FKeyAction::Mkdir => ctx.focused_connected,
        FKeyAction::Delete => ctx.has_selection && ctx.focused_connected,
        FKeyAction::Console | FKeyAction::Hosts | FKeyAction::SwitchPane => true,
    }
}

/// Draw the bar; returns the action clicked this frame, if any.
///
/// The bar spans the available width, is `FKEY_BAR_H` tall, filled `bg2`,
/// with a hairline along its top edge.
pub fn fkey_bar(ui: &mut Ui, ctx: FKeyContext) -> Option<FKeyAction> {
    let p = theme::current(ui);
    let bar = Frame::new()
        .fill(p.bg2)
        .inner_margin(Margin::symmetric(BAR_MARGIN_X, BAR_MARGIN_Y))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            slot_row(ui, ctx)
        });
    let rect = bar.response.rect;
    ui.painter()
        .hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0, p.line));
    bar.inner
}

/// The horizontal row of slots.
///
/// Sized explicitly to `SLOT_ROW_H` rather than via `ui.horizontal`, whose
/// row height follows the style's `interact_size` and would push the bar
/// past its 24 px.
fn slot_row(ui: &mut Ui, ctx: FKeyContext) -> Option<FKeyAction> {
    let size = vec2(ui.available_width(), SLOT_ROW_H);
    ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_height(SLOT_ROW_H);
        ui.spacing_mut().item_spacing.x = SLOT_GAP;
        let mut clicked = None;
        for item in FKEY_ITEMS {
            if fkey_slot(ui, item, fkey_enabled(item.action, ctx)) {
                clicked = Some(item.action);
            }
        }
        clicked
    })
    .inner
}

/// One "F5 Copy" slot; true when clicked (never for a disabled slot).
///
/// The labels are plain so the click falls through to one hit rect spanning
/// the whole row height.
fn fkey_slot(ui: &mut Ui, item: FKeyItem, enabled: bool) -> bool {
    let p = theme::current(ui);
    let (key_color, label_color) = if enabled {
        (p.accent, p.text2)
    } else {
        (p.text3, p.text3)
    };
    let group = ui
        .horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = KEY_LABEL_GAP;
            plain_label(ui, mono(item.key, KEY_FONT_SIZE, key_color));
            plain_label(ui, RichText::new(item.label).color(label_color));
        })
        .response;
    let hit = Rect::from_x_y_ranges(group.rect.x_range(), ui.max_rect().y_range());
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui
        .interact(hit, ui.id().with(item.key), sense)
        .on_hover_text(item.action.name());
    enabled && response.on_hover_cursor(CursorIcon::PointingHand).clicked()
}

#[cfg(test)]
mod tests {
    use super::*;
    use FKeyAction as A;
    use egui::{Context, Event, PointerButton, Pos2, RawInput};

    /// Every action, for exhaustiveness checks.
    const ALL_ACTIONS: [FKeyAction; 8] = [
        A::Rename,
        A::Edit,
        A::Copy,
        A::Mkdir,
        A::Delete,
        A::Console,
        A::Hosts,
        A::SwitchPane,
    ];

    /// A context in which every slot is enabled.
    const EVERYTHING: FKeyContext = FKeyContext {
        has_selection: true,
        single_selection: true,
        focused_is_remote: true,
        focused_connected: true,
        other_usable: true,
    };

    /// Only the slots that never depend on context.
    const GLOBALS: [FKeyAction; 3] = [A::Console, A::Hosts, A::SwitchPane];

    /// X offset from the bar's left edge that lands inside the "F2" caption,
    /// past the 10 px margin.
    const FIRST_SLOT_X: f32 = 16.0;

    fn enabled_set(ctx: FKeyContext) -> Vec<FKeyAction> {
        ALL_ACTIONS
            .into_iter()
            .filter(|a| fkey_enabled(*a, ctx))
            .collect()
    }

    // -- FKEY_ITEMS ---------------------------------------------------------

    #[test]
    fn fkey_items_are_in_commander_display_order() {
        // Arrange
        let expected = [
            ("F2", "Rename", A::Rename),
            ("F4", "Edit", A::Edit),
            ("F5", "Copy", A::Copy),
            ("F7", "Mkdir", A::Mkdir),
            ("F8", "Delete", A::Delete),
            ("F9", "Console", A::Console),
            ("F10", "Hosts", A::Hosts),
            ("Tab", "Switch pane", A::SwitchPane),
        ];
        // Act
        let actual = FKEY_ITEMS.map(|i| (i.key, i.label, i.action));
        // Assert
        assert_eq!(actual, expected);
    }

    #[test]
    fn fkey_items_bind_every_action_exactly_once() {
        for action in ALL_ACTIONS {
            let count = FKEY_ITEMS.iter().filter(|i| i.action == action).count();
            assert_eq!(count, 1, "{action:?} appears {count} times");
        }
        assert_eq!(FKEY_ITEMS.len(), ALL_ACTIONS.len());
    }

    #[test]
    fn fkey_items_leave_out_view_and_move() {
        let keys: Vec<&str> = FKEY_ITEMS.iter().map(|i| i.key).collect();
        assert!(!keys.contains(&"F3"), "no viewer, so no F3 View");
        assert!(!keys.contains(&"F6"), "no move, so no F6 Move");
    }

    #[test]
    fn fkey_items_key_captions_name_the_key_that_triggers_them() {
        for item in FKEY_ITEMS {
            // Arrange: the caption must be a real egui key name.
            let key = Key::from_name(item.key)
                .unwrap_or_else(|| panic!("{} is not a key name", item.key));
            // Act
            let bound = fkey_action(key, Modifiers::NONE);
            // Assert
            assert_eq!(bound, Some(item.action), "caption {}", item.key);
        }
    }

    // -- fkey_action --------------------------------------------------------

    #[test]
    fn fkey_action_maps_each_bare_function_key() {
        let cases = [
            (Key::F2, A::Rename),
            (Key::F4, A::Edit),
            (Key::F5, A::Copy),
            (Key::F7, A::Mkdir),
            (Key::F8, A::Delete),
            (Key::F9, A::Console),
            (Key::F10, A::Hosts),
        ];
        for (key, expected) in cases {
            assert_eq!(fkey_action(key, Modifiers::NONE), Some(expected), "{key:?}");
        }
    }

    #[test]
    fn fkey_action_tab_switches_pane() {
        assert_eq!(fkey_action(Key::Tab, Modifiers::NONE), Some(A::SwitchPane));
    }

    #[test]
    fn fkey_action_shift_tab_also_switches_pane() {
        assert_eq!(fkey_action(Key::Tab, Modifiers::SHIFT), Some(A::SwitchPane));
    }

    #[test]
    fn fkey_action_ignores_tab_with_any_other_modifier() {
        let held = [
            Modifiers::CTRL,
            Modifiers::ALT,
            Modifiers::COMMAND,
            Modifiers::SHIFT | Modifiers::CTRL,
        ];
        for mods in held {
            assert_eq!(fkey_action(Key::Tab, mods), None, "{mods:?}");
        }
    }

    #[test]
    fn fkey_action_ignores_function_keys_with_a_modifier_held() {
        let held = [
            Modifiers::CTRL,
            Modifiers::ALT,
            Modifiers::SHIFT,
            Modifiers::COMMAND,
        ];
        for mods in held {
            assert_eq!(fkey_action(Key::F5, mods), None, "{mods:?}");
        }
    }

    #[test]
    fn fkey_action_has_no_binding_for_view_or_move() {
        assert_eq!(fkey_action(Key::F3, Modifiers::NONE), None);
        assert_eq!(fkey_action(Key::F6, Modifiers::NONE), None);
    }

    #[test]
    fn fkey_action_ignores_unrelated_keys() {
        for key in [Key::F1, Key::F11, Key::F12, Key::Enter, Key::A, Key::Escape] {
            assert_eq!(fkey_action(key, Modifiers::NONE), None, "{key:?}");
        }
    }

    // -- fkey_enabled -------------------------------------------------------

    #[test]
    fn fkey_enabled_console_hosts_and_switch_pane_are_always_on() {
        assert_eq!(enabled_set(FKeyContext::default()), GLOBALS);
    }

    #[test]
    fn fkey_enabled_everything_is_on_for_a_connected_remote_single_selection() {
        assert_eq!(enabled_set(EVERYTHING), ALL_ACTIONS);
    }

    #[test]
    fn fkey_enabled_disconnected_pane_keeps_only_the_globals() {
        let ctx = FKeyContext {
            focused_connected: false,
            ..EVERYTHING
        };
        assert_eq!(enabled_set(ctx), GLOBALS);
    }

    #[test]
    fn fkey_enabled_local_selection_can_copy_only_when_the_other_side_is_usable() {
        // Arrange
        let local = FKeyContext {
            focused_is_remote: false,
            other_usable: false,
            ..EVERYTHING
        };
        let with_target = FKeyContext {
            other_usable: true,
            ..local
        };
        // Act / Assert
        assert!(!fkey_enabled(A::Copy, local));
        assert!(fkey_enabled(A::Copy, with_target));
    }

    #[test]
    fn fkey_enabled_edit_is_remote_only() {
        let local = FKeyContext {
            focused_is_remote: false,
            ..EVERYTHING
        };
        assert!(!fkey_enabled(A::Edit, local));
        assert!(fkey_enabled(A::Edit, EVERYTHING));
    }

    #[test]
    fn fkey_enabled_multi_selection_can_copy_and_delete_but_not_rename_or_edit() {
        let multi = FKeyContext {
            single_selection: false,
            ..EVERYTHING
        };
        assert_eq!(
            enabled_set(multi),
            [
                A::Copy,
                A::Mkdir,
                A::Delete,
                A::Console,
                A::Hosts,
                A::SwitchPane
            ]
        );
    }

    #[test]
    fn fkey_enabled_mkdir_needs_only_a_connected_pane() {
        let empty = FKeyContext {
            focused_connected: true,
            ..FKeyContext::default()
        };
        assert_eq!(
            enabled_set(empty),
            [A::Mkdir, A::Console, A::Hosts, A::SwitchPane]
        );
    }

    // -- fkey_bar (headless egui frames) -----------------------------------

    /// Run one headless frame with `events`; what the bar reported and the
    /// rect it occupied.
    fn run_bar_frame(
        ctx: &Context,
        events: Vec<Event>,
        fctx: FKeyContext,
    ) -> (Option<FKeyAction>, Rect) {
        let mut result = (None, Rect::NOTHING);
        let input = RawInput {
            events,
            ..Default::default()
        };
        ctx.run_ui(input, |ui| {
            let clicked = fkey_bar(ui, fctx);
            result = (clicked, ui.min_rect());
        })
        .drop_without_applying_deltas();
        result
    }

    fn button(pos: Pos2, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button: PointerButton::Primary,
            pressed,
            modifiers: Modifiers::NONE,
        }
    }

    /// Hover, press and release at `pos` over three frames; what the release
    /// frame reports. Nothing may fire before it.
    fn click_at(ctx: &Context, pos: Pos2, fctx: FKeyContext) -> Option<FKeyAction> {
        let (hover, _) = run_bar_frame(ctx, vec![Event::PointerMoved(pos)], fctx);
        let (press, _) = run_bar_frame(ctx, vec![button(pos, true)], fctx);
        assert_eq!((hover, press), (None, None), "fired before the release");
        run_bar_frame(ctx, vec![button(pos, false)], fctx).0
    }

    #[test]
    fn fkey_bar_is_exactly_twenty_four_pixels_tall() {
        // Arrange
        let ctx = Context::default();
        // Act
        let (clicked, rect) = run_bar_frame(&ctx, Vec::new(), FKeyContext::default());
        // Assert
        assert_eq!(clicked, None);
        assert!(
            (rect.height() - FKEY_BAR_H).abs() < 1e-3,
            "bar rect {rect:?}"
        );
    }

    #[test]
    fn fkey_bar_reports_the_enabled_slot_that_was_clicked() {
        // Arrange: lay the bar out once so egui knows where the slots are.
        let ctx = Context::default();
        let (_, rect) = run_bar_frame(&ctx, Vec::new(), EVERYTHING);
        let first_slot = rect.left_top() + vec2(FIRST_SLOT_X, FKEY_BAR_H / 2.0);
        // Act
        let clicked = click_at(&ctx, first_slot, EVERYTHING);
        // Assert
        assert_eq!(clicked, Some(A::Rename));
    }

    #[test]
    fn fkey_bar_ignores_a_click_on_a_disabled_slot() {
        // Arrange: default context leaves Rename (the first slot) disabled.
        let ctx = Context::default();
        let (_, rect) = run_bar_frame(&ctx, Vec::new(), FKeyContext::default());
        let first_slot = rect.left_top() + vec2(FIRST_SLOT_X, FKEY_BAR_H / 2.0);
        // Act
        let clicked = click_at(&ctx, first_slot, FKeyContext::default());
        // Assert
        assert_eq!(clicked, None);
    }
}
