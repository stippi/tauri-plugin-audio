use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use tauri::{AppHandle, Runtime};

use crate::ffi;
use crate::models::*;

/// Desktop audio engine using cpal for cross-platform capture and playback.
///
/// On macOS (and other desktop platforms), this replaces the iOS AVAudioEngine
/// approach. The audio data path is identical: capture samples are pushed into
/// the same FFI ring buffers (pre-roll + capture FIFO), and playback samples
/// are pulled from the playback ring buffer.
///
/// cpal::Stream is !Send, so we run the audio streams on a dedicated thread
/// and communicate via a shutdown flag.
pub struct Audio<R: Runtime> {
    _app: AppHandle<R>,
    /// Shared flag: set to true to tell the audio thread to shut down.
    shutdown_flag: Arc<AtomicBool>,
    /// Handle to the audio thread (for join on teardown).
    audio_thread: Mutex<Option<thread::JoinHandle<()>>>,
    /// Whether a session is currently active.
    active: AtomicBool,
    /// Capture sample rate resolved at session start.
    capture_sample_rate: Mutex<u32>,
}

pub fn init<R: Runtime>(
    app: &AppHandle<R>,
    _api: tauri::plugin::PluginApi<R, ()>,
) -> crate::Result<Audio<R>> {
    Ok(Audio {
        _app: app.clone(),
        shutdown_flag: Arc::new(AtomicBool::new(false)),
        audio_thread: Mutex::new(None),
        active: AtomicBool::new(false),
        capture_sample_rate: Mutex::new(0),
    })
}

impl<R: Runtime> Audio<R> {
    pub fn init_session(&self, config: SessionConfig) -> crate::Result<OkResponse> {
        if self.active.load(Ordering::SeqCst) {
            return Err(crate::Error::SessionAlreadyActive);
        }

        // Reset shutdown flag for this session.
        self.shutdown_flag.store(false, Ordering::SeqCst);
        let shutdown = self.shutdown_flag.clone();

        let playback_content_rate = config.playback_sample_rate;

        // Spawn a dedicated thread for cpal streams (Stream is !Send).
        let handle = thread::Builder::new()
            .name("audio-cpal".into())
            .spawn(move || {
                if let Err(e) = run_audio_session(shutdown.clone(), playback_content_rate) {
                    log::error!("audio: Session thread error: {}", e);
                }
                ffi::rust_audio_report_engine_running(0);
                log::info!("audio: Session thread exited");
            })
            .map_err(|e| {
                crate::Error::SessionSetupFailed(format!("Failed to spawn audio thread: {}", e))
            })?;

        // Wait briefly for the thread to start (it reports engine running via atomic).
        // If it fails immediately, we'll catch it via the active flag.
        for _ in 0..50 {
            if ffi::diagnostics().engine_running {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(20));
        }

        if !ffi::diagnostics().engine_running {
            // Thread may have failed; wait for it and propagate error.
            let _ = handle.join();
            return Err(crate::Error::SessionSetupFailed(
                "Audio streams failed to start".into(),
            ));
        }

        // Store the capture rate for callers.
        if let Ok(mut rate) = self.capture_sample_rate.lock() {
            *rate = ffi::diagnostics().engine_hw_rate as u32;
        }

        if let Ok(mut th) = self.audio_thread.lock() {
            *th = Some(handle);
        }
        self.active.store(true, Ordering::SeqCst);

        log::info!("audio: Session active");
        Ok(OkResponse::ok())
    }

    pub fn teardown_session(&self) -> crate::Result<OkResponse> {
        if !self.active.load(Ordering::SeqCst) {
            return Ok(OkResponse::ok());
        }

        // Signal the audio thread to shut down.
        self.shutdown_flag.store(true, Ordering::SeqCst);

        // Wait for the thread to finish.
        if let Ok(mut th) = self.audio_thread.lock() {
            if let Some(handle) = th.take() {
                let _ = handle.join();
            }
        }

        self.active.store(false, Ordering::SeqCst);
        log::info!("audio: Session torn down");
        Ok(OkResponse::ok())
    }

    pub fn enqueue_audio(&self, _chunk: AudioChunk) -> crate::Result<OkResponse> {
        // Primary playback path is Rust → ring buffer → render callback.
        Ok(OkResponse::ok())
    }

    pub fn get_status(&self) -> crate::Result<AudioStatus> {
        Ok(AudioStatus {
            session: if self.active.load(Ordering::SeqCst) {
                SessionState::Active
            } else {
                SessionState::Inactive
            },
            playback_buffered: ffi::playback_level(),
        })
    }
}

