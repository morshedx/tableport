import AppKit
import TablePortCore
import MacKit
import SwiftUI

extension ColumnDetail: Identifiable {
    public var id: String { name }
}

extension IndexDetail: Identifiable {
    public var id: String { name }
}

extension ConstraintDetail: Identifiable {
    public var id: String { name }
}

/// The Structure view of a table tab: columns, indexes, foreign keys and
/// other constraints. On a table that can be changed, columns, indexes and
/// foreign keys can be added, edited and dropped; every form shows the SQL it
/// will run, and each change runs as one transaction.
struct StructureView: View {
    @ObservedObject var model: TableTabModel
    @State private var sheet: StructureSheet?
    @State private var failure: String?

    private var canEdit: Bool { model.canAddRows && model.relation.kind != .foreignTable }

    var body: some View {
        Group {
            if let structure = model.structure {
                ScrollView {
                    VStack(alignment: .leading, spacing: 18) {
                        if canEdit { toolbar }
                        if let failure {
                            Label(failure, systemImage: "exclamationmark.triangle")
                                .font(Theme.caption)
                                .foregroundStyle(Theme.danger)
                                .textSelection(.enabled)
                        }
                        columns(structure.columns)
                        indexes(structure.indexes)
                        constraints("Foreign keys", structure.constraints.filter { $0.kind == .foreignKey }, showsKind: false)
                        constraints("Constraints", structure.constraints.filter { $0.kind != .foreignKey }, showsKind: true)
                    }
                    .padding(16)
                }
            } else if let error = model.structureError {
                VStack(spacing: 8) {
                    Text(error).multilineTextAlignment(.center).textSelection(.enabled)
                    Button("Retry") { model.loadStructure() }
                }
                .font(Theme.caption)
                .foregroundStyle(Theme.danger)
                .padding(24)
            } else {
                ProgressView().controlSize(.small)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.card)
        .onAppear { model.loadStructure() }
        .sheet(item: $sheet) { sheet in
            switch sheet {
            case .addColumn: ColumnSheet(model: model, original: nil)
            case .editColumn(let column): ColumnSheet(model: model, original: column)
            case .addIndex: IndexSheet(model: model)
            case .addForeignKey: ForeignKeySheet(model: model)
            }
        }
    }

    private var toolbar: some View {
        HStack(spacing: 8) {
            Button { sheet = .addColumn } label: { Label("Column", systemImage: "plus") }
            Button { sheet = .addIndex } label: { Label("Index", systemImage: "plus") }
            Button { sheet = .addForeignKey } label: { Label("Foreign Key", systemImage: "plus") }
            Spacer()
            Text("Double-click a column to edit it. Right-click for more.")
                .font(Theme.meta)
                .foregroundStyle(Theme.inkSecondary)
        }
        .controlSize(.small)
    }

    // MARK: - Sections

    private func columns(_ columns: [ColumnDetail]) -> some View {
        section("Columns", count: columns.count) {
            Table(columns) {
                TableColumn("name") { Text($0.name).font(AppFont.monoFont(size: 12)) }
                TableColumn("type") { Text($0.columnType).font(AppFont.monoFont(size: 12)) }
                TableColumn("nullable") { Text($0.nullable ? "YES" : "NO").font(AppFont.monoFont(size: 12)) }
                    .width(70)
                TableColumn("default") { cell($0.defaultValue) }
                TableColumn("comment") { cell($0.comment) }
            }
            .contextMenu(forSelectionType: ColumnDetail.ID.self) { names in
                if let column = columns.first(where: { names.contains($0.name) }) {
                    if canEdit { Button("Edit Column…") { sheet = .editColumn(column) } }
                    Button("Copy Name") { copy(column.name) }
                    if canEdit {
                        Divider()
                        Button("Drop Column…") { dropColumn(column.name) }
                    }
                }
            } primaryAction: { names in
                if canEdit, let column = columns.first(where: { names.contains($0.name) }) { sheet = .editColumn(column) }
            }
            .scrollDisabled(true)
            .frame(height: Self.height(rows: columns.count))
        }
    }

    private func indexes(_ indexes: [IndexDetail]) -> some View {
        section("Indexes", count: indexes.count) {
            Table(indexes) {
                TableColumn("name") { Text($0.name).font(AppFont.monoFont(size: 12)) }
                TableColumn("unique") { Text($0.isUnique ? "YES" : "NO").font(AppFont.monoFont(size: 12)) }.width(60)
                TableColumn("primary") { Text($0.isPrimary ? "YES" : "NO").font(AppFont.monoFont(size: 12)) }.width(60)
                TableColumn("definition") { Text($0.definition).font(AppFont.monoFont(size: 12)).help($0.definition) }
            }
            .contextMenu(forSelectionType: IndexDetail.ID.self) { names in
                if let index = indexes.first(where: { names.contains($0.name) }) {
                    Button("Copy Definition") { copy(index.definition) }
                    // A primary key's index goes with its constraint.
                    if canEdit && !index.isPrimary {
                        Divider()
                        Button("Drop Index…") { dropIndex(index.name) }
                    }
                }
            }
            .scrollDisabled(true)
            .frame(height: Self.height(rows: indexes.count))
        }
    }

    private func constraints(_ title: String, _ constraints: [ConstraintDetail], showsKind: Bool) -> some View {
        section(title, count: constraints.count) {
            Table(constraints) {
                TableColumn("name") { Text($0.name).font(AppFont.monoFont(size: 12)) }
                TableColumn("kind") { Text(showsKind ? $0.kind.label : "FOREIGN KEY").font(AppFont.monoFont(size: 12)) }.width(110)
                TableColumn("definition") { Text($0.definition).font(AppFont.monoFont(size: 12)).help($0.definition) }
            }
            .contextMenu(forSelectionType: ConstraintDetail.ID.self) { names in
                if let constraint = constraints.first(where: { names.contains($0.name) }) {
                    Button("Copy Definition") { copy(constraint.definition) }
                    if canEdit {
                        Divider()
                        Button("Drop Constraint…") { dropConstraint(constraint.name) }
                    }
                }
            }
            .scrollDisabled(true)
            .frame(height: Self.height(rows: constraints.count))
        }
    }

    @ViewBuilder
    private func section(_ title: String, count: Int, @ViewBuilder content: () -> some View) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("\(title) (\(count))")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Theme.ink)
            if count == 0 {
                Text("None").font(Theme.caption).foregroundStyle(Theme.inkSecondary)
            } else {
                content()
                    .overlay(RoundedRectangle(cornerRadius: 4).stroke(Theme.hairline))
            }
        }
    }

    private func cell(_ value: String?) -> some View {
        Text(value ?? "NULL")
            .font(AppFont.monoFont(size: 12))
            .foregroundStyle(value == nil ? Theme.inkSecondary : Theme.ink)
            .help(value ?? "")
    }

    /// Header plus every row, so the page scrolls instead of each table.
    private static func height(rows: Int) -> CGFloat {
        CGFloat(rows) * 24 + 38
    }

    // MARK: - Drops

    private func dropColumn(_ name: String) {
        confirmDrop("Drop column \(name)?", option: "Cascade (also views and constraints that use it)") { cascade in
            .dropColumn(name: name, cascade: cascade)
        }
    }

    private func dropIndex(_ name: String) {
        confirmDrop("Drop index \(name)?", option: "Cascade") { cascade in .dropIndex(name: name, cascade: cascade) }
    }

    private func dropConstraint(_ name: String) {
        confirmDrop("Drop constraint \(name)?", option: "Cascade") { cascade in .dropConstraint(name: name, cascade: cascade) }
    }

    private func confirmDrop(_ title: String, option: String, change: @escaping (Bool) -> StructureChange) {
        let relation = model.relation
        DangerConfirm.run(
            title: title,
            typeToConfirm: model.isProduction ? relation.name : nil,
            options: [option],
            button: "Drop",
            sql: { options in
                ((try? structureChangeSql(relation: relation, change: change(options[0]))) ?? []).joined(separator: "\n")
            }
        ) { _, options in
            Task {
                do {
                    failure = nil
                    try await model.applyStructure(change(options[0]))
                } catch {
                    failure = error.displayMessage
                }
            }
        }
    }

    private func copy(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }
}

