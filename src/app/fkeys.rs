//! Function keys: the context they run in, which pane they act on, and the
//! dispatch of a key press or a bar click onto the app.

use super::*;
use crate::widgets::{self, FKEY_BAR_H, FKeyAction, FKeyContext, fkey_action, fkey_enabled};
use egui::{FocusDirection, Frame, Key, Modifiers, Ui};

/// The bar's context for a pane with `selected` entries selected.
///
/// A local pane always has a listing to act on, so it counts as connected.
fn fkey_context_for(
    selected: usize,
    is_remote: bool,
    connected: bool,
    other_usable: bool,
) -> FKeyContext {
    FKeyContext {
        has_selection: selected > 0,
        single_selection: selected == 1,
        focused_is_remote: is_remote,
        focused_connected: !is_remote || connected,
        other_usable,
    }
}

/// `(key, modifiers)` of every fresh key press among `events`; releases and
/// key-repeats are left out.
fn pressed_keys(events: &[egui::Event]) -> Vec<(Key, Modifiers)> {
    events
        .iter()
        .filter_map(|event| match event {
            egui::Event::Key {
                key,
                pressed: true,
                repeat: false,
                modifiers,
                ..
            } => Some((*key, *modifiers)),
            _ => None,
        })
        .collect()
}

/// The pane action behind a file key, given the single selected name (if any).
/// The global actions (console, hosts, switch pane) have no pane outcome.
fn pane_outcome(action: FKeyAction, single: Option<String>) -> Option<PaneOutcome> {
    match action {
        FKeyAction::Rename => single.map(PaneOutcome::Rename),
        FKeyAction::Edit => single.map(PaneOutcome::Edit),
        FKeyAction::Copy => Some(PaneOutcome::Transfer),
        FKeyAction::Mkdir => Some(PaneOutcome::NewFolder),
        FKeyAction::Delete => Some(PaneOutcome::DeleteSelected),
        FKeyAction::Console | FKeyAction::Hosts | FKeyAction::SwitchPane => None,
    }
}

impl FerropipeApp {
    /// True while a modal window owns the keyboard (and the workspace is disabled).
    pub(super) fn modal_open(&self) -> bool {
        self.editor.is_some()
            || self.prompt.is_some()
            || self.confirm_delete.is_some()
            || self.conflict_prompt.is_some()
            || self.ssh_panel.is_some()
    }

    /// The function-key context of the focused pane.
    pub(super) fn fkey_context(&self) -> FKeyContext {
        let side = self.focused;
        fkey_context_for(
            self.side_pane(side).selected.len(),
            self.side_is_remote(side),
            self.side_connected(side),
            self.side_usable(side.other()),
        )
    }

    /// Dispatch this frame's function-key presses to the focused pane.
    ///
    /// Nothing runs while a text field (or any widget) has keyboard focus, so
    /// Tab keeps moving between widgets and F-keys stay out of the filter box,
    /// nor while a modal is open. A handled press is consumed so no widget
    /// drawn later in the frame sees it.
    pub(super) fn handle_fkeys(&mut self, ctx: &egui::Context) {
        if self.modal_open() || ctx.memory(|m| m.focused().is_some()) {
            return;
        }
        for (key, modifiers) in ctx.input(|i| pressed_keys(&i.events)) {
            let Some(action) = fkey_action(key, modifiers) else {
                continue;
            };
            if !fkey_enabled(action, self.fkey_context()) {
                continue;
            }
            ctx.input_mut(|i| i.consume_key(modifiers, key));
            self.run_fkey(ctx, action);
        }
    }

    /// Run one function-key action (from a key press or a bar click).
    pub(super) fn run_fkey(&mut self, ctx: &egui::Context, action: FKeyAction) {
        let side = self.focused;
        match action {
            FKeyAction::Console => self.console_open = !self.console_open,
            FKeyAction::Hosts => {
                ctx.memory_mut(|m| m.request_focus(sidebar::host_filter_id()));
            }
            FKeyAction::SwitchPane => {
                self.focused = side.other();
                // egui queued its own Tab focus traversal before this frame
                // began; cancel it so the first focusable widget does not
                // take the keyboard along with the pane switch.
                ctx.memory_mut(|m| m.move_focus(FocusDirection::None));
            }
            _ => {
                if let Some(outcome) = pane_outcome(action, self.single_selected(side)) {
                    self.handle_pane(ctx, side, vec![outcome]);
                }
            }
        }
    }

    /// The selected name of `side` when exactly one entry is selected.
    fn single_selected(&self, side: Side) -> Option<String> {
        let selected = &self.side_pane(side).selected;
        if selected.len() != 1 {
            return None;
        }
        selected.iter().next().cloned()
    }

