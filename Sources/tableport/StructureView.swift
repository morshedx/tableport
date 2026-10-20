import TablePortCore
import MacKit
import SwiftUI

/// The Structure view of a table tab: columns, indexes, foreign keys and
/// other constraints, each in a read-only grid like the Data view.
struct StructureView: View {
    @ObservedObject var model: TableTabModel

    var body: some View {
        Group {
            if let structure = model.structure {
                ScrollView {
                    VStack(alignment: .leading, spacing: 18) {
                        section("Columns", headers: ["name", "type", "nullable", "default", "comment"],
                                rows: structure.columns.map { [$0.name, $0.columnType, $0.nullable ? "YES" : "NO", $0.defaultValue, $0.comment] })
                        section("Indexes", headers: ["name", "unique", "primary", "definition"],
                                rows: structure.indexes.map { [$0.name, $0.isUnique ? "YES" : "NO", $0.isPrimary ? "YES" : "NO", $0.definition] })
                        let foreignKeys = structure.constraints.filter { $0.kind == .foreignKey }
                        section("Foreign keys", headers: ["name", "definition"],
                                rows: foreignKeys.map { [$0.name, $0.definition] })
                        let others = structure.constraints.filter { $0.kind != .foreignKey }
                        section("Constraints", headers: ["name", "kind", "definition"],
                                rows: others.map { [$0.name, $0.kind.label, $0.definition] })
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
    }

    @ViewBuilder
    private func section(_ title: String, headers: [String], rows: [[String?]]) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("\(title) (\(rows.count))")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Theme.ink)
            if rows.isEmpty {
                Text("None").font(Theme.caption).foregroundStyle(Theme.inkSecondary)
            } else {
                DataGrid.readOnly(columns: headers, rows: rows)
                    // Header plus every row, so the page scrolls instead of each grid.
                    .frame(height: 28 + CGFloat(rows.count) * 24 + 2)
                    .overlay(RoundedRectangle(cornerRadius: 4).stroke(Theme.hairline))
            }
        }
    }
}

extension DataGrid {
    /// A grid with no editing, sorting or keys, for lists like the structure.
    static func readOnly(columns: [String], rows: [[String?]]) -> DataGrid {
        DataGrid(
            columns: columns.map { Column(name: $0, columnType: "", kind: .other, enumValues: []) },
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