private enum StructureSheet: Identifiable {
    case addColumn
    case editColumn(ColumnDetail)
    case addIndex
    case addForeignKey

    var id: String {
        switch self {
        case .addColumn: "add-column"
        case .editColumn(let column): "edit:\(column.name)"
        case .addIndex: "add-index"
        case .addForeignKey: "add-foreign-key"
        }
    }
}

// MARK: - Forms

/// The SQL a form will run, shown live, and Apply. Shared by the forms.
private struct ChangeFooter: View {
    let model: TableTabModel
    let change: StructureChange?
    let note: String?
    @Binding var error: String?
    @Environment(\.dismiss) private var dismiss
    @State private var isApplying = false

    private var preview: Result<[String], Error>? {
        guard let change else { return nil }
        return Result { try structureChangeSql(relation: model.relation, change: change) }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("SQL").font(.system(size: 11, weight: .semibold)).foregroundStyle(Theme.inkSecondary)
            Group {
                switch preview {
                case .success(let statements) where statements.isEmpty:
                    Text("No changes").foregroundStyle(Theme.inkSecondary)
                case .success(let statements):
                    Text(statements.joined(separator: "\n")).textSelection(.enabled)
                case .failure(let problem):
                    Text(problem.displayMessage).foregroundStyle(Theme.inkSecondary)
                case nil:
                    Text("—").foregroundStyle(Theme.inkSecondary)
                }
            }
            .font(AppFont.monoFont(size: 12))
            // Long statements wrap instead of ending in "…".
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(8)
            .background(Theme.windowBg, in: RoundedRectangle(cornerRadius: 6))
            if let note {
                Label(note, systemImage: "info.circle").font(Theme.meta).foregroundStyle(Theme.inkSecondary)
            }
            if model.isProduction {
                Label("Production connection: this changes the live table.", systemImage: "exclamationmark.triangle")
                    .font(Theme.meta)
                    .foregroundStyle(Theme.danger)
            }
            if let error {
                Text(error).font(Theme.caption).foregroundStyle(Theme.danger).textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(isApplying ? "Applying…" : "Apply", action: apply)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!canApply || isApplying)
            }
        }
    }

    private var canApply: Bool {
        if case .success(let statements) = preview { return !statements.isEmpty }
        return false
    }

    private func apply() {
        guard let change else { return }
        isApplying = true
        error = nil
        Task {
            do {
                try await model.applyStructure(change)
                dismiss()
            } catch {
                self.error = error.displayMessage
            }
            isApplying = false
        }
    }
}

