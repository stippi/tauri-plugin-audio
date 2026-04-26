//! Unified capture queue for pre-roll and live recording.
//!
//! # Design
//!
//! The Swift/cpal audio callback pushes raw samples into a lock-free SPSC ring
//! buffer (realtime-safe, no allocations). A **collector task** (`run_collector`)
//! continuously drains that SPSC into a `VecDeque<Vec<f32>>` of audio chunks:
//!
//! - **Idle mode** (default): the queue is trimmed to hold at most `preroll_ms`
//!   worth of audio. For every new chunk appended, the oldest chunk(s) are
//!   discarded from the front — a rolling window.
//!
//! - **Recording mode**: trimming stops. All chunks — including the pre-roll
//!   that was already buffered — become available to the consumer via a
//!   `tokio::sync::mpsc` channel. The pre-roll chunks are sent first (faster
//!   than realtime), then live chunks follow seamlessly with zero gaps or
//!   overlaps.
//!
//! The mode switch is a single `AtomicBool`. The collector task checks it on
//! every iteration. The consumer side is a standard async channel receiver.
//!
//! # Thread safety
//!
//! - SPSC ring: lock-free, single-producer (audio callback) / single-consumer
//!   (collector task).
//! - `CaptureQueue` internal state: only accessed by the collector task (single
//!   thread), so no locking needed.
//! - Mode flag: `AtomicBool`, written from any thread, read by collector.
//! - Output channel: `tokio::sync::mpsc`, safe for async consumers.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::OnceLock;

use tokio::sync::mpsc;

use crate::ffi;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Default pre-roll duration in milliseconds.
const DEFAULT_PREROLL_MS: u32 = 500;

/// How often the collector drains the SPSC ring, in milliseconds.
/// 20ms is ~960 samples at 48kHz — small enough for low latency,
/// large enough to avoid busy-spinning.
const COLLECTOR_INTERVAL_MS: u64 = 20;

/// Channel capacity for audio chunks sent to the STT consumer.
/// Sized generously to absorb the pre-roll burst without blocking.
const CHUNK_CHANNEL_CAPACITY: usize = 256;

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

/// When true, the collector sends chunks to the consumer instead of trimming.
static RECORDING: AtomicBool = AtomicBool::new(false);

/// Configured pre-roll duration in milliseconds.
static PREROLL_MS: AtomicUsize = AtomicUsize::new(DEFAULT_PREROLL_MS as usize);

/// Hardware sample rate, cached from FFI diagnostics. Set by the collector
/// on first successful drain (avoids querying atomics every iteration).
static HW_SAMPLE_RATE: AtomicUsize = AtomicUsize::new(0);

/// Sender half of the chunk channel. Set when recording starts.
/// The collector task reads this to send chunks to the STT consumer.
static CHUNK_TX: OnceLock<std::sync::Mutex<Option<mpsc::Sender<Vec<f32>>>>> = OnceLock::new();

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Set the pre-roll duration. Call during session init.
pub fn set_preroll_ms(ms: u32) {
    PREROLL_MS.store(ms as usize, Ordering::Relaxed);
}

/// Start recording: the collector will stop trimming and begin sending all
/// queued chunks (pre-roll + live) to the returned receiver.
///
/// Returns a channel receiver that yields `Vec<f32>` audio chunks in
/// chronological order. The first chunks are the pre-roll (sent as fast as
/// the channel allows), followed by live audio seamlessly.
///
/// If already recording, the previous channel is replaced (old receiver will
/// see the channel close).
pub fn start_recording() -> mpsc::Receiver<Vec<f32>> {
    let (tx, rx) = mpsc::channel(CHUNK_CHANNEL_CAPACITY);

    // Store the sender so the collector can use it.
    let mutex = CHUNK_TX.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut slot) = mutex.lock() {
        *slot = Some(tx);
    }

    // Flip the flag AFTER installing the sender, so the collector sees
    // both atomically (it checks RECORDING first, then reads the sender).
    RECORDING.store(true, Ordering::Release);

    rx
}

/// Stop recording: the collector resumes trimming to the pre-roll window.
/// The channel sender is dropped, causing the receiver to see `None` on
/// the next recv (clean shutdown signal).
pub fn stop_recording() {
    RECORDING.store(false, Ordering::Release);

    // Drop the sender to signal the consumer.
    let mutex = CHUNK_TX.get_or_init(|| std::sync::Mutex::new(None));
    if let Ok(mut slot) = mutex.lock() {
        *slot = None;
    }
}

/// Returns whether the collector is in recording mode.
pub fn is_recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Collector task
// ---------------------------------------------------------------------------

