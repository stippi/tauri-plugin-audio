# tauri-plugin-audio

Native audio capture and playback plugin for Tauri 2. Designed for AI voice applications that need simultaneous, always-on microphone capture and streaming TTS playback without ducking or round-tripping audio through the frontend.

## Architecture & Features

The plugin runs a single audio session for the entire lifetime of the tutor screen — one path for capture, one for playback, both active at the same time.

### Data path

```
Mic → [Swift tap / cpal callback]
    → FFI: rust_audio_capture_callback()   (realtime, no alloc)
    → SPSC ring buffer (192k sample capacity)
    → collector task (async, 20ms tick)
    → pre-roll VecDeque  ──► start_recording() → mpsc channel → STT

TTS Rust thread
    → ffi::push_playback_samples_all()
    → SPSC ring buffer (96k sample capacity)
    → FFI: rust_audio_render_callback()    (realtime, no alloc)
    → Speaker
```

Both FFI callbacks are realtime-safe: no heap allocation, no blocking locks (`try_lock` only), no logging.

### Pre-roll buffer

The collector task keeps a rolling window of captured audio (configurable, default 500 ms). When `start_recording()` is called the pre-roll is flushed to the mpsc channel first, followed by live chunks seamlessly — so push-to-talk includes audio from before the button press.

### Playback visualization

The render callback feeds played samples through a 5-band biquad bandpass analyzer (120 Hz, 350 Hz, 1 kHz, 3 kHz, 8 kHz). Band levels are exponentially smoothed (fast attack, slow release) and published as `AtomicU32` values. The frontend polls `getPlaybackLevels()` at ~30 fps for equalizer bar animation — no locking involved.

### Drain detection

After `ffi::mark_playback_all_pushed()` is called, the render callback stamps a timestamp the first time it observes an empty ring buffer. `ffi::ms_since_playback_drained()` lets the agent loop emit a `NativePlaybackComplete` event with an accurate end-of-speech timestamp, accounting for OS pipeline latency.

### Platform support

| Platform | Audio engine |
|---|---|
| iOS | AVAudioEngine via Swift FFI (`ios/Sources/AudioPlugin.swift`) |
| macOS / Linux / Windows | [cpal](https://github.com/RustAudio/cpal) (`src/desktop.rs`) |

---

## Using in a Tauri App

### 1. Add the dependency

```toml
# src-tauri/Cargo.toml
[dependencies]
tauri-plugin-audio = { path = "../tauri-plugin-audio" }
```

### 2. Register the plugin

```rust
// src-tauri/src/main.rs
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_audio::init())
        .run(tauri::generate_context!())
        .expect("error running app");
}
```

### 3. Allow permissions

```toml
# src-tauri/capabilities/default.json  (or add to an existing capability)
"permissions": ["audio:default"]
```

The `default` permission set allows all five commands: `init-session`, `teardown-session`, `enqueue-audio`, `get-status`, `get-playback-levels`.

### 4. Frontend (TypeScript)

```typescript
import {
  initSession,
  teardownSession,
  getPlaybackLevels,
} from "tauri-plugin-audio";

// Start the session when entering the voice screen.
await initSession({ prerollMs: 500, playbackSampleRate: 24000 });

// Poll for visualization (call from your animation loop).
const levels = await getPlaybackLevels(); // [f32; 5], each 0.0–1.0

// Tear down when leaving the screen.
await teardownSession();
```

### 5. Rust — streaming TTS playback

Push PCM samples (f32, mono, matching `playbackSampleRate`) directly from any Rust thread:

```rust
use tauri_plugin_audio::ffi;

// At the start of each agent turn:
ffi::begin_playback_turn();

// Stream TTS chunks (blocks with short sleeps if ring buffer is full):
ffi::push_playback_samples_all(&samples);

// Signal that no more audio is coming for this turn:
ffi::mark_playback_all_pushed();

// Poll until drained (the render callback confirms the buffer is empty):
while ffi::ms_since_playback_drained().is_none() {
    tokio::time::sleep(Duration::from_millis(20)).await;
}
```

### 6. Rust — capture / STT

```rust
use tauri_plugin_audio::capture_queue;

// Returns a channel receiver; pre-roll chunks arrive first.
let mut rx = capture_queue::start_recording();

while let Some(chunk) = rx.recv().await {
    // chunk: Vec<f32>, mono, at the hardware capture rate (typically 48 kHz)
    // Downsample to 16 kHz here before handing to your STT engine.
}

capture_queue::stop_recording();
```
