//! Playback audio analyzer — 5-band biquad bandpass filters for visualization.
//!
//! Called from the realtime render callback (`ffi::rust_audio_render_callback`).
//! Produces 5 smoothed band levels (0.0–1.0) stored in atomics for lock-free
//! reading from the frontend via `get_levels()`.
//!
//! # Design
//!
//! Five second-order IIR bandpass filters (biquad, Direct Form II Transposed)
//! split the playback signal into frequency bands tuned for speech:
//!
//! | Band | Center | Character                          |
//! |------|--------|------------------------------------|
//! | 0    | 120 Hz | Chest voice fundamentals, low hum  |
//! | 1    | 350 Hz | Lower formants, vowel body         |
//! | 2    | 1 kHz  | Mid formants, main speech energy   |
//! | 3    | 3 kHz  | Presence, consonant clarity        |
//! | 4    | 8 kHz  | Sibilance, "air"                   |
//!
//! Each filter is a handful of multiply-adds per sample — negligible even on
//! a realtime audio thread at 24 kHz.
//!
//! The per-band RMS energy is exponentially smoothed (fast attack, slow release)
//! to produce visually pleasing bar animations.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Center frequencies for the 5 bands (Hz).
const BAND_FREQS: [f32; 5] = [120.0, 350.0, 1000.0, 3000.0, 8000.0];

/// Q factor for each bandpass filter. Lower Q = wider band.
/// We use moderate Q so adjacent bands overlap naturally — looks better
/// for visualization than sharp notches.
const BAND_Q: f32 = 1.2;

/// Default sample rate for coefficient computation.
/// Matches the playback content rate (TTS output). Reconfigured at runtime
/// when the engine reports its actual rate.
const DEFAULT_SAMPLE_RATE: f32 = 24_000.0;

/// Attack coefficient for exponential smoothing (level rising).
/// Higher = faster response. 0.4 gives snappy attack at ~20ms callback intervals.
const SMOOTH_ATTACK: f32 = 0.4;

/// Release coefficient for exponential smoothing (level falling).
/// Lower = slower decay. 0.08 gives a pleasant ~200ms tail.
const SMOOTH_RELEASE: f32 = 0.08;

/// Gain applied to RMS before clamping to 0–1. Speech is quiet compared to
/// full-scale, so we boost to make the bars lively.
const RMS_GAIN: f32 = 4.0;

