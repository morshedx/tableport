import AppKit
import os
import TablePortCore
import MacKit
import SwiftUI

/// One query tab: the SQL, what ran last, and its results.
@MainActor
final class QueryTabModel: ObservableObject {
    struct HistoryItem: Identifiable {
        let id: Int64
        let sql: String
        let date: Date
        let elapsedMs: UInt64?
        let failed: Bool
    }

    /// The saved query this tab edits; Save updates it.
    @Published var savedQuery: SavedQuery?


    let session: DatabaseSession
    let isProduction: Bool
    @Published var sql = ""
    @Published var selection = NSRange(location: 0, length: 0)
    @Published private(set) var isRunning = false
    @Published private(set) var result: QueryResult?
    @Published private(set) var error: String?
    @Published private(set) var errorRange: NSRange?
    /// 0..<result sets are results; the last tab is Messages.
    @Published var selectedResult = 0
    /// Rows kept per result for this tab; nil keeps them all. Starts from Settings.
    @Published var rowLimit: Int? = AppSettings.shared.queryRowLimit

    static let rowLimits: [Int?] = [100, 500, 1000, 5000, 10000, nil]
    /// Shared by the query tabs of one connection window, newest first.
    let history: QueryHistory
    private var runTask: Task<Void, Never>?

    init(session: DatabaseSession, isProduction: Bool, history: QueryHistory) {
        self.session = session
        self.isProduction = isProduction
        self.history = history
    }

    /// Save Query (⌘S): updates the saved query this tab came from, or asks
    /// for a name and saves a new one. `asNew` always asks (Save As).
    func save(to saved: SavedQueries, asNew: Bool = false) {
        guard !sql.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        if let current = savedQuery, !asNew {
            savedQuery = saved.save(id: current.id, name: current.name, sql: sql) ?? current
            return
        }
        let suggestion = asNew ? (savedQuery.map { "\($0.name) copy" } ?? "") : ""
        guard let name = askQueryName(title: "Save Query", current: suggestion) else { return }
        if let new = saved.save(id: nil, name: name, sql: sql) {
            savedQuery = new
        }
    }

    /// Whether the editor differs from the saved query.
    var hasUnsavedEdits: Bool { savedQuery.map { $0.sql != sql } ?? false }

    enum Scope {
        /// The selection, or the statement under the cursor.
        case current
        case all
    }

    /// The range Run would use now, for the status line and the run itself.
    func range(for scope: Scope) -> NSRange? {
        let text = sql as NSString
        if selection.length > 0 && NSMaxRange(selection) <= text.length { return selection }
        switch scope {
        case .all:
            return NSRange(location: 0, length: text.length)
        case .current:
            return statementRange(sql: sql, cursor: UInt32(clamping: selection.location)).map {
                NSRange(location: Int($0.start), length: Int($0.end - $0.start))
            }
        }
    }

    /// Beautify: the selection, or all the text.
    func beautify() {
        let text = sql as NSString
        if selection.length > 0 && NSMaxRange(selection) <= text.length {
            let formatted = formatSql(sql: text.substring(with: selection))
            sql = text.replacingCharacters(in: selection, with: formatted)
            selection = NSRange(location: selection.location, length: (formatted as NSString).length)
        } else {
            sql = formatSql(sql: sql)
        }
    }

    /// Runs the selection, the statement under the cursor (Run Current), or
    /// everything (Run All).
    func run(_ scope: Scope = .current) {
        guard !isRunning, let range = range(for: scope) else { return }
        let text = sql as NSString
        let toRun = text.substring(with: range)
        guard !toRun.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        if isProduction && AppSettings.shared.confirmProductionWrites && Self.changesData(toRun) && !confirmOnProduction() {
            return
        }
        // Rows kept per result; the core still counts all of them.
        let maxRows = rowLimit.map { UInt64($0) } ?? UInt64.max

        isRunning = true
        error = nil
        errorRange = nil
        let session = session
        runTask = Task {
            do {
                let result = try await session.runQuery(sql: toRun, maxRows: maxRows)
                self.result = result
                selectedResult = result.resultSets.isEmpty ? result.resultSets.count : 0
                history.add(sql: toRun, elapsedMs: result.elapsedMs, failed: false)
            } catch {
                result = nil
                self.error = error.displayMessage
                selectedResult = 0
                if let position = error.sqlPosition {
                    errorRange = Self.wordRange(in: text, at: range.location + position - 1)
                }
                history.add(sql: toRun, elapsedMs: nil, failed: true)
            }
            isRunning = false
        }
    }

