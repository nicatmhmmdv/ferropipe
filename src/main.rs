// Ferropipe — a native Rust SSH/SFTP connection manager with dual-pane file transfer.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod external;
mod localfs;
mod model;
mod native_rdp;
mod rdp;
mod remote;
mod sshconfig;
mod store;
mod vault;
mod winssh;

use anyhow::Context;

fn main() -> eframe::Result<()> {
    let dirs = store::project_dirs().expect("resolve config dir");
    let store_path = store::store_path(&dirs);
    let key_path = vault::default_key_path(dirs.config_dir());

    let store = store::Store::load(&store_path).unwrap_or_else(|e| {
        eprintln!("warning: could not load store: {e:#}");
        store::Store::default()
    });
    let vault = vault::Vault::load_or_create(&key_path)
        .context("init vault")
        .expect("vault");

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1240.0, 780.0])
            .with_min_inner_size([860.0, 520.0])
            .with_title("Ferropipe"),
        ..Default::default()
    };

    eframe::run_native(
        "Ferropipe",
        native_options,
        Box::new(move |cc| Ok(Box::new(app::FerropipeApp::new(cc, store, vault, store_path)))),
    )
}
