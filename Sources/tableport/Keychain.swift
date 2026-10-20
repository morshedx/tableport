import Foundation
import os
import Security

/// Connection passwords in the login keychain, one item per saved connection.
enum Keychain {
    struct Failure: LocalizedError {
        let status: OSStatus
        var errorDescription: String? {
            let reason = SecCopyErrorMessageString(status, nil) as String? ?? "error \(status)"
            return "Keychain: \(reason)"
        }
    }

    private static let service = (Bundle.main.bundleIdentifier ?? "tableport") + ".connection"
    private static let logger = Logger(subsystem: Bundle.main.bundleIdentifier ?? "tableport", category: "Keychain")

    /// The secrets one connection can have. The database password keeps the
    /// plain connection ID as its account, as the first version stored it.
    enum Secret: CaseIterable {
        case databasePassword
        case sshPassword
        case sshKeyPassphrase

        func account(_ id: UUID) -> String {
            switch self {
            case .databasePassword: id.uuidString
            case .sshPassword: "\(id.uuidString).ssh-password"
            case .sshKeyPassphrase: "\(id.uuidString).ssh-key-passphrase"
            }
        }
    }

    private static func query(_ secret: Secret, _ id: UUID, service: String = service) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: secret.account(id),
        ]
    }

    /// `service` is only for reading another app's items: the migration
    /// from the "bangladb" build.
    static func secret(_ secret: Secret, for id: UUID, service: String = service) throws -> String? {
        var query = query(secret, id, service: service)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let data = result as? Data else { throw Failure(status: status) }
        return String(decoding: data, as: UTF8.self)
    }

    /// Stores the secret, or removes the item when it is empty. Always a new
    /// item, never an update: macOS records the creating app's Team ID on an
    /// item when it is created, and an item made by an older build (or a
    /// build without a team) would keep asking on every new build.
    static func setSecret(_ value: String, _ secret: Secret, for id: UUID) throws {
        try deleteSecret(secret, for: id)
        guard !value.isEmpty else { return }
        var item = query(secret, id)
        item[kSecValueData as String] = Data(value.utf8)
        let status = SecItemAdd(item as CFDictionary, nil)
        guard status == errSecSuccess else { throw Failure(status: status) }
    }

    /// Re-creates a connection's secrets under this build's Team ID, once.
    /// Items saved before the app was team-signed are tied to single build
    /// hashes, so every build asked for the password again.
    static func resealOnce(for id: UUID) {
        let key = "keychainResealed.\(id.uuidString)"
        guard !UserDefaults.standard.bool(forKey: key) else { return }
        do {
            for secret in Secret.allCases {
                if let value = try self.secret(secret, for: id) {
                    try setSecret(value, secret, for: id)
                }
            }
            UserDefaults.standard.set(true, forKey: key)
        } catch {
            logger.error("Could not re-save the keychain items: \(error.localizedDescription, privacy: .public)")
        }
    }

    static func deleteSecret(_ secret: Secret, for id: UUID) throws {
        let status = SecItemDelete(query(secret, id) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw Failure(status: status) }
    }

    /// Every secret of a connection, when it is deleted.
    static func deleteAll(for id: UUID) throws {
        for secret in Secret.allCases {
            try deleteSecret(secret, for: id)
        }
    }
}
