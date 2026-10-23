import AppKit
import CoreText
import os
import SwiftUI
import TablePortCore

extension Bundle {
    /// The SwiftPM resource bundle, found without `Bundle.module`.
    ///
    /// `Bundle.module` only looks at the .app's root (where codesign refuses
    /// extra files) and at the absolute `.build` path, and crashes when both
    /// are missing, so an installed app dies once `.build` is cleaned. The
    /// bundle is packaged in Contents/Resources; a bare `swift run` binary
    /// finds it next to itself, which is its `resourceURL` too.
    static let resources: Bundle? = Bundle.main.resourceURL
        .flatMap { Bundle(url: $0.appendingPathComponent("tableport_tableport.bundle")) }
}

/// JetBrains Mono, bundled (SIL OFL 1.1) so data looks the same on every Mac.
enum AppFont {
    private static let logger = Logger(subsystem: Bundle.main.bundleIdentifier ?? "tableport", category: "AppFont")

    /// Registers the bundled fonts for this process. Call once at launch.
    static func register() {
        for name in ["JetBrainsMono-Regular", "JetBrainsMono-Bold"] {
            guard let url = Bundle.resources?.url(forResource: name, withExtension: "ttf") else {
                logger.error("\(name, privacy: .public).ttf is missing from the app bundle")
                continue
            }
            var error: Unmanaged<CFError>?
            if !CTFontManagerRegisterFontsForURL(url as CFURL, .process, &error) {
                let reason = error?.takeRetainedValue().localizedDescription ?? "unknown error"
                logger.error("Could not register \(name, privacy: .public): \(reason, privacy: .public)")
            }
        }
    }

    /// Falls back to the system monospaced font if registration failed.
    static func mono(size: CGFloat, bold: Bool = false) -> NSFont {
        NSFont(name: bold ? "JetBrainsMono-Bold" : "JetBrainsMono-Regular", size: size)
            ?? .monospacedSystemFont(ofSize: size, weight: bold ? .bold : .regular)
    }

    static func monoFont(size: CGFloat, bold: Bool = false) -> Font {
        Font(mono(size: size, bold: bold))
    }
}

/// Lucide SVG icons (ISC licence) as template images, tinted by the view.
@MainActor
enum LucideIcon {
    private static var cache: [String: NSImage] = [:]

    static func image(_ name: String, size: CGFloat) -> NSImage? {
        let key = "\(name)@\(size)"
        if let cached = cache[key] { return cached }
        guard let url = Bundle.resources?.url(forResource: name, withExtension: "svg"),
              let source = NSImage(contentsOf: url) else { return nil }
        source.size = NSSize(width: size, height: size)
        let image = NSImage(size: NSSize(width: size, height: size))
        image.lockFocus()
        source.draw(in: NSRect(x: 0, y: 0, width: size, height: size))
        image.unlockFocus()
        image.isTemplate = true
        cache[key] = image
        return image
    }
}

/// The engine's initials on a tile in its colour. Most database owners'
/// trademark rules need written permission to show their logos here.
struct EngineLogo: View {
    let engine: Engine
    let size: CGFloat

    var body: some View {
        switch engine {
        case .postgres:
            tile(label: "PostgreSQL", letters: "PG", color: Color(red: 0.20, green: 0.40, blue: 0.60))
        case .sqlite:
            tile(label: "SQLite", letters: "SL", color: Color(red: 0.06, green: 0.45, blue: 0.75))
        case .mysql:
            tile(label: "MySQL", letters: "MY", color: Color(red: 0.27, green: 0.47, blue: 0.63))
        case .libsql:
            tile(label: "libSQL", letters: "TU", color: Color(red: 0.10, green: 0.62, blue: 0.54))
        case .redis:
            tile(label: "Redis", letters: "RE", color: Color(red: 1.0, green: 0.27, blue: 0.22))
        case .mongo:
            tile(label: "MongoDB", letters: "MO", color: Color(red: 0.28, green: 0.64, blue: 0.28))
        case .firestore:
            tile(label: "Firestore", letters: "FS", color: Color(red: 0.96, green: 0.62, blue: 0.10))
        case .d1:
            tile(label: "Cloudflare D1", letters: "D1", color: Color(red: 0.95, green: 0.50, blue: 0.13))
        case .oracle:
            tile(label: "Oracle", letters: "OR", color: Color(red: 0.78, green: 0.27, blue: 0.20))
        case .cassandra:
            tile(label: "Cassandra", letters: "CA", color: Color(red: 0.07, green: 0.53, blue: 0.69))
        case .mssql:
            tile(label: "SQL Server", letters: "SS", color: Color(red: 0.80, green: 0.16, blue: 0.15))
        }
    }

    private func tile(label: String, letters: String, color: Color) -> some View {
        Text(letters)
            .font(.system(size: size * 0.38, weight: .semibold))
            .lineLimit(1)
            .minimumScaleFactor(0.6)
            .foregroundStyle(.white)
            .frame(width: size, height: size)
            .background(RoundedRectangle(cornerRadius: size * 0.24).fill(color))
            .accessibilityLabel(label)
    }
}