    func cancel() {
        guard isRunning else { return }
        let session = session
        Task {
            do {
                try await session.cancelQuery()
            } catch {
                self.error = "Could not cancel: \(error.displayMessage)"
            }
        }
    }

    /// The word starting at a UTF-16 offset, for the error underline.
    private static func wordRange(in text: NSString, at offset: Int) -> NSRange? {
        guard offset >= 0, offset < text.length else { return nil }
        var end = offset
        while end < text.length, let scalar = UnicodeScalar(text.character(at: end)),
              !CharacterSet.whitespacesAndNewlines.union(.punctuationCharacters).contains(scalar) || end == offset {
            end += 1
        }
        return NSRange(location: offset, length: max(1, end - offset))
    }

    /// Whether any statement starts with a keyword that writes. A guess for
    /// the production warning, not a parser: SELECT can call a function that
    /// writes too.
    static func changesData(_ sql: String) -> Bool {
        let writes: Set<String> = [
            "insert", "update", "delete", "merge", "drop", "alter", "truncate", "create", "grant", "revoke",
            "comment", "copy", "vacuum", "reindex", "cluster", "refresh", "call", "do",
        ]
        let withoutComments = sql
            .replacingOccurrences(of: "--[^\n]*", with: "", options: .regularExpression)
            .replacingOccurrences(of: "/\\*[\\s\\S]*?\\*/", with: "", options: .regularExpression)
        return withoutComments.split(separator: ";").contains { statement in
            let first = statement.split(whereSeparator: { $0.isWhitespace }).first.map { $0.lowercased() } ?? ""
            return writes.contains(first)
        }
    }

    private func confirmOnProduction() -> Bool {
        let alert = NSAlert()
        alert.messageText = "Run SQL that changes data on a production connection?"
        alert.informativeText = "This query contains statements such as INSERT, UPDATE, DELETE or ALTER."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Run")
        return alert.runModal() == .alertSecondButtonReturn
    }
}

/// Queries run in one connection window, newest first. Kept in memory; the
/// SQLite history comes with plan step 16.
@MainActor
final class QueryHistory: ObservableObject {
    @Published private(set) var items: [QueryTabModel.HistoryItem] = []
    private let connectionID: String
    private let logger = Logger(subsystem: Bundle.main.bundleIdentifier ?? "tableport", category: "History")

    /// The queries run on one saved connection, kept across launches.
    init(connectionID: UUID) {
        self.connectionID = connectionID.uuidString
        reload()
    }

    func add(sql: String, elapsedMs: UInt64?, failed: Bool) {
        do {
            try LocalData.store?.addHistory(connectionId: connectionID, sql: sql, elapsedMs: elapsedMs, failed: failed)
        } catch {
            logger.error("Could not save the query history: \(error.displayMessage, privacy: .public)")
        }
        reload()
    }

    func clear() {
        do {
            try LocalData.store?.clearHistory(connectionId: connectionID)
        } catch {
            NSAlert(error: error).runModal()
        }
        reload()
    }

    private func reload() {
        let entries = (try? LocalData.store?.history(connectionId: connectionID, search: "", limit: 1000)) ?? []
        items = entries.map {
            .init(id: $0.id, sql: $0.sql, date: Date(timeIntervalSince1970: Double($0.ranAt) / 1000), elapsedMs: $0.elapsedMs, failed: $0.failed)
        }
    }
}

/// The named queries of one saved connection.
@MainActor
final class SavedQueries: ObservableObject {
    @Published private(set) var items: [SavedQuery] = []
    private let connectionID: String

    init(connectionID: UUID) {
        self.connectionID = connectionID.uuidString
        reload()
    }

    /// A new query when `id` is nil; nil when saving failed (the user saw why).
    func save(id: Int64?, name: String, sql: String) -> SavedQuery? {
        do {
            guard let store = LocalData.store else { throw LocalData.Unavailable() }
            let saved = try store.saveQuery(connectionId: connectionID, id: id, name: name, sql: sql)
            reload()
            return saved
        } catch {
            NSAlert(error: error).runModal()
            return nil
        }
    }

