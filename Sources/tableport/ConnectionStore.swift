import Foundation
import os
import TablePortCore

/// The saved connections, kept as JSON in Application Support.
@MainActor
final class ConnectionStore: ObservableObject {
    @Published private(set) var connections: [SavedConnection] = []

    private let fileURL: URL
    private let logger = Logger(subsystem: Bundle.main.bundleIdentifier ?? "tableport", category: "ConnectionStore")

    init(fileURL: URL = ConnectionStore.defaultFileURL) {
        self.fileURL = fileURL
        load()
    }

    static var defaultFileURL: URL {
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return support
            .appendingPathComponent(Bundle.main.bundleIdentifier ?? "tableport", isDirectory: true)
            .appendingPathComponent("connections.json")
    }

    func add(_ connection: SavedConnection) {
        connections.append(connection)
        save()
    }

    func update(_ connection: SavedConnection) {
        guard let index = connections.firstIndex(where: { $0.id == connection.id }) else { return }
        connections[index] = connection
        save()
    }

    func duplicate(_ connection: SavedConnection) {
        var copy = connection
        copy.id = UUID()
        copy.name = "\(connection.name) copy"
        add(copy)
    }

    func delete(_ connection: SavedConnection) {
        connections.removeAll { $0.id == connection.id }
        save()
    }

    private func load() {
        guard FileManager.default.fileExists(atPath: fileURL.path) else { return }
        do {
            let data = try Data(contentsOf: fileURL)
            connections = try JSONDecoder().decode([SavedConnection].self, from: data)
        } catch {
            // Keep the unreadable file instead of overwriting it on the next save.
            logger.error("Could not read \(self.fileURL.path, privacy: .public): \(error.localizedDescription, privacy: .public)")
            let backup = fileURL.appendingPathExtension("unreadable")
            do {
                try? FileManager.default.removeItem(at: backup)
                try FileManager.default.moveItem(at: fileURL, to: backup)
            } catch {
                logger.error("Could not move the unreadable file aside: \(error.localizedDescription, privacy: .public)")
            }
        }
    }

    private func save() {
        do {
            try FileManager.default.createDirectory(at: fileURL.deletingLastPathComponent(), withIntermediateDirectories: true)
            let encoder = JSONEncoder()
            encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
            try encoder.encode(connections).write(to: fileURL, options: .atomic)
        } catch {
            logger.error("Could not save connections: \(error.localizedDescription, privacy: .public)")
        }
    }
}

/// The SQLite file for query history and saved queries, next to
/// connections.json. Nil when it can't be opened; history then isn't kept.
@MainActor
enum LocalData {
    struct Unavailable: LocalizedError {
        var errorDescription: String? { "The local store for saved queries could not be opened." }
    }

    static let store: LocalStore? = {
        let url = ConnectionStore.defaultFileURL.deletingLastPathComponent().appendingPathComponent("tableport.sqlite")
        do {
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            return try LocalStore.open(path: url.path)
        } catch {
            Logger(subsystem: Bundle.main.bundleIdentifier ?? "tableport", category: "LocalData")
                .error("Could not open the local store: \(error.localizedDescription, privacy: .public)")
            return nil
        }
    }()
}
