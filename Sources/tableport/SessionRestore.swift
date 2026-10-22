import Foundation
import os
import TablePortCore

/// What reopens on the next launch: each connection window that was open,
/// with its database, schema, tabs and their SQL. Closing a window yourself
/// takes it out; quitting keeps it.
struct SavedWindow: Codable, Equatable {
    enum Tab: Codable, Equatable {
        case relation(schema: String, name: String, kind: String)
        case query(sql: String, savedQueryID: Int64?)
    }

    var connectionID: UUID
    var database: String
    var schema: String
    var tabs: [Tab]
    var selectedTab: Int?
}

/// Kept in UserDefaults as JSON: a few kilobytes of window state.
@MainActor
enum SessionStore {
    private static let key = "savedSession"
    private static let logger = Logger(subsystem: Bundle.main.bundleIdentifier ?? "tableport", category: "SessionStore")
    private static var lastSaved: Data?

    static func load() -> [SavedWindow] {
        guard let data = UserDefaults.standard.data(forKey: key) else { return [] }
        do {
            return try JSONDecoder().decode([SavedWindow].self, from: data)
        } catch {
            logger.error("Could not read the saved session: \(error.localizedDescription, privacy: .public)")
            return []
        }
    }

    /// Writes only when something changed: this runs every few seconds.
    static func save(_ windows: [SavedWindow]) {
        do {
            let data = try JSONEncoder().encode(windows)
            guard data != lastSaved else { return }
            UserDefaults.standard.set(data, forKey: key)
            lastSaved = data
        } catch {
            logger.error("Could not save the session: \(error.localizedDescription, privacy: .public)")
        }
    }
}

extension RelationKind {
    /// A stable name for saving; the generated enum isn't Codable.
    var savedName: String {
        switch self {
        case .table: "table"
        case .partitionedTable: "partitioned"
        case .view: "view"
        case .materializedView: "materialized"
        case .foreignTable: "foreign"
        }
    }

    init?(savedName: String) {
        switch savedName {
        case "table": self = .table
        case "partitioned": self = .partitionedTable
        case "view": self = .view
        case "materialized": self = .materializedView
        case "foreign": self = .foreignTable
        default: return nil
        }
    }
}
