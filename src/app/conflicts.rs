//! Overwrite protection: the "N files already exist" prompt a transfer opens
//! when it would write over existing files, and the re-issue of that transfer
//! with the user's answer.
//!
//! The worker stops before writing anything and reports `Event::Conflicts`;
//! the row leaves the drawer and the prompt takes its place. Answering
//! re-sends the same request with an `OnConflict` policy; cancelling drops it.

use super::*;
use crate::localfs::human_size;
use crate::remote::Conflict;
use crate::theme;
use crate::widgets;
use egui::{FontFamily, FontId, Key, Ui};
use transfers::{RetryRequest, StageJob};

/// Rows listed in the prompt before the rest is summed up as "+ K more".
const MAX_PROMPT_ROWS: usize = 8;

/// What the answer re-issues.
pub(super) enum PolicyTarget {
    /// A transfer started from the panes: re-run the request with the answer.
    Fresh(RetryRequest),
    /// The upload leg of a staged remote→remote copy: re-send it from the
    /// temp dir the download leg filled, without downloading again.
    StagedUpload(StageJob),
}

/// The open prompt: what it is about and what an answer re-issues.
pub(super) struct ConflictPrompt {
    pub target: PolicyTarget,
    pub conflicts: Vec<Conflict>,
}

impl ConflictPrompt {
    /// Give the transfer up: a staged copy's temp dir goes with it.
    pub(super) fn discard(self) {
        if let PolicyTarget::StagedUpload(job) = self.target {
            let _ = std::fs::remove_dir_all(&job.temp_dir);
        }
    }
}

/// What the user chose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Apply(OnConflict),
    Cancel,
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// The prompt's title for `n` conflicts.
pub(super) fn prompt_title(n: usize) -> String {
    if n == 1 {
        "1 file already exists".to_string()
    } else {
        format!("{n} files already exist")
    }
}

/// One prompt row: `name · existing 1.2 MB → incoming 3.4 MB`, or what is in
/// the way when the kinds differ.
pub(super) fn conflict_line(c: &Conflict) -> String {
    if c.kind_mismatch {
        format!("{} · folder in the way", c.name)
    } else {
        format!(
            "{} · existing {} → incoming {}",
            c.name,
            human_size(c.existing_size),
            human_size(c.incoming_size)
        )
    }
}

/// Up to `MAX_PROMPT_ROWS` rows, then "+ K more" for the rest.
pub(super) fn conflict_lines(conflicts: &[Conflict]) -> Vec<String> {
    let shown = conflicts.iter().take(MAX_PROMPT_ROWS).map(conflict_line);
    let rest = conflicts.len().saturating_sub(MAX_PROMPT_ROWS);
    let more = (rest > 0).then(|| format!("+ {rest} more"));
    shown.chain(more).collect()
}

// ---------------------------------------------------------------------------
// App wiring
// ---------------------------------------------------------------------------

impl FerropipeApp {
    /// Transfer `id` stopped before writing: take its row and ask what to do.
    pub(super) fn on_conflicts(&mut self, ctx: &egui::Context, id: u64, conflicts: Vec<Conflict>) {
        let staged = self.stage_cleanup.remove(&id);
        let Some(pos) = self.transfers.iter().position(|t| t.id == id) else {
            if let Some(job) = staged {
                let _ = std::fs::remove_dir_all(&job.temp_dir);
            }
            return;
        };
        let row = self.transfers.remove(pos);
        let target = match (staged, row.retry) {
            (Some(job), _) => PolicyTarget::StagedUpload(job),
            (None, Some(req)) => PolicyTarget::Fresh(req),
            (None, None) => {
                self.toast(ctx, format!("{}: files already exist", row.label), true);
                return;
            }
        };
        self.conflict_prompt = Some(ConflictPrompt { target, conflicts });
    }

    /// The modal "N files already exist" prompt, while one is open.
    pub(super) fn render_conflict_prompt(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &self.conflict_prompt else {
            return;
        };
        let title = prompt_title(prompt.conflicts.len());
        let lines = conflict_lines(&prompt.conflicts);
        if let Some(answer) = show_conflict_window(ctx, &title, &lines) {
            self.answer_conflicts(ctx, answer);
        }
    }

