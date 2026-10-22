import AppKit
import UniformTypeIdentifiers
import TablePortCore
import MacKit
import SwiftUI

/// The Data view of one table tab: columns, the current page, sort and filter.
@MainActor
final class TableTabModel: ObservableObject {
    let relation: Relation
    let session: DatabaseSession
    /// From Settings when the tab opens; changing it later applies to new tabs.
    let pageSize = AppSettings.shared.pageSize

    @Published private(set) var columns: [Column] = []
    @Published private(set) var rows: [[String?]] = []
    @Published private(set) var offset = 0
    @Published private(set) var total: Int?
    /// `total` is Postgres's statistics estimate, not an exact count.
    @Published private(set) var totalIsEstimate = false
    /// From this many rows (by the estimate), the exact count waits for the
    /// Count button: count(*) reads the whole table.
    static let exactCountLimit: UInt64 = 1_000_000
    /// Stores the first page for the window's cache, to show at once when
    /// the table opens again.
    var onCache: ((CachedPage) -> Void)?
    @Published private(set) var sort: RowSort?
    /// The filters the page uses, from Apply or Apply All.
    @Published private(set) var filters: [RowFilter] = []
    /// The rows of the filter bar, as edited; applied ones are in `filters`.
    @Published var filterRows: [FilterRow] = [FilterRow()]
    /// Bumped by ⌘F; the filter bar focuses its first value field.
    @Published private(set) var filterFocusRequest = 0
    @Published private(set) var isLoading = false
    @Published private(set) var error: String?
    /// By column name; single-column keys only.
    @Published private(set) var foreignKeys: [String: ForeignKey] = [:]
    /// Rows from "+ Row", shown after the page until Commit or Discard.
    @Published private(set) var newRows: [NewRow] = [] {
        didSet { changeToken += 1 }
    }
    /// Changed cells of existing rows, by primary key, then column name.
    @Published private(set) var updates: [[String]: [String: CellDraft]] = [:] {
        didSet { changeToken += 1 }
    }
    /// Primary keys of rows marked for delete.
    @Published private(set) var deletes: Set<[String]> = [] {
        didSet { changeToken += 1 }
    }
    /// Bumped on every pending change. The grid reads edits through
    /// closures, which SwiftUI can't compare, so this tells it to redraw.
    private(set) var changeToken = 0
    /// Empty when the table has none; then rows can't be edited or deleted.
    @Published private(set) var primaryKey: [String] = []
    /// Loaded the first time the Structure view opens.
    @Published private(set) var structure: TableStructure?
    @Published private(set) var structureError: String?
    private var structureTask: Task<Void, Never>?
    @Published private(set) var isCommitting = false
    /// Per tab, so one tab's filter row doesn't show in another.
    @Published var showsFilters = false
    /// The row panel on the right (Space), and the page row it shows.
    /// The row panel stays open or closed for every table, as in TablePlus.
    @Published var showsInspector = UserDefaults.standard.bool(forKey: "showsRowPanel") {
        didSet { UserDefaults.standard.set(showsInspector, forKey: "showsRowPanel") }
    }
    @Published var inspectedRow: Int?
    @Published var view: TableView = .data
    /// The selected page rows, for copy and export. Not published: the grid
    /// owns the selection, and redrawing for it is not needed.
    var selectedRows: [Int] = []
    @Published var isExportSheetShown = false
    @Published private(set) var isExporting = false

    enum TableView: String, CaseIterable {
        case data = "Data"
        case structure = "Structure"
    }

    let isProduction: Bool
    let isReadOnly: Bool
    @Published var isReviewing = false
    private var pageTask: Task<Void, Never>?
    private var countTask: Task<Void, Never>?
    private var commitTask: Task<Void, Never>?

    init(relation: Relation, session: DatabaseSession, filter: RowFilter? = nil, cached: CachedPage? = nil,
         isProduction: Bool, isReadOnly: Bool) {
        if let filter {
            filters = [filter]
            filterRows = [FilterRow(filter)]
        }
        self.relation = relation
        self.session = session
        self.showsFilters = filter != nil
        self.isProduction = isProduction
        self.isReadOnly = isReadOnly
        // The last first page shows at once; fresh rows replace it below.
        if let cached, filter == nil {
            columns = cached.columns
            primaryKey = cached.primaryKey
            rows = cached.rows
            total = cached.total
            totalIsEstimate = cached.totalIsEstimate
            foreignKeys = cached.foreignKeys
        }
        loadPage()
        loadCount(keepingShownTotal: cached != nil && filter == nil)
        loadForeignKeys()
    }

    // MARK: Copy and export

    /// Copy as CSV or SQL. The grid cuts long values, so rows with a cut
    /// value are read again whole by their primary key first.
    func copy(rows indices: [Int], as format: GridCopyFormat) {
        let indices = indices.filter { rows.indices.contains($0) }.sorted()
        guard !indices.isEmpty else { return }
        let shown = indices.map { rows[$0] }
        let names = columns.map(\.name)
        let (relation, session, columns, primaryKey) = (relation, session, columns, primaryKey)
        let isCut = shown.contains { $0.contains { ($0?.count ?? 0) >= Grid.cellLimit } }
        let keys = indices.compactMap(key(ofRow:))
        Task {
            var values = shown
            var note: String?
            if isCut {
                if !primaryKey.isEmpty && keys.count == indices.count {
                    do {
                        values = try await session.fullRows(relation: relation, columns: columns, primaryKey: primaryKey, keys: keys)
                    } catch {
                        NSAlert(error: error).runModal()
                        return
                    }
                } else {
                    note = "Some values are longer than \(Grid.cellLimit.formatted()) characters. They were copied shortened, because this table has no primary key to read them again."
                }
            }
            let text: String
            switch format {
            case .text: text = values.map { $0.map { $0 ?? "" }.joined(separator: "\t") }.joined(separator: "\n")
            case .csv: text = rowsToCsv(columns: names, rows: values, header: true)
            case .sql:
                do {
                    text = try rowsToInsertSql(relation: relation, columns: names, rows: values)
                } catch {
                    NSAlert(error: error).runModal()
                    return
                }
            }
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
            if let note {
                let alert = NSAlert()
                alert.messageText = "Copied with shortened values"
                alert.informativeText = note
                alert.runModal()
            }
        }
    }

