use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

pub use models::*;

#[cfg(desktop)]
mod desktop;
#[cfg(mobile)]
mod mobile;

pub mod capture_queue;
mod commands;
mod error;
pub mod ffi;
mod models;

pub use error::{Error, Result};

#[cfg(desktop)]
use desktop::Audio;
#[cfg(mobile)]
use mobile::Audio;

/// Extensions to [`tauri::App`], [`tauri::AppHandle`] and [`tauri::Window`] to access the audio APIs.
pub trait AudioExt<R: Runtime> {
    fn audio(&self) -> &Audio<R>;
}

impl<R: Runtime, T: Manager<R>> crate::AudioExt<R> for T {
    fn audio(&self) -> &Audio<R> {
        self.state::<Audio<R>>().inner()
    }
}

/// Initialize the audio plugin.
///
/// Ring buffers are created at plugin setup time. The capture queue collector
/// task is spawned as a background async task — it runs forever, managing
/// the pre-roll window and recording flow.
///
/// The actual audio session is started later via `initSession`
/// (when the user enters the tutor screen) and torn down via
/// `teardownSession` (when they leave).
pub fn init<R: Runtime>() -> TauriPlugin<R> {
    Builder::new("audio")
        .invoke_handler(tauri::generate_handler![
            commands::init_session,
            commands::teardown_session,
            commands::enqueue_audio,
            commands::get_status,
        ])
        .setup(|app, api| {
            // Pre-allocate SPSC ring buffers.
            ffi::init_ring_buffers();

            // Spawn the collector task that drains the capture SPSC
            // into the managed queue (pre-roll window / recording).
            tauri::async_runtime::spawn(capture_queue::run_collector());

            #[cfg(mobile)]
            let audio = mobile::init(app, api)?;
            #[cfg(desktop)]
            let audio = desktop::init(app, api)?;
            app.manage(audio);
            Ok(())
        })
        .build()
}
