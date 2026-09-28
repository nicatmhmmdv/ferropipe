//! Remote → remote copies, staged through a private temp dir: the source
//! worker downloads into it, then the destination worker uploads from it. The
//! job is kept until the upload completes so that leg can be re-issued after a
//! conflict prompt without downloading again.

use super::*;

/// A remote→remote staged transfer: what its upload leg needs, kept from the
/// download leg until the upload completes so the leg can be re-issued.
pub(in crate::app) struct StageJob {
    pub temp_dir: PathBuf,
    pub names: Vec<String>,
    pub dest_dir: String,
    pub dest: Side,
    /// The host `dest` showed when the copy started; the upload must go there.
    pub dest_host: Option<Uuid>,
    pub label: String,
    pub dest_label: String,
    /// What the upload leg does with files already in `dest_dir`.
    pub on_conflict: OnConflict,
}

impl FerropipeApp {
    /// remote → remote: download into a temp dir now, upload from it when that
    /// finishes. The temp dir is private, so only the upload leg can conflict.
    pub(super) fn start_staged(
        &mut self,
        ctx: &egui::Context,
        payload: DragPayload,
        dest: Side,
        on_conflict: OnConflict,
    ) {
        let temp_dir = std::env::temp_dir().join(format!(
            "ferropipe-r2r-{}-{}",
            std::process::id(),
            self.next_id
        ));
        if let Err(e) = std::fs::create_dir_all(&temp_dir) {
            self.toast(ctx, format!("stage: {e:#}"), true);
            return;
        }
        let spec = self.transfer_spec(&payload, dest, payload.side, Verb::Copy);
        let job = StageJob {
            temp_dir: temp_dir.clone(),
            names: payload.paths.iter().map(|p| base_name(p)).collect(),
            dest_dir: self.side_pane(dest).cwd.clone(),
            dest,
            dest_host: self.side_host(dest),
            label: spec.label.clone(),
            dest_label: spec.dest.clone(),
            on_conflict,
        };
        let id = self.push_transfer(ctx, spec);
        let cmd = Command::Download {
            id,
            remote: payload.paths,
            local_dir: temp_dir,
            on_conflict: OnConflict::Overwrite,
        };
        self.side_send(payload.side, cmd);
        self.stage_download.insert(id, job);
    }

    /// The staged download leg ended: start the upload leg, or give up.
    pub(super) fn finish_stage_download(
        &mut self,
        ctx: &egui::Context,
        job: StageJob,
        done: Finished,
    ) {
        if done.error.is_some() {
            let _ = std::fs::remove_dir_all(&job.temp_dir);
            self.toast_result(ctx, &done);
            return;
        }
        self.start_stage_upload(ctx, job);
    }

    /// Issue (or, after a conflict prompt, re-issue) the upload leg of a
    /// staged copy from its temp dir with the job's policy. The leg is dropped
    /// when the destination pane no longer shows the host it was aimed at.
    pub(in crate::app) fn start_stage_upload(&mut self, ctx: &egui::Context, job: StageJob) {
        if self.side_host(job.dest) != job.dest_host {
            let _ = std::fs::remove_dir_all(&job.temp_dir);
            self.toast(
                ctx,
                "The destination pane changed host — copy dropped",
                true,
            );
            return;
        }
        let spec = NewTransfer {
            worker: job.dest,
            label: job.label.clone(),
            dest: job.dest_label.clone(),
            upload: true,
            verb: Verb::Copy,
            retry: None,
        };
        let id = self.push_transfer(ctx, spec);
        let local = job.names.iter().map(|n| job.temp_dir.join(n)).collect();
        let cmd = Command::Upload {
            id,
            local,
            remote_dir: job.dest_dir.clone(),
            on_conflict: job.on_conflict,
        };
        self.side_send(job.dest, cmd);
        self.stage_cleanup.insert(id, job);
    }
}
