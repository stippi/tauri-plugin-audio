// swift-tools-version:5.3
// The swift-tools-version declares the minimum version of Swift required to build this package.

import PackageDescription

let package = Package(
    name: "tauri-plugin-audio",
    platforms: [
        .macOS(.v10_15),
        .iOS(.v13),
    ],
    products: [
        .library(
            name: "tauri-plugin-audio",
            type: .static,
            targets: ["tauri-plugin-audio"]),
    ],
    dependencies: [
        .package(name: "Tauri", path: "../.tauri/tauri-api")
    ],
    targets: [
        .target(
            name: "tauri-plugin-audio",
            dependencies: [
                .byName(name: "Tauri")
            ],
            path: "Sources")
    ]
)