    func rename(_ query: SavedQuery, to name: String) {
        do {
            try LocalData.store?.renameQuery(id: query.id, name: name)
        } catch {
            NSAlert(error: error).runModal()
        }
        reload()
    }

    func delete(_ query: SavedQuery) {
        do {
            try LocalData.store?.deleteQuery(id: query.id)
        } catch {
            NSAlert(error: error).runModal()
        }
        reload()
    }

    private func reload() {
        items = (try? LocalData.store?.savedQueries(connectionId: connectionID)) ?? []
    }
}

/// Asks for a saved query's name, prefilled; nil on Cancel.
@MainActor
func askQueryName(title: String, current: String) -> String? {
    let alert = NSAlert()
    alert.messageText = title
    let field = NSTextField(string: current)
    field.frame = NSRect(x: 0, y: 0, width: 260, height: 24)
    field.placeholderString = "Daily orders"
    alert.accessoryView = field
    alert.addButton(withTitle: "Save")
    alert.addButton(withTitle: "Cancel")
    alert.window.initialFirstResponder = field
    guard alert.runModal() == .alertFirstButtonReturn else { return nil }
    let name = field.stringValue.trimmingCharacters(in: .whitespaces)
    return name.isEmpty ? nil : name
}

struct QueryTabView: View {
    @ObservedObject var model: QueryTabModel
    @ObservedObject var history: QueryHistory

    var body: some View {
        VSplitView {
            VStack(spacing: 0) {
                SQLEditor(text: $model.sql, selection: $model.selection, errorRange: model.errorRange)
                editorBar
            }
            .frame(minHeight: 120, idealHeight: 260)
            results
                .frame(minHeight: 120)
        }
        .background(Theme.card)
    }

    /// Under the editor, after TablePlus: what is selected on the left;
    /// history, limit, Beautify and Run on the right.
    private var editorBar: some View {
        HStack(spacing: 8) {
            Text(statusText)
                .font(AppFont.monoFont(size: 11))
                .foregroundStyle(Theme.inkSecondary)
            Spacer()
            if model.isRunning {
                ProgressView().controlSize(.small)
                Button("Cancel", action: model.cancel)
                    .help("Stop the running query (⌘.)")
            }
            Menu {
                if history.items.isEmpty {
                    Text("No queries yet")
                }
                ForEach(history.items.prefix(30)) { item in
                    Button(Self.historyTitle(item)) { model.sql = item.sql }
                }
            } label: {
                Image(systemName: "clock.arrow.circlepath")
            }
            .menuIndicator(.hidden)
            .fixedSize()
            .help("History")
            Picker("Limit", selection: $model.rowLimit) {
                ForEach(QueryTabModel.rowLimits, id: \.self) { limit in
                    Text(limit.map { "Limit \($0.formatted())" } ?? "No limit").tag(limit)
                }
            }
            .labelsHidden()
            .fixedSize()
            .help("Rows kept per result; the full count is always shown")
            Button("Beautify ⌘I", action: model.beautify)
                .help("Format the selection, or all the SQL")
            Menu {
                Button("Run Current  ⌘↩") { model.run(.current) }
                Button("Run All  ⇧⌘↩") { model.run(.all) }
            } label: {
                Text(model.selection.length > 0 ? "Run Selection ⌘↩" : "Run Current ⌘↩")
            } primaryAction: {
                model.run(.current)
            }
            .fixedSize()
            .disabled(model.isRunning)
        }
        .controlSize(.small)
        .padding(.horizontal, 10)
        .frame(height: 34)
        .background(Theme.windowBg)
    }

    /// "2 lines, 120 characters selected", or the cursor's line and column.
    private var statusText: String {
        let text = model.sql as NSString
        let selection = model.selection
        guard NSMaxRange(selection) <= text.length else { return "" }
        if selection.length > 0 {
            let selected = text.substring(with: selection)
            let lines = selected.components(separatedBy: "\n").count
            return "\(lines) \(lines == 1 ? "line" : "lines"), \(selection.length) characters selected"
        }
        let before = text.substring(to: selection.location)
        let line = before.components(separatedBy: "\n").count
        let lastBreak = (before as NSString).range(of: "\n", options: .backwards)
        let lineStart = lastBreak.location == NSNotFound ? 0 : lastBreak.location + 1
        let column = selection.location - lineStart + 1
        return "Ln \(line), Col \(column)"
    }

