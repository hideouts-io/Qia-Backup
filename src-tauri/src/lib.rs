//! Tauri desktop shell for the Qia Backup engine (macOS).

pub mod app;
mod auth;
mod power;

pub fn run() {
    app::run();
}
