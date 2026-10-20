import AppKit
import CoreText
import os
import SwiftUI

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
