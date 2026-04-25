//! Lock-free FFI bridge between Swift audio callbacks and Rust.
//!
//! # Architecture
//!
//! There are three buffers:
//!
//! 1. **Pre-roll ring buffer** — A fixed-size circular buffer that always
//!    holds the last N ms of captured audio. New samples overwrite the
//!    oldest. This is *not* a FIFO pipe — it's a rolling window.
//!    When PTT fires, consumers snapshot this buffer to get the audio
//!    from before the button press.
//!
//! 2. **Capture FIFO** — A standard SPSC ring buffer for live capture
//!    streaming. When the Rust consumer wants real-time mic data (e.g.
//!    during an active PTT/STT session), it drains from here.
//!    The Swift tap pushes into *both* the pre-roll and the capture FIFO.
//!
//! 3. **Playback FIFO** — SPSC ring buffer for TTS output. Rust pushes
//!    samples in, Swift's `AVAudioSourceNode` render block pulls them out.
//!
//! # Safety
//!
//! The FFI functions run on iOS realtime audio threads. Rules:
//! - No heap allocations
//! - No blocking locks (only `try_lock`)
//! - No logging / println
//! - If a lock is contended, drop the buffer and move on

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapRb,
};

// ---------------------------------------------------------------------------
// Capacities
// ---------------------------------------------------------------------------

/// Capture FIFO capacity. At 48 kHz mono, 48000 = 1 second.
/// Sized to absorb up to 1s of latency in the Rust consumer.
const CAPTURE_FIFO_CAPACITY: usize = 48_000;

/// Playback ring buffer — 2 seconds at 48 kHz to absorb jitter.
const PLAYBACK_RING_CAPACITY: usize = 48_000 * 2;

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

type RbProducer = ringbuf::HeapProd<f32>;
type RbConsumer = ringbuf::HeapCons<f32>;

// ---------------------------------------------------------------------------
// Pre-roll rolling window
// ---------------------------------------------------------------------------

/// A fixed-size circular buffer that overwrites the oldest samples.
/// NOT a FIFO — all data is always accessible via `snapshot()`.
///
/// Thread safety: the Swift audio tap (single producer) writes via
/// `push_samples`. The Rust consumer (single reader) calls `snapshot`.
/// These can race, so we use a Mutex with try_lock on the write side
/// (realtime thread) and blocking lock on the read side (worker thread).
struct PreRollBuffer {
    data: Vec<f32>,
    capacity: usize,
    write_pos: usize,
    len: usize,
}

