import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface SessionConfig {
  /** Capture sample rate in Hz. Default: 48000. */
  captureSampleRate?: number;
  /** Playback sample rate in Hz. Default: 24000. */
  playbackSampleRate?: number;
  /** Capture channels (1 = mono). Default: 1. */
  captureChannels?: number;
  /** Playback channels. Default: 1. */
  playbackChannels?: number;
  /** Capture tap buffer size hint in frames. Default: 1024. */
  captureBufferSize?: number;
  /** Pre-roll buffer duration in ms. Default: 800. */
  prerollMs?: number;
}

export interface AudioChunk {
  /** PCM samples as f32 array, interleaved if multi-channel. */
  samples: number[];
}

export type SessionState = "inactive" | "active";

export interface AudioStatus {
  session: SessionState;
  /** Approximate samples buffered for playback. */
  playbackBuffered: number;
}

/** 5 frequency band levels (0.0–1.0) for playback visualization. */
export type PlaybackLevels = [number, number, number, number, number];

export interface AudioError {
  code: string;
  message: string;
}

export function isAudioError(value: unknown): value is AudioError {
  return (
    typeof value === "object" &&
    value !== null &&
    "code" in value &&
    "message" in value
  );
}

// ---------------------------------------------------------------------------
// Event types
// ---------------------------------------------------------------------------

export interface PlaybackStatusEvent {
  playing: boolean;
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
export async function initSession(config?: SessionConfig): Promise<void> {
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
export async function teardownSession(): Promise<void> {
  await invoke("plugin:audio|teardown_session");
}

/**
 * Enqueue a chunk of audio samples for playback.
 *
 * For high-throughput streaming (TTS), prefer pushing from Rust directly
 * via `ffi::push_playback_samples()`. This command exists for cases where
 * the frontend has audio data to play.
 */
export async function enqueueAudio(chunk: AudioChunk): Promise<void> {
  await invoke("plugin:audio|enqueue_audio", { payload: chunk });
}

/** Get current audio session status. */
export async function getStatus(): Promise<AudioStatus> {
  return await invoke<AudioStatus>("plugin:audio|get_status");
}

/**
 * Get current playback visualization levels — 5 frequency bands (0.0–1.0).
 *
 * Intended to be polled at ~30fps during active playback for smooth
 * equalizer bar animation. Cheap to call (reads pre-computed atomics).
 */
export async function getPlaybackLevels(): Promise<PlaybackLevels> {
  return await invoke<PlaybackLevels>("plugin:audio|get_playback_levels");
}

// ---------------------------------------------------------------------------
// Event listeners
// ---------------------------------------------------------------------------

/** Listen for playback status changes. */
export async function onPlaybackStatus(
  callback: (event: PlaybackStatusEvent) => void
): Promise<UnlistenFn> {
  return listen<PlaybackStatusEvent>("audio://playback-status", (event) => {
    callback(event.payload);
  });
}