/// Run the cpal audio session on a dedicated thread.
/// Blocks until `shutdown` is set to true.
fn run_audio_session(shutdown: Arc<AtomicBool>, playback_content_rate: u32) -> Result<(), String> {
    let host = cpal::default_host();

    // --- Input (capture) stream ---
    let input_device = host
        .default_input_device()
        .ok_or("No default input device found")?;

    let input_config = input_device
        .default_input_config()
        .map_err(|e| format!("Failed to get input config: {}", e))?;

    let hw_capture_rate = input_config.sample_rate().0;
    let capture_channels = input_config.channels() as usize;

    log::info!(
        "audio: Input device: {:?} ({}Hz, {}ch, {:?})",
        input_device.name().unwrap_or_default(),
        hw_capture_rate,
        capture_channels,
        input_config.sample_format(),
    );

    let input_stream_config = cpal::StreamConfig {
        channels: input_config.channels(),
        sample_rate: input_config.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };

    let input_stream = input_device
        .build_input_stream(
            &input_stream_config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if capture_channels == 1 {
                    unsafe {
                        ffi::rust_audio_capture_callback(
                            data.as_ptr(),
                            data.len() as u32,
                            hw_capture_rate as f64,
                        );
                    }
                } else {
                    // Extract mono from interleaved multi-channel
                    let mono: Vec<f32> = data
                        .chunks(capture_channels)
                        .map(|frame| frame[0])
                        .collect();
                    unsafe {
                        ffi::rust_audio_capture_callback(
                            mono.as_ptr(),
                            mono.len() as u32,
                            hw_capture_rate as f64,
                        );
                    }
                }
            },
            |err| log::error!("audio: Input stream error: {}", err),
            None,
        )
        .map_err(|e| format!("Failed to build input stream: {}", e))?;

    input_stream
        .play()
        .map_err(|e| format!("Failed to start input stream: {}", e))?;

    // --- Output (playback) stream ---
    let output_device = host
        .default_output_device()
        .ok_or("No default output device found")?;

    let output_config = output_device
        .default_output_config()
        .map_err(|e| format!("Failed to get output config: {}", e))?;

    let hw_output_rate = output_config.sample_rate().0;
    let output_channels = output_config.channels() as usize;

    log::info!(
        "audio: Output device: {:?} ({}Hz, {}ch, {:?})",
        output_device.name().unwrap_or_default(),
        hw_output_rate,
        output_channels,
        output_config.sample_format(),
    );

    let upsample_ratio = (hw_output_rate / playback_content_rate).max(1) as usize;

    let output_stream_config = cpal::StreamConfig {
        channels: output_config.channels(),
        sample_rate: output_config.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };

    let output_stream = output_device
        .build_output_stream(
            &output_stream_config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                let mono_frames = data.len() / output_channels;

                if upsample_ratio <= 1 {
                    if output_channels == 1 {
                        unsafe {
                            ffi::rust_audio_render_callback(
                                data.as_mut_ptr(),
                                mono_frames as u32,
                            );
                        }
                    } else {
                        let mut mono_buf = vec![0.0f32; mono_frames];
                        unsafe {
                            ffi::rust_audio_render_callback(
                                mono_buf.as_mut_ptr(),
                                mono_frames as u32,
                            );
                        }
                        for (i, &sample) in mono_buf.iter().enumerate() {
                            for ch in 0..output_channels {
                                data[i * output_channels + ch] = sample;
                            }
                        }
                    }
                } else {
                    // Upsample: pull fewer source samples, duplicate each
                    let src_frames = mono_frames / upsample_ratio;
                    let mut src_buf = vec![0.0f32; src_frames];
                    let pulled = unsafe {
                        ffi::rust_audio_render_callback(
                            src_buf.as_mut_ptr(),
                            src_frames as u32,
                        )
                    } as usize;

                    let mut dst = 0;
                    for &sample in &src_buf[..pulled] {
                        for _ in 0..upsample_ratio {
                            if dst < mono_frames {
                                if output_channels == 1 {
                                    data[dst] = sample;
                                } else {
                                    for ch in 0..output_channels {
                                        data[dst * output_channels + ch] = sample;
                                    }
                                }
                                dst += 1;
                            }
                        }
                    }
                    // Zero-fill remainder
                    while dst < mono_frames {
                        if output_channels == 1 {
                            data[dst] = 0.0;
                        } else {
                            for ch in 0..output_channels {
                                data[dst * output_channels + ch] = 0.0;
                            }
                        }
                        dst += 1;
                    }
                }
            },
            |err| log::error!("audio: Output stream error: {}", err),
            None,
        )
        .map_err(|e| format!("Failed to build output stream: {}", e))?;

    output_stream
        .play()
        .map_err(|e| format!("Failed to start output stream: {}", e))?;

    // Report engine config via the same atomics used by iOS.
    ffi::rust_audio_report_engine_config(hw_capture_rate, playback_content_rate, upsample_ratio as u32);
    ffi::rust_audio_report_engine_running(1);

    log::info!(
        "audio: Streams running — capture {}Hz, playback {}Hz (hw {}Hz, ratio {})",
        hw_capture_rate,
        playback_content_rate,
        hw_output_rate,
        upsample_ratio,
    );

    // Park the thread until shutdown is requested.
    // The cpal streams run on their own OS callback threads.
    while !shutdown.load(Ordering::SeqCst) {
        thread::sleep(std::time::Duration::from_millis(50));
    }

    // Streams are dropped when this function returns, stopping audio.
    log::info!("audio: Shutting down streams");
    Ok(())
}
