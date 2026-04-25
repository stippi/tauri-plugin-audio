import AVFoundation
import Tauri
import UIKit

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

// ---------------------------------------------------------------------------
// MARK: - AudioPlugin
// ---------------------------------------------------------------------------

/// Native audio plugin providing always-on mic capture and playback via a
/// single AVAudioEngine instance. Audio data flows through Rust ring buffers
/// via direct FFI — not through the JS bridge.
///
/// Lifecycle:
///   1. Frontend calls `initSession` when entering the tutor screen.
///      → AVAudioSession configured (.playAndRecord, .voiceChat, NO ducking)
///      → AVAudioEngine created with input tap + source node
///      → engine.start() — mic is hot, speaker ready
///   2. Frontend calls `teardownSession` when leaving the tutor screen.
///      → engine stopped, tap removed, source node detached, session deactivated
class AudioPlugin: Plugin {

    // -- Engine --
    private var audioEngine: AVAudioEngine?
    private var sourceNode: AVAudioSourceNode?
    private var tapInstalled = false
    private var sessionActive = false

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
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
        teardownEngineSync()
    }

    // -----------------------------------------------------------------------
    // MARK: - Audio session (no ducking)
    // -----------------------------------------------------------------------

    private func activateAudioSession() throws {
        let session = AVAudioSession.sharedInstance()
        // .playAndRecord: simultaneous mic + speaker.
        // .voiceChat: echo cancellation, low latency.
        // Options: bluetooth, default to speaker. Explicitly NO .duckOthers —
        // our own playback goes through the same engine, so there is nothing to duck.
        try session.setCategory(
            .playAndRecord,
            mode: .voiceChat,
            options: [.allowBluetooth, .allowBluetoothA2DP, .defaultToSpeaker]
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
    private func setupEngine() throws {
        // Clean slate.
        teardownEngineSync()

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
        let pbFormat = AVAudioFormat(
            standardFormatWithSampleRate: playbackSampleRate,
            channels: playbackChannels
        )!

        let node = AVAudioSourceNode(format: pbFormat) { _, _, frameCount, audioBufferList -> OSStatus in
            let ablPointer = UnsafeMutableAudioBufferListPointer(audioBufferList)
            guard let buf = ablPointer.first,
                  let data = buf.mData?.assumingMemoryBound(to: Float.self) else {
                return noErr
            }
            let _ = rust_audio_render_callback(data, frameCount)
            return noErr
        }

        sourceNode = node
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: pbFormat)

        // -- Start --
        try engine.start()
        sessionActive = true
    }

    /// Synchronous teardown — safe to call from any context on pluginQueue.
    private func teardownEngineSync() {
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

        if sessionActive {
            deactivateAudioSession()
        }
        sessionActive = false
    }

    // -----------------------------------------------------------------------
    // MARK: - Interruption / route change
    // -----------------------------------------------------------------------

    @objc private func handleInterruption(_ notification: Notification) {
        guard let info = notification.userInfo,
              let typeValue = info[AVAudioSessionInterruptionTypeKey] as? UInt,
              let type = AVAudioSession.InterruptionType(rawValue: typeValue) else { return }

        pluginQueue.async { [weak self] in
            guard let self = self, self.sessionActive else { return }
            switch type {
            case .began:
                break // Engine is paused by the system.
            case .ended:
                // Try to restart.
                if let engine = self.audioEngine, !engine.isRunning {
                    do {
                        try engine.start()
                    } catch {
                        NSLog("[AudioPlugin] Failed to restart after interruption: \(error)")
                    }
                }
            @unknown default:
                break
            }
        }
    }

    @objc private func handleRouteChange(_ notification: Notification) {
        guard let info = notification.userInfo,
              let reasonValue = info[AVAudioSessionRouteChangeReasonKey] as? UInt,
              let reason = AVAudioSession.RouteChangeReason(rawValue: reasonValue) else { return }

        pluginQueue.async { [weak self] in
            guard let self = self, self.sessionActive else { return }
            if reason == .oldDeviceUnavailable {
                if let engine = self.audioEngine, !engine.isRunning {
                    do {
                        try engine.start()
                    } catch {
                        NSLog("[AudioPlugin] Failed to restart after route change: \(error)")
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // MARK: - Tauri commands
    // -----------------------------------------------------------------------

    @objc func initSession(_ invoke: Invoke) {
        let args = invoke.parseArgs(InitSessionArgs.self)

        pluginQueue.async { [weak self] in
            guard let self = self else { return }

            if self.sessionActive {
                invoke.reject("Audio session already active")
                return
            }

            // Apply config from args.
            if let sr = args?.captureSampleRate { self.captureSampleRate = Double(sr) }
            if let sr = args?.playbackSampleRate { self.playbackSampleRate = Double(sr) }
            if let ch = args?.captureChannels { self.captureChannels = UInt32(ch) }
            if let ch = args?.playbackChannels { self.playbackChannels = UInt32(ch) }
            if let bs = args?.captureBufferSize { self.captureBufferSize = UInt32(bs) }

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
            invoke.resolve([
                "session": self.sessionActive ? "active" : "inactive",
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
}

// ---------------------------------------------------------------------------
// MARK: - C entry point for Tauri iOS plugin binding
// ---------------------------------------------------------------------------

@_cdecl("init_plugin_audio")
func initPlugin() -> Plugin {
    return AudioPlugin()
}
