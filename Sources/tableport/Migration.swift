import Foundation
import os

/// Moves, once, what the app kept while it was the "bangladb" build: the
/// saved connections, the settings and the Keychain passwords. The bundle ID
/// names all three places, so the renamed app starts with none of them.
/// The old copies stay where they are.
@MainActor
enum CodenameMigration {
    static let legacyBundleID = "com.morshedx.bangladb"
    private static let doneKey = "migratedFromBangladb"
    private static let logger = Logger(subsystem: Bundle.main.bundleIdentifier ?? "tableport", category: "Migration")

    static func run() {
        let defaults = UserDefaults.standard
        guard Bundle.main.bundleIdentifier != legacyBundleID, !defaults.bool(forKey: doneKey) else { return }

        // Settings and window frames. Keys this app already set win.
        if let old = defaults.persistentDomain(forName: legacyBundleID) {
            for (key, value) in old where defaults.object(forKey: key) == nil
                && !key.hasPrefix("NS") && !key.hasPrefix("keychainResealed.") {
                defaults.set(value, forKey: key)
            }
        }

        let newFile = ConnectionStore.defaultFileURL
        let oldFile = newFile.deletingLastPathComponent().deletingLastPathComponent()
            .appendingPathComponent(legacyBundleID, isDirectory: true)
            .appendingPathComponent("connections.json")
        let files = FileManager.default
        guard files.fileExists(atPath: oldFile.path) else {
            defaults.set(true, forKey: doneKey)
            return
        }
        do {
            if !files.fileExists(atPath: newFile.path) {
                try files.createDirectory(at: newFile.deletingLastPathComponent(), withIntermediateDirectories: true)
                try files.copyItem(at: oldFile, to: newFile)
            }
            let connections = try JSONDecoder().decode([SavedConnection].self, from: Data(contentsOf: oldFile))
            // macOS may ask once per password: the items belong to the old app.
            let oldService = legacyBundleID + ".connection"
            for connection in connections {
                for secret in Keychain.Secret.allCases {
                    guard try Keychain.secret(secret, for: connection.id) == nil,
                          let value = try Keychain.secret(secret, for: connection.id, service: oldService) else { continue }
                    try Keychain.setSecret(value, secret, for: connection.id)
                }
            }
            defaults.set(true, forKey: doneKey)
        } catch {
            // Not marked done, so the next launch tries again.
            logger.error("Could not move the bangladb data: \(error.localizedDescription, privacy: .public)")
        }
    }
}