/// Add Column, or Edit Column with the column's current values.
private struct ColumnSheet: View {
    let model: TableTabModel
    let original: ColumnDetail?
    @State private var name = ""
    @State private var columnType = "text"
    @State private var nullable = true
    @State private var defaultValue = ""
    @State private var comment = ""
    @State private var error: String?


    /// Identity and generated columns show how they get their value in place
    /// of a default; that can't be edited here.
    private var isGenerated: Bool { original?.defaultValue?.hasPrefix("GENERATED ") ?? false }

    private var spec: ColumnSpec {
        ColumnSpec(name: name, columnType: columnType, nullable: nullable,
                   defaultValue: defaultValue.isEmpty ? nil : defaultValue, comment: comment.isEmpty ? nil : comment)
    }

    private var change: StructureChange {
        original.map { .editColumn(original: $0, column: spec) } ?? .addColumn(column: spec)
    }

    private var note: String? {
        guard let original, columnType.trimmingCharacters(in: .whitespaces) != original.columnType else { return nil }
        return "Changing the type converts every value with USING and may rewrite the table."
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text(original == nil ? "Add Column to \(model.relation.name)" : "Edit Column \(original?.name ?? "")")
                .font(.headline)
            Form {
                TextField("Name", text: $name)
                HStack(spacing: 4) {
                    TextField("Type", text: $columnType)
                    TypeMenu(columnType: $columnType)
                }
                Toggle("Allow NULL", isOn: $nullable)
                TextField("Default", text: $defaultValue, prompt: Text(isGenerated ? "" : "none, or an expression such as now()"))
                    .disabled(isGenerated)
                    .help(isGenerated ? "Identity and generated columns are changed in SQL" : "")
                TextField("Comment", text: $comment, prompt: Text("none"))
            }
            .font(AppFont.monoFont(size: 12))
            ChangeFooter(model: model, change: change, note: note, error: $error)
        }
        .padding(20)
        .frame(width: 520)
        .onAppear {
            guard let original else { return }
            name = original.name
            columnType = original.columnType
            nullable = original.nullable
            defaultValue = original.defaultValue ?? ""
            comment = original.comment ?? ""
        }
    }
}

