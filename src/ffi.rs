//! Lock-free FFI bridge between Swift audio callbacks and Rust.
//!
//! # Architecture
//!
//! There are two ring buffers:
//!
//! 1. **Capture SPSC** — A standard SPSC ring buffer for mic capture.
//!    The Swift/cpal audio callback pushes raw samples here. The Rust-side
//!    collector task (`capture_queue::run_collector`) drains them into a
//!    managed `VecDeque` that handles pre-roll windowing and recording.
//!
//! 2. **Playback FIFO** — SPSC ring buffer for TTS output. Rust pushes
//!    samples in, Swift's `AVAudioSourceNode` render block pulls them out.
//!
//! # Safety
//!
//! The FFI functions run on iOS realtime audio threads. Rules:
//! - No heap allocations
//! - No blocking locks (only `try_lock`)
//! - No logging / println
//! - If a lock is contended, drop the buffer and move on

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;

use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapRb,
};

// ---------------------------------------------------------------------------
// Capacities
// ---------------------------------------------------------------------------

/// Capture FIFO capacity. Sized to absorb up to 2s of latency in the Rust
/// collector at up to 96 kHz mono.
const CAPTURE_FIFO_CAPACITY: usize = 96_000 * 2;

/// Playback ring buffer — 2 seconds at 48 kHz to absorb jitter.
/// (Playback is driven at 24kHz content rate, so this is ~4s of audio.)
const PLAYBACK_RING_CAPACITY: usize = 48_000 * 2;

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

type RbProducer = ringbuf::HeapProd<f32>;
type RbConsumer = ringbuf::HeapCons<f32>;

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static CAPTURE_PRODUCER: OnceLock<std::sync::Mutex<RbProducer>> = OnceLock::new();
static CAPTURE_CONSUMER: OnceLock<std::sync::Mutex<RbConsumer>> = OnceLock::new();

static PLAYBACK_PRODUCER: OnceLock<std::sync::Mutex<RbProducer>> = OnceLock::new();
static PLAYBACK_CONSUMER: OnceLock<std::sync::Mutex<RbConsumer>> = OnceLock::new();

/// Tracks how many samples are currently in the playback buffer.
/// Incremented on push, decremented on pull. Approximate (relaxed ordering)
/// but good enough for status reporting.
static PLAYBACK_LEVEL: AtomicUsize = AtomicUsize::new(0);

/// Monotonic instant (as nanos since an arbitrary epoch via `Instant`) at which
/// the render callback last pulled >0 real samples from the playback ring.
/// Used by the drain monitor to add a grace period after the ring buffer empties,
/// accounting for OS audio pipeline latency between ring-buffer drain and actual
/// speaker output (especially relevant on macOS where cpal pulls aggressively).
static LAST_NONZERO_PULL_NANOS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Sentence progress tracking
// ---------------------------------------------------------------------------

/// Cumulative samples pushed into the playback ring this turn.
/// Reset by `begin_playback_turn()`.
static PLAYBACK_TOTAL_PUSHED: AtomicUsize = AtomicUsize::new(0);

/// Cumulative samples pulled by the render callback this turn.
/// Reset by `begin_playback_turn()`.
static PLAYBACK_TOTAL_RENDERED: AtomicUsize = AtomicUsize::new(0);

/// Sentence boundaries: `(sentence_index, cumulative_push_offset)`.
/// Each entry records the cumulative sample count *before* the sentence's
/// audio was pushed. Together with `PLAYBACK_TOTAL_RENDERED`, this lets us
/// compute which sentence is currently playing and how far into it we are.
static SENTENCE_BOUNDARIES: std::sync::Mutex<Vec<(u32, usize)>> =
    std::sync::Mutex::new(Vec::new());

/// When true, the render callback skips pulling samples and outputs silence.
/// Samples remain in the ring buffer and resume exactly where they left off.
static PLAYBACK_PAUSED: AtomicBool = AtomicBool::new(false);

/// Set by the agent loop when all audio for the current turn has been pushed
/// into the ring buffer. The render callback checks this: when it's set AND
/// the ring buffer is empty, it stamps `PLAYBACK_DRAINED_NANOS`.
/// Reset at the start of each new turn via `begin_playback_turn()`.
static PLAYBACK_ALL_PUSHED: AtomicBool = AtomicBool::new(false);