impl PreRollBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            data: vec![0.0; capacity],
            capacity,
            write_pos: 0,
            len: 0,
        }
    }

    /// Write samples into the rolling window, overwriting the oldest.
    fn push_samples(&mut self, samples: &[f32]) {
        for &s in samples {
            self.data[self.write_pos] = s;
            self.write_pos = (self.write_pos + 1) % self.capacity;
            if self.len < self.capacity {
                self.len += 1;
            }
        }
    }

    /// Get a snapshot of the current rolling window contents in
    /// chronological order (oldest first).
    fn snapshot(&self) -> Vec<f32> {
        if self.len < self.capacity {
            // Buffer not yet full — data starts at 0.
            self.data[..self.len].to_vec()
        } else {
            // Buffer is full — oldest sample is at write_pos.
            let mut out = Vec::with_capacity(self.capacity);
            out.extend_from_slice(&self.data[self.write_pos..]);
            out.extend_from_slice(&self.data[..self.write_pos]);
            out
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    /// Clear the buffer.
    fn clear(&mut self) {
        self.write_pos = 0;
        self.len = 0;
    }
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static PRE_ROLL: OnceLock<std::sync::Mutex<PreRollBuffer>> = OnceLock::new();

static CAPTURE_PRODUCER: OnceLock<std::sync::Mutex<RbProducer>> = OnceLock::new();
static CAPTURE_CONSUMER: OnceLock<std::sync::Mutex<RbConsumer>> = OnceLock::new();

static PLAYBACK_PRODUCER: OnceLock<std::sync::Mutex<RbProducer>> = OnceLock::new();
static PLAYBACK_CONSUMER: OnceLock<std::sync::Mutex<RbConsumer>> = OnceLock::new();

/// Tracks how many samples are currently in the playback buffer.
/// Incremented on push, decremented on pull. Approximate (relaxed ordering)
/// but good enough for status reporting.
static PLAYBACK_LEVEL: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// Diagnostic counters (relaxed atomics, safe from realtime threads)
// ---------------------------------------------------------------------------

/// Total samples pushed into the capture FIFO by the Swift tap.
static CAPTURE_SAMPLES_PUSHED: AtomicUsize = AtomicUsize::new(0);
/// Total samples dropped (FIFO full) by the Swift tap.
static CAPTURE_SAMPLES_DROPPED: AtomicUsize = AtomicUsize::new(0);
/// Number of capture callback invocations.
static CAPTURE_CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Number of times the capture callback failed to acquire the pre-roll lock.
static CAPTURE_PREROLL_LOCK_FAIL: AtomicUsize = AtomicUsize::new(0);
/// Number of times the capture callback failed to acquire the FIFO producer lock.
static CAPTURE_FIFO_LOCK_FAIL: AtomicUsize = AtomicUsize::new(0);
/// Number of render callback invocations.
static RENDER_CALLBACK_COUNT: AtomicUsize = AtomicUsize::new(0);
/// Total samples pulled from the playback ring by the render callback.
static RENDER_SAMPLES_PULLED: AtomicUsize = AtomicUsize::new(0);
/// Number of times the render callback failed to acquire the playback consumer lock.
static RENDER_LOCK_FAIL: AtomicUsize = AtomicUsize::new(0);

/// Gate: when false, the capture callback skips pushing to the FIFO.
/// This allows the drain task to clear stale data before enabling live flow.
static CAPTURE_FIFO_ENABLED: AtomicBool = AtomicBool::new(true);

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
    pub capture_preroll_lock_fail: usize,
    pub capture_fifo_lock_fail: usize,
    pub capture_fifo_level: usize,
    pub capture_fifo_enabled: bool,
    pub render_callback_count: usize,
    pub render_samples_pulled: usize,
    pub render_lock_fail: usize,
    pub playback_level: usize,
    pub preroll_level: usize,
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
            "capture(cb={} pushed={} dropped={} fifo_lvl={} fifo_on={} pr_lockfail={} fifo_lockfail={}) \
             render(cb={} pulled={} lockfail={}) \
             playback_lvl={} preroll_lvl={} engine(hw={}Hz pb={}Hz ratio={} running={} builds={})",
            self.capture_callback_count,
            self.capture_samples_pushed,
            self.capture_samples_dropped,
            self.capture_fifo_level,
            self.capture_fifo_enabled,
            self.capture_preroll_lock_fail,
            self.capture_fifo_lock_fail,
            self.render_callback_count,
            self.render_samples_pulled,
            self.render_lock_fail,
            self.playback_level,
            self.preroll_level,
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
///
/// `preroll_capacity` is the number of samples for the rolling window
/// (e.g. 800ms * 48kHz = 38400 samples).
pub fn init_ring_buffers(preroll_capacity: usize) {
    let _ = PRE_ROLL.set(std::sync::Mutex::new(PreRollBuffer::new(preroll_capacity)));

    let capture_rb = HeapRb::<f32>::new(CAPTURE_FIFO_CAPACITY);
    let (prod, cons) = capture_rb.split();
    let _ = CAPTURE_PRODUCER.set(std::sync::Mutex::new(prod));
    let _ = CAPTURE_CONSUMER.set(std::sync::Mutex::new(cons));

    let playback_rb = HeapRb::<f32>::new(PLAYBACK_RING_CAPACITY);
    let (prod, cons) = playback_rb.split();
    let _ = PLAYBACK_PRODUCER.set(std::sync::Mutex::new(prod));
    let _ = PLAYBACK_CONSUMER.set(std::sync::Mutex::new(cons));

    // Start with FIFO enabled.
    CAPTURE_FIFO_ENABLED.store(true, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Rust-side API (called from normal threads, can block on locks)
// ---------------------------------------------------------------------------

/// Push samples into the playback ring buffer.
/// Called from Rust when TTS produces audio or via `enqueue_audio`.
/// Returns the number of samples actually written.
///
/// This runs on a normal (non-realtime) thread, so it uses a blocking lock.
/// The realtime render callback uses `try_lock` on the consumer side, so
/// contention is brief (render only holds the lock for a memcpy).
pub fn push_playback_samples(samples: &[f32]) -> usize {
    if let Some(prod) = PLAYBACK_PRODUCER.get() {
        if let Ok(mut prod) = prod.lock() {
            let written = prod.push_slice(samples);
            PLAYBACK_LEVEL.fetch_add(written, Ordering::Relaxed);
            return written;
        }
    }
    0
}

/// Drain captured audio from the live capture FIFO.
/// Called from a Rust worker thread during active STT streaming.
/// Returns the number of samples read.
pub fn drain_capture_samples(buf: &mut [f32]) -> usize {
    if let Some(cons) = CAPTURE_CONSUMER.get() {
        if let Ok(mut cons) = cons.lock() {
            return cons.pop_slice(buf);
        }
    }
    0
}

/// Snapshot the pre-roll buffer — returns the last N ms of audio in
/// chronological order. Non-destructive: the pre-roll keeps rolling.
pub fn snapshot_preroll() -> Vec<f32> {
    if let Some(pr) = PRE_ROLL.get() {
        if let Ok(pr) = pr.lock() {
            return pr.snapshot();
        }
    }
    Vec::new()
}

/// Clear the pre-roll buffer (e.g. after snapshotting at PTT press).
pub fn clear_preroll() {
    if let Some(pr) = PRE_ROLL.get() {
        if let Ok(mut pr) = pr.lock() {
            pr.clear();
        }
    }
}

/// Get the approximate number of samples in the playback buffer.
pub fn playback_level() -> usize {
    PLAYBACK_LEVEL.load(Ordering::Relaxed)
}

/// Get the number of samples in the pre-roll buffer.
pub fn preroll_level() -> usize {
    if let Some(pr) = PRE_ROLL.get() {
        if let Ok(pr) = pr.lock() {
            return pr.len();
        }
    }
    0
}

/// Get the number of samples currently in the capture FIFO.
pub fn capture_fifo_level() -> usize {
    if let Some(cons) = CAPTURE_CONSUMER.get() {
        if let Ok(cons) = cons.lock() {
            return cons.occupied_len();
        }
    }
    0
}

/// Reset the capture FIFO: drain all stale samples and reset counters.
///
/// Call this right before starting a new drain session. This ensures the
/// drain loop starts with a clean FIFO rather than stale audio that
/// accumulated between sessions. The capture callback keeps pushing to
/// the pre-roll regardless.
pub fn reset_capture_fifo() {
    // 1. Temporarily disable FIFO pushes from the capture callback.
    CAPTURE_FIFO_ENABLED.store(false, Ordering::SeqCst);

    // 2. Drain any stale data from the FIFO.
    if let Some(cons) = CAPTURE_CONSUMER.get() {
        if let Ok(mut cons) = cons.lock() {
            // Skip all available samples.
            let occupied = cons.occupied_len();
            cons.skip(occupied);
        }
    }

    // 3. Reset capture-side diagnostic counters for the new drain session.
    //    Render counters are NOT reset — they track cumulative engine activity
    //    independent of capture drain sessions.
    CAPTURE_SAMPLES_PUSHED.store(0, Ordering::Relaxed);
    CAPTURE_SAMPLES_DROPPED.store(0, Ordering::Relaxed);
    CAPTURE_CALLBACK_COUNT.store(0, Ordering::Relaxed);
    CAPTURE_PREROLL_LOCK_FAIL.store(0, Ordering::Relaxed);
    CAPTURE_FIFO_LOCK_FAIL.store(0, Ordering::Relaxed);

    // 4. Re-enable FIFO pushes. From now on, only fresh audio enters.
    CAPTURE_FIFO_ENABLED.store(true, Ordering::SeqCst);
}

/// Read all diagnostic counters as a snapshot.
pub fn diagnostics() -> AudioDiagnostics {
    AudioDiagnostics {
        capture_callback_count: CAPTURE_CALLBACK_COUNT.load(Ordering::Relaxed),
        capture_samples_pushed: CAPTURE_SAMPLES_PUSHED.load(Ordering::Relaxed),
        capture_samples_dropped: CAPTURE_SAMPLES_DROPPED.load(Ordering::Relaxed),
        capture_preroll_lock_fail: CAPTURE_PREROLL_LOCK_FAIL.load(Ordering::Relaxed),
        capture_fifo_lock_fail: CAPTURE_FIFO_LOCK_FAIL.load(Ordering::Relaxed),
        capture_fifo_level: capture_fifo_level(),
        capture_fifo_enabled: CAPTURE_FIFO_ENABLED.load(Ordering::Relaxed),
        render_callback_count: RENDER_CALLBACK_COUNT.load(Ordering::Relaxed),
        render_samples_pulled: RENDER_SAMPLES_PULLED.load(Ordering::Relaxed),
        render_lock_fail: RENDER_LOCK_FAIL.load(Ordering::Relaxed),
        playback_level: PLAYBACK_LEVEL.load(Ordering::Relaxed),
        preroll_level: preroll_level(),
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
/// Pushes samples into both:
/// 1. The pre-roll rolling window (always, for PTT pre-roll)
/// 2. The capture FIFO (for live STT streaming, when enabled)
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

    // 1. Push into pre-roll rolling window (always).
    if let Some(pr) = PRE_ROLL.get() {
        if let Ok(mut pr) = pr.try_lock() {
            pr.push_samples(slice);
        } else {
            CAPTURE_PREROLL_LOCK_FAIL.fetch_add(1, Ordering::Relaxed);
        }
    }

    // 2. Push into capture FIFO for live streaming (when enabled).
    if CAPTURE_FIFO_ENABLED.load(Ordering::Relaxed) {
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

    let read = if let Some(cons) = PLAYBACK_CONSUMER.get() {
        if let Ok(mut cons) = cons.try_lock() {
            let n = cons.pop_slice(slice);
            if n > 0 {
                PLAYBACK_LEVEL.fetch_sub(n, Ordering::Relaxed);
                RENDER_SAMPLES_PULLED.fetch_add(n, Ordering::Relaxed);
            }
            n
        } else {
            RENDER_LOCK_FAIL.fetch_add(1, Ordering::Relaxed);
            0
        }
    } else {
        0
    };

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
    fn preroll_buffer_basic() {
        let mut buf = PreRollBuffer::new(4);
        buf.push_samples(&[1.0, 2.0, 3.0]);
        assert_eq!(buf.snapshot(), vec![1.0, 2.0, 3.0]);
        assert_eq!(buf.len(), 3);
    }

    #[test]
    fn preroll_buffer_overwrite() {
        let mut buf = PreRollBuffer::new(4);
        buf.push_samples(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        // Capacity is 4, so oldest (1, 2) are overwritten.
        assert_eq!(buf.snapshot(), vec![3.0, 4.0, 5.0, 6.0]);
        assert_eq!(buf.len(), 4);
    }

    #[test]
    fn preroll_buffer_exact_fill() {
        let mut buf = PreRollBuffer::new(3);
        buf.push_samples(&[10.0, 20.0, 30.0]);
        assert_eq!(buf.snapshot(), vec![10.0, 20.0, 30.0]);
    }

    #[test]
    fn preroll_buffer_wraparound_snapshot_order() {
        let mut buf = PreRollBuffer::new(4);
        buf.push_samples(&[1.0, 2.0, 3.0, 4.0]);
        buf.push_samples(&[5.0]); // overwrites 1.0
        assert_eq!(buf.snapshot(), vec![2.0, 3.0, 4.0, 5.0]);
        buf.push_samples(&[6.0]); // overwrites 2.0
        assert_eq!(buf.snapshot(), vec![3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn preroll_buffer_clear() {
        let mut buf = PreRollBuffer::new(4);
        buf.push_samples(&[1.0, 2.0, 3.0]);
        buf.clear();
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.snapshot(), Vec::<f32>::new());
    }

    #[test]
    fn preroll_buffer_empty() {
        let buf = PreRollBuffer::new(4);
        assert_eq!(buf.snapshot(), Vec::<f32>::new());
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn diagnostics_display() {
        let d = AudioDiagnostics {
            capture_callback_count: 100,
            capture_samples_pushed: 50000,
            capture_samples_dropped: 200,
            capture_preroll_lock_fail: 0,
            capture_fifo_lock_fail: 1,
            capture_fifo_level: 2400,
            capture_fifo_enabled: true,
            render_callback_count: 80,
            render_samples_pulled: 40000,
            render_lock_fail: 0,
            playback_level: 1000,
            preroll_level: 38400,
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