/// Types offered next to a type field; any other type can be typed.
private let commonColumnTypes = [
    "integer", "bigint", "smallint", "serial", "bigserial", "numeric(12,2)", "real", "double precision", "text",
    "varchar(255)", "boolean", "date", "timestamp", "timestamptz", "time", "uuid", "jsonb", "json", "bytea", "inet", "text[]",
]

/// The ▾ next to a type field: pick a common type.
private struct TypeMenu: View {
    @Binding var columnType: String

    var body: some View {
        Menu {
            ForEach(commonColumnTypes, id: \.self) { type in Button(type) { columnType = type } }
        } label: {
            Image(systemName: "chevron.down")
        }
        .menuIndicator(.hidden)
        .fixedSize()
        .help("Common types")
    }
}

/// New Table: a name, its columns and the primary key. The SQL shows live,
/// as in the other forms; Create runs it as one transaction and opens the
/// new table.
struct NewTableSheet: View {
    let workspace: WorkspaceModel
    @State private var schema: String
    @State private var name = ""
    @State private var rows: [Row] = [
        Row(name: "id", columnType: "bigserial", primaryKey: true, nullable: false),
        Row(name: "", columnType: "text", primaryKey: false, nullable: true),
    ]
    @State private var error: String?
    @State private var isCreating = false
    @Environment(\.dismiss) private var dismiss

    struct Row: Identifiable {
        let id = UUID()
        var name: String
        var columnType: String
        var primaryKey: Bool
        var nullable: Bool
        var defaultValue = ""
    }

    init(workspace: WorkspaceModel) {
        self.workspace = workspace
        _schema = State(initialValue: workspace.sidebar.schema)
    }

    /// Rows left with no name are ignored, so a spare empty row is harmless.
    private var columns: [NewTableColumn] {
        rows.filter { !$0.name.trimmingCharacters(in: .whitespaces).isEmpty }.map { row in
            NewTableColumn(
                column: ColumnSpec(name: row.name, columnType: row.columnType, nullable: row.nullable && !row.primaryKey,
                                   defaultValue: row.defaultValue.isEmpty ? nil : row.defaultValue, comment: nil),
                primaryKey: row.primaryKey)
        }
    }

