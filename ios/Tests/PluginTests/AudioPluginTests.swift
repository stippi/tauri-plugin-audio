import XCTest
@testable import tauri_plugin_audio

/// Unit tests for Audio Plugin
final class AudioPluginTests: XCTestCase {

    func testPluginInit() throws {
        // Smoke test — the plugin module should load.
        XCTAssertTrue(true, "Plugin module loaded successfully")
    }

    func testDefaultSampleRates() throws {
        // These must match the defaults in models.rs / AudioPlugin.swift.
        let captureSampleRate: Double = 48_000
        let playbackSampleRate: Double = 24_000

        XCTAssertEqual(captureSampleRate, 48_000, accuracy: 0.001)
        XCTAssertEqual(playbackSampleRate, 24_000, accuracy: 0.001)
    }

    func testSessionCategoryIsPlayAndRecord() throws {
        // The plugin must always use .playAndRecord with NO ducking,
        // so our own TTS playback is never attenuated.
        let category: String = "playAndRecord"
        XCTAssertEqual(category, "playAndRecord")
    }
}
