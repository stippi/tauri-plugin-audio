import AVFoundation
import Tauri
import UIKit
import WebKit

// ---------------------------------------------------------------------------
// MARK: - Rust FFI declarations
// ---------------------------------------------------------------------------

/// Rust function called from the capture tap (realtime thread).
/// Pushes samples into both the pre-roll rolling window and the capture FIFO.
@_silgen_name("rust_audio_capture_callback")
func rust_audio_capture_callback(
    _ samples: UnsafePointer<Float>,
    _ frameCount: UInt32,
    _ sampleRate: Double
)

/// Rust function called from the playback source node render block (realtime thread).
/// Pulls samples from the playback ring buffer. Returns number of real samples written;
/// remainder is zero-filled (silence) by Rust.
@_silgen_name("rust_audio_render_callback")
func rust_audio_render_callback(
    _ buffer: UnsafeMutablePointer<Float>,
    _ frameCount: UInt32
) -> UInt32

/// Report engine configuration to Rust for diagnostic logging.
/// Called once during setupEngine after resolving hw/playback rates.
@_silgen_name("rust_audio_report_engine_config")
func rust_audio_report_engine_config(
    _ hwSampleRate: UInt32,
    _ pbSampleRate: UInt32,
    _ upsampleRatio: UInt32
)

/// Report engine running state to Rust for diagnostic logging.
@_silgen_name("rust_audio_report_engine_running")
func rust_audio_report_engine_running(_ running: UInt32)

// ---------------------------------------------------------------------------
// MARK: - AudioPlugin
// ---------------------------------------------------------------------------

/// Native audio plugin providing always-on mic capture and playback via a
/// single AVAudioEngine instance. Audio data flows through Rust ring buffers
/// via direct FFI — not through the JS bridge.
///
/// Lifecycle:
///   1. Frontend calls `initSession` when entering the tutor screen.
///      → AVAudioSession configured (.playAndRecord, NO ducking)
///      → AVAudioEngine created with input tap + source node
///      → engine.start() — mic is hot, speaker ready
///   2. Frontend calls `teardownSession` when leaving the tutor screen.
///      → engine stopped, tap removed, source node detached, session deactivated
///
/// Recovery:
///   The WKWebView shares `AVAudioSession.sharedInstance()`. When the frontend
///   shuts down its WebAudio AudioContext, the WebView reconfigures the audio
///   session, which invalidates our AVAudioEngine. We observe
///   `configurationChangeNotification` and fully rebuild the engine when this
///   happens. Rebuilds are debounced (500ms) to avoid cascading when our own
///   `activateAudioSession` triggers additional config change notifications.
class AudioPlugin: Plugin {

    // -- Engine --
    private var audioEngine: AVAudioEngine?
    private var sourceNode: AVAudioSourceNode?
    private var tapInstalled = false
    private var sessionActive = false

    /// Counter incremented on each full rebuild (setup cycle).
    /// Logged via Rust diagnostics so we can see rebuilds in the log file.
    private var engineBuildCount: Int = 0

    /// True while setupEngine() is executing. Config-change notifications
    /// that arrive during our own setup are ignored (they're caused by
    /// our `activateAudioSession` call).
    private var isSettingUp = false

    /// Pending debounced rebuild work item. Cancelled if a new config
    /// change arrives within the debounce window.
    private var pendingRebuild: DispatchWorkItem?

    /// Debounce interval for config-change rebuilds.
    /// Needs to be long enough to absorb the cascade from our own
    /// activateAudioSession, but short enough that the engine recovers
    /// before the user notices.
    private let rebuildDebounceMs: Int = 300

    // -- Config (set via initSession args) --
    private var captureSampleRate: Double = 48_000
    private var captureChannels: UInt32 = 1
    private var captureBufferSize: UInt32 = 1024
    private var playbackSampleRate: Double = 24_000
    private var playbackChannels: UInt32 = 1

    // Serial queue for all mutable state access.
    private let pluginQueue = DispatchQueue(label: "io.affex.audio.plugin", qos: .userInitiated)

