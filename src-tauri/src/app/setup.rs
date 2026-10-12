use tauri::{App, Manager};

use crate::app::platform;
use crate::telemetry::handle_initial_run_telemetry;

/// Shared app setup logic for both desktop and mobile
pub fn setup_tauri_app(
    app: &App,
    specta_builder: &tauri_specta::Builder,
) -> Result<(), Box<dyn std::error::Error>> {
    log::info!("Setting up tauri application");

    platform::init_platform(app)?;
    app.manage(crate::DeepAnalysisState::default());
    // A damaged Deep Analysis database must not prevent the rest of the app from opening.
    // Deep Analysis commands will report the database error when invoked.
    if let Err(error) = crate::deep_analysis::recover_on_startup(app.handle()) {
        log::error!("Deep Analysis recovery failed: {error}");
    }

    specta_builder.mount_events(app);

    let _ = log::info!("Finished tauri application initialization");
    let _ = handle_initial_run_telemetry(&app.handle());
    Ok(())
}
