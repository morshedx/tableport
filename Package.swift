// swift-tools-version: 6.0
import Foundation
import PackageDescription

// libtableport_core.a, built by `make core` in core/. Linked by its full path:
// the same folder also holds the .dylib (read only by uniffi-bindgen), and
// -ltableport_core would pick that instead.
let coreLibrary = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent()
    .appendingPathComponent("core/target/release/libtableport_core.a")
    .path

let package = Package(
    name: "tableport",
    platforms: [.macOS(.v14)],
    products: [
        .executable(name: "tableport", targets: ["tableport"])
    ],
    dependencies: [
        // Shared UI (Settings window, theme), checked out next to this repo.
        .package(path: "../mac-kit"),
    ],
    targets: [
        // The C side of the Rust core's bindings (header + module map, generated).
        .systemLibrary(name: "tableport_coreFFI", path: "Sources/tableport_coreFFI"),
        // The Rust core's Swift API: generated bindings plus small extensions.
        .target(
            name: "TablePortCore",
            dependencies: ["tableport_coreFFI"],
            linkerSettings: [
                .unsafeFlags([coreLibrary]),
                .linkedFramework("Security"),          // native-tls
                .linkedFramework("SystemConfiguration"),
                .linkedFramework("CoreFoundation"),
            ]
        ),
        .executableTarget(
            name: "tableport",
            dependencies: [
                "TablePortCore",
                .product(name: "MacKit", package: "mac-kit"),
                // Sparkle, behind Settings › Updates.
                .product(name: "MacKitUpdates", package: "mac-kit"),
            ],
            path: "Sources/tableport",
            resources: [.process("Resources")],   // JetBrains Mono, Lucide icons (SVG)
            linkerSettings: [
                // Sparkle.framework lives in Contents/Frameworks.
                .unsafeFlags(["-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"])
            ]
        ),
    ]
)
