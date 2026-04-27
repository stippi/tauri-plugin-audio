use tauri::{command, AppHandle, Runtime};

use crate::models::*;
use crate::AudioExt;
use crate::Result;

/// Initialize the audio session. Creates an AVAudioEngine with both
/// a capture tap (mic → ring buffer) and a playback source node
/// (ring buffer → speaker). Both are always active until teardown.
///
/// Call when entering the tutor screen.
#[command]
pub(crate) async fn init_session<R: Runtime>(
    app: AppHandle<R>,
    payload: SessionConfig,
) -> Result<OkResponse> {
    // Configure the capture queue pre-roll window from session config.
    crate::capture_queue::set_preroll_ms(payload.preroll_ms);
    app.audio().init_session(payload)
}

/// Tear down the audio session. Stops the engine, removes the tap and
/// source node, deactivates the audio session.
///
/// Call when leaving the tutor screen.
#[command]
pub(crate) async fn teardown_session<R: Runtime>(app: AppHandle<R>) -> Result<OkResponse> {
    app.audio().teardown_session()
}

/// Enqueue a chunk of audio samples for playback.
/// For high-throughput streaming (TTS), prefer pushing from Rust directly
/// via `ffi::push_playback_samples()`.
#[command]
pub(crate) async fn enqueue_audio<R: Runtime>(
    app: AppHandle<R>,
    payload: AudioChunk,
) -> Result<OkResponse> {
    app.audio().enqueue_audio(payload)
}

/// Get current session status.
#[command]
pub(crate) async fn get_status<R: Runtime>(app: AppHandle<R>) -> Result<AudioStatus> {
    app.audio().get_status()
}

/// Get current playback visualization levels (5 frequency bands, 0.0–1.0).
/// Intended to be polled by the frontend during active playback (~30fps).
#[command]
pub(crate) async fn get_playback_levels() -> Result<[f32; 5]> {
    Ok(crate::analyzer::get_levels())
}

/// Get current sentence playback progress.
///
/// Returns `(sentence_index, progress)` where `progress` is `0.0..=1.0`,
/// representing how far through the current sentence the audio output has
/// progressed. Used by the frontend to place the `[interrupted]` marker
/// accurately when the user interrupts playback.
#[command]
pub(crate) async fn get_playback_progress() -> Result<(u32, f32)> {
    Ok(crate::ffi::playback_sentence_progress())
}
