use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Session configuration
// ---------------------------------------------------------------------------

/// Configuration for the always-on audio session.
///
/// Called once at app startup via `initSession`. Creates a single
/// `AVAudioEngine` with both a capture tap (mic → ring buffer) and a
/// playback source node (ring buffer → speaker). Both are always active
/// for the lifetime of the session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionConfig {
    /// Capture sample rate in Hz. Default: 48000.
    /// This is the rate at which the mic tap delivers samples.
    /// Resampling to 16kHz for STT is done downstream in Rust.
    #[serde(default = "default_capture_sample_rate")]
    pub capture_sample_rate: u32,

    /// Playback sample rate in Hz. Default: 24000.
    /// Must match the rate of audio pushed into the playback ring buffer.
    #[serde(default = "default_playback_sample_rate")]
    pub playback_sample_rate: u32,

    /// Number of capture channels (1 = mono). Default: 1.
    #[serde(default = "default_channels")]
    pub capture_channels: u16,

    /// Number of playback channels. Default: 1.
    #[serde(default = "default_channels")]
    pub playback_channels: u16,

    /// Buffer size hint for the capture tap, in frames. Default: 1024.
    #[serde(default = "default_buffer_size")]
    pub capture_buffer_size: u32,

    /// Duration of the pre-roll buffer in milliseconds. Default: 800.
    /// The plugin keeps a rolling window of the last N ms of captured audio
    /// so that PTT can include audio from before the button press.
    #[serde(default = "default_preroll_ms")]
    pub preroll_ms: u32,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            capture_sample_rate: default_capture_sample_rate(),
            playback_sample_rate: default_playback_sample_rate(),
            capture_channels: default_channels(),
            playback_channels: default_channels(),
            capture_buffer_size: default_buffer_size(),
            preroll_ms: default_preroll_ms(),
        }
    }
}

fn default_capture_sample_rate() -> u32 {
    48_000
}

fn default_playback_sample_rate() -> u32 {
    24_000
}

fn default_channels() -> u16 {
    1
}

fn default_buffer_size() -> u32 {
    1024
}

fn default_preroll_ms() -> u32 {
    800
}

// ---------------------------------------------------------------------------
// Audio chunk (for playback enqueue)
// ---------------------------------------------------------------------------

/// A chunk of audio samples to enqueue for playback.
/// Pushed from Rust (TTS provider) via `ffi::push_playback_samples()`,
/// or from the frontend via the `enqueue_audio` command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioChunk {
    /// PCM samples as f32, interleaved if multi-channel.
    pub samples: Vec<f32>,
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Overall session state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum SessionState {
    /// No session active. `initSession` has not been called or `teardownSession` was called.
    Inactive,
    /// Session is active. Mic tap and playback source node are live.
    Active,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStatus {
    pub session: SessionState,
    /// Number of samples currently in the playback ring buffer.
    pub playback_buffered: usize,
    /// Number of samples currently in the pre-roll window.
    pub preroll_buffered: usize,
}

/// Generic success response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OkResponse {
    pub success: bool,
}

impl OkResponse {
    pub fn ok() -> Self {
        Self { success: true }
    }
}

// ---------------------------------------------------------------------------
// Events emitted to the frontend
// ---------------------------------------------------------------------------

/// Event payload for playback status changes.
/// Emitted on the `audio://playback-status` channel.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackStatusEvent {
    /// true when playback ring buffer has data being consumed, false when drained.
    pub playing: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_config_defaults() {
        let cfg = SessionConfig::default();
        assert_eq!(cfg.capture_sample_rate, 48_000);
        assert_eq!(cfg.playback_sample_rate, 24_000);
        assert_eq!(cfg.capture_channels, 1);
        assert_eq!(cfg.playback_channels, 1);
        assert_eq!(cfg.capture_buffer_size, 1024);
        assert_eq!(cfg.preroll_ms, 800);
    }

    #[test]
    fn session_config_from_json() {
        let json = r#"{"captureSampleRate": 16000, "playbackSampleRate": 22050, "prerollMs": 500}"#;
        let cfg: SessionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.capture_sample_rate, 16_000);
        assert_eq!(cfg.playback_sample_rate, 22_050);
        assert_eq!(cfg.preroll_ms, 500);
        // defaults for unset fields
        assert_eq!(cfg.capture_channels, 1);
        assert_eq!(cfg.playback_channels, 1);
        assert_eq!(cfg.capture_buffer_size, 1024);
    }

    #[test]
    fn session_config_empty_json() {
        let json = r#"{}"#;
        let cfg: SessionConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg, SessionConfig::default());
    }

    #[test]
    fn audio_chunk_roundtrip() {
        let chunk = AudioChunk {
            samples: vec![0.0, 0.5, -0.5, 1.0],
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let decoded: AudioChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(chunk.samples, decoded.samples);
    }

    #[test]
    fn audio_status_serialization() {
        let status = AudioStatus {
            session: SessionState::Active,
            playback_buffered: 1024,
            preroll_buffered: 8000,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"session\":\"active\""));
        assert!(json.contains("\"playbackBuffered\":1024"));
        assert!(json.contains("\"prerollBuffered\":8000"));
    }

    // Needed so session_config_empty_json comparison works
    impl PartialEq for SessionConfig {
        fn eq(&self, other: &Self) -> bool {
            self.capture_sample_rate == other.capture_sample_rate
                && self.playback_sample_rate == other.playback_sample_rate
                && self.capture_channels == other.capture_channels
                && self.playback_channels == other.playback_channels
                && self.capture_buffer_size == other.capture_buffer_size
                && self.preroll_ms == other.preroll_ms
        }
    }
}