/// Monotonic timestamp (nanos) at which the render callback first observed
/// an empty ring buffer after `PLAYBACK_ALL_PUSHED` was set. Zero means
/// drain hasn't been detected yet.
static PLAYBACK_DRAINED_NANOS: AtomicU64 = AtomicU64::new(0);

/// Returns a monotonic nanosecond timestamp (relative to process start).
/// Safe to call from realtime threads — no allocation, no syscall on most platforms.
fn mono_nanos() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(std::time::Instant::now);
    epoch.elapsed().as_nanos() as u64
}

// ---------------------------------------------------------------------------
// Diagnostic counters (relaxed atomics, safe from realtime threads)
// ---------------------------------------------------------------------------

/// Total samples pushed into the capture FIFO by the Swift tap.
static CAPTURE_SAMPLES_PUSHED: AtomicUsize = AtomicUsize::new(0);
/// Total samples dropped (FIFO full) by the Swift tap.
static CAPTURE_SAMPLES_DROPPED: AtomicUsize = AtomicUsize::new(0);
/// Number of capture callback invocations.
static CAPTURE_CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Number of times the capture callback failed to acquire the FIFO producer lock.
static CAPTURE_FIFO_LOCK_FAIL: AtomicUsize = AtomicUsize::new(0);
/// Number of render callback invocations.
static RENDER_CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Total samples pulled from the playback ring by the render callback.
static RENDER_SAMPLES_PULLED: AtomicUsize = AtomicUsize::new(0);
/// Number of times the render callback failed to acquire the playback consumer lock.
static RENDER_LOCK_FAIL: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// Engine config reported by Swift at setup time (stored as atomics)
// ---------------------------------------------------------------------------

/// Hardware sample rate (e.g. 48000), stored as integer Hz.
static ENGINE_HW_SAMPLE_RATE: AtomicUsize = AtomicUsize::new(0);
/// Playback sample rate the source node was configured with.
static ENGINE_PB_SAMPLE_RATE: AtomicUsize = AtomicUsize::new(0);
/// Upsample ratio (hw_rate / pb_rate), e.g. 2 for 48k/24k.
static ENGINE_UPSAMPLE_RATIO: AtomicUsize = AtomicUsize::new(0);
/// 1 when the AVAudioEngine is running, 0 when stopped/rebuilding.
static ENGINE_RUNNING: AtomicBool = AtomicBool::new(false);
/// Number of times the engine has been built/rebuilt.
static ENGINE_BUILD_COUNT: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// Diagnostics struct
// ---------------------------------------------------------------------------

/// Snapshot of all diagnostic counters. Useful for logging / debugging.
#[derive(Debug, Clone)]
pub struct AudioDiagnostics {
    pub capture_callback_count: usize,
    pub capture_samples_pushed: usize,
    pub capture_samples_dropped: usize,
    pub capture_fifo_lock_fail: usize,
    pub capture_fifo_level: usize,
    pub render_callback_count: usize,
    pub render_samples_pulled: usize,
    pub render_lock_fail: usize,
    pub playback_level: usize,
    pub engine_hw_rate: usize,
    pub engine_pb_rate: usize,
    pub engine_upsample_ratio: usize,
    pub engine_running: bool,
    pub engine_build_count: usize,
}

impl std::fmt::Display for AudioDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "capture(cb={} pushed={} dropped={} fifo_lvl={} fifo_lockfail={}) \
             render(cb={} pulled={} lockfail={}) \
             playback_lvl={} engine(hw={}Hz pb={}Hz ratio={} running={} builds={})",
            self.capture_callback_count,
            self.capture_samples_pushed,
            self.capture_samples_dropped,
            self.capture_fifo_level,
            self.capture_fifo_lock_fail,
            self.render_callback_count,
            self.render_samples_pulled,
            self.render_lock_fail,
            self.playback_level,
            self.engine_hw_rate,
            self.engine_pb_rate,
            self.engine_upsample_ratio,
            self.engine_running,
            self.engine_build_count,
        )
    }
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Initialize all ring buffers. Called once during plugin setup.
pub fn init_ring_buffers() {
    let capture_rb = HeapRb::<f32>::new(CAPTURE_FIFO_CAPACITY);
    let (prod, cons) = capture_rb.split();
    let _ = CAPTURE_PRODUCER.set(std::sync::Mutex::new(prod));
    let _ = CAPTURE_CONSUMER.set(std::sync::Mutex::new(cons));

    let playback_rb = HeapRb::<f32>::new(PLAYBACK_RING_CAPACITY);
    let (prod, cons) = playback_rb.split();
    let _ = PLAYBACK_PRODUCER.set(std::sync::Mutex::new(prod));
    let _ = PLAYBACK_CONSUMER.set(std::sync::Mutex::new(cons));
}

