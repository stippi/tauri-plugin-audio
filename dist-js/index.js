import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';

function isAudioError(value) {
    return (typeof value === "object" &&
        value !== null &&
        "code" in value &&
        "message" in value);
}
// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------
/**
 * Initialize the audio session. Creates a native AVAudioEngine with:
 * - An always-on mic capture tap (samples flow into Rust ring buffers)
 * - An always-on playback source node (pulls from Rust ring buffer)
 *
 * The mic is hot from this point — captured audio accumulates in the
 * pre-roll buffer so PTT can include audio from before the button press.
 *
 * No audio ducking: capture and playback share the same engine/session.
 *
 * Call when entering the tutor screen.
 */
async function initSession(config) {
    await invoke("plugin:audio|init_session", {
        payload: config ?? {},
    });
}
/**
 * Tear down the audio session. Stops the engine, removes the tap and
 * source node, deactivates the audio session.
 *
 * Call when leaving the tutor screen.
 */
async function teardownSession() {
    await invoke("plugin:audio|teardown_session");
}
/** Get current audio session status. */
async function getStatus() {
    return await invoke("plugin:audio|get_status");
}
/**
 * Get current playback visualization levels — 5 frequency bands (0.0–1.0).
 *
 * Intended to be polled at ~30fps during active playback for smooth
 * equalizer bar animation. Cheap to call (reads pre-computed atomics).
 */
async function getPlaybackLevels() {
    return await invoke("plugin:audio|get_playback_levels");
}
/**
 * Get current sentence playback progress.
 *
 * Returns the sentence index currently being played and how far through it
 * the audio output has progressed (0.0–1.0). Used to accurately place the
 * `[interrupted]` marker when the user interrupts playback.
 *
 * Cheap to call (reads atomics + small vec lookup).
 */
async function getPlaybackProgress() {
    return await invoke("plugin:audio|get_playback_progress");
}
// ---------------------------------------------------------------------------
// Event listeners
// ---------------------------------------------------------------------------
/** Listen for playback status changes. */
async function onPlaybackStatus(callback) {
    return listen("audio://playback-status", (event) => {
        callback(event.payload);
    });
}

export { getPlaybackLevels, getPlaybackProgress, getStatus, initSession, isAudioError, onPlaybackStatus, teardownSession };