    /// One cell read whole by the row's primary key; the page holds long
    /// values shortened.
    func fullValue(row: Int, column: Int) async throws -> String? {
        guard let key = key(ofRow: row), columns.indices.contains(column) else { return nil }
        let rows = try await session.fullRows(relation: relation, columns: [columns[column]], primaryKey: primaryKey, keys: [key])
        guard let first = rows.first, let value = first.first else {
            throw CocoaError(.fileReadNoSuchFile, userInfo: [NSLocalizedDescriptionKey: "The row no longer exists."])
        }
        return value
    }

    /// Writes the chosen rows to a file and tells the user how many.
    func export(scope: ExportScope, format: ExportFormat, to url: URL) {
        guard !isExporting else { return }
        isExporting = true
        let (relation, session, columns, filters, sort, primaryKey) = (relation, session, columns, filters, sort, primaryKey)
        Task {
            defer { isExporting = false }
            do {
                let count = try await session.exportRows(
                    relation: relation, columns: columns, filters: filters, sort: sort, primaryKey: primaryKey,
                    scope: scope, format: format, path: url.path
                )
                let alert = NSAlert()
                alert.messageText = "Exported \(count.formatted()) \(count == 1 ? "row" : "rows")"
                alert.informativeText = url.lastPathComponent
                alert.addButton(withTitle: "Show in Finder")
                alert.addButton(withTitle: "OK")
                if alert.runModal() == .alertFirstButtonReturn {
                    NSWorkspace.shared.activateFileViewerSelecting([url])
                }
            } catch {
                NSAlert(error: error).runModal()
            }
        }
    }

    /// Hands the first page to the window's cache. Only the plain first
    /// page is kept: it is what opening the table shows.
    private func cacheFirstPage() {
        guard offset == 0, filters.isEmpty, sort == nil, !columns.isEmpty, error == nil else { return }
        onCache?(CachedPage(
            columns: columns, primaryKey: primaryKey, rows: rows,
            total: total, totalIsEstimate: totalIsEstimate, foreignKeys: foreignKeys
        ))
    }

    /// Views can't always take inserts; tables can.
    var canAddRows: Bool { !isReadOnly && [.table, .partitionedTable, .foreignTable].contains(relation.kind) }

    func addRow() {
        newRows.append(NewRow(values: [:]))
    }

    /// nil puts the column back to its DEFAULT.
    func setDraft(_ draft: CellDraft?, row: Int, column: Int) {
        guard newRows.indices.contains(row), columns.indices.contains(column) else { return }
        // An explicit DEFAULT is the same as no draft for a new row.
        newRows[row].values[columns[column].name] = draft == .default ? nil : draft
    }

    func removeNewRow(_ row: Int) {
        guard newRows.indices.contains(row) else { return }
        newRows.remove(at: row)
    }

    /// Existing rows can change only with a primary key to find them by.
    var canEditRows: Bool { canAddRows && !primaryKey.isEmpty }

    var changeCount: Int { newRows.count + updates.count + deletes.count }

    /// "2 edited · 1 deleted · 1 new".
    var changeSummary: String {
        [
            updates.isEmpty ? nil : "\(updates.count) edited",
            deletes.isEmpty ? nil : "\(deletes.count) deleted",
            newRows.isEmpty ? nil : "\(newRows.count) new",
        ]
        .compactMap { $0 }
        .joined(separator: " · ")
    }

    /// The primary key of a row on this page, or nil when it can't be used:
    /// no key, a NULL, or a value cut for the grid.
    func key(ofRow row: Int) -> [String]? {
        guard canEditRows, rows.indices.contains(row) else { return nil }
        var key: [String] = []
        for name in primaryKey {
            guard let index = columns.firstIndex(where: { $0.name == name }),
                  rows[row].indices.contains(index), let value = rows[row][index],
                  value.count < Grid.cellLimit else { return nil }
            key.append(value)
        }
        return key
    }

    /// Changed cells of a page row, by column index.
    func edits(ofRow row: Int) -> [Int: CellDraft] {
        guard let key = key(ofRow: row), let values = updates[key] else { return [:] }
        var edits: [Int: CellDraft] = [:]
        for (index, column) in columns.enumerated() {
            if let draft = values[column.name] { edits[index] = draft }
        }
        return edits
    }

    func isDeleted(row: Int) -> Bool {
        key(ofRow: row).map(deletes.contains) ?? false
    }

    /// nil reverts the cell. A draft equal to the stored value reverts too.
    func setCell(_ draft: CellDraft?, row: Int, column: Int) {
        guard let key = key(ofRow: row), columns.indices.contains(column) else { return }
        let name = columns[column].name
        let stored: CellDraft = rows[row][column].map { .value(text: $0) } ?? .null
        var values = updates[key] ?? [:]
        values[name] = draft == stored ? nil : draft
        updates[key] = values.isEmpty ? nil : values
    }

