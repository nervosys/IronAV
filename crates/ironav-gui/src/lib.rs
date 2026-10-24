//! IronAV's native desktop GUI (egui/eframe), laid out like HandBrake: toolbar, source
//! summary, settings tabs, and a job queue. It calls the engine in-process (no server, no
//! sockets), and its "Agent" tab shows the exact `ironav` command and job JSON behind the form.

mod app;
mod player;
pub mod model;

use ironav_core::{Error, ErrorCode, Result};

/// Open the main window and block until it is closed. Must run on the main thread.
pub fn run(initial_file: Option<String>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("IronAV")
            .with_inner_size([1040.0, 760.0])
            .with_min_inner_size([680.0, 520.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "IronAV",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc.egui_ctx.clone(), initial_file.clone())))),
    )
    .map_err(|e| {
        Error::new(ErrorCode::Internal, format!("could not start the native GUI: {e}"))
            .hint("on a headless machine use `ironav gui --web` or the CLI/MCP interfaces")
    })
}

/// Open the player window and block until it is closed. Must run on the main thread.
pub fn run_player(initial_file: Option<String>) -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("IronAV Player")
            .with_inner_size([1024.0, 640.0])
            .with_min_inner_size([520.0, 320.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("IronAV Player", options, Box::new(move |_cc| Ok(Box::new(player::PlayerApp::new(initial_file.clone())))))
        .map_err(|e| Error::new(ErrorCode::Internal, format!("could not start the player window: {e}")).hint("on a headless machine use `ironav play`, which has a terminal and a JSON mode"))
}
