//! Unit tests for the panes' pure helpers.

use super::*;

fn pane() -> PaneState {
    PaneState::new(true, "/srv".into())
}

// -- header_rule -------------------------------------------------------

#[test]
fn header_rule_is_a_two_pixel_accent_line_for_the_focused_pane() {
    // Arrange
    let p = &theme::DARK;
    // Act
    let rule = header_rule(true, p);
    // Assert
    assert_eq!(rule.width, 2.0);
    assert_eq!(rule.color, p.accent);
}

#[test]
fn header_rule_is_a_one_pixel_hairline_for_the_other_pane() {
    let p = &theme::LIGHT;
    let rule = header_rule(false, p);
    assert_eq!(rule.width, 1.0);
    assert_eq!(rule.color, p.line);
}

fn selected(names: &[&str]) -> HashSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

// -- transfer_menu_label -----------------------------------------------

#[test]
fn transfer_menu_label_points_right_from_the_left_pane() {
    // Arrange / Act
    let label = transfer_menu_label(Side::Left, Verb::Upload);
    // Assert
    assert_eq!(label, "Upload ▶");
}

#[test]
fn transfer_menu_label_points_left_from_the_right_pane() {
    assert_eq!(
        transfer_menu_label(Side::Right, Verb::Download),
        "◀ Download"
    );
}

#[test]
fn transfer_menu_label_says_copy_between_two_local_or_two_remote_panes() {
    assert_eq!(transfer_menu_label(Side::Right, Verb::Copy), "◀ Copy");
    assert_eq!(transfer_menu_label(Side::Left, Verb::Copy), "Copy ▶");
}

// -- apply_click -------------------------------------------------------

#[test]
fn apply_click_plain_click_replaces_the_selection() {
    // Arrange
    let mut pane = pane();
    pane.selected = selected(&["a", "b"]);
    // Act
    apply_click(&mut pane, "c".into(), false);
    // Assert
    assert_eq!(pane.selected, selected(&["c"]));
}

#[test]
fn apply_click_ctrl_click_adds_an_unselected_entry() {
    let mut pane = pane();
    pane.selected = selected(&["a"]);
    apply_click(&mut pane, "b".into(), true);
    assert_eq!(pane.selected, selected(&["a", "b"]));
}

#[test]
fn apply_click_ctrl_click_removes_an_already_selected_entry() {
    let mut pane = pane();
    pane.selected = selected(&["a", "b"]);
    apply_click(&mut pane, "a".into(), true);
    assert_eq!(pane.selected, selected(&["b"]));
}

// -- apply_sort --------------------------------------------------------

#[test]
fn apply_sort_same_column_flips_the_direction() {
    // Arrange: a fresh pane sorts by name, ascending.
    let mut pane = pane();
    // Act
    apply_sort(&mut pane, SortCol::Name);
    // Assert
    assert_eq!(pane.sort_col, SortCol::Name);
    assert!(!pane.sort_asc);
    apply_sort(&mut pane, SortCol::Name);
    assert!(pane.sort_asc);
}

#[test]
fn apply_sort_new_column_resets_to_ascending() {
    let mut pane = pane();
    pane.sort_col = SortCol::Modified;
    pane.sort_asc = false;
    apply_sort(&mut pane, SortCol::Size);
    assert_eq!(pane.sort_col, SortCol::Size);
    assert!(pane.sort_asc);
}
