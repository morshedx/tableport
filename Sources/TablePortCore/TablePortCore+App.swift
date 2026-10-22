import Foundation

// Additions to the generated bindings in tableport_core.swift. Logic stays in
// the Rust core; this is only what Swift views need from these types.

extension Error {
    /// The text to show the user. UniFFI's own description of a
    /// DatabaseError is a debug dump, so the core's message is taken out.
    public var displayMessage: String {
        switch self as? DatabaseError {
        case .Failed(let message, let detail, let hint, _):
            // Postgres' detail says which value or row was the problem
            // ("Key (status)=(new) is duplicated."); the hint, what to do.
            return [message, detail, hint.map { "Hint: \($0)" }].compactMap { $0 }.joined(separator: "\n")
        case .UnknownHostKey(let host, let fingerprint):
            return "The SSH host key of \(host) is not known yet (\(fingerprint))."
        case nil:
            return localizedDescription
        }
    }

    /// Where in the SQL the server found the problem: a 1-based character
    /// offset, when it says.
    /// Set when the SSH server's key needs the user's trust first.
    public var unknownHostKey: (host: String, fingerprint: String)? {
        if case .UnknownHostKey(let host, let fingerprint) = self as? DatabaseError {
            return (host, fingerprint)
        }
        return nil
    }

    public var sqlPosition: Int? {
        if let error = self as? DatabaseError, case .Failed(_, _, _, let position) = error {
            return position.map(Int.init)
        }
        return nil
    }
}

extension Relation: Identifiable {
    public var id: String { "\(schema).\(name)" }
}

extension SchemaContents: Identifiable {
    public var id: String { name }
}

extension FilterOperator: CaseIterable {
    public static let allCases: [FilterOperator] = [
        .equals, .notEquals, .contains, .notContains, .startsWith, .endsWith,
        .greaterThan, .greaterOrEqual, .lessThan, .lessOrEqual, .in, .notIn, .like, .isNull, .isNotNull,
    ]

    public var label: String {
        switch self {
        case .equals: "="
        case .notEquals: "≠"
        case .contains: "Contains"
        case .notContains: "Not contains"
        case .startsWith: "Starts with"
        case .endsWith: "Ends with"
        case .greaterThan: ">"
        case .greaterOrEqual: "≥"
        case .lessThan: "<"
        case .lessOrEqual: "≤"
        case .in: "IN"
        case .notIn: "NOT IN"
        case .like: "LIKE"
        case .isNull: "IS NULL"
        case .isNotNull: "IS NOT NULL"
        }
    }

    public var needsValue: Bool { self != .isNull && self != .isNotNull }
}

extension TlsMode: CaseIterable {
    public static let allCases: [TlsMode] = [.disable, .prefer, .require, .verifyFull]
}

public enum Grid {
    /// Cells longer than this come cut from the core, so they can't be
    /// edited inline.
    public static let cellLimit = Int(gridCellLimit())
}