    /// Marks rows for delete, or restores them when all are marked already.
    func toggleDelete(rows indices: [Int]) {
        let keys = indices.compactMap(key(ofRow:))
        guard !keys.isEmpty else { return }
        if keys.allSatisfy(deletes.contains) {
            deletes.subtract(keys)
        } else {
            deletes.formUnion(keys)
        }
    }

    /// Applies a change from the Structure view, then reloads the structure
    /// and the page: columns may have been added, renamed or dropped.
    func applyStructure(_ change: StructureChange) async throws {
        try await session.applyStructureChange(relation: relation, change: change)
        structureTask?.cancel()
        structureTask = nil
        structure = nil
        loadStructure()
        // Read the columns and the key again with the next page.
        columns = []
        primaryKey = []
        reload()
    }

    func loadStructure() {
        guard structure == nil, structureTask == nil else { return }
        structureError = nil
        let (session, relation) = (session, relation)
        structureTask = Task {
            do {
                structure = try await session.structure(relation: relation)
            } catch {
                structureError = error.displayMessage
            }
            structureTask = nil
        }
    }

    func discardChanges() {
        newRows = []
        updates = [:]
        deletes = []
    }

    private var pendingChanges: PendingChanges {
        PendingChanges(
            inserts: newRows,
            updates: updates.map { RowUpdate(key: $0.key, values: $0.value) },
            deletes: Array(deletes)
        )
    }

    /// The statements Commit will run, for the review sheet.
    func reviewStatements() -> Result<[String], Error> {
        Result { try changesSql(changes: pendingChanges, relation: relation, columns: columns, primaryKey: primaryKey,
                              engine: session.engine()) }
    }

    /// Applies all changes in one transaction. Production connections ask first.
    func commit() {
        guard changeCount > 0, !isCommitting else { return }
        if isProduction && AppSettings.shared.confirmProductionWrites {
            let alert = NSAlert()
            alert.messageText = "Commit \(changeSummary) in \(relation.name)?"
            alert.informativeText = "This is a production connection."
            alert.alertStyle = .warning
            alert.addButton(withTitle: "Cancel")
            alert.addButton(withTitle: "Commit")
            guard alert.runModal() == .alertSecondButtonReturn else { return }
        }
        isCommitting = true
        let changes = pendingChanges
        isReviewing = false
        let (session, relation, columns, primaryKey) = (session, relation, columns, primaryKey)
        commitTask = Task {
            do {
                try await session.commit(changes: changes, relation: relation, columns: columns, primaryKey: primaryKey)
                discardChanges()
                reload()
            } catch {
                let alert = NSAlert()
                alert.messageText = "Nothing was saved"
                alert.informativeText = error.displayMessage
                alert.alertStyle = .warning
                alert.runModal()
            }
            isCommitting = false
        }
    }

    private func loadForeignKeys() {
        let (relation, session) = (relation, session)
        Task {
            // Without keys the cells simply have no arrows.
            if let keys = try? await session.foreignKeys(relation: relation) {
                foreignKeys = Dictionary(keys.map { ($0.column, $0) }, uniquingKeysWith: { first, _ in first })
                cacheFirstPage()
            }
        }
    }

    deinit {
        pageTask?.cancel()
        countTask?.cancel()
    }

    // commitTask is not cancelled with the tab: a started COMMIT must finish.

    var hasPrevious: Bool { offset > 0 }
    var hasNext: Bool {
        // An estimate can be low or high; a full page says there may be more.
        if let total, !totalIsEstimate { return offset + pageSize < total }
        return rows.count == pageSize
    }

    func reload() {
        loadPage()
        loadCount()
    }

    func nextPage() {
        guard hasNext else { return }
        offset += pageSize
        loadPage()
    }

    func previousPage() {
        guard hasPrevious else { return }
        offset = max(0, offset - pageSize)
        loadPage()
    }

    func setSort(_ sort: RowSort?) {
        self.sort = sort
        offset = 0
        loadPage()
    }

    /// A filter from outside, such as following a foreign key: it replaces
    /// the bar's rows.
    func apply(_ filter: RowFilter) {
        filterRows = [FilterRow(filter)]
        use([filter])
        showsFilters = true
    }

    /// Apply: this row only, as in TablePlus.
    func applyRow(_ id: FilterRow.ID) {
        guard let row = filterRows.first(where: { $0.id == id }) else { return }
        use(row.filter.map { [$0] } ?? [])
    }

    /// Apply All: every row that is switched on.
    func applyAllFilters() {
        use(filterRows.filter(\.isOn).compactMap(\.filter))
    }

    func clearFilters() {
        filterRows = [FilterRow()]
        use([])
    }

    func addFilterRow(after id: FilterRow.ID) {
        let index = filterRows.firstIndex { $0.id == id }.map { $0 + 1 } ?? filterRows.endIndex
        filterRows.insert(FilterRow(), at: index)
    }

    func removeFilterRow(_ id: FilterRow.ID) {
        filterRows.removeAll { $0.id == id }
        if filterRows.isEmpty { clearFilters() }
    }

    /// ⌘F: show the bar on the Data view and put the cursor in it.
    func focusFilters() {
        view = .data
        showsFilters = true
        filterFocusRequest += 1
    }

    private func use(_ filters: [RowFilter]) {
        self.filters = filters
        offset = 0
        reload()
    }