    /// The function-key bar along the very bottom of the window, when shown.
    ///
    /// Drawn before the other bottom panels so it sits under all of them;
    /// the panel adds no margins of its own since the bar paints its own fill
    /// and fits `FKEY_BAR_H` exactly.
    pub(super) fn render_fkey_bar(&mut self, ctx: &egui::Context, ui: &mut Ui) {
        if !self.store.settings.show_fkeys {
            return;
        }
        let fctx = self.fkey_context();
        let clicked = egui::Panel::bottom("fkeys")
            .exact_size(FKEY_BAR_H)
            .resizable(false)
            .show_separator_line(false)
            .frame(Frame::NONE)
            .show(ui, |ui| widgets::fkey_bar(ui, fctx))
            .inner;
        if let Some(action) = clicked {
            self.run_fkey(ctx, action);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use FKeyAction as A;

    fn key_event(key: Key, pressed: bool, repeat: bool, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat,
            modifiers,
        }
    }

    // -- fkey_context_for ---------------------------------------------------

    #[test]
    fn fkey_context_for_no_selection_sets_neither_selection_flag() {
        // Arrange / Act
        let ctx = fkey_context_for(0, true, true, true);
        // Assert
        assert!(!ctx.has_selection);
        assert!(!ctx.single_selection);
    }

    #[test]
    fn fkey_context_for_one_selection_is_a_single_selection() {
        let ctx = fkey_context_for(1, true, true, true);
        assert!(ctx.has_selection);
        assert!(ctx.single_selection);
    }

    #[test]
    fn fkey_context_for_two_selections_is_a_selection_but_not_single() {
        let ctx = fkey_context_for(2, true, true, true);
        assert!(ctx.has_selection);
        assert!(!ctx.single_selection);
    }

    #[test]
    fn fkey_context_for_local_pane_counts_as_connected() {
        // Arrange: a local pane has no session, so `connected` is false.
        let ctx = fkey_context_for(0, false, false, true);
        // Assert
        assert!(!ctx.focused_is_remote);
        assert!(ctx.focused_connected);
    }

    #[test]
    fn fkey_context_for_disconnected_remote_pane_is_not_connected() {
        let ctx = fkey_context_for(1, true, false, true);
        assert!(ctx.focused_is_remote);
        assert!(!ctx.focused_connected);
    }

    #[test]
    fn fkey_context_for_connected_remote_pane_is_connected() {
        let ctx = fkey_context_for(1, true, true, false);
        assert!(ctx.focused_is_remote);
        assert!(ctx.focused_connected);
    }

    #[test]
    fn fkey_context_for_passes_other_usable_through() {
        assert!(fkey_context_for(0, false, false, true).other_usable);
        assert!(!fkey_context_for(0, false, false, false).other_usable);
    }

    // -- pressed_keys -------------------------------------------------------

    #[test]
    fn pressed_keys_keeps_fresh_presses_with_their_modifiers() {
        // Arrange
        let events = vec![
            key_event(Key::F5, true, false, Modifiers::NONE),
            key_event(Key::Tab, true, false, Modifiers::SHIFT),
        ];
        // Act
        let pressed = pressed_keys(&events);
        // Assert
        assert_eq!(
            pressed,
            vec![(Key::F5, Modifiers::NONE), (Key::Tab, Modifiers::SHIFT)]
        );
    }

    #[test]
    fn pressed_keys_drops_releases_repeats_and_other_events() {
        let events = vec![
            key_event(Key::F5, false, false, Modifiers::NONE),
            key_event(Key::F7, true, true, Modifiers::NONE),
            egui::Event::Text("x".into()),
            egui::Event::PointerGone,
        ];
        assert!(pressed_keys(&events).is_empty());
    }

    // -- pane_outcome -------------------------------------------------------

    #[test]
    fn pane_outcome_rename_carries_the_selected_name() {
        let out = pane_outcome(A::Rename, Some("notes.txt".into()));
        assert!(matches!(out, Some(PaneOutcome::Rename(name)) if name == "notes.txt"));
    }

    #[test]
    fn pane_outcome_edit_carries_the_selected_name() {
        let out = pane_outcome(A::Edit, Some("notes.txt".into()));
        assert!(matches!(out, Some(PaneOutcome::Edit(name)) if name == "notes.txt"));
    }

    #[test]
    fn pane_outcome_rename_and_edit_need_a_single_name() {
        assert!(pane_outcome(A::Rename, None).is_none());
        assert!(pane_outcome(A::Edit, None).is_none());
    }

    #[test]
    fn pane_outcome_copy_mkdir_and_delete_map_to_their_pane_actions() {
        assert!(matches!(
            pane_outcome(A::Copy, None),
            Some(PaneOutcome::Transfer)
        ));
        assert!(matches!(
            pane_outcome(A::Mkdir, None),
            Some(PaneOutcome::NewFolder)
        ));
        assert!(matches!(
            pane_outcome(A::Delete, None),
            Some(PaneOutcome::DeleteSelected)
        ));
    }

    #[test]
    fn pane_outcome_is_none_for_the_global_actions() {
        for action in [A::Console, A::Hosts, A::SwitchPane] {
            assert!(
                pane_outcome(action, Some("x".into())).is_none(),
                "{action:?}"
            );
        }
    }
}