    private var preview: Result<[String], Error> {
        Result { try createTableSql(schema: schema, name: name, columns: columns) }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("New Table").font(.headline)
            HStack(spacing: 10) {
                Picker("Schema", selection: $schema) {
                    ForEach(workspace.schemas, id: \.name) { Text($0.name).tag($0.name) }
                }
                .fixedSize()
                TextField("Name", text: $name, prompt: Text("table name"))
                    .font(AppFont.monoFont(size: 12))
            }
            columnsEditor
            Text("SQL").font(.system(size: 11, weight: .semibold)).foregroundStyle(Theme.inkSecondary)
            Group {
                switch preview {
                case .success(let statements): Text(statements.joined(separator: "\n")).textSelection(.enabled)
                case .failure(let problem): Text(problem.displayMessage).foregroundStyle(Theme.inkSecondary)
                }
            }
            .font(AppFont.monoFont(size: 12))
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(8)
            .background(Theme.windowBg, in: RoundedRectangle(cornerRadius: 6))
            if workspace.isProduction {
                Label("Production connection: the table is created on the live database.", systemImage: "exclamationmark.triangle")
                    .font(Theme.meta)
                    .foregroundStyle(Theme.danger)
            }
            if let error {
                Text(error).font(Theme.caption).foregroundStyle(Theme.danger).textSelection(.enabled)
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(isCreating ? "Creating…" : "Create", action: create)
                    .keyboardShortcut(.defaultAction)
                    .disabled(isCreating || (try? preview.get()) == nil)
            }
        }
        .padding(20)
        .frame(width: 700)
    }

    private var columnsEditor: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Text("Column").frame(width: 170, alignment: .leading)
                Text("Type").frame(width: 190, alignment: .leading)
                Text("Key").frame(width: 34)
                Text("NULL").frame(width: 38)
                Text("Default").frame(maxWidth: .infinity, alignment: .leading)
                Color.clear.frame(width: 20)
            }
            .font(Theme.meta)
            .foregroundStyle(Theme.inkSecondary)
            ScrollView {
                VStack(spacing: 6) {
                    ForEach($rows) { $row in
                        HStack(spacing: 8) {
                            TextField("name", text: $row.name).frame(width: 170)
                            HStack(spacing: 2) {
                                TextField("type", text: $row.columnType)
                                TypeMenu(columnType: $row.columnType)
                            }
                            .frame(width: 190)
                            Toggle("", isOn: $row.primaryKey).labelsHidden().frame(width: 34)
                                .help("Part of the primary key")
                            Toggle("", isOn: $row.nullable).labelsHidden().frame(width: 38)
                                .disabled(row.primaryKey)
                                .help(row.primaryKey ? "Key columns are never NULL" : "Allow NULL")
                            TextField("none", text: $row.defaultValue)
                            Button {
                                rows.removeAll { $0.id == row.id }
                            } label: {
                                Image(systemName: "minus.circle")
                            }
                            .buttonStyle(.borderless)
                            .frame(width: 20)
                            .help("Remove this column")
                        }
                        .textFieldStyle(.roundedBorder)
                        .font(AppFont.monoFont(size: 12))
                    }
                }
                // Room for the fields' focus rings inside the scroll area.
                .padding(4)
            }
            // A scroll view in a sheet takes no height of its own: one per row, up to a limit.
            .frame(height: min(CGFloat(rows.count) * 32 + 8, 260))
            Button {
                rows.append(Row(name: "", columnType: "text", primaryKey: false, nullable: true))
            } label: {
                Label("Column", systemImage: "plus")
            }
            .controlSize(.small)
        }
    }

    private func create() {
        isCreating = true
        error = nil
        let (schema, name, columns) = (schema, name.trimmingCharacters(in: .whitespaces), columns)
        Task {
            do {
                try await workspace.session.createTable(schema: schema, name: name, columns: columns)
                dismiss()
                workspace.open(Relation(schema: schema, name: name, kind: .table))
                workspace.reload()
            } catch {
                self.error = error.displayMessage
            }
            isCreating = false
        }
    }
}

/// Add Index: its columns, in the order checked, and whether it is unique.
private struct IndexSheet: View {
    let model: TableTabModel
    @State private var chosen: [String] = []
    @State private var unique = false
    @State private var name = ""
    /// The name follows the columns until the user types one.
    @State private var nameEdited = false
    @State private var error: String?

    private var suggestedName: String {
        ([model.relation.name] + chosen + [unique ? "key" : "idx"]).joined(separator: "_")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Add Index to \(model.relation.name)").font(.headline)
            Text("Columns, in order").font(Theme.caption).foregroundStyle(Theme.inkSecondary)
            ScrollView {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(model.structure?.columns ?? []) { column in
                        Toggle(isOn: Binding(
                            get: { chosen.contains(column.name) },
                            set: { on in
                                if on { chosen.append(column.name) } else { chosen.removeAll { $0 == column.name } }
                                if !nameEdited { name = suggestedName }
                            }
                        )) {
                            HStack {
                                Text(column.name).font(AppFont.monoFont(size: 12))
                                if let position = chosen.firstIndex(of: column.name) {
                                    Text("\(position + 1)").font(Theme.meta).foregroundStyle(Theme.inkSecondary)
                                }
                            }
                        }
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                // Room for the checkboxes' focus ring inside the scroll area.
                .padding(4)
            }
            .frame(maxHeight: 180)
            Form {
                Toggle("Unique", isOn: $unique)
                    .onChange(of: unique) { if !nameEdited { name = suggestedName } }
                TextField("Name", text: Binding(get: { name }, set: { name = $0; nameEdited = true }))
                    .font(AppFont.monoFont(size: 12))
            }
            ChangeFooter(model: model, change: .addIndex(name: name, columns: chosen, unique: unique),
                         note: "Building the index blocks writes to the table until it is done.", error: $error)
        }
        .padding(20)
        .frame(width: 520)
    }
}