    private func loadPage() {
        pageTask?.cancel()
        isLoading = true
        error = nil
        let (relation, session, filters, sort, offset, limit) = (relation, session, filters, sort, offset, pageSize)
        let (knownColumns, knownKey) = (columns, primaryKey)
        pageTask = Task {
            do {
                // The first page also reads the primary key: unsorted pages
                // are ordered by it, and editing needs it.
                var (columns, primaryKey) = (knownColumns, knownKey)
                if columns.isEmpty {
                    async let loadedColumns = session.columns(relation: relation)
                    async let loadedKey = session.primaryKey(relation: relation)
                    (columns, primaryKey) = try await (loadedColumns, loadedKey)
                }
                let page = try await session.rows(
                    relation: relation, columns: columns, filters: filters, sort: sort, primaryKey: primaryKey,
                    offset: UInt64(offset), limit: UInt64(limit)
                )
                guard !Task.isCancelled else { return }
                self.columns = columns
                self.primaryKey = primaryKey
                self.rows = page.rows
                self.isLoading = false
                self.cacheFirstPage()
            } catch {
                guard !Task.isCancelled else { return }
                self.rows = []
                self.error = error.displayMessage
            }
            self.isLoading = false
        }
    }

    /// Without filters the statistics estimate comes first; it is instant
    /// on any size. Below `exactCountLimit` the exact count follows; above
    /// it, the Count button asks for it. Filtered pages always count.
    private func loadCount(keepingShownTotal: Bool = false) {
        countTask?.cancel()
        if !keepingShownTotal {
            total = nil
            totalIsEstimate = false
        }
        let (relation, session, filters, columns) = (relation, session, filters, columns)
        countTask = Task {
            if filters.isEmpty, let estimate = try? await session.estimatedRowCount(relation: relation) {
                guard !Task.isCancelled else { return }
                self.total = Int(clamping: estimate)
                self.totalIsEstimate = true
                self.cacheFirstPage()
                if estimate >= Self.exactCountLimit { return }
            }
            await self.countExactly(relation: relation, session: session, filters: filters, columns: columns)
        }
    }

    /// The Count button: the exact number, however long count(*) takes.
    func countRows() {
        countTask?.cancel()
        let (relation, session, filters, columns) = (relation, session, filters, columns)
        countTask = Task { await countExactly(relation: relation, session: session, filters: filters, columns: columns) }
    }

    @Published private(set) var isCounting = false

    private func countExactly(relation: Relation, session: DatabaseSession, filters: [RowFilter], columns: [Column]) async {
        isCounting = true
        defer { if !Task.isCancelled { isCounting = false } }
        // A failed count leaves the total as it was; the page shows the error.
        guard let count = try? await session.rowCount(relation: relation, columns: columns, filters: filters),
              !Task.isCancelled else { return }
        total = Int(clamping: count)
        totalIsEstimate = false
        cacheFirstPage()
    }
}

struct TableTabView: View {
    @ObservedObject var model: TableTabModel
    /// Opens the referenced table filtered to the clicked value.
    let onFollow: (ForeignKey, String) -> Void
    /// The row panel's width, the same for every table.
    @AppStorage("rowPanelWidth") private var inspectorWidth = 340.0

    var body: some View {
        VStack(spacing: 0) {
            switch model.view {
            case .data:
                if model.showsFilters {
                    FilterBar(model: model)
                        .id(model.relation.id)
                    Divider()
                }
                HStack(spacing: 0) {
                    grid
                    if model.showsInspector {
                        PanelResizeHandle(width: $inspectorWidth)
                        RowInspector(model: model, onFollow: onFollow)
                            .id(model.relation.id)
                            .frame(width: inspectorWidth)
                    }
                }
            case .structure:
                StructureView(model: model)
                    .id(model.relation.id)
            }
            Divider()
            bottomBar
        }
        .sheet(isPresented: $model.isReviewing) {
            ReviewChangesSheet(model: model)
        }
        .sheet(isPresented: $model.isExportSheetShown) {
            ExportSheet(model: model)
        }
    }

    private var grid: some View {
        ZStack {
            DataGrid(
                columns: model.columns,
                rows: model.rows,
                offset: model.offset,
                sort: model.sort,
                foreignKeys: model.foreignKeys,
                newRows: model.newRows,
                isEditable: model.canEditRows,
                changeToken: model.changeToken,
                edits: model.edits(ofRow:),
                isDeleted: model.isDeleted(row:),
                onSort: model.setSort,
                onFollow: onFollow,
                onEditNewRow: model.setDraft,
                onRemoveNewRow: model.removeNewRow,
                onEditCell: model.setCell,
                onToggleDelete: model.toggleDelete,
                onSelectRow: { model.inspectedRow = $0 },
                onSpace: { model.showsInspector.toggle() },
                identity: model.relation.id,
                onCopyAs: { format, rows in model.copy(rows: rows, as: format) },
                onExport: { selected in
                    model.selectedRows = selected
                    model.isExportSheetShown = true
                },
                onSelectRows: { model.selectedRows = $0 },
                fullValue: model.primaryKey.isEmpty ? nil : { row, column in try await model.fullValue(row: row, column: column) }
            )
            if let error = model.error {
                VStack(spacing: 8) {
                    Image(systemName: "exclamationmark.triangle").font(.system(size: 22))
                    Text(error).multilineTextAlignment(.center).textSelection(.enabled)
                    Button("Retry") { model.reload() }
                }
                .font(Theme.caption)
                .foregroundStyle(Theme.danger)
                .padding(24)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .background(Theme.card)
            } else if !model.isLoading && model.rows.isEmpty && !model.columns.isEmpty {
                Text(model.filters.isEmpty ? "No rows" : "No rows match the filters")
                    .font(Theme.caption)
                    .foregroundStyle(Theme.inkSecondary)
            }
        }
    }

