use serde::de::DeserializeOwned;
use tauri::{
    plugin::{PluginApi, PluginHandle},
    AppHandle, Runtime,
};

use crate::models::*;

#[cfg(target_os = "ios")]
tauri::ios_plugin_binding!(init_plugin_audio);

pub fn init<R: Runtime, C: DeserializeOwned>(
    _app: &AppHandle<R>,
    api: PluginApi<R, C>,
) -> crate::Result<Audio<R>> {
    #[cfg(target_os = "android")]
    let handle = api.register_android_plugin("io.affex.audio", "AudioPlugin")?;
    #[cfg(target_os = "ios")]
    let handle = api.register_ios_plugin(init_plugin_audio)?;
    Ok(Audio(handle))
}

/// Mobile audio engine — delegates control-plane commands to native plugins.
///
/// The audio data path bypasses this entirely: Swift's AVAudioEngine tap
/// calls `rust_audio_capture_callback` (FFI) → capture ring buffer + pre-roll,
/// and `rust_audio_render_callback` (FFI) pulls from the playback ring buffer.
pub struct Audio<R: Runtime>(PluginHandle<R>);

impl<R: Runtime> Audio<R> {
    pub fn init_session(&self, payload: SessionConfig) -> crate::Result<OkResponse> {
        self.0
            .run_mobile_plugin("initSession", payload)
            .map_err(Into::into)
    }

    pub fn teardown_session(&self) -> crate::Result<OkResponse> {
        self.0
            .run_mobile_plugin("teardownSession", ())
            .map_err(Into::into)
    }

    pub fn enqueue_audio(&self, payload: AudioChunk) -> crate::Result<OkResponse> {
        self.0
            .run_mobile_plugin("enqueueAudio", payload)
            .map_err(Into::into)
    }

    pub fn get_status(&self) -> crate::Result<AudioStatus> {
        self.0
            .run_mobile_plugin("getStatus", ())
            .map_err(Into::into)
    }
}