/// Internal queue state. Only accessed by the collector task.
struct CollectorState {
    /// Buffered audio chunks in chronological order.
    queue: VecDeque<Vec<f32>>,
    /// Total number of samples across all chunks in the queue.
    total_samples: usize,
    /// Scratch buffer for draining the SPSC ring.
    drain_buf: Vec<f32>,
}

impl CollectorState {
    fn new() -> Self {
        // Size the drain buffer for one collector interval at up to 96kHz.
        // 20ms * 96000 = 1920 samples. Round up for safety.
        let drain_buf_size = 96_000 * COLLECTOR_INTERVAL_MS as usize / 1000 + 512;
        Self {
            queue: VecDeque::with_capacity(128),
            total_samples: 0,
            drain_buf: vec![0.0f32; drain_buf_size],
        }
    }

    /// Drain all available samples from the SPSC ring into the queue.
    fn drain_spsc(&mut self) {
        loop {
            let read = ffi::drain_capture_samples(&mut self.drain_buf);
            if read == 0 {
                break;
            }
            let chunk = self.drain_buf[..read].to_vec();
            self.total_samples += chunk.len();
            self.queue.push_back(chunk);
        }
    }

    /// Trim the queue to at most `max_samples` total samples by removing
    /// chunks from the front.
    fn trim_to(&mut self, max_samples: usize) {
        while self.total_samples > max_samples && !self.queue.is_empty() {
            if let Some(front) = self.queue.pop_front() {
                self.total_samples -= front.len();
            }
        }
    }

    /// Take all chunks out of the queue, returning them in order.
    fn take_all(&mut self) -> Vec<Vec<f32>> {
        self.total_samples = 0;
        self.queue.drain(..).collect()
    }
}

/// Run the collector loop. Call this once at plugin setup time (spawned as
/// a background tokio task). It runs forever, draining the SPSC ring and
/// managing the queue.
///
/// The task is lightweight: it sleeps most of the time and does a brief
/// drain + trim (or send) on each wake.
pub async fn run_collector() {
    let mut state = CollectorState::new();
    let mut interval = tokio::time::interval(std::time::Duration::from_millis(COLLECTOR_INTERVAL_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Track whether we were recording on the previous tick so we can
    // detect the idle→recording transition and flush the pre-roll.
    let mut was_recording = false;

    loop {
        interval.tick().await;

        // Update cached hw sample rate if not yet known.
        if HW_SAMPLE_RATE.load(Ordering::Relaxed) == 0 {
            let rate = ffi::diagnostics().engine_hw_rate;
            if rate > 0 {
                HW_SAMPLE_RATE.store(rate, Ordering::Relaxed);
            }
        }

        // Drain SPSC → queue.
        state.drain_spsc();

        let recording = RECORDING.load(Ordering::Acquire);

        if recording {
            // --- Recording mode ---
            // On the idle→recording transition, we have pre-roll chunks
            // already in the queue. Send them all immediately (burst).
            // On subsequent ticks, newly drained chunks are sent too.

            let chunks = state.take_all();
            if !chunks.is_empty() {
                let mutex = CHUNK_TX.get_or_init(|| std::sync::Mutex::new(None));
                if let Ok(slot) = mutex.lock() {
                    if let Some(tx) = slot.as_ref() {
                        for chunk in chunks {
                            // Use try_send to avoid blocking the collector.
                            // If the channel is full, we still need to keep
                            // going — log a warning via counter in the future.
                            if tx.try_send(chunk).is_err() {
                                // Channel full or closed. If closed, stop_recording
                                // will be called shortly. If full, the consumer is
                                // too slow — drop the chunk to keep the collector
                                // running. This should be rare with the generous
                                // channel capacity.
                            }
                        }
                    }
                }
            }
            was_recording = true;
        } else {
            // --- Idle mode ---
            if was_recording {
                // recording→idle transition: the queue may have leftover
                // chunks from after stop_recording was called. Clear them
                // to start fresh for the next pre-roll window.
                state.total_samples = 0;
                state.queue.clear();
                was_recording = false;
            }

            // Trim to pre-roll window.
            let hw_rate = HW_SAMPLE_RATE.load(Ordering::Relaxed);
            if hw_rate > 0 {
                let preroll_ms = PREROLL_MS.load(Ordering::Relaxed);
                let max_samples = hw_rate * preroll_ms / 1000;
                state.trim_to(max_samples);
            } else {
                // No hw rate yet — keep a reasonable default (2s at 48kHz).
                state.trim_to(96_000);
            }
        }
    }
}