    /// Close the prompt and re-issue (or drop) the transfer it was about.
    fn answer_conflicts(&mut self, ctx: &egui::Context, answer: Answer) {
        let Some(prompt) = self.conflict_prompt.take() else {
            return;
        };
        let policy = match answer {
            Answer::Apply(policy) => policy,
            Answer::Cancel => return prompt.discard(),
        };
        match prompt.target {
            PolicyTarget::Fresh(req) => self.replay_request(ctx, req, policy),
            PolicyTarget::StagedUpload(job) => self.start_stage_upload(
                ctx,
                StageJob {
                    on_conflict: policy,
                    ..job
                },
            ),
        }
    }
}

/// Draw the prompt window; `Some` once a button or its key was pressed.
fn show_conflict_window(ctx: &egui::Context, title: &str, lines: &[String]) -> Option<Answer> {
    let mut answer = None;
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            let p = theme::current(ui);
            for line in lines {
                let text = RichText::new(line)
                    .font(FontId::new(12.0, FontFamily::Monospace))
                    .color(p.text2);
                ui.label(text);
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| answer = conflict_buttons(ui));
            if ui.input(|i| i.key_pressed(Key::Enter)) {
                answer = Some(Answer::Apply(OnConflict::Rename));
            }
            if ui.input(|i| i.key_pressed(Key::Escape)) {
                answer = Some(Answer::Cancel);
            }
        });
    answer
}

/// Keep both · Skip existing · Overwrite · Cancel, in that order.
fn conflict_buttons(ui: &mut Ui) -> Option<Answer> {
    let p = theme::current(ui);
    let mut answer = None;
    if widgets::primary_button(ui, "Keep both").clicked() {
        answer = Some(Answer::Apply(OnConflict::Rename));
    }
    if ui.button("Skip existing").clicked() {
        answer = Some(Answer::Apply(OnConflict::Skip));
    }
    if ui.button(RichText::new("Overwrite").color(p.err)).clicked() {
        answer = Some(Answer::Apply(OnConflict::Overwrite));
    }
    if ui.button("Cancel").clicked() {
        answer = Some(Answer::Cancel);
    }
    answer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict(name: &str, existing_size: u64, incoming_size: u64) -> Conflict {
        Conflict {
            name: name.into(),
            existing_size,
            incoming_size,
            kind_mismatch: false,
        }
    }

    // -- prompt_title -------------------------------------------------------

    #[test]
    fn prompt_title_uses_the_singular_for_one_file() {
        assert_eq!(prompt_title(1), "1 file already exists");
        assert_eq!(prompt_title(3), "3 files already exist");
    }

    // -- conflict_line ------------------------------------------------------

    #[test]
    fn conflict_line_shows_the_name_and_both_sizes() {
        // Arrange
        let c = conflict("report.pdf", 2048, 3 * 1024 * 1024);
        // Act
        let line = conflict_line(&c);
        // Assert
        assert_eq!(line, "report.pdf · existing 2.0 KB → incoming 3.0 MB");
    }

    #[test]
    fn conflict_line_says_what_is_in_the_way_on_a_kind_mismatch() {
        let c = Conflict {
            kind_mismatch: true,
            ..conflict("d/notes", 9, 0)
        };
        assert_eq!(conflict_line(&c), "d/notes · folder in the way");
    }

    // -- conflict_lines -----------------------------------------------------

    #[test]
    fn conflict_lines_lists_every_row_up_to_eight() {
        let conflicts: Vec<Conflict> = (0..8).map(|i| conflict(&format!("f{i}"), 1, 1)).collect();
        let lines = conflict_lines(&conflicts);
        assert_eq!(lines.len(), 8);
        assert!(lines.iter().all(|l| l.contains("existing")));
    }

    #[test]
    fn conflict_lines_cuts_after_eight_and_counts_the_rest() {
        let conflicts: Vec<Conflict> = (0..11).map(|i| conflict(&format!("f{i}"), 1, 1)).collect();
        let lines = conflict_lines(&conflicts);
        assert_eq!(lines.len(), 9);
        assert!(lines[7].starts_with("f7 ·"));
        assert_eq!(lines[8], "+ 3 more");
    }

    #[test]
    fn conflict_lines_is_empty_without_conflicts() {
        assert!(conflict_lines(&[]).is_empty());
    }
}
