import TablePortCore
import Foundation
import MacKit
import SwiftUI

/// Where a connection points. Shown after the name in the Connections window
/// and, later, as the colored bar of a production main window.
enum ConnectionTag: String, Codable, CaseIterable, Identifiable {
    case local, development, staging, production

    var id: String { rawValue }

    var label: String { rawValue }

    var color: Color {
        switch self {
        case .local: Theme.success
        case .development: Theme.Tile.blue
        case .staging: Theme.Tile.orange
        case .production: Theme.danger
        }
    }
}

/// A saved PostgreSQL connection. The password is not part of it: it goes to
/// the Keychain, keyed by `id`.
struct SavedConnection: Codable, Identifiable, Hashable {
    var id = UUID()
    var name: String
    var host: String
    var port: Int = 5432
    var database: String
    var user: String
    var tag: ConnectionTag?
    var tls: TlsMode = .prefer
    /// An SSH tunnel to reach the server through; `host` is then the
    /// database server as the SSH server sees it.
    var ssh: SavedSSH?
    /// The server refuses every change; the grid offers no editing.
    var readOnly = false
    /// AI tools may read it through TablePort's MCP server (always read-only).
    var aiAccess = false
    /// With AI access: every database on the same server, with the same login.
    var aiAllDatabases = false

    /// "host : database", the second line of a row.
    var subtitle: String {
        ssh.map { "\(host) : \(database) via \($0.host)" } ?? "\(host) : \(database)"
    }

    /// The settings for the core, with the secrets read from the Keychain.
    func settings() throws -> ConnectionSettings {
        ConnectionSettings(
            host: host,
            port: UInt16(clamping: port),
            database: database,
            user: user,
            password: try Keychain.secret(.databasePassword, for: id),
            tls: tls,
            ssh: try ssh.map { ssh in
                SshSettings(
                    host: ssh.host,
                    port: UInt16(clamping: ssh.port),
                    user: ssh.user,
                    password: ssh.keyPath == nil ? try Keychain.secret(.sshPassword, for: id) : nil,
                    privateKeyPath: ssh.keyPath,
                    keyPassphrase: ssh.keyPath == nil ? nil : try Keychain.secret(.sshKeyPassphrase, for: id),
                    trustedFingerprint: ssh.trustedFingerprint
                )
            },
            readOnly: readOnly
        )
    }
}

/// The SSH part of a saved connection. Its password or key passphrase is in
/// the Keychain.
struct SavedSSH: Codable, Hashable {
    var host: String
    var port: Int = 22
    var user: String
    /// nil signs in with the password instead.
    var keyPath: String?
    /// A host key the user chose to trust, when ~/.ssh/known_hosts lacks it.
    var trustedFingerprint: String?
}

extension SavedConnection {
    // Fields added after the first version are optional in the file, so
    // older connections.json files still load.
    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        id = try container.decode(UUID.self, forKey: .id)
        name = try container.decode(String.self, forKey: .name)
        host = try container.decode(String.self, forKey: .host)
        port = try container.decode(Int.self, forKey: .port)
        database = try container.decode(String.self, forKey: .database)
        user = try container.decode(String.self, forKey: .user)
        tag = try container.decodeIfPresent(ConnectionTag.self, forKey: .tag)
        tls = try container.decodeIfPresent(TlsMode.self, forKey: .tls) ?? .prefer
        ssh = try container.decodeIfPresent(SavedSSH.self, forKey: .ssh)
        readOnly = try container.decodeIfPresent(Bool.self, forKey: .readOnly) ?? false
        aiAccess = try container.decodeIfPresent(Bool.self, forKey: .aiAccess) ?? false
        aiAllDatabases = try container.decodeIfPresent(Bool.self, forKey: .aiAllDatabases) ?? false
    }
}

// Stored in connections.json by these names, the same ones the first
// (Swift) version wrote.
extension TlsMode: Codable {
    private var storedName: String {
        switch self {
        case .disable: "disable"
        case .prefer: "prefer"
        case .require: "require"
        case .verifyFull: "verifyFull"
        }
    }

    public init(from decoder: Decoder) throws {
        let name = try decoder.singleValueContainer().decode(String.self)
        guard let mode = TlsMode.allCases.first(where: { $0.storedName == name }) else {
            throw DecodingError.dataCorrupted(.init(codingPath: decoder.codingPath, debugDescription: "Unknown TLS mode \(name)"))
        }
        self = mode
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        try container.encode(storedName)
    }

    var label: String {
        switch self {
        case .disable: "Disable"
        case .prefer: "Prefer"
        case .require: "Require"
        case .verifyFull: "Verify full"
        }
    }
}
