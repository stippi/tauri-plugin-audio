use tauri::{AppHandle, Runtime};

use crate::models::*;

/// Desktop audio engine — stub implementation.
///
/// The real audio work happens on iOS via AVAudioEngine + FFI.
/// On desktop the plugin compiles and logs warnings so the tutor app
/// can be developed/tested on macOS.
pub struct Audio<R: Runtime> {
    _app: AppHandle<R>,
}

pub fn init<R: Runtime>(
    app: &AppHandle<R>,
    _api: tauri::plugin::PluginApi<R, ()>,
) -> crate::Result<Audio<R>> {
    Ok(Audio {
        _app: app.clone(),
    })
}

impl<R: Runtime> Audio<R> {
    pub fn init_session(&self, _config: SessionConfig) -> crate::Result<OkResponse> {
        log::warn!("audio: init_session is a no-op on desktop");
        Ok(OkResponse::ok())
    }

    pub fn teardown_session(&self) -> crate::Result<OkResponse> {
        log::warn!("audio: teardown_session is a no-op on desktop");
        Ok(OkResponse::ok())
    }

    pub fn enqueue_audio(&self, _chunk: AudioChunk) -> crate::Result<OkResponse> {
        log::warn!("audio: enqueue_audio is a no-op on desktop");
        Ok(OkResponse::ok())
    }

    pub fn get_status(&self) -> crate::Result<AudioStatus> {
        Ok(AudioStatus {
            session: SessionState::Inactive,
            playback_buffered: 0,
            preroll_buffered: 0,
        })
    }
}
