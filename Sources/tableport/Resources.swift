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

/// The PostgreSQL elephant (Slonik), in its own colours, to mark a
/// connection as PostgreSQL. The PostgreSQL trademark policy allows the logo
/// for referring to PostgreSQL.
/// The engine's logo: PostgreSQL's elephant, or a SQLite badge.
struct EngineLogo: View {
    let engine: Engine
    let size: CGFloat

    var body: some View {
        switch engine {
        case .postgres:
            PostgresLogo(size: size)
        case .sqlite:
            logo(Self.sqlite, label: "SQLite", letters: "Sl", color: Color(red: 0.06, green: 0.45, blue: 0.75))
        case .mysql:
            // One colour, so it can lighten in dark mode, where MySQL's blue is too dark to see.
            logo(Self.mysql, label: "MySQL", letters: "My", color: Self.mysqlBlue, tint: true)
        case .libsql:
            logo(Self.turso, label: "libSQL", letters: "Tu", color: Self.tursoTeal, tint: true)
        case .redis:
            logo(Self.redis, label: "Redis", letters: "Re", color: Color(red: 1.0, green: 0.27, blue: 0.22))
        case .mongo:
            logo(Self.mongo, label: "MongoDB", letters: "Mo", color: Color(red: 0.28, green: 0.64, blue: 0.28))
        case .mssql:
            // Microsoft's trademark rules don't allow its logo here: letters, in SQL Server's red.
            logo(nil, label: "SQL Server", letters: "Ss", color: Color(red: 0.80, green: 0.16, blue: 0.15))
        }
    }

    /// The engine's own logo, or its letters on a tile if the file is missing.
    @ViewBuilder
    private func logo(_ image: NSImage?, label: String, letters: String, color: Color, tint: Bool = false) -> some View {
        if let image {
            Image(nsImage: image)
                .resizable()
                .renderingMode(tint ? .template : .original)
                .interpolation(.high)
                .aspectRatio(contentMode: .fit)
                .foregroundStyle(color)
                .frame(width: size, height: size)
                .accessibilityLabel(label)
        } else {
            Text(letters)
                .font(.system(size: size * 0.42, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: size, height: size)
                .background(RoundedRectangle(cornerRadius: size * 0.24).fill(color))
                .accessibilityLabel(label)
        }
    }

    private static func image(_ name: String) -> NSImage? {
        Bundle.resources?.url(forResource: name, withExtension: "svg").flatMap(NSImage.init(contentsOf:))
    }

    private static let sqlite = image("sqlite")
    private static let mysql = image("mysql")
    private static let turso = image("turso")
    private static let redis = image("redis")
    private static let mongo = image("mongodb")
    /// Turso's own teal in dark mode; darker in light mode, where it would vanish.
    private static let tursoTeal = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            ? NSColor(red: 0.31, green: 0.97, blue: 0.82, alpha: 1)
            : NSColor(red: 0.10, green: 0.62, blue: 0.54, alpha: 1)
    })
    private static let mysqlBlue = Color(nsColor: NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            ? NSColor(red: 0.47, green: 0.66, blue: 0.84, alpha: 1)
            : NSColor(red: 0.27, green: 0.47, blue: 0.63, alpha: 1)
    })
}

struct PostgresLogo: View {
    let size: CGFloat
    private static let image: NSImage? = Bundle.resources?.url(forResource: "postgresql", withExtension: "svg")
        .flatMap(NSImage.init(contentsOf:))

    var body: some View {
        if let image = Self.image {
            Image(nsImage: image)
                .resizable()
                .interpolation(.high)
                .aspectRatio(contentMode: .fit)
                .frame(width: size, height: size)
                .accessibilityLabel("PostgreSQL")
        } else {
            Text("Pg")
                .font(.system(size: size * 0.4, weight: .semibold))
                .foregroundStyle(.white)
                .frame(width: size, height: size)
                .background(Circle().fill(Color(red: 0.2, green: 0.4, blue: 0.6)))
        }
    }
}