    /// Data | Structure and + Row on the left, the row range in the middle,
    /// pending changes, Filters and paging on the right, as in TablePlus.
    private var bottomBar: some View {
        HStack(spacing: 8) {
            HStack(spacing: 8) {
                Picker("View", selection: $model.view) {
                    ForEach(TableTabModel.TableView.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()
                Button {
                    model.view = .data
                    model.addRow()
                } label: {
                    Label("Row", systemImage: "plus")
                }
                .disabled(!model.canAddRows || model.columns.isEmpty)
                .help("Add a row (pending until Commit)")
                .opacity(model.view == .data ? 1 : 0)
            }
            .frame(maxWidth: .infinity, alignment: .leading)

            HStack(spacing: 6) {
                if model.isLoading { ProgressView().controlSize(.mini) }
                Text(rangeText).lineLimit(1)
                if model.totalIsEstimate && !model.isCounting {
                    Button("Count") { model.countRows() }
                        .help("Count the rows exactly. On a large table this reads every row.")
                } else if model.isCounting && model.totalIsEstimate {
                    ProgressView().controlSize(.mini)
                }
            }
            // The Structure view has no rows to count or page.
            .opacity(model.view == .data ? 1 : 0)
            .font(Theme.caption)
            .foregroundStyle(Theme.inkSecondary)
            .fixedSize()

            HStack(spacing: 8) {
                if model.changeCount > 0 {
                    Text(model.changeSummary)
                        .font(Theme.caption)
                        .foregroundStyle(Theme.successInk)
                        .lineLimit(1)
                    Button("Review") { model.isReviewing = true }
                        .help("Show the SQL that Commit will run")
                    Button("Discard", action: model.discardChanges)
                        .disabled(model.isCommitting)
                        .help("Drop all changes")
                    Button(model.isCommitting ? "Committing…" : "Commit", action: model.commit)
                        .buttonStyle(.borderedProminent)
                        .disabled(model.isCommitting)
                        .help("Save all changes (⌘S)")
                }
                if model.view == .data {
                    Toggle(isOn: $model.showsInspector) {
                        Image(systemName: "sidebar.right")
                    }
                    .toggleStyle(.button)
                    .help("Row panel (Space)")
                    Toggle(isOn: $model.showsFilters) {
                        Label(model.filters.isEmpty ? "Filters" : "Filters (\(model.filters.count))", systemImage: "line.3.horizontal.decrease")
                    }
                    .toggleStyle(.button)
                    Button {
                        model.previousPage()
                    } label: {
                        Image(systemName: "chevron.left")
                    }
                    .disabled(!model.hasPrevious || model.isLoading)
                    .help("Previous page")
                    Button {
                        model.nextPage()
                    } label: {
                        Image(systemName: "chevron.right")
                    }
                    .disabled(!model.hasNext || model.isLoading)
                    .help("Next page")
                }
            }
            .frame(maxWidth: .infinity, alignment: .trailing)
        }
        .controlSize(.small)
        .padding(.horizontal, 10)
        .frame(height: 34)
        .background(Theme.windowBg)
    }

    private var rangeText: String {
        guard !model.rows.isEmpty else { return model.total == nil ? "Counting rows…" : "0 rows" }
        let first = model.offset + 1
        let last = model.offset + model.rows.count
        let total = model.total.map { "\(model.totalIsEstimate ? "~" : "")\($0.formatted()) rows" } ?? "… rows"
        return "\(first.formatted())–\(last.formatted()) of \(total)"
    }
}

/// One row of the filter bar while it is edited.
struct FilterRow: Identifiable, Equatable {
    enum Target: Hashable {
        case column(String)
        case anyColumn
        case rawSQL
    }

    let id = UUID()
    var isOn = true
    /// A new row searches every column: ⌘F, then type.
    var target: Target = .anyColumn
    var op: FilterOperator = .contains
    var value = ""

    init() {}

    init(_ filter: RowFilter) {
        switch filter.target {
        case .column(let name): target = .column(name)
        case .anyColumn: target = .anyColumn
        case .rawSql: target = .rawSQL
        }
        op = filter.operator
        value = filter.value
    }

    /// nil while there is nothing to filter by yet.
    var filter: RowFilter? {
        let trimmed = value.trimmingCharacters(in: .whitespaces)
        switch target {
        case .rawSQL:
            return trimmed.isEmpty ? nil : RowFilter(target: .rawSql, operator: .equals, value: value)
        case .column, .anyColumn:
            if op.needsValue && trimmed.isEmpty { return nil }
            let coreTarget: FilterTarget = if case .column(let name) = target { .column(name: name) } else { .anyColumn }
            return RowFilter(target: coreTarget, operator: op, value: value)
        }
    }
}

/// The filter rows above the grid, after TablePlus: a column (or any column,
/// or raw SQL), an operator and a value per row; Apply uses that row, Apply
/// All every row that is on.
private struct FilterBar: View {
    @ObservedObject var model: TableTabModel
    @FocusState private var focusedRow: FilterRow.ID?

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach($model.filterRows) { $row in
                rowView($row)
            }
            HStack(spacing: 10) {
                Text("Show: ⌘F    Apply: ↩    Apply All: ⌘↩    Exit: Esc")
                    .font(Theme.meta)
                    .foregroundStyle(Theme.inkSecondary)
                Spacer()
                Button("Clear", action: model.clearFilters)
                Button("Apply All", action: model.applyAllFilters)
                    .keyboardShortcut(.return, modifiers: .command)
            }
        }
        .controlSize(.small)
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
        .onExitCommand { model.showsFilters = false }
        .onChange(of: model.filterFocusRequest, initial: true) { _, request in
            if request > 0 { focusedRow = model.filterRows.first?.id }
        }
    }

