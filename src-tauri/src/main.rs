// Prevents an extra console window on Windows in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // `--apply-update` runs the self-update helper instead of the app.
    if let Some(code) = steam_transfer_lib::handle_cli() {
        std::process::exit(code);
    }
    steam_transfer_lib::run()
}
