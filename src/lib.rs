use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Runtime,
};

pub use models::*;

#[cfg(desktop)]
mod desktop;
#[cfg(mobile)]
mod mobile;

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
/// Ring buffers are created at plugin setup time with a default pre-roll
/// capacity. The actual audio session is started later via `initSession`
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
            // Pre-allocate ring buffers with a generous default pre-roll.
            // 800ms at 48kHz = 38400 samples. The actual session config
            // can request a different pre-roll size; we size for the max.
            ffi::init_ring_buffers(48_000); // ~1 second headroom

            #[cfg(mobile)]
            let audio = mobile::init(app, api)?;
            #[cfg(desktop)]
            let audio = desktop::init(app, api)?;
            app.manage(audio);
            Ok(())
        })
        .build()
}
