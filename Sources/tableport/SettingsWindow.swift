import AppKit
import MacKit
import MacKitUpdates
import SwiftUI

enum SettingsPageID: Hashable {
    case general, editor, updates, about
}

@MainActor
func makeSettingsWindow() -> HostedWindow<SettingsView> {
    // Resizable: Updates and its version history grow with every release.
    HostedWindow(size: NSSize(width: 720, height: 500), autosaveName: "SettingsWindow", resizable: true, opensUnfocused: true) {
        SettingsView(settings: .shared)
    }
}

struct SettingsView: View {
    @ObservedObject var settings: AppSettings
    @State private var page = SettingsPageID.general

    private let pages = [
        SettingsPage(SettingsPageID.general, title: "General", subtitle: "Look, tables and safety.",
                     symbol: "slider.horizontal.3", tile: Theme.Tile.gray),
        SettingsPage(.editor, title: "Editor", subtitle: "Fonts, autocomplete and query results.",
                     symbol: "chevron.left.forwardslash.chevron.right", tile: Theme.Tile.blue),
        SettingsPage(.updates, title: "Updates", subtitle: "New versions and what changed.",
                     symbol: "arrow.down.circle", tile: Theme.Tile.green, startsGroup: true),
        SettingsPage(.about, title: "About", subtitle: "Version and licences.",
                     symbol: "info.circle", tile: Theme.Tile.purple, showsHeader: false),
    ]

    var body: some View {
        SettingsShell(appName: AppInfo.name, pages: pages, selection: $page) { id in
            switch id {
            case .general: general
            case .editor: editor
            case .updates: UpdatesPane(updater: .shared, notes: ReleaseNotes.notes)
            case .about: about
            }
        }
    }

    @ViewBuilder
    private var general: some View {
        SettingsGroup("Appearance") {
            SettingsRow("circle.lefthalf.filled", Theme.Tile.gray, "Theme") {
                picker("Theme", $settings.appearance, AppSettings.Appearance.allCases.map { ($0, $0.rawValue) })
            }
        }
        SettingsGroup("Tables") {
            SettingsRow("tablecells", Theme.Tile.blue, "Rows per page", hint: "How many rows a table tab loads at a time.") {
                picker("Rows per page", $settings.pageSize, AppSettings.pageSizes.map { ($0, "\($0)") })
            }
        }
        SettingsGroup("Safety") {
            SettingsRow("exclamationmark.shield", Theme.Tile.red, "Ask before changing production",
                        hint: "Commit, and SQL that changes data, ask first on connections tagged production.") {
                Toggle("Ask before changing production", isOn: $settings.confirmProductionWrites)
                    .toggleStyle(ThemeToggleStyle())
                    .labelsHidden()
            }
        }
    }

    @ViewBuilder
    private var editor: some View {
        SettingsGroup("Fonts") {
            SettingsRow("textformat.size", Theme.Tile.blue, "SQL editor", hint: "JetBrains Mono, \(Int(settings.editorFontSize)) pt.") {
                Stepper("SQL editor", value: $settings.editorFontSize, in: 11...20, step: 1).labelsHidden()
            }
            RowDivider()
            SettingsRow("tablecells", Theme.Tile.teal, "Data grid", hint: "JetBrains Mono, \(Int(settings.gridFontSize)) pt.") {
                Stepper("Data grid", value: $settings.gridFontSize, in: 10...16, step: 1).labelsHidden()
            }
        }
        SettingsGroup("Autocomplete") {
            SettingsRow("textformat", Theme.Tile.purple, "Keyword case",
                        hint: "How suggested keywords are written. Table and column names keep their own case.") {
                picker("Keyword case", $settings.keywordCase, AppSettings.keywordCases)
            }
        }
        SettingsGroup("Queries") {
            SettingsRow("list.number", Theme.Tile.orange, "Rows kept per result",
                        hint: "More rows use more memory; the full count is always shown.") {
                picker("Rows kept per result", $settings.queryRowLimit, AppSettings.queryRowLimits.map { ($0, $0.formatted()) })
            }
        }
    }

    private func picker<T: Hashable>(_ title: String, _ selection: Binding<T>, _ options: [(T, String)]) -> some View {
        Picker(title, selection: selection) {
            ForEach(options, id: \.0) { Text($0.1).tag($0.0) }
        }
        .labelsHidden()
        .pickerStyle(.menu)
        .fixedSize()
    }
}

extension SettingsView {
    var about: some View {
        AboutPane(
            updater: .shared,
            appName: AppInfo.name,
            summary: "A fast, native PostgreSQL client. Free and open source.",
            links: [
                AboutLink("Website", url: "https://github.com/morshedx/tableport"),
                AboutLink("Report a Problem", url: "https://github.com/morshedx/tableport/issues/new?template=bug_report.yml"),
                AboutLink("What's New") { page = .updates },
            ],
            footer: "Apache-2.0. JetBrains Mono: SIL Open Font License 1.1. Icons: Lucide, ISC License. The PostgreSQL logo is a trademark of the PostgreSQL Community Association.",
            showUpdates: { page = .updates }
        )
    }
}