// ---------------------------------------------------------------------------
// Biquad filter (Direct Form II Transposed)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    /// Compute bandpass filter coefficients for given center frequency and Q.
    ///
    /// Reference: Audio EQ Cookbook by Robert Bristow-Johnson.
    fn bandpass(freq: f32, q: f32, sample_rate: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let a0 = 1.0 + alpha;
        Self {
            b0: alpha / a0,
            b1: 0.0,
            b2: -alpha / a0,
            a1: -2.0 * cos_w0 / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Process one sample through the filter, returning the filtered output.
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

// ---------------------------------------------------------------------------
// Analyzer state
// ---------------------------------------------------------------------------

struct AnalyzerState {
    bands: [Biquad; 5],
    levels: [f32; 5],
    sample_rate: f32,
}

impl AnalyzerState {
    fn new(sample_rate: f32) -> Self {
        let bands = std::array::from_fn(|i| Biquad::bandpass(BAND_FREQS[i], BAND_Q, sample_rate));
        Self {
            bands,
            levels: [0.0; 5],
            sample_rate,
        }
    }

    /// Reconfigure filter coefficients for a new sample rate.
    fn reconfigure(&mut self, sample_rate: f32) {
        if (self.sample_rate - sample_rate).abs() < 1.0 {
            return;
        }
        self.sample_rate = sample_rate;
        for (i, band) in self.bands.iter_mut().enumerate() {
            *band = Biquad::bandpass(BAND_FREQS[i], BAND_Q, sample_rate);
        }
    }

    /// Process a buffer of playback samples and update smoothed levels.
    fn process(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            // No audio — decay levels toward zero.
            for level in &mut self.levels {
                *level *= 1.0 - SMOOTH_RELEASE;
            }
            return;
        }

        // Compute per-band RMS energy over this buffer.
        let mut energy = [0.0f32; 5];
        for &sample in samples {
            for (i, band) in self.bands.iter_mut().enumerate() {
                let filtered = band.process(sample);
                energy[i] += filtered * filtered;
            }
        }

        let inv_len = 1.0 / samples.len() as f32;
        for i in 0..5 {
            let rms = (energy[i] * inv_len).sqrt() * RMS_GAIN;
            let rms = rms.min(1.0);

            // Exponential smoothing with asymmetric attack/release.
            let coeff = if rms > self.levels[i] {
                SMOOTH_ATTACK
            } else {
                SMOOTH_RELEASE
            };
            self.levels[i] = coeff * rms + (1.0 - coeff) * self.levels[i];
        }
    }
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

/// Analyzer behind a Mutex — the render callback uses `try_lock`.
static ANALYZER: OnceLock<std::sync::Mutex<AnalyzerState>> = OnceLock::new();

/// 5 band levels stored as `f32` bit patterns in `AtomicU32`.
/// Written by the render callback, read lock-free by the frontend command.
static BAND_LEVELS: [AtomicU32; 5] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Initialize the analyzer. Called once during plugin setup.
pub fn init() {
    let _ = ANALYZER.set(std::sync::Mutex::new(AnalyzerState::new(
        DEFAULT_SAMPLE_RATE,
    )));
}

/// Process playback samples through the band filters and update levels.
///
/// Called from the realtime render callback. Uses `try_lock` — if the lock
/// is contended (shouldn't happen in practice), the buffer is skipped.
pub fn process(samples: &[f32]) {
    if let Some(analyzer) = ANALYZER.get() {
        if let Ok(mut state) = analyzer.try_lock() {
            state.process(samples);
            // Publish levels to atomics for lock-free reading.
            for (i, &level) in state.levels.iter().enumerate() {
                BAND_LEVELS[i].store(level.to_bits(), Ordering::Relaxed);
            }
        }
    }
}

/// Read the current 5-band levels. Lock-free, safe from any thread.
/// Returns values in 0.0–1.0 range.
pub fn get_levels() -> [f32; 5] {
    std::array::from_fn(|i| f32::from_bits(BAND_LEVELS[i].load(Ordering::Relaxed)))
}

/// Update the analyzer's sample rate (e.g. when the engine reports its
/// playback content rate). Called from a normal thread, can block.
pub fn set_sample_rate(rate: f32) {
    if let Some(analyzer) = ANALYZER.get() {
        if let Ok(mut state) = analyzer.lock() {
            state.reconfigure(rate);
        }
    }
}

/// Reset filter state and levels to zero. Called on session teardown.
pub fn reset() {
    if let Some(analyzer) = ANALYZER.get() {
        if let Ok(mut state) = analyzer.lock() {
            for band in &mut state.bands {
                band.reset();
            }
            state.levels = [0.0; 5];
        }
    }
    for level in &BAND_LEVELS {
        level.store(0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn biquad_bandpass_coefficients_are_finite() {
        for &freq in &BAND_FREQS {
            let bq = Biquad::bandpass(freq, BAND_Q, 24000.0);
            assert!(bq.b0.is_finite());
            assert!(bq.b1.is_finite());
            assert!(bq.b2.is_finite());
            assert!(bq.a1.is_finite());
            assert!(bq.a2.is_finite());
        }
    }

    #[test]
    fn biquad_passes_center_frequency() {
        // Generate a 1kHz sine at 24kHz sample rate — should pass through band 2 (1kHz).
        let sr = 24000.0;
        let freq = 1000.0;
        let mut bq = Biquad::bandpass(freq, BAND_Q, sr);

        let mut energy = 0.0f32;
        let n = 2400; // 100ms
        for i in 0..n {
            let sample = (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin();
            let out = bq.process(sample);
            // Skip first 100 samples (filter settling).
            if i >= 100 {
                energy += out * out;
            }
        }
        let rms = (energy / (n - 100) as f32).sqrt();
        // Bandpass at center should pass most energy. For Q=1.2 and unity
        // input amplitude, RMS should be > 0.3.
        assert!(rms > 0.3, "RMS at center frequency too low: {}", rms);
    }

    #[test]
    fn biquad_rejects_far_frequency() {
        // 100Hz input through the 8kHz bandpass should be strongly attenuated.
        let sr = 24000.0;
        let mut bq = Biquad::bandpass(8000.0, BAND_Q, sr);

        let mut energy = 0.0f32;
        let n = 2400;
        for i in 0..n {
            let sample = (2.0 * std::f32::consts::PI * 100.0 * i as f32 / sr).sin();
            let out = bq.process(sample);
            if i >= 100 {
                energy += out * out;
            }
        }
        let rms = (energy / (n - 100) as f32).sqrt();
        assert!(rms < 0.01, "RMS at far frequency too high: {}", rms);
    }

    #[test]
    fn analyzer_levels_rise_and_decay() {
        let mut state = AnalyzerState::new(24000.0);

        // Feed a 1kHz sine — band 2 should rise.
        let samples: Vec<f32> = (0..2400)
            .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 24000.0).sin() * 0.5)
            .collect();
        state.process(&samples);
        let level_after_signal = state.levels[2];
        assert!(
            level_after_signal > 0.1,
            "Band 2 should have risen: {}",
            level_after_signal
        );

        // Feed silence — levels should decay.
        for _ in 0..20 {
            state.process(&[]);
        }
        let level_after_silence = state.levels[2];
        assert!(
            level_after_silence < level_after_signal * 0.5,
            "Band 2 should have decayed: {} -> {}",
            level_after_signal,
            level_after_silence
        );
    }

    #[test]
    fn get_levels_returns_zeros_initially() {
        // Note: this test uses the global BAND_LEVELS atomics which may be
        // modified by other tests. We just check they return finite values.
        let levels = get_levels();
        for &l in &levels {
            assert!(l.is_finite());
            assert!(l >= 0.0);
            assert!(l <= 1.0 || l == 0.0); // Allow exactly 0.0 from fresh atomics
        }
    }
}
