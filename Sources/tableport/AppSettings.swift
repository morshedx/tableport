import AppKit
import Foundation
import TablePortCore

/// Preferences from the Settings window, kept in UserDefaults. Code that
/// draws or loads reads them when it needs them, so a change applies to the
/// next page, query or redraw.
@MainActor
final class AppSettings: ObservableObject {
    static let shared = AppSettings()

    enum Appearance: String, CaseIterable {
        case system = "System"
        case light = "Light"
        case dark = "Dark"
    }

    private let defaults = UserDefaults.standard

    @Published var appearance: Appearance {
        didSet {
            defaults.set(appearance.rawValue, forKey: "appearance")
            applyAppearance()
        }
    }
    /// Rows in one page of a table tab.
    @Published var pageSize: Int {
        didSet { defaults.set(pageSize, forKey: "pageSize") }
    }
    @Published var confirmProductionWrites: Bool {
        didSet { defaults.set(confirmProductionWrites, forKey: "confirmProductionWrites") }
    }
    @Published var editorFontSize: Double {
        didSet { defaults.set(editorFontSize, forKey: "editorFontSize") }
    }
    @Published var gridFontSize: Double {
        didSet { defaults.set(gridFontSize, forKey: "gridFontSize") }
    }
    /// Rows each query result keeps; the core still counts all of them.
    @Published var queryRowLimit: Int {
        didSet { defaults.set(queryRowLimit, forKey: "queryRowLimit") }
    }

    /// Reopen the last session's connections and tabs at launch.
    @Published var restoreSession: Bool {
        didSet { defaults.set(restoreSession, forKey: "restoreSession") }
    }
    /// How autocomplete writes keywords.
    @Published var keywordCase: KeywordCase {
        didSet { defaults.set(Self.name(of: keywordCase), forKey: "keywordCase") }
    }

    static let keywordCases: [(KeywordCase, String)] = [(.upper, "Uppercase"), (.lower, "Lowercase"), (.asTyped, "As typed")]

    private static func name(of keywordCase: KeywordCase) -> String {
        keywordCases.first { $0.0 == keywordCase }?.1 ?? "Uppercase"
    }

    static let pageSizes = [100, 300, 500, 1000]
    static let queryRowLimits = [500, 1000, 5000, 10000]

    private init() {
        appearance = defaults.string(forKey: "appearance").flatMap(Appearance.init) ?? .system
        pageSize = Self.pageSizes.contains(defaults.integer(forKey: "pageSize")) ? defaults.integer(forKey: "pageSize") : 300
        confirmProductionWrites = defaults.object(forKey: "confirmProductionWrites") as? Bool ?? true
        editorFontSize = (11...20).contains(defaults.double(forKey: "editorFontSize")) ? defaults.double(forKey: "editorFontSize") : 13
        gridFontSize = (10...16).contains(defaults.double(forKey: "gridFontSize")) ? defaults.double(forKey: "gridFontSize") : 12
        restoreSession = defaults.object(forKey: "restoreSession") as? Bool ?? true
        let savedCase = defaults.string(forKey: "keywordCase")
        keywordCase = Self.keywordCases.first { $0.1 == savedCase }?.0 ?? .upper
        queryRowLimit = Self.queryRowLimits.contains(defaults.integer(forKey: "queryRowLimit")) ? defaults.integer(forKey: "queryRowLimit") : 1000
    }

    func applyAppearance() {
        NSApp.appearance = switch appearance {
        case .system: nil
        case .light: NSAppearance(named: .aqua)
        case .dark: NSAppearance(named: .darkAqua)
        }
    }
}