    @ViewBuilder
    private func rowView(_ row: Binding<FilterRow>) -> some View {
        let id = row.wrappedValue.id
        HStack(spacing: 6) {
            Toggle("On", isOn: row.isOn)
                .toggleStyle(.checkbox)
                .labelsHidden()
                .fixedSize()
                .help("Use this filter in Apply All")
            Picker("Column", selection: row.target) {
                ForEach(model.columns, id: \.name) { column in
                    Text(column.name).tag(FilterRow.Target.column(column.name))
                }
                Divider()
                Text("Any column").tag(FilterRow.Target.anyColumn)
                Text("Raw SQL").tag(FilterRow.Target.rawSQL)
            }
            .labelsHidden()
            // Sized to the menu: a wider frame left uneven gaps around it.
            .fixedSize()
            if row.wrappedValue.target != .rawSQL {
                Picker("Operator", selection: row.op) {
                    ForEach(FilterOperator.allCases, id: \.self) { op in
                        Text(op.label).tag(op)
                    }
                }
                .labelsHidden()
                .fixedSize()
            }
            TextField(placeholder(row.wrappedValue), text: row.value)
                .searchInput()
                .disabled(row.wrappedValue.target != .rawSQL && !row.wrappedValue.op.needsValue)
                .focused($focusedRow, equals: id)
                .onSubmit { model.applyRow(id) }
            Button("Apply") { model.applyRow(id) }
            Button {
                model.removeFilterRow(id)
            } label: {
                Image(systemName: "minus")
            }
            .help("Remove this filter")
            Button {
                model.addFilterRow(after: id)
            } label: {
                Image(systemName: "plus")
            }
            .help("Add a filter")
        }
    }

    private func placeholder(_ row: FilterRow) -> String {
        switch row.target {
        case .rawSQL: "WHERE condition, e.g. created_at > now() - interval '1 day'"
        case .column, .anyColumn: row.op == .in || row.op == .notIn ? "Values, separated by commas" : "Value"
        }
    }
}

/// The SQL that Commit will run, before it runs.
struct ReviewChangesSheet: View {
    @ObservedObject var model: TableTabModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Review changes · \(model.changeSummary)")
                .font(.system(size: 15, weight: .semibold))
            switch model.reviewStatements() {
            case .success(let statements):
                let sql = statements.map { $0 + ";" }.joined(separator: "\n")
                ScrollView {
                    Text(SQLSyntax.highlighted(sql))
                        .font(AppFont.monoFont(size: 12))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(10)
                }
                .background(RoundedRectangle(cornerRadius: 6).fill(Theme.windowBg))
                Text("Commit runs these together: all of them are saved, or none.")
                    .font(Theme.caption)
                    .foregroundStyle(Theme.inkSecondary)
                HStack {
                    Button("Copy SQL") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(sql, forType: .string)
                    }
                    Button("Discard All") {
                        model.discardChanges()
                        dismiss()
                    }
                    Spacer()
                    Button("Cancel") { dismiss() }
                        .keyboardShortcut(.cancelAction)
                    Button("Commit") { model.commit() }
                        .keyboardShortcut(.defaultAction)
                        .disabled(model.isCommitting)
                }
            case .failure(let error):
                Text(error.displayMessage).foregroundStyle(Theme.danger)
                HStack {
                    Spacer()
                    Button("Close") { dismiss() }.keyboardShortcut(.cancelAction)
                }
            }
        }
        .padding(20)
        .frame(width: 620, height: 380)
    }
}

/// The selected row as a list of fields, after TablePlus' row panel: column
/// name and type over each value, a search field, and editing that goes into
/// the tab's pending changes.
/// The row panel (Space): the selected row as a form, as in TablePlus. Each
/// field edits as a pending change, like a cell.
private struct RowInspector: View {
    @ObservedObject var model: TableTabModel
    let onFollow: (ForeignKey, String) -> Void
    @State private var search = ""

