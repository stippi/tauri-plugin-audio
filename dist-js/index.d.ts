import { type UnlistenFn } from "@tauri-apps/api/event";
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
    /** Pre-roll buffer duration in ms. Default: 500. */
    prerollMs?: number;
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
export declare function isAudioError(value: unknown): value is AudioError;
export interface PlaybackStatusEvent {
    playing: boolean;
}
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
export declare function initSession(config?: SessionConfig): Promise<void>;
/**
 * Tear down the audio session. Stops the engine, removes the tap and
 * source node, deactivates the audio session.
 *
 * Call when leaving the tutor screen.
 */
export declare function teardownSession(): Promise<void>;
/** Get current audio session status. */
export declare function getStatus(): Promise<AudioStatus>;
/**
 * Get current playback visualization levels — 5 frequency bands (0.0–1.0).
 *
 * Intended to be polled at ~30fps during active playback for smooth
 * equalizer bar animation. Cheap to call (reads pre-computed atomics).
 */
export declare function getPlaybackLevels(): Promise<PlaybackLevels>;
/** Sentence playback progress: `[sentenceIndex, progress (0.0–1.0)]`. */
export type PlaybackProgress = [number, number];
/**
 * Get current sentence playback progress.
 *
 * Returns the sentence index currently being played and how far through it
 * the audio output has progressed (0.0–1.0). Used to accurately place the
 * `[interrupted]` marker when the user interrupts playback.
 *
 * Cheap to call (reads atomics + small vec lookup).
 */
export declare function getPlaybackProgress(): Promise<PlaybackProgress>;
/** Listen for playback status changes. */
export declare function onPlaybackStatus(callback: (event: PlaybackStatusEvent) => void): Promise<UnlistenFn>;