/// Add Foreign Key: a column of this table that references a column of
/// another table.
private struct ForeignKeySheet: View {
    let model: TableTabModel
    @State private var column = ""
    @State private var tables: [Relation] = []
    @State private var referencedID = ""
    @State private var referencedColumns: [String] = []
    @State private var referencedColumn = ""
    @State private var onDelete = "NO ACTION"
    @State private var name = ""
    @State private var error: String?

    private static let actions = ["NO ACTION", "CASCADE", "SET NULL", "RESTRICT", "SET DEFAULT"]

    private var referenced: Relation? { tables.first { $0.id == referencedID } }

    private var change: StructureChange? {
        guard let referenced, !column.isEmpty, !referencedColumn.isEmpty else { return nil }
        return .addForeignKey(name: name.isEmpty ? nil : name, column: column, referenced: referenced,
                              referencedColumn: referencedColumn, onDelete: onDelete == "NO ACTION" ? nil : onDelete)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Add Foreign Key to \(model.relation.name)").font(.headline)
            Form {
                Picker("Column", selection: $column) {
                    Text("Choose…").tag("")
                    ForEach(model.structure?.columns ?? []) { Text($0.name).tag($0.name) }
                }
                Picker("References table", selection: $referencedID) {
                    Text(tables.isEmpty ? "Loading…" : "Choose…").tag("")
                    ForEach(tables, id: \.id) { table in
                        Text(table.schema == model.relation.schema ? table.name : "\(table.schema).\(table.name)").tag(table.id)
                    }
                }
                Picker("References column", selection: $referencedColumn) {
                    Text("Choose…").tag("")
                    ForEach(referencedColumns, id: \.self) { Text($0).tag($0) }
                }
                .disabled(referenced == nil)
                Picker("On delete", selection: $onDelete) {
                    ForEach(Self.actions, id: \.self) { Text($0).tag($0) }
                }
                TextField("Name", text: $name, prompt: Text("automatic"))
            }
            .font(AppFont.monoFont(size: 12))
            ChangeFooter(model: model, change: change, note: nil, error: $error)
        }
        .padding(20)
        .frame(width: 520)
        .task { await loadTables() }
        .onChange(of: referencedID) { Task { await loadColumns() } }
    }

    private func loadTables() async {
        do {
            tables = try await model.session.schemas()
                .flatMap(\.relations)
                .filter { [.table, .partitionedTable].contains($0.kind) }
        } catch {
            self.error = error.displayMessage
        }
    }

    private func loadColumns() async {
        referencedColumns = []
        referencedColumn = ""
        guard let referenced else { return }
        do {
            referencedColumns = try await model.session.columns(relation: referenced).map(\.name)
            // Most keys point at an id.
            if referencedColumns.contains("id") { referencedColumn = "id" }
        } catch {
            self.error = error.displayMessage
        }
    }
}

extension DataGrid {
    /// A grid with no editing, sorting or keys, for lists like the structure.
    /// `numeric`: one per column, true for number columns (right-aligned).
    static func readOnly(columns: [String], rows: [[String?]], numeric: [Bool] = []) -> DataGrid {
        DataGrid(
            columns: columns.enumerated().map { index, name in
                Column(name: name, columnType: "", kind: .other, enumValues: [],
                       isNumeric: numeric.indices.contains(index) && numeric[index])
            },
            rows: rows,
            offset: 0,
            sort: nil,
            foreignKeys: [:],
            newRows: [],
            isEditable: false,
            changeToken: 0,
            edits: { _ in [:] },
            isDeleted: { _ in false },
            onSort: { _ in },
            onFollow: { _, _ in },
            onEditNewRow: { _, _, _ in },
            onRemoveNewRow: { _ in },
            onEditCell: { _, _, _ in },
            onToggleDelete: { _ in },
            isSortable: false
        )
    }
}

extension ConstraintKind {
    var label: String {
        switch self {
        case .primaryKey: "PRIMARY KEY"
        case .foreignKey: "FOREIGN KEY"
        case .unique: "UNIQUE"
        case .check: "CHECK"
        case .exclusion: "EXCLUDE"
        }
    }
}