    var body: some View {
        VStack(spacing: 0) {
            TextField("Search for field…", text: $search)
                .searchInput()
                .padding(8)
            Divider()
            if let row = model.inspectedRow, model.rows.indices.contains(row) {
                header(row)
                Divider()
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 12) {
                        ForEach(Array(model.columns.enumerated()), id: \.element.name) { index, column in
                            if search.isEmpty || column.name.localizedCaseInsensitiveContains(search) {
                                InspectorField(model: model, row: row, column: index, onFollow: onFollow)
                            }
                        }
                    }
                    .padding(10)
                }
            } else {
                Text("Select a row")
                    .font(Theme.caption)
                    .foregroundStyle(Theme.inkSecondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .background(Theme.card)
    }

    /// Which row this is, and why it can't be edited when it can't.
    private func header(_ row: Int) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Row \((model.offset + row + 1).formatted())")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Theme.ink)
            if let key = model.key(ofRow: row) {
                Text(zip(model.primaryKey, key).map { "\($0) = \($1)" }.joined(separator: ", "))
                    .font(AppFont.monoFont(size: 11))
                    .foregroundStyle(Theme.inkSecondary)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            if let reason = readOnlyReason(row) {
                Label(reason, systemImage: "lock")
                    .font(Theme.meta)
                    .foregroundStyle(Theme.inkSecondary)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.horizontal, 10)
        .padding(.vertical, 8)
    }

    private func readOnlyReason(_ row: Int) -> String? {
        if model.isReadOnly { return "Read-only connection" }
        if !model.canAddRows { return "Views can't be edited" }
        if model.primaryKey.isEmpty { return "Read-only: the table has no primary key" }
        if model.isDeleted(row: row) { return "Marked for delete" }
        return nil
    }
}

/// One field of the row panel: the column's name and type, a control that
/// fits its kind (a picker for booleans and enums, a tall editor for JSON
/// and long text), and an arrow that follows a foreign key.
private struct InspectorField: View {
    @ObservedObject var model: TableTabModel
    let row: Int
    let column: Int
    let onFollow: (ForeignKey, String) -> Void
    @State private var text = ""
    /// The whole value, read from the server, when the page holds it cut.
    @State private var fullText: String?
    @State private var isLoadingFull = false
    @State private var loadError: String?
    @FocusState private var focused: Bool

    private var info: Column { model.columns[column] }
    private var stored: String? { model.rows[row].indices.contains(column) ? model.rows[row][column] : nil }
    private var edit: CellDraft? { model.edits(ofRow: row)[column] }
    private var shown: CellDraft { edit ?? (fullText ?? stored).map { .value(text: $0) } ?? .null }
    private var isCut: Bool { (stored?.count ?? 0) >= Grid.cellLimit && fullText == nil }
    private var isJSON: Bool { info.columnType.hasPrefix("json") }
    /// Cut values would be saved cut, so they stay read-only until loaded whole.
    private var editable: Bool {
        model.canEditRows && !model.isDeleted(row: row) && !isCut
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(spacing: 4) {
                Text(info.name)
                    .font(.system(size: 11, weight: .medium))
                    .foregroundStyle(Theme.ink)
                if let key = model.foreignKeys[info.name], let value = stored {
                    Button {
                        onFollow(key, value)
                    } label: {
                        Image(systemName: "arrow.right.circle")
                    }
                    .buttonStyle(.borderless)
                    .help("Open \(key.referencedRelation.name) where \(key.referencedColumn) = \(value)")
                }
                Spacer()
                Text(info.columnType)
                    .font(Theme.meta)
                    .foregroundStyle(Theme.inkSecondary)
                    .lineLimit(1)
            }
            control
                .background(edit == nil ? Color.clear : Color.yellow.opacity(0.25))
                .contextMenu { menu }
            if isCut {
                HStack(spacing: 6) {
                    Text("Shortened in the grid")
                    Button(isLoadingFull ? "Loading…" : "Load full value", action: loadFull)
                        .buttonStyle(.link)
                        .disabled(isLoadingFull || model.primaryKey.isEmpty)
                }
                .font(Theme.meta)
                .foregroundStyle(Theme.inkSecondary)
            }
            if let loadError {
                Text(loadError).font(Theme.meta).foregroundStyle(Theme.danger)
            }
        }
        .onAppear(perform: load)
        .onChange(of: row) { _, _ in
            fullText = nil
            loadError = nil
            load()
        }
        .onChange(of: model.changeToken) { _, _ in if !focused { load() } }
        .onChange(of: focused) { _, isFocused in if !isFocused { save() } }
    }

    @ViewBuilder
    private var control: some View {
        switch info.kind {
        case .boolean:
            choice(["true", "false"])
        case .enum:
            choice(info.enumValues)
        default:
            TextField(placeholder, text: $text, axis: .vertical)
                .textFieldStyle(.roundedBorder)
                .font(AppFont.monoFont(size: 12))
                // JSON and long text get room; ⌥↩ adds a line, ↩ saves.
                .lineLimit(isJSON || text.count > 120 ? 4...18 : 1...6)
                .disabled(!editable)
                .focused($focused)
                .onSubmit(save)
        }
    }

    /// A pop-up of the allowed values, plus NULL.
    private func choice(_ values: [String]) -> some View {
        Picker(info.name, selection: Binding(
            get: { GridCellPaint.text(for: shown) ?? "NULL" },
            set: { value in
                model.setCell(value == "NULL" ? .null : .value(text: value), row: row, column: column)
            }
        )) {
            ForEach(values, id: \.self) { Text($0).tag($0) }
            Divider()
            Text("NULL").tag("NULL")
        }
        .labelsHidden()
        .disabled(!editable)
        .frame(maxWidth: .infinity, alignment: .leading)
    }

    @ViewBuilder
    private var menu: some View {
        Button("Copy Value") { copy(GridCellPaint.text(for: shown) ?? "") }
            .disabled(isCut)
        Button("Copy Name") { copy(info.name) }
        if editable {
            Divider()
            Button("Set NULL") { model.setCell(.null, row: row, column: column) }
            if edit != nil { Button("Revert") { model.setCell(nil, row: row, column: column) } }
        }
    }

    private var placeholder: String {
        if case .null = shown { return "NULL" }
        if case .default = shown { return "DEFAULT" }
        return ""
    }