// ---------------------------------------------------------------------------
// Rust-side API (called from normal threads, can block on locks)
// ---------------------------------------------------------------------------

/// Push samples into the playback ring buffer.
///
/// Writes as many samples as currently fit into the ring buffer and returns
/// the count. Callers that need backpressure (to avoid dropping audio) should
/// use `push_playback_samples_all()` instead, which retries in a loop.
///
/// Called from a normal (non-realtime) thread. The realtime render callback
/// uses `try_lock` on the consumer side, so contention is brief.
pub fn push_playback_samples(samples: &[f32]) -> usize {
    if let Some(prod) = PLAYBACK_PRODUCER.get() {
        if let Ok(mut prod) = prod.lock() {
            let written = prod.push_slice(samples);
            if written > 0 {
                PLAYBACK_LEVEL.fetch_add(written, Ordering::Relaxed);
                PLAYBACK_TOTAL_PUSHED.fetch_add(written, Ordering::Relaxed);
            }
            return written;
        }
    }
    0
}

/// Push **all** samples into the playback ring buffer, blocking with short
/// sleeps until space becomes available if the buffer is full. This prevents
/// silent drops when TTS produces audio faster than the output device consumes
/// it. **Must be called from a blocking context** (e.g., `spawn_blocking`).
///
/// Returns the total number of samples written (always `samples.len()`
/// unless the producer lock is permanently poisoned).
pub fn push_playback_samples_all(samples: &[f32]) -> usize {
    let Some(prod) = PLAYBACK_PRODUCER.get() else {
        return 0;
    };
    let mut written = 0;
    while written < samples.len() {
        if let Ok(mut prod) = prod.lock() {
            let n = prod.push_slice(&samples[written..]);
            if n > 0 {
                PLAYBACK_LEVEL.fetch_add(n, Ordering::Relaxed);
                PLAYBACK_TOTAL_PUSHED.fetch_add(n, Ordering::Relaxed);
                written += n;
            }
        }
        if written < samples.len() {
            // Ring buffer full — sleep briefly to let the render callback drain.
            // 5ms ≈ 240 samples at 48kHz — short enough to avoid audible gaps.
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    written
}

/// Drain captured audio from the capture SPSC ring.
///
/// Called by the collector task (`capture_queue::run_collector`) to move
/// samples from the lock-free ring into the managed queue.
/// Returns the number of samples read.
pub fn drain_capture_samples(buf: &mut [f32]) -> usize {
    if let Some(cons) = CAPTURE_CONSUMER.get() {
        if let Ok(mut cons) = cons.lock() {
            return cons.pop_slice(buf);
        }
    }
    0
}

/// Get the approximate number of samples in the playback buffer.
pub fn playback_level() -> usize {
    PLAYBACK_LEVEL.load(Ordering::Relaxed)
}

/// Milliseconds elapsed since the render callback last pulled real (non-silent)
/// samples from the playback ring buffer. Returns `u64::MAX` if no pull has
/// ever occurred (i.e. playback never started).
///
/// The drain monitor uses this to add a grace period after `playback_level()`
/// reaches zero, compensating for OS audio pipeline latency between ring-buffer
/// drain and actual speaker output.
pub fn ms_since_last_playback_pull() -> u64 {
    let last = LAST_NONZERO_PULL_NANOS.load(Ordering::Relaxed);
    if last == 0 {
        return u64::MAX;
    }
    let now = mono_nanos();
    now.saturating_sub(last) / 1_000_000
}

/// Reset playback turn state. Call at the start of each agent turn, before
/// any audio chunks are pushed. Clears the `all_pushed` and `drained` flags
/// so the drain monitor can work cleanly for the new turn.
pub fn begin_playback_turn() {
    PLAYBACK_ALL_PUSHED.store(false, Ordering::Relaxed);
    PLAYBACK_DRAINED_NANOS.store(0, Ordering::Relaxed);
    PLAYBACK_PAUSED.store(false, Ordering::Relaxed);
    PLAYBACK_TOTAL_PUSHED.store(0, Ordering::Relaxed);
    PLAYBACK_TOTAL_RENDERED.store(0, Ordering::Relaxed);
    if let Ok(mut boundaries) = SENTENCE_BOUNDARIES.lock() {
        boundaries.clear();
    }
}

/// Signal that no more audio will be pushed for the current turn. The render
/// callback will stamp `PLAYBACK_DRAINED_NANOS` once it observes an empty
/// ring buffer after this flag is set.
pub fn mark_playback_all_pushed() {
    PLAYBACK_ALL_PUSHED.store(true, Ordering::Release);
}

/// Pause playback — the render callback will output silence while samples
/// remain queued in the ring buffer. Call `resume_playback()` to continue.
pub fn pause_playback() {
    PLAYBACK_PAUSED.store(true, Ordering::Release);
}

/// Resume playback after `pause_playback()`. Samples continue from where
/// they were paused.
pub fn resume_playback() {
    PLAYBACK_PAUSED.store(false, Ordering::Release);
}

/// Returns whether playback is currently paused.
pub fn is_playback_paused() -> bool {
    PLAYBACK_PAUSED.load(Ordering::Acquire)
}

/// Discard all queued playback samples. The ring buffer consumer is drained
/// and the level counter is reset. This is a blocking operation (locks the
/// consumer mutex) — call from a normal thread, not a realtime callback.
pub fn clear_playback_buffer() {
    if let Some(cons) = PLAYBACK_CONSUMER.get() {
        if let Ok(mut cons) = cons.lock() {
            // Drain all remaining samples
            let mut scratch = [0.0f32; 4096];
            loop {
                let n = cons.pop_slice(&mut scratch);
                if n == 0 {
                    break;
                }
            }
        }
    }
    PLAYBACK_LEVEL.store(0, Ordering::Relaxed);
    PLAYBACK_PAUSED.store(false, Ordering::Relaxed);
}

/// Milliseconds elapsed since the render callback confirmed all audio has been
/// drained *after* `mark_playback_all_pushed()` was called. Returns `None` if
/// either the flag hasn't been set or drain hasn't been detected yet.
///
/// The drain monitor polls this to know when to emit `NativePlaybackComplete`.
pub fn ms_since_playback_drained() -> Option<u64> {
    if !PLAYBACK_ALL_PUSHED.load(Ordering::Acquire) {
        return None;
    }
    let ts = PLAYBACK_DRAINED_NANOS.load(Ordering::Relaxed);
    if ts == 0 {
        return None;
    }
    Some(mono_nanos().saturating_sub(ts) / 1_000_000)
}

// ---------------------------------------------------------------------------
// Sentence progress tracking
// ---------------------------------------------------------------------------

/// Register a sentence boundary. Call **before** pushing the sentence's audio
/// samples. `sentence_index` is the 0-based sentence number within the turn,
/// and the cumulative push offset is captured automatically from
/// `PLAYBACK_TOTAL_PUSHED`.
pub fn register_sentence_boundary(sentence_index: u32) {
    let offset = PLAYBACK_TOTAL_PUSHED.load(Ordering::Relaxed);
    if let Ok(mut boundaries) = SENTENCE_BOUNDARIES.lock() {
        boundaries.push((sentence_index, offset));
    }
}

/// Query which sentence is currently playing and how far into it we are.
///
/// Returns `(sentence_index, progress)` where progress is in `[0.0, 1.0]`.
/// Returns `(0, 0.0)` if no boundaries have been registered or playback
/// hasn't started.
pub fn playback_sentence_progress() -> (u32, f32) {
    let rendered = PLAYBACK_TOTAL_RENDERED.load(Ordering::Relaxed);
    let total_pushed = PLAYBACK_TOTAL_PUSHED.load(Ordering::Relaxed);

    let boundaries = match SENTENCE_BOUNDARIES.lock() {
        Ok(b) => b.clone(),
        Err(_) => return (0, 0.0),
    };

    if boundaries.is_empty() {
        return (0, 0.0);
    }

    // Find the current sentence: the last boundary whose offset <= rendered.
    let mut current_idx = 0;
    for (i, &(_, offset)) in boundaries.iter().enumerate() {
        if offset <= rendered {
            current_idx = i;
        } else {
            break;
        }
    }

    let (sentence_index, sentence_start) = boundaries[current_idx];

    // End of this sentence = start of next sentence, or total_pushed if last.
    let sentence_end = if current_idx + 1 < boundaries.len() {
        boundaries[current_idx + 1].1
    } else {
        total_pushed
    };

    let sentence_len = sentence_end.saturating_sub(sentence_start);
    if sentence_len == 0 {
        return (sentence_index, 0.0);
    }

    let played_in_sentence = rendered.saturating_sub(sentence_start);
    let progress = (played_in_sentence as f32 / sentence_len as f32).clamp(0.0, 1.0);

    (sentence_index, progress)
}

/// Get the number of samples currently in the capture SPSC ring.
pub fn capture_fifo_level() -> usize {
    if let Some(cons) = CAPTURE_CONSUMER.get() {
        if let Ok(cons) = cons.lock() {
            return cons.occupied_len();
        }
    }
    0
}

/// Read all diagnostic counters as a snapshot.
pub fn diagnostics() -> AudioDiagnostics {
    AudioDiagnostics {
        capture_callback_count: CAPTURE_CALLBACK_COUNT.load(Ordering::Relaxed),
        capture_samples_pushed: CAPTURE_SAMPLES_PUSHED.load(Ordering::Relaxed),
        capture_samples_dropped: CAPTURE_SAMPLES_DROPPED.load(Ordering::Relaxed),
        capture_fifo_lock_fail: CAPTURE_FIFO_LOCK_FAIL.load(Ordering::Relaxed),
        capture_fifo_level: capture_fifo_level(),
        render_callback_count: RENDER_CALLBACK_COUNT.load(Ordering::Relaxed),
        render_samples_pulled: RENDER_SAMPLES_PULLED.load(Ordering::Relaxed),
        render_lock_fail: RENDER_LOCK_FAIL.load(Ordering::Relaxed),
        playback_level: PLAYBACK_LEVEL.load(Ordering::Relaxed),
        engine_hw_rate: ENGINE_HW_SAMPLE_RATE.load(Ordering::Relaxed),
        engine_pb_rate: ENGINE_PB_SAMPLE_RATE.load(Ordering::Relaxed),
        engine_upsample_ratio: ENGINE_UPSAMPLE_RATIO.load(Ordering::Relaxed),
        engine_running: ENGINE_RUNNING.load(Ordering::Relaxed),
        engine_build_count: ENGINE_BUILD_COUNT.load(Ordering::Relaxed),
    }
}

// ===========================================================================
// FFI functions called from Swift realtime audio threads
// ===========================================================================

/// Called by Swift's `AVAudioEngine` input tap on the realtime audio thread.
///
/// Pushes samples into the capture SPSC ring. The collector task
/// (`capture_queue::run_collector`) drains them on a normal thread.
///
/// # Safety
///
/// - `samples` must point to at least `frame_count` valid `f32` values.
/// - Pointer is only valid for the duration of this call.
/// - Runs on a realtime thread — must not allocate, lock, or block.
#[no_mangle]
pub unsafe extern "C" fn rust_audio_capture_callback(
    samples: *const f32,
    frame_count: u32,
    _sample_rate: f64,
) {
    if samples.is_null() || frame_count == 0 {
        return;
    }

    let count = frame_count as usize;
    let slice = unsafe { std::slice::from_raw_parts(samples, count) };

    CAPTURE_CALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);

    // Push into capture SPSC ring.
    if let Some(prod) = CAPTURE_PRODUCER.get() {
        if let Ok(mut prod) = prod.try_lock() {
            let written = prod.push_slice(slice);
            CAPTURE_SAMPLES_PUSHED.fetch_add(written, Ordering::Relaxed);
            let dropped = count - written;
            if dropped > 0 {
                CAPTURE_SAMPLES_DROPPED.fetch_add(dropped, Ordering::Relaxed);
            }
        } else {
            CAPTURE_FIFO_LOCK_FAIL.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Called by Swift during engine setup to report the resolved config.
/// Stored in atomics so `diagnostics()` can include them in Rust-side logs.
#[no_mangle]
pub extern "C" fn rust_audio_report_engine_config(
    hw_sample_rate: u32,
    pb_sample_rate: u32,
    upsample_ratio: u32,
) {
    ENGINE_HW_SAMPLE_RATE.store(hw_sample_rate as usize, Ordering::Relaxed);
    ENGINE_PB_SAMPLE_RATE.store(pb_sample_rate as usize, Ordering::Relaxed);
    ENGINE_UPSAMPLE_RATIO.store(upsample_ratio as usize, Ordering::Relaxed);
    ENGINE_BUILD_COUNT.fetch_add(1, Ordering::Relaxed);

    // Reconfigure the band analyzer for the actual playback content rate.
    // The render callback delivers samples at the content rate (before
    // upsampling to hw rate), so the analyzer filters must match.
    if pb_sample_rate > 0 {
        crate::analyzer::set_sample_rate(pb_sample_rate as f32);
    }
}

/// Called by Swift when the engine starts or stops.
/// `running`: 1 = running, 0 = stopped.
#[no_mangle]
pub extern "C" fn rust_audio_report_engine_running(running: u32) {
    ENGINE_RUNNING.store(running != 0, Ordering::Relaxed);
}

/// Called by Swift's `AVAudioSourceNode` render block on the realtime audio
/// thread to pull playback samples.
///
/// # Safety
///
/// - `buffer` must point to at least `frame_count` writable `f32` slots.
/// - Runs on a realtime thread — must not allocate, lock, or block.
///
/// Returns the number of real samples written. Remainder is zero-filled (silence).
#[no_mangle]
pub unsafe extern "C" fn rust_audio_render_callback(
    buffer: *mut f32,
    frame_count: u32,
) -> u32 {
    if buffer.is_null() || frame_count == 0 {
        return 0;
    }

    let slice = unsafe { std::slice::from_raw_parts_mut(buffer, frame_count as usize) };

    RENDER_CALLBACK_COUNT.fetch_add(1, Ordering::Relaxed);

    // If paused, output silence — samples stay in the ring buffer.
    let read = if PLAYBACK_PAUSED.load(Ordering::Acquire) {
        0
    } else if let Some(cons) = PLAYBACK_CONSUMER.get() {
        if let Ok(mut cons) = cons.try_lock() {
            let n = cons.pop_slice(slice);
            if n > 0 {
                PLAYBACK_LEVEL.fetch_sub(n, Ordering::Relaxed);
                RENDER_SAMPLES_PULLED.fetch_add(n, Ordering::Relaxed);
                PLAYBACK_TOTAL_RENDERED.fetch_add(n, Ordering::Relaxed);
                LAST_NONZERO_PULL_NANOS.store(mono_nanos(), Ordering::Relaxed);
            } else if PLAYBACK_ALL_PUSHED.load(Ordering::Acquire) {
                // Ring buffer empty AND the producer has signalled no more audio
                // will arrive for this turn. Stamp the drained timestamp (only
                // once — compare-exchange ensures first-write-wins).
                let _ = PLAYBACK_DRAINED_NANOS.compare_exchange(
                    0,
                    mono_nanos(),
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            }
            n
        } else {
            RENDER_LOCK_FAIL.fetch_add(1, Ordering::Relaxed);
            0
        }
    } else {
        0
    };

    // Feed the played samples (at content rate) into the band analyzer.
    crate::analyzer::process(&slice[..read]);

    // Zero-fill remainder (silence).
    for sample in &mut slice[read..] {
        *sample = 0.0;
    }

    read as u32
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostics_display() {
        let d = AudioDiagnostics {
            capture_callback_count: 100,
            capture_samples_pushed: 50000,
            capture_samples_dropped: 200,
            capture_fifo_lock_fail: 1,
            capture_fifo_level: 2400,
            render_callback_count: 80,
            render_samples_pulled: 40000,
            render_lock_fail: 0,
            playback_level: 1000,
            engine_hw_rate: 48000,
            engine_pb_rate: 24000,
            engine_upsample_ratio: 2,
            engine_running: true,
            engine_build_count: 1,
        };
        let s = d.to_string();
        assert!(s.contains("capture("));
        assert!(s.contains("render("));
        assert!(s.contains("pushed=50000"));
        assert!(s.contains("engine(hw=48000Hz pb=24000Hz ratio=2 running=true builds=1)"));
    }
}