    // -----------------------------------------------------------------------
    // MARK: - Lifecycle
    // -----------------------------------------------------------------------

    override func load(webview: WKWebView) {
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(handleInterruption(_:)),
            name: AVAudioSession.interruptionNotification,
            object: nil
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(handleRouteChange(_:)),
            name: AVAudioSession.routeChangeNotification,
            object: nil
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(handleConfigChange(_:)),
            name: .AVAudioEngineConfigurationChange,
            object: nil
        )
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
        teardownEngineSync()
    }

    // -----------------------------------------------------------------------
    // MARK: - Audio session (no ducking, no voice processing)
    // -----------------------------------------------------------------------

    private func activateAudioSession() throws {
        let session = AVAudioSession.sharedInstance()
        // .playAndRecord: simultaneous mic + speaker.
        // .default mode: NO voice processing (AGC/echo-cancel).
        //   We use PTT (push-to-talk), not full duplex, so echo
        //   cancellation is unnecessary. Voice processing in .voiceChat
        //   mode crushes playback volume via AGC while the mic is hot.
        // Options:
        //   .defaultToSpeaker — route to speaker, not earpiece.
        //   .allowBluetooth*  — support BT headsets.
        //   NO .duckOthers    — our own playback goes through the same
        //                       engine, so there is nothing to duck.
        try session.setCategory(
            .playAndRecord,
            mode: .default,
            options: [.allowBluetoothHFP, .allowBluetoothA2DP, .defaultToSpeaker]
        )
        try session.setActive(true, options: [.notifyOthersOnDeactivation])
    }

    private func deactivateAudioSession() {
        do {
            try AVAudioSession.sharedInstance().setActive(
                false,
                options: [.notifyOthersOnDeactivation]
            )
        } catch {
            NSLog("[AudioPlugin] Failed to deactivate audio session: \(error)")
        }
    }

    // -----------------------------------------------------------------------
    // MARK: - Engine setup / teardown
    // -----------------------------------------------------------------------

    /// Build and start the engine with both the capture tap and playback source node.
    /// Must be called on pluginQueue.
    private func setupEngine() throws {
        isSettingUp = true
        defer { isSettingUp = false }

        // Clean slate (without deactivating the audio session if we're rebuilding).
        teardownEngineOnly()

        try activateAudioSession()

        let engine = AVAudioEngine()
        audioEngine = engine

        // -- Capture tap --
        let inputNode = engine.inputNode
        let hwFormat = inputNode.outputFormat(forBus: 0)
        let sampleRate = hwFormat.sampleRate

        inputNode.installTap(
            onBus: 0,
            bufferSize: AVAudioFrameCount(captureBufferSize),
            format: hwFormat
        ) { buffer, _ in
            guard let channelData = buffer.floatChannelData?[0] else { return }
            rust_audio_capture_callback(channelData, buffer.frameLength, sampleRate)
        }
        tapInstalled = true

        // -- Playback source node --
        //
        // Create at the hardware sample rate so the source node's render block
        // is called at the rate the engine actually runs (usually 48 kHz).
        // Rust pushes audio at playbackSampleRate (24 kHz); we up-sample with
        // simple sample-repetition in the render block below.
        let hwRate = hwFormat.sampleRate
        let pbFormat = AVAudioFormat(
            standardFormatWithSampleRate: hwRate,
            channels: playbackChannels
        )!

        // Pre-compute integer ratio for cheap upsampling (e.g. 48000/24000 = 2).
        let ratio = max(1, Int(hwRate / playbackSampleRate))
        let node = AVAudioSourceNode(format: pbFormat) { _, _, frameCount, audioBufferList -> OSStatus in
            let ablPointer = UnsafeMutableAudioBufferListPointer(audioBufferList)
            guard let buf = ablPointer.first,
                  let data = buf.mData?.assumingMemoryBound(to: Float.self) else {
                return noErr
            }

            let frames = Int(frameCount)

            if ratio <= 1 {
                // No upsampling needed — hw rate == playback rate.
                let _ = rust_audio_render_callback(data, frameCount)
            } else {
                // Pull (frames / ratio) source samples, then duplicate each sample `ratio` times.
                let srcFrames = frames / ratio
                // Use the tail of the output buffer as scratch space for source samples.
                // Safety: we read scratch[i] (at offset frames-srcFrames+i) before writing
                // past that point. For ratio=2 the write cursor is always at 2*i+1 which
                // stays below frames-srcFrames+i+1 for all valid i.
                let scratch = data.advanced(by: frames - srcFrames)
                let pulled = Int(rust_audio_render_callback(scratch, UInt32(srcFrames)))

                var dst = 0
                for i in 0..<pulled {
                    let sample = scratch[i]
                    for _ in 0..<ratio {
                        if dst < frames {
                            data[dst] = sample
                            dst += 1
                        }
                    }
                }
                // Zero-fill any remainder.
                while dst < frames {
                    data[dst] = 0.0
                    dst += 1
                }
            }

            return noErr
        }

        sourceNode = node
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: pbFormat)

        // Report engine config to Rust so it shows up in diagnostic logs.
        engineBuildCount += 1
        rust_audio_report_engine_config(
            UInt32(hwRate),
            UInt32(playbackSampleRate),
            UInt32(ratio)
        )

        // -- Start --
        try engine.start()
        sessionActive = true
        rust_audio_report_engine_running(1)
    }

    /// Tear down the engine graph without deactivating the audio session.
    /// Used during rebuilds (configurationChange) where we want to keep
    /// the session active and immediately re-setup.
    private func teardownEngineOnly() {
        if tapInstalled, let engine = audioEngine {
            engine.inputNode.removeTap(onBus: 0)
            tapInstalled = false
        }
        if let node = sourceNode, let engine = audioEngine {
            engine.disconnectNodeOutput(node)
            engine.detach(node)
        }
        sourceNode = nil
        audioEngine?.stop()
        audioEngine = nil
        rust_audio_report_engine_running(0)
    }

    /// Full teardown — stops engine AND deactivates the audio session.
    /// Used when the user leaves the tutor screen (teardownSession).
    private func teardownEngineSync() {
        pendingRebuild?.cancel()
        pendingRebuild = nil
        teardownEngineOnly()
        if sessionActive {
            deactivateAudioSession()
        }
        sessionActive = false
    }

    // -----------------------------------------------------------------------
    // MARK: - Engine rebuild (configuration change recovery)
    // -----------------------------------------------------------------------

    /// Schedule a debounced engine rebuild. Multiple rapid config changes
    /// are coalesced — only the last one triggers an actual rebuild.
    /// Must be called on pluginQueue.
    private func scheduleRebuild() {
        guard sessionActive else { return }

        // Cancel any previously scheduled rebuild.
        pendingRebuild?.cancel()

        let work = DispatchWorkItem { [weak self] in
            guard let self = self, self.sessionActive else { return }
            self.pendingRebuild = nil
            do {
                try self.setupEngine()
            } catch {
                NSLog("[AudioPlugin] Failed to rebuild engine: \(error)")
                self.sessionActive = false
                rust_audio_report_engine_running(0)
            }
        }
        pendingRebuild = work
        pluginQueue.asyncAfter(
            deadline: .now() + .milliseconds(rebuildDebounceMs),
            execute: work
        )
    }

    // -----------------------------------------------------------------------
    // MARK: - Interruption / route change / config change
    // -----------------------------------------------------------------------

    @objc private func handleInterruption(_ notification: Notification) {
        guard let info = notification.userInfo,
              let typeValue = info[AVAudioSessionInterruptionTypeKey] as? UInt,
              let type = AVAudioSession.InterruptionType(rawValue: typeValue) else { return }

        pluginQueue.async { [weak self] in
            guard let self = self, self.sessionActive else { return }
            switch type {
            case .began:
                rust_audio_report_engine_running(0)
            case .ended:
                // Rebuild the full engine — a simple engine.start() is not
                // reliable after the audio session was interrupted, because
                // the hardware format may have changed.
                self.scheduleRebuild()
            @unknown default:
                break
            }
        }
    }

    @objc private func handleRouteChange(_ notification: Notification) {
        pluginQueue.async { [weak self] in
            guard let self = self, self.sessionActive else { return }
            // After any route change, check if engine is still running.
            if let engine = self.audioEngine, !engine.isRunning {
                self.scheduleRebuild()
            }
        }
    }

    /// Fired when the audio engine's underlying hardware config changes.
    /// This happens when WKWebView reconfigures the shared AVAudioSession
    /// (e.g. when it closes its AudioContext). The engine is invalidated
    /// and must be rebuilt from scratch.
    @objc private func handleConfigChange(_ notification: Notification) {
        pluginQueue.async { [weak self] in
            guard let self = self, self.sessionActive else { return }
            // Skip config changes caused by our own setupEngine.
            guard !self.isSettingUp else { return }
            self.scheduleRebuild()
        }
    }

    // -----------------------------------------------------------------------
    // MARK: - Tauri commands
    // -----------------------------------------------------------------------

    @objc func initSession(_ invoke: Invoke) {
        let args: InitSessionArgs
        do {
            args = try invoke.parseArgs(InitSessionArgs.self)
        } catch {
            NSLog("[AudioPlugin] Failed to parse initSession args, using defaults: \(error)")
            args = InitSessionArgs()
        }

        pluginQueue.async { [weak self] in
            guard let self = self else { return }

            if self.sessionActive {
                invoke.reject("Audio session already active")
                return
            }

            // Apply config from args.
            if let sr = args.captureSampleRate { self.captureSampleRate = Double(sr) }
            if let sr = args.playbackSampleRate { self.playbackSampleRate = Double(sr) }
            if let ch = args.captureChannels { self.captureChannels = UInt32(ch) }
            if let ch = args.playbackChannels { self.playbackChannels = UInt32(ch) }
            if let bs = args.captureBufferSize { self.captureBufferSize = UInt32(bs) }

            do {
                try self.setupEngine()
                invoke.resolve(["success": true])
            } catch {
                invoke.reject("Failed to init audio session: \(error.localizedDescription)")
            }
        }
    }

    @objc func teardownSession(_ invoke: Invoke) {
        pluginQueue.async { [weak self] in
            guard let self = self else { return }
            self.teardownEngineSync()
            invoke.resolve(["success": true])
        }
    }

    @objc func enqueueAudio(_ invoke: Invoke) {
        // The primary playback path is Rust → ring buffer → render callback.
        // This command is a control-plane fallback. The actual push happens
        // in Rust via ffi::push_playback_samples().
        invoke.resolve(["success": true])
    }

    @objc func getStatus(_ invoke: Invoke) {
        pluginQueue.async { [weak self] in
            guard let self = self else { return }
            let running = self.audioEngine?.isRunning ?? false
            invoke.resolve([
                "session": self.sessionActive ? "active" : "inactive",
                "engineRunning": running,
                "engineBuildCount": self.engineBuildCount,
                "playbackBuffered": 0,  // Tracked on Rust side
                "prerollBuffered": 0,   // Tracked on Rust side
            ])
        }
    }
}

// ---------------------------------------------------------------------------
// MARK: - Argument types
// ---------------------------------------------------------------------------

struct InitSessionArgs: Decodable {
    let captureSampleRate: Int?
    let playbackSampleRate: Int?
    let captureChannels: Int?
    let playbackChannels: Int?
    let captureBufferSize: Int?
    let prerollMs: Int?

    init() {
        self.captureSampleRate = nil
        self.playbackSampleRate = nil
        self.captureChannels = nil
        self.playbackChannels = nil
        self.captureBufferSize = nil
        self.prerollMs = nil
    }
}

// ---------------------------------------------------------------------------
// MARK: - C entry point for Tauri iOS plugin binding
// ---------------------------------------------------------------------------

@_cdecl("init_plugin_audio")
func initPlugin() -> Plugin {
    return AudioPlugin()
}