    /// JSON shows indented; saving compares with what was shown, so only a
    /// real change becomes an edit.
    private func displayText() -> String {
        let value = GridCellPaint.text(for: shown) ?? ""
        if isJSON, edit == nil, let pretty = formatJson(text: value) { return pretty }
        return value
    }

    private func load() {
        text = displayText()
    }

    /// Typing over NULL or DEFAULT with nothing keeps them; any other change
    /// becomes a pending edit.
    private func save() {
        guard editable, text != displayText() else { return }
        model.setCell(.value(text: text), row: row, column: column)
    }

    private func loadFull() {
        isLoadingFull = true
        loadError = nil
        Task {
            do {
                fullText = try await model.fullValue(row: row, column: column)
                load()
            } catch {
                loadError = error.displayMessage
            }
            isLoadingFull = false
        }
    }

    private func copy(_ value: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(value, forType: .string)
    }
}

/// The edge between the grid and the row panel: drag to resize it.
private struct PanelResizeHandle: View {
    @Binding var width: Double
    @State private var startWidth: Double?

    var body: some View {
        Rectangle()
            .fill(Theme.hairline)
            .frame(width: 1)
            .overlay {
                // A wider, invisible grip around the 1 pt line.
                Color.clear
                    .frame(width: 8)
                    .contentShape(Rectangle())
                    .onHover { inside in
                        if inside { NSCursor.resizeLeftRight.push() } else { NSCursor.pop() }
                    }
                    .gesture(DragGesture(minimumDistance: 1, coordinateSpace: .global)
                        .onChanged { drag in
                            let start = startWidth ?? width
                            startWidth = start
                            // Dragging left makes the panel wider.
                            width = min(max(start - drag.translation.width, 240), 720)
                        }
                        .onEnded { _ in startWidth = nil })
            }
    }
}

/// The warning before closing or quitting would drop uncommitted edits, as
/// in TablePlus. Cancel is the default button, so Return keeps the edits.
@MainActor
enum DiscardPrompt {
    static func confirm(changeCount: Int) -> Bool {
        guard changeCount > 0 else { return true }
        let alert = NSAlert()
        alert.messageText = "Discard all changes?"
        alert.informativeText = "\(changeCount) uncommitted \(changeCount == 1 ? "change" : "changes") will be lost.\n"
            + "Tip: you can commit changes with ⌘S."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Discard").hasDestructiveAction = true
        return alert.runModal() == .alertSecondButtonReturn
    }
}

/// A table's first page as last seen, kept by the window so the table shows
/// at once when it opens again.
struct CachedPage {
    let columns: [Column]
    let primaryKey: [String]
    let rows: [[String?]]
    let total: Int?
    let totalIsEstimate: Bool
    let foreignKeys: [String: ForeignKey]
}

/// Export…: the format and which rows, then the save panel. Rows go from the
/// server to the file; the grid's shortened values are not used.
private struct ExportSheet: View {
    @ObservedObject var model: TableTabModel
    @Environment(\.dismiss) private var dismiss
    @State private var format = ExportFormat.csv
    @State private var rows = Rows.all

    enum Rows: Hashable {
        case all, page, selected
    }

    private var allLabel: String {
        let filtered = model.filters.isEmpty ? "All rows" : "All rows that match the filters"
        guard let total = model.total else { return filtered }
        return "\(filtered) (\(model.totalIsEstimate ? "~" : "")\(total.formatted()))"
    }

    private var formatNote: String {
        switch format {
        case .csv: "CSV with a header row. NULL is an empty field."
        case .json: "A JSON array of objects. Numbers, booleans and JSON values keep their types."
        case .sql: "One INSERT statement per row, to load into another database."
        }
    }

    private var canExportSelected: Bool {
        !model.selectedRows.isEmpty && !model.primaryKey.isEmpty
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Export \(model.relation.name)").font(.headline)
            Form {
                Picker("Format", selection: $format) {
                    Text("CSV").tag(ExportFormat.csv)
                    Text("JSON").tag(ExportFormat.json)
                    Text("SQL").tag(ExportFormat.sql)
                }
                .pickerStyle(.segmented)
                Picker("Rows", selection: $rows) {
                    Text(allLabel).tag(Rows.all)
                    Text("This page (\(model.rows.count.formatted()))").tag(Rows.page)
                    Text("Selected rows (\(model.selectedRows.count.formatted()))").tag(Rows.selected)
                        .selectionDisabled(!canExportSelected)
                }
                .pickerStyle(.radioGroup)
            }
            Text(formatNote)
                .font(Theme.caption)
                .foregroundStyle(Theme.inkSecondary)
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Export…") { choosePlace() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(rows == .selected && !canExportSelected)
            }
        }
        .padding(20)
        .frame(width: 420)
        .onAppear { if canExportSelected && model.selectedRows.count > 1 { rows = .selected } }
    }

    private func choosePlace() {
        let scope: ExportScope = switch rows {
        case .all: .all
        case .page: .page(offset: UInt64(model.offset), limit: UInt64(model.pageSize))
        case .selected: .keys(keys: model.selectedRows.compactMap(model.key(ofRow:)))
        }
        dismiss()
        let panel = NSSavePanel()
        let (fileExtension, type): (String, UTType) = switch format {
        case .csv: ("csv", .commaSeparatedText)
        case .json: ("json", .json)
        case .sql: ("sql", UTType(filenameExtension: "sql") ?? .plainText)
        }
        panel.nameFieldStringValue = "\(model.relation.name).\(fileExtension)"
        panel.allowedContentTypes = [type]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        model.export(scope: scope, format: format, to: url)
    }
}
