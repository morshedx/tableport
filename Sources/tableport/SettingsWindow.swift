import AppKit
import MacKit
import MacKitUpdates
import SwiftUI

enum SettingsPageID: Hashable {
    case general, editor, ai, updates, about
}

@MainActor
func makeSettingsWindow(store: ConnectionStore, aiAccess: AiAccess) -> HostedWindow<SettingsView> {
    // Resizable: Updates and its version history grow with every release.
    HostedWindow(size: NSSize(width: 720, height: 500), autosaveName: "SettingsWindow", resizable: true, opensUnfocused: true) {
        SettingsView(settings: .shared, store: store, aiLog: aiAccess.log)
    }
}

struct SettingsView: View {
    @ObservedObject var settings: AppSettings
    @ObservedObject var store: ConnectionStore
    @ObservedObject var aiLog: AiAccessLog
    @State private var page = SettingsPageID.general

    private let pages = [
        SettingsPage(SettingsPageID.general, title: "General", subtitle: "Look, tables and safety.",
                     symbol: "slider.horizontal.3", tile: Theme.Tile.gray),
        SettingsPage(.editor, title: "Editor", subtitle: "Fonts, autocomplete and query results.",
                     symbol: "chevron.left.forwardslash.chevron.right", tile: Theme.Tile.blue),
        SettingsPage(.ai, title: "AI access", subtitle: "Let Claude, Cursor and other AI tools read your databases.",
                     symbol: "sparkles", tile: Theme.Tile.orange),
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
            case .ai: AiAccessPane(store: store, log: aiLog)
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
        SettingsGroup("Startup") {
            SettingsRow("arrow.counterclockwise", Theme.Tile.green, "Reopen connections and tabs",
                        hint: "At launch, reconnect what was open when you quit, with its tabs and SQL.") {
                Toggle("Reopen connections and tabs", isOn: $settings.restoreSession)
                    .toggleStyle(ThemeToggleStyle())
                    .labelsHidden()
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
            summary: "A fast, native database client. Free and open source.",
            links: [
                AboutLink("Website", url: "https://github.com/morshedx/tableport"),
                AboutLink("Report a Problem", url: "https://github.com/morshedx/tableport/issues/new?template=bug_report.yml"),
                AboutLink("What's New") { page = .updates },
                AboutLink("Licences") {
                    if let url = Bundle.resources?.url(forResource: "ThirdPartyLicences", withExtension: "txt") { NSWorkspace.shared.open(url) }
                },
            ],
            footer: "Apache-2.0. JetBrains Mono: SIL Open Font License 1.1. Icons: Lucide, ISC License. Database names are trademarks of their owners. TablePort is not affiliated with or endorsed by them.",
            showUpdates: { page = .updates }
        )
    }
}

/// Settings › AI access: which connections AI tools may read, how to set an
/// AI tool up, and what they ran.
struct AiAccessPane: View {
    @ObservedObject var store: ConnectionStore
    @ObservedObject var log: AiAccessLog
    @State private var copied: String?

    private var claudeCode: String { "claude mcp add tableport -- \"\(AiAccess.command)\" mcp" }
    private var json: String {
        """
        {
          "mcpServers": {
            "tableport": { "command": "\(AiAccess.command)", "args": ["mcp"] }
          }
        }
        """
    }

    var body: some View {
        SettingsGroup("Connections") {
            if store.connections.isEmpty {
                Text("No saved connections yet.").font(Theme.caption).foregroundStyle(Theme.inkSecondary).padding(12)
            }
            ForEach(Array(store.connections.enumerated()), id: \.element.id) { index, connection in
                if index > 0 { RowDivider() }
                SettingsRow("cylinder", connection.tag == .production ? Theme.Tile.red : Theme.Tile.blue, connection.name,
                            hint: connection.tag == .production ? "Production: AI tools would read live data." : connection.subtitle) {
                    Toggle(connection.name, isOn: Binding(
                        get: { connection.aiAccess },
                        set: { on in
                            var changed = connection
                            changed.aiAccess = on
                            store.update(changed)
                        }
                    ))
                    .toggleStyle(ThemeToggleStyle())
                    .labelsHidden()
                }
            }
        }
        Text("AI tools can only read: every query runs in a read-only transaction, one statement at a time, with a 30-second limit and at most 1,000 rows. They see only the connections switched on here, and never your passwords.")
            .font(Theme.meta)
            .foregroundStyle(Theme.inkSecondary)
            .fixedSize(horizontal: false, vertical: true)

        SettingsGroup("Set up an AI tool") {
            SettingsRow("terminal", Theme.Tile.gray, "Claude Code", hint: "Runs this in Terminal.") {
                copyButton(claudeCode, id: "code")
            }
            RowDivider()
            SettingsRow("doc.text", Theme.Tile.gray, "Claude Desktop, Cursor, others",
                        hint: "Paste into the tool's MCP settings (mcpServers).") {
                copyButton(json, id: "json")
            }
        }

        SettingsGroup("Recent activity") {
            if log.entries.isEmpty {
                Text("No AI queries since TablePort opened.")
                    .font(Theme.caption)
                    .foregroundStyle(Theme.inkSecondary)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(12)
            }
            ForEach(Array(log.entries.prefix(50).enumerated()), id: \.element.id) { index, entry in
                if index > 0 { RowDivider() }
                VStack(alignment: .leading, spacing: 3) {
                    HStack(spacing: 6) {
                        Image(systemName: entry.call.succeeded ? "checkmark.circle" : "xmark.octagon")
                            .foregroundStyle(entry.call.succeeded ? Theme.success : Theme.danger)
                        Text(entry.call.tool).font(.system(size: 12, weight: .medium))
                        if !entry.call.connection.isEmpty {
                            Text("· \(entry.call.connection)").font(Theme.caption).foregroundStyle(Theme.inkSecondary)
                        }
                        Spacer()
                        Text("\(entry.call.summary) · \(entry.call.elapsedMs) ms · \(entry.date.formatted(date: .omitted, time: .standard))")
                            .font(Theme.meta)
                            .foregroundStyle(Theme.inkSecondary)
                            .lineLimit(1)
                    }
                    if let sql = entry.call.sql {
                        Text(sql.replacingOccurrences(of: "\\s+", with: " ", options: .regularExpression))
                            .font(AppFont.monoFont(size: 11))
                            .foregroundStyle(Theme.inkSecondary)
                            .lineLimit(2)
                            .textSelection(.enabled)
                    }
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 8)
            }
        }
    }

    private func copyButton(_ text: String, id: String) -> some View {
        Button(copied == id ? "Copied" : "Copy") {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
            copied = id
        }
        .help(text)
    }
}