    /// "Statement 1 OK: UPDATE, 2 rows affected", after TablePlus.
    private static func statementMessage(index: Int, count: UInt64, verb: String?) -> String {
        let rows = "\(count.formatted()) \(count == 1 ? "row" : "rows")"
        let head = "Statement \(index + 1) OK"
        switch verb ?? "" {
        case "INSERT", "UPDATE", "DELETE", "MERGE":
            return "\(head): \(verb ?? ""), \(rows) affected"
        case "", "SELECT", "WITH", "VALUES", "TABLE", "SHOW", "EXPLAIN", "FETCH":
            return "\(head): \(rows)"
        default:
            return "\(head): \(verb ?? "")"
        }
    }

    private static func historyTitle(_ item: QueryTabModel.HistoryItem) -> String {
        let oneLine = item.sql.replacingOccurrences(of: "\\s+", with: " ", options: .regularExpression)
            .trimmingCharacters(in: .whitespaces)
        let time = item.date.formatted(date: .omitted, time: .shortened)
        let outcome = item.failed ? "failed" : item.elapsedMs.map { "\($0) ms" } ?? ""
        return "\(time) · \(outcome) · \(oneLine.prefix(70))"
    }

    @ViewBuilder
    private var results: some View {
        let sets = model.result?.resultSets ?? []
        VStack(spacing: 0) {
            HStack(spacing: 0) {
                ForEach(sets.indices, id: \.self) { index in
                    resultTab("Result \(index + 1)", index: index)
                }
                resultTab(model.error == nil ? "Messages" : "Error", index: sets.count)
                Spacer()
                if let set = sets.indices.contains(model.selectedResult) ? sets[model.selectedResult] : nil {
                    Text(Self.countText(set, elapsedMs: model.result?.elapsedMs))
                        .font(Theme.meta)
                        .foregroundStyle(Theme.inkSecondary)
                        .padding(.trailing, 10)
                }
            }
            .frame(height: 28)
            .background(Theme.sidebarBg)
            Divider()
            if sets.indices.contains(model.selectedResult) {
                let set = sets[model.selectedResult]
                DataGrid.readOnly(columns: set.columns, rows: set.rows)
                    .id(model.selectedResult)
            } else {
                messages
            }
        }
    }

    private func resultTab(_ title: String, index: Int) -> some View {
        Text(title)
            .font(Theme.caption)
            .foregroundStyle(model.selectedResult == index ? Theme.ink : Theme.inkSecondary)
            .padding(.horizontal, 12)
            .frame(height: 28)
            .background(model.selectedResult == index ? Theme.card : Color.clear)
            .contentShape(Rectangle())
            .onTapGesture { model.selectedResult = index }
            .accessibilityAddTraits(model.selectedResult == index ? [.isButton, .isSelected] : .isButton)
            .accessibilityAction(.default) { model.selectedResult = index }
    }

    private var messages: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 6) {
                if let error = model.error {
                    Text(error)
                        .foregroundStyle(Theme.danger)
                        .textSelection(.enabled)
                } else if let result = model.result {
                    ForEach(Array(result.statementCounts.enumerated()), id: \.offset) { index, count in
                        Text(Self.statementMessage(index: index, count: count, verb: result.statementVerbs.indices.contains(index) ? result.statementVerbs[index] : nil))
                    }
                    Text("Finished in \(result.elapsedMs) ms").foregroundStyle(Theme.inkSecondary)
                } else {
                    Text("Press ⌘↩ to run the SQL above.").foregroundStyle(Theme.inkSecondary)
                }
            }
            .font(AppFont.monoFont(size: 12))
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(12)
        }
    }

    private static func countText(_ set: ResultSet, elapsedMs: UInt64?) -> String {
        let shown = set.rows.count
        let total = Int(clamping: set.totalRows)
        let rows = shown < total
            ? "first \(shown.formatted()) of \(total.formatted()) rows"
            : "\(total.formatted()) \(total == 1 ? "row" : "rows")"
        return elapsedMs.map { "\(rows) · \($0) ms" } ?? rows
    }
}
