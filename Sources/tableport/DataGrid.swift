import AppKit
import TablePortCore
import SwiftUI

/// The rows of one page in an AppKit table, styled after TablePlus: row
/// numbers, centered headers, grid lines, monospaced cells. AppKit only
/// makes views for the visible rows, which SwiftUI's Table does not do well
/// at this size.
struct DataGrid: NSViewRepresentable {
    let columns: [Column]
    let rows: [[String?]]
    /// The row number of the first row, minus one.
    let offset: Int
    let sort: RowSort?
    /// By column name. Cells of these columns get an arrow that follows the key.
    let foreignKeys: [String: ForeignKey]
    /// Pending rows from "+ Row", drawn after the page and editable.
    let newRows: [NewRow]
    /// Whether rows of the page can be edited and deleted (the table has a
    /// primary key).
    let isEditable: Bool
    /// Changes with every pending edit, so SwiftUI updates the grid.
    let changeToken: Int
    /// Changed cells of a page row, by column index.
    let edits: (Int) -> [Int: CellDraft]
    let isDeleted: (Int) -> Bool
    let onSort: (RowSort?) -> Void
    let onFollow: (ForeignKey, String) -> Void
    let onEditNewRow: (CellDraft?, Int, Int) -> Void
    let onRemoveNewRow: (Int) -> Void
    /// nil reverts the cell.
    let onEditCell: (CellDraft?, Int, Int) -> Void
    let onToggleDelete: ([Int]) -> Void
    /// Header clicks sort the rows on the server; off for fixed lists.
    var isSortable = true
    /// The first selected row, or nil.
    var onSelectRow: (Int?) -> Void = { _ in }
    /// Space on the grid (not while typing in a cell): the row panel.
    var onSpace: () -> Void = {}
    /// The table shown. The grid is reused across table tabs; when this
    /// changes, selection and scroll position start fresh.
    var identity = ""
    /// Copy as CSV or SQL from a table tab, which can read long values again
    /// and knows the table's name. Without it the grid copies what it holds,
    /// as CSV only.
    var onCopyAs: ((GridCopyFormat, [Int]) -> Void)? = nil
    /// Export… from a table tab (the database's rows). Without it the grid
    /// exports the rows it holds.
    var onExport: (([Int]) -> Void)? = nil
    /// All selected page rows, whenever the selection changes.
    var onSelectRows: ([Int]) -> Void = { _ in }
    /// Reads a page cell whole, for the cell editor: the grid holds long
    /// values shortened. Table tabs with a primary key only.
    var fullValue: ((Int, Int) async throws -> String?)? = nil

    private static let numberColumnID = NSUserInterfaceItemIdentifier("#")

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSScrollView {
        let table = GridTableView()
        table.delegate = context.coordinator
        table.dataSource = context.coordinator
        table.style = .plain
        // Stripes run on below the last row; grid lines stop at the data
        // (rows draw their own).
        table.usesAlternatingRowBackgroundColors = true
        table.backgroundColor = .textBackgroundColor
        table.rowHeight = 24
        table.intercellSpacing = NSSize(width: 0, height: 0)
        table.columnAutoresizingStyle = .noColumnAutoresizing
        table.allowsMultipleSelection = true
        table.allowsColumnReordering = false
        let header = GridHeaderView()
        header.onFit = { [weak coordinator = context.coordinator] in coordinator?.fitColumns() }
        table.headerView = header
        let menu = NSMenu()
        menu.delegate = context.coordinator
        table.menu = menu

        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = true
        scroll.autohidesScrollers = true
        // The window draws under its title bar; without this the scroll view
        // adds that height as an inset and the first row hides under the header.
        scroll.automaticallyAdjustsContentInsets = false
        scroll.contentInsets = NSEdgeInsets()
        context.coordinator.table = table
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        let coordinator = context.coordinator
        // SwiftUI calls this for any change of the tab, such as a row being
        // selected. A reload makes new row views without the selection
        // highlight, so it only runs when what the grid shows has changed.
        let identityChanged = coordinator.identity != identity
        coordinator.identity = identity
        let contentChanged = identityChanged
            || coordinator.columns != columns
            || coordinator.rows != rows
            || coordinator.newRows != newRows
            || coordinator.changeToken != changeToken
            || coordinator.foreignKeys != foreignKeys
            || coordinator.isEditable != isEditable
            || coordinator.offset != offset
        coordinator.changeToken = changeToken
        coordinator.onSort = onSort
        coordinator.onFollow = onFollow
        coordinator.onEditNewRow = onEditNewRow
        coordinator.onRemoveNewRow = onRemoveNewRow
        coordinator.isEditable = isEditable
        coordinator.edits = edits
        coordinator.isDeleted = isDeleted
        coordinator.onEditCell = onEditCell
        coordinator.onToggleDelete = onToggleDelete
        coordinator.onSelectRow = onSelectRow
        coordinator.onSelectRows = onSelectRows
        coordinator.onCopyAs = onCopyAs
        coordinator.onExport = onExport
        coordinator.fullValue = fullValue
        coordinator.table?.onSpace = onSpace
        coordinator.table?.grid = coordinator
        coordinator.table?.onDeleteKey = { [weak coordinator] rows in coordinator?.toggleDelete(rows) }
        coordinator.offset = offset
        coordinator.foreignKeys = foreignKeys
        guard let table = coordinator.table else { return }
        coordinator.isUpdating = true
        defer { coordinator.isUpdating = false }

        if identityChanged {
            coordinator.selectedCell = nil
            table.deselectAll(nil)
            coordinator.columns = []
        }
        if coordinator.columns != columns {
            // Adding a column builds its cell in every row view on screen, so
            // with rows showing a column swap cost rows × columns cells.
            // Empty the table first; the reload below fills it once.
            coordinator.rows = []
            coordinator.newRows = []
            table.rows = []
            table.reloadData()
            coordinator.columns = columns
            coordinator.widenedForArrow = []
            // Existing columns are renamed and resized, not replaced: a removed
            // column takes its reusable cells with it, and the next table would
            // build every cell again.
            if table.tableColumn(withIdentifier: Self.numberColumnID) == nil {
                table.addTableColumn(numberColumn())
            }
            for tableColumn in table.tableColumns {
                if let index = Int(tableColumn.identifier.rawValue), index >= columns.count {
                    table.removeTableColumn(tableColumn)
                }
            }
            for (index, column) in columns.enumerated() {
                let identifier = NSUserInterfaceItemIdentifier(String(index))
                let existing = table.tableColumn(withIdentifier: identifier)
                let tableColumn = existing ?? NSTableColumn(identifier: identifier)
                tableColumn.title = column.name
                tableColumn.headerCell.alignment = .center
                tableColumn.headerToolTip = "\(column.name) · \(column.columnType)"
                tableColumn.width = Self.width(for: column, values: rows.prefix(50).map { $0.indices.contains(index) ? $0[index] : nil })
                tableColumn.minWidth = 40
                tableColumn.sortDescriptorPrototype = isSortable ? NSSortDescriptor(key: column.name, ascending: true) : nil
                if existing == nil { table.addTableColumn(tableColumn) }
            }
            Self.scrollToTop(scroll, table)
        }
        // Foreign keys load after the columns; make room for their arrow once.
        for (index, column) in columns.enumerated()
        where foreignKeys[column.name] != nil && !coordinator.widenedForArrow.contains(column.name) {
            coordinator.widenedForArrow.insert(column.name)
            if let tableColumn = table.tableColumn(withIdentifier: NSUserInterfaceItemIdentifier(String(index))) {
                tableColumn.width += 18
            }
        }
        let descriptors = sort.map { [NSSortDescriptor(key: $0.column, ascending: $0.ascending)] } ?? []
        if table.sortDescriptors != descriptors { table.sortDescriptors = descriptors }

        let pageChanged = coordinator.rows.count != rows.count || coordinator.firstRowOffset != offset
        let addedRow = newRows.count > coordinator.newRows.count
        coordinator.rows = rows
        coordinator.newRows = newRows
        coordinator.firstRowOffset = offset
        table.rows = rows
        // Reloading while a cell is being typed in would end the edit, so then
        // only the other visible cells are redrawn.
        if !contentChanged {
            // Nothing to redraw.
        } else if coordinator.editing == nil {
            let selected = table.selectedRowIndexes
            table.reloadData()
            let kept = selected.filteredIndexSet { $0 < table.numberOfRows }
            if !kept.isEmpty { table.selectRowIndexes(kept, byExtendingSelection: false) }
            // reloadData keeps the row views, so their tints are set here.
            table.enumerateAvailableRowViews { rowView, row in
                guard let rowView = rowView as? GridRowView else { return }
                rowView.isNew = coordinator.newRowIndex(row) != nil
                rowView.isDeleted = row < rows.count && isDeleted(row)
            }
        } else {
            coordinator.refreshVisible()
        }
        if pageChanged && !addedRow { Self.scrollToTop(scroll, table) }
        if addedRow {
            let row = rows.count + newRows.count - 1
            table.scrollRowToVisible(row)
            table.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
            if !columns.isEmpty { coordinator.beginEditing(row: row, column: 0) }
        }
    }

    /// The header sits inside the scrolled area, so the top of the rows is at
    /// y = −header height. Scrolling to 0 hid the first row under the header.
    private static func scrollToTop(_ scroll: NSScrollView, _ table: NSTableView) {
        let header = table.headerView?.frame.height ?? 0
        scroll.contentView.scroll(to: NSPoint(x: 0, y: -header))
        scroll.reflectScrolledClipView(scroll.contentView)
    }

    private func numberColumn() -> NSTableColumn {
        let column = NSTableColumn(identifier: Self.numberColumnID)
        column.title = ""
        column.width = 44
        column.minWidth = 32
        column.resizingMask = []
        return column
    }

    /// Fits the header and the first rows, within limits: one long value
    /// must not push every other column off screen. The header's fit button
    /// passes a higher limit.
    static func width(for column: Column, values: [String?], maxCharacters: Int = 48) -> CGFloat {
        let longest = values.map { $0?.count ?? 4 }.max() ?? 0
        let characters = max(column.name.count + 3, min(longest, maxCharacters))
        // Room for the ⌃⌄ value button of dates, booleans and enums.
        let accessory: CGFloat = column.kind == .other ? 0 : 18
        // JetBrains Mono is about 0.62 em wide per character.
        let characterWidth = CGFloat(AppSettings.shared.gridFontSize) * 0.625
        let limit = maxCharacters > 48 ? CGFloat(maxCharacters) * characterWidth : 380 * characterWidth / 7.5
        return min(max(CGFloat(characters) * characterWidth + 16, 64), limit) + accessory
    }

    @MainActor
    final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSTextFieldDelegate, NSMenuDelegate {
        weak var table: GridTableView?
        var columns: [Column] = []
        var rows: [[String?]] = []
        var newRows: [NewRow] = []
        var foreignKeys: [String: ForeignKey] = [:]
        var widenedForArrow: Set<String> = []
        var onFollow: ((ForeignKey, String) -> Void)?
        var onEditNewRow: ((CellDraft?, Int, Int) -> Void)?
        var onRemoveNewRow: ((Int) -> Void)?
        var isEditable = false
        var edits: ((Int) -> [Int: CellDraft])?
        var isDeleted: ((Int) -> Bool)?
        var onEditCell: ((CellDraft?, Int, Int) -> Void)?
        var onToggleDelete: (([Int]) -> Void)?
        var onSelectRow: ((Int?) -> Void)?
        var onSelectRows: (([Int]) -> Void)?
        var onCopyAs: ((GridCopyFormat, [Int]) -> Void)?
        var onExport: (([Int]) -> Void)?
        var fullValue: ((Int, Int) async throws -> String?)?
        var offset = 0
        var firstRowOffset = -1
        var changeToken = -1
        var identity: String?
        var onSort: ((RowSort?) -> Void)?
        /// Set while SwiftUI pushes state in, so setting the sort indicator
        /// doesn't echo back as a user sort.
        var isUpdating = false

        // Read on each draw, so a Settings change shows on the next redraw.
        private var textFont: NSFont { AppFont.mono(size: AppSettings.shared.gridFontSize) }
        private var numberFont: NSFont { AppFont.mono(size: AppSettings.shared.gridFontSize - 1) }
        /// The clicked cell, outlined, as in TablePlus: arrow keys move it,
        /// Return edits it, ⌘C copies its value. Column is a data column.
        var selectedCell: (row: Int, column: Int)? {
            didSet {
                guard let table else { return }
                for row in Set([oldValue?.row, selectedCell?.row].compactMap { $0 }) where row < table.numberOfRows {
                    table.rowView(atRow: row, makeIfNecessary: false)?.needsDisplay = true
                }
            }
        }

        /// The cell being typed in, and the field over it.
        private(set) var editing: (row: Int, column: Int, field: NSTextField)? {
            didSet {
                // The row draws the red outline, or not while its cell is edited.
                for row in [oldValue?.row, editing?.row].compactMap({ $0 }) {
                    table?.rowView(atRow: row, makeIfNecessary: false)?.needsDisplay = true
                }
            }
        }

        func isEditing(row: Int, column: Int) -> Bool {
            editing.map { $0.row == row && $0.column == column } ?? false
        }

        func numberOfRows(in tableView: NSTableView) -> Int { rows.count + newRows.count }

        /// The index into `newRows`, or nil for a row of the page.
        func newRowIndex(_ row: Int) -> Int? {
            row >= rows.count && row < rows.count + newRows.count ? row - rows.count : nil
        }

        /// No views per cell: each row view draws its cells (see GridRowView).
        /// Hundreds of cell views per table made switching tables slow.
        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? { nil }

        /// Redraws the visible rows and their tints, e.g. after an edit.
        func refreshVisible() {
            guard let table else { return }
            table.enumerateAvailableRowViews { rowView, row in
                guard let rowView = rowView as? GridRowView else { return }
                rowView.isNew = newRowIndex(row) != nil
                rowView.isDeleted = row < rows.count && (isDeleted?(row) ?? false)
                rowView.needsDisplay = true
            }
        }

        // MARK: Cell contents

        func numberText(row: Int) -> String {
            newRowIndex(row) == nil ? String(offset + row + 1) : "+"
        }

        /// What one cell draws: text or a grey placeholder, a tint when
        /// changed, and the arrow or ⌃⌄ at its right edge.
        func paint(row: Int, column index: Int) -> GridCellPaint? {
            guard columns.indices.contains(index) else { return nil }
            let column = columns[index]
            if let newIndex = newRowIndex(row) {
                let draft = newRows[newIndex].values[column.name]
                let text = GridCellPaint.text(for: draft)
                return GridCellPaint(text: text ?? "DEFAULT", isPlaceholder: text == nil, isDimmed: false, tint: nil,
                                     accessory: column.kind == .other ? nil : .valueMenu, isNumeric: column.isNumeric)
            }
            guard rows.indices.contains(row), rows[row].indices.contains(index) else { return nil }
            let stored = rows[row][index]
            let edit = edits?(row)[index]
            let deleted = isDeleted?(row) ?? false
            let shown = edit.map(GridCellPaint.text(for:)) ?? stored
            var accessory: GridCellPaint.Accessory?
            if edit == nil, !deleted, stored != nil, foreignKeys[column.name] != nil {
                accessory = .foreignKey
            } else if isEditable && !deleted && column.kind != .other {
                accessory = .valueMenu
            }
            return GridCellPaint(
                text: shown.map { $0.replacingOccurrences(of: "\n", with: "⏎ ") } ?? "NULL",
                isPlaceholder: shown == nil, isDimmed: deleted,
                tint: edit == nil ? nil : NSColor.systemYellow.withAlphaComponent(0.3),
                accessory: accessory,
                isNumeric: column.isNumeric
            )
        }

        /// Whether a double click may type into this cell. Values cut for the
        /// grid would be saved cut, so they stay read-only.
        func canEdit(row: Int, column: Int) -> Bool {
            guard columns.indices.contains(column) else { return false }
            if newRowIndex(row) != nil { return true }
            guard isEditable, rows.indices.contains(row), !(isDeleted?(row) ?? false) else { return false }
            return (rows[row][column]?.count ?? 0) < Grid.cellLimit
        }

        /// What ⌘C copies for one cell: its shown value, NULL as empty.
        func cellText(row: Int, column: Int) -> String? {
            guard columns.indices.contains(column), row < rows.count + newRows.count else { return nil }
            return GridCellPaint.text(for: shownDraft(row: row, column: column)) ?? ""
        }

        /// Long, multi-line and JSON values open the cell editor on double
        /// click; other cells edit in place.
        func prefersEditor(row: Int, column: Int) -> Bool {
            guard columns.indices.contains(column) else { return false }
            if columns[column].columnType.hasPrefix("json") { return true }
            guard rows.indices.contains(row), rows[row].indices.contains(column),
                  let text = edits?(row)[column].flatMap(GridCellPaint.text(for:)) ?? rows[row][column] else { return false }
            return text.count >= Grid.cellLimit || text.contains("\n")
        }

        /// The cell editor over a cell: editable where the grid allows edits,
        /// otherwise to view the value. Shortened values are read again whole.
        func openEditor(row: Int, column: Int) {
            guard let table, columns.indices.contains(column), row < numberOfRows(in: table) else { return }
            endEditing(commit: true)
            let draft = shownDraft(row: row, column: column)
            let isNew = newRowIndex(row) != nil
            let deleted = !isNew && (isDeleted?(row) ?? false)
            let isCut = !isNew && (GridCellPaint.text(for: draft)?.count ?? 0) >= Grid.cellLimit && edits?(row)[column] == nil
            var reason: String?
            if deleted {
                reason = "Read-only: the row is marked for delete."
            } else if !isNew && !isEditable {
                reason = "Read-only."
            }
            let title = "\(columns[column].name) · \(columns[column].columnType.isEmpty ? "value" : columns[column].columnType)"
            let editor = CellEditorController(
                content: .init(title: title, text: isCut && fullValue != nil ? nil : GridCellPaint.text(for: draft) ?? "",
                               isNull: draft == nil || draft == .null || draft == .default, readOnlyReason: reason)
            ) { [weak self] draft in
                self?.setDraft(draft, row: row, column: column)
                self?.refreshVisible()
            }
            if isCut {
                if let fullValue {
                    Task { @MainActor in
                        do {
                            editor.show(text: try await fullValue(row, column))
                        } catch {
                            editor.show(text: GridCellPaint.text(for: draft), readOnlyReason: "Only the first \(Grid.cellLimit.formatted()) characters could be read: \(error.localizedDescription)")
                        }
                    }
                } else {
                    editor.show(text: GridCellPaint.text(for: draft),
                                readOnlyReason: "Read-only: shortened to \(Grid.cellLimit.formatted()) characters, and this table has no primary key to read it whole.")
                }
            }
            table.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
            let anchor = editorAnchor(row: row, column: column)
            // A narrow cell at the right edge: open beside it, to the left.
            let crowded = anchor.midX > table.visibleRect.maxX - CellEditorController.width / 2 - 8
            editor.show(relativeTo: anchor, of: table, preferredEdge: crowded ? .minX : .maxY)
        }

        /// Where the editor's arrow points: the visible part of the cell, moved
        /// in from the window's edges so the popover (centred on it) stays
        /// inside the window rather than running off past a wide cell.
        private func editorAnchor(row: Int, column: Int) -> NSRect {
            guard let table else { return .zero }
            table.scrollRowToVisible(row)
            let cell = table.frameOfCell(atColumn: column + 1, row: row)
            let visible = table.visibleRect
            let shown = cell.intersection(visible).isNull ? cell : cell.intersection(visible)
            let half = CellEditorController.width / 2 + 8
            let lowest = min(visible.minX + half, visible.maxX)
            let highest = max(visible.maxX - half, lowest)
            let x = min(max(shown.midX, lowest), highest)
            // Inside the cell when it can be; at the edge of what is shown otherwise.
            let anchorX = min(max(x, shown.minX + 4), shown.maxX - 4)
            return NSRect(x: anchorX - 1, y: shown.minY, width: 2, height: shown.height)
        }

        /// The cell's arrow or ⌃⌄ was clicked.
        func performAccessory(row: Int, column: Int) {
            guard let table, let paint = paint(row: row, column: column), let accessory = paint.accessory else { return }
            let rect = table.frameOfCell(atColumn: column + 1, row: row)
            switch accessory {
            case .foreignKey:
                if let key = foreignKeys[columns[column].name], let value = rows[row][column] {
                    onFollow?(key, value)
                }
            case .valueMenu:
                showValueMenu(row: row, column: column, anchor: rect)
            }
        }

        /// Marks the rows for delete (or restores them); new rows are removed.        /// Marks the rows for delete (or restores them); new rows are removed.
        func toggleDelete(_ selected: [Int]) {
            let pageRows = selected.filter { $0 < rows.count }
            if isEditable, !pageRows.isEmpty { onToggleDelete?(pageRows) }
            for newIndex in selected.compactMap(newRowIndex).sorted(by: >) {
                onRemoveNewRow?(newIndex)
            }
        }

        // MARK: Editing

        /// Puts a text field over the cell, as TablePlus does on double click.
        func beginEditing(row: Int, column: Int) {
            guard let table, canEdit(row: row, column: column) else { return }
            endEditing(commit: true)
            let field = NSTextField()
            field.isBordered = false
            field.drawsBackground = true
            field.backgroundColor = .textBackgroundColor
            // macOS' own focus ring marks the cell being edited; the grid's
            // red outline steps aside while it does.
            field.focusRingType = .default
            field.cell?.usesSingleLineMode = true
            field.cell?.isScrollable = true
            field.delegate = self
            table.addSubview(field)
            editing = (row, column, field)
            place(field, row: row, column: column)
            field.stringValue = GridCellPaint.text(for: shownDraft(row: row, column: column)) ?? ""
            table.window?.makeFirstResponder(field)
            field.currentEditor()?.selectAll(nil)
        }

        /// Sizes and positions the field over a cell, with that cell's placeholder.
        private func place(_ field: NSTextField, row: Int, column: Int) {
            guard let table else { return }
            table.scrollRowToVisible(row)
            table.scrollColumnToVisible(column + 1)
            field.font = textFont
            field.alignment = columns.indices.contains(column) && columns[column].isNumeric ? .right : .natural
            field.placeholderString = newRowIndex(row) == nil ? "NULL" : "DEFAULT"
            let cell = table.frameOfCell(atColumn: column + 1, row: row)
            let height = ceil(field.intrinsicContentSize.height)
            field.frame = NSRect(x: cell.minX + 2, y: cell.midY - height / 2, width: cell.width - 4, height: height)
        }

        /// Removes the field; `commit` keeps what was typed.
        func endEditing(commit: Bool) {
            guard let editing else { return }
            self.editing = nil
            let text = editing.field.stringValue
            editing.field.delegate = nil
            editing.field.removeFromSuperview()
            table?.window?.makeFirstResponder(table)
            if commit { save(text, row: editing.row, column: editing.column) }
        }

        private func save(_ text: String, row: Int, column: Int) {
            // Leaving a cell without typing keeps what it had: an empty field
            // over NULL or DEFAULT, or "NOW()" from the menu, is not new text.
            if text == (GridCellPaint.text(for: shownDraft(row: row, column: column)) ?? "") { return }
            if let newIndex = newRowIndex(row) {
                onEditNewRow?(text.isEmpty ? nil : .value(text: text), newIndex, column)
            } else {
                onEditCell?(.value(text: text), row, column)
            }
            refreshVisible()
        }

        func controlTextDidEndEditing(_ notification: Notification) {
            guard let editing, notification.object as? NSTextField === editing.field else { return }
            endEditing(commit: true)
        }

        func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
            // Esc leaves the cell as it was.
            if commandSelector == #selector(NSResponder.cancelOperation(_:)) {
                endEditing(commit: false)
                return true
            }
            // Tab and ⇧Tab move along the row, as in a spreadsheet. The same
            // field moves and keeps the focus, so nothing flashes; AppKit's
            // own Tab handling would end editing and focus the table first.
            let step = commandSelector == #selector(NSResponder.insertTab(_:)) ? 1
                : commandSelector == #selector(NSResponder.insertBacktab(_:)) ? -1 : 0
            guard step != 0, let editing else { return false }
            let next = editing.column + step
            save(textView.string, row: editing.row, column: editing.column)
            guard columns.indices.contains(next), canEdit(row: editing.row, column: next) else {
                endEditing(commit: false)
                return true
            }
            self.editing = (editing.row, next, editing.field)
            place(editing.field, row: editing.row, column: next)
            textView.string = GridCellPaint.text(for: shownDraft(row: editing.row, column: next)) ?? ""
            textView.selectAll(nil)
            return true
        }

        /// What a cell shows now: its pending draft, or the stored value.        /// What a cell shows now: its pending draft, or the stored value.
        private func shownDraft(row: Int, column: Int) -> CellDraft? {
            if let newIndex = newRowIndex(row) {
                return newRows[newIndex].values[columns[column].name]
            }
            guard rows.indices.contains(row), rows[row].indices.contains(column) else { return nil }
            return edits?(row)[column] ?? rows[row][column].map { .value(text: $0) } ?? .null
        }

        private func setDraft(_ draft: CellDraft?, row: Int, column: Int) {
            if let newIndex = newRowIndex(row) {
                onEditNewRow?(draft, newIndex, column)
            } else {
                onEditCell?(draft, row, column)
            }
        }

        // MARK: Value menu (the ⌃⌄ button)

        private func showValueMenu(row: Int, column: Int, anchor: NSRect) {
            let info = columns[column]
            let menu = NSMenu()
            let set = { [weak self] (draft: CellDraft?) in self?.setDraft(draft, row: row, column: column) }
            menu.addItem(ClosureMenuItem(title: "NULL") { set(.null) })
            menu.addItem(ClosureMenuItem(title: "DEFAULT") { set(.default) })
            menu.addItem(.separator())
            switch info.kind {
            case .date:
                menu.addItem(ClosureMenuItem(title: "CURRENT_DATE") { set(.function(function: .currentDate)) })
            case .time:
                menu.addItem(ClosureMenuItem(title: "CURRENT_TIME") { set(.function(function: .currentTime)) })
            case .timestamp, .timestampTz:
                menu.addItem(ClosureMenuItem(title: "NOW()") { set(.function(function: .now)) })
            case .boolean:
                menu.addItem(ClosureMenuItem(title: "TRUE") { set(.value(text: "true")) })
                menu.addItem(ClosureMenuItem(title: "FALSE") { set(.value(text: "false")) })
            case .enum:
                for label in info.enumValues {
                    menu.addItem(ClosureMenuItem(title: label) { set(.value(text: label)) })
                }
            case .other:
                break
            }
            if [.date, .timestamp, .timestampTz].contains(info.kind) {
                menu.addItem(.separator())
                menu.addItem(ClosureMenuItem(title: "Date Picker…") { [weak self] in
                    self?.showDatePicker(row: row, column: column, anchor: anchor)
                })
            }
            menu.addItem(ClosureMenuItem(title: "Manual input…") { [weak self] in
                self?.beginEditing(row: row, column: column)
            })
            guard let table else { return }
            menu.popUp(positioning: nil, at: NSPoint(x: anchor.maxX - 18, y: anchor.maxY + 2), in: table)
        }

        private func showDatePicker(row: Int, column: Int, anchor: NSRect) {
            let kind = columns[column].kind
            var current: Date?
            if case .value(let text) = shownDraft(row: row, column: column) {
                current = DateText.parse(text)
            }
            let picker = DatePickerController(date: current ?? Date(), withTime: kind != .date) { [weak self] date in
                self?.setDraft(.value(text: DateText.format(date, kind: kind)), row: row, column: column)
            }
            if let table { picker.show(relativeTo: anchor, of: table) }
        }

        // MARK: Context menu

        func menuNeedsUpdate(_ menu: NSMenu) {
            menu.removeAllItems()
            guard let table else { return }
            let row = table.clickedRow
            let column = table.clickedColumn - 1
            guard row >= 0 else { return }
            if let newIndex = newRowIndex(row) {
                if columns.indices.contains(column) {
                    menu.addItem(ClosureMenuItem(title: "Set NULL") { [weak self] in self?.onEditNewRow?(.null, newIndex, column) })
                    menu.addItem(ClosureMenuItem(title: "Set DEFAULT") { [weak self] in self?.onEditNewRow?(nil, newIndex, column) })
                    menu.addItem(.separator())
                }
                menu.addItem(ClosureMenuItem(title: "Remove New Row") { [weak self] in self?.onRemoveNewRow?(newIndex) })
                return
            }
            guard rows.indices.contains(row) else { return }
            let deleted = isDeleted?(row) ?? false
            if isEditable && !deleted && columns.indices.contains(column) {
                menu.addItem(ClosureMenuItem(title: "Set NULL") { [weak self] in self?.onEditCell?(.null, row, column) })
                if edits?(row)[column] != nil {
                    menu.addItem(ClosureMenuItem(title: "Revert Cell") { [weak self] in self?.onEditCell?(nil, row, column) })
                }
                menu.addItem(.separator())
            }
            if isEditable {
                // The clicked row, or the whole selection when it is part of it.
                let targets = table.selectedRowIndexes.contains(row) ? Array(table.selectedRowIndexes) : [row]
                let title = deleted ? "Restore Row" : (targets.count > 1 ? "Delete \(targets.count) Rows" : "Delete Row")
                menu.addItem(ClosureMenuItem(title: title) { [weak self] in self?.toggleDelete(targets) })
            }
            if columns.indices.contains(column), let key = foreignKeys[columns[column].name], let value = rows[row][column] {
                if menu.numberOfItems > 0 { menu.addItem(.separator()) }
                menu.addItem(ClosureMenuItem(title: "Open \(key.referencedRelation.name) where \(key.referencedColumn) = \(value)") { [weak self] in
                    self?.onFollow?(key, value)
                })
            }
            // The clicked row, or the whole selection when it is part of it.
            let targets = (table.selectedRowIndexes.contains(row) ? Array(table.selectedRowIndexes) : [row]).filter { $0 < rows.count }
            if menu.numberOfItems > 0 { menu.addItem(.separator()) }
            if columns.indices.contains(column) {
                menu.addItem(ClosureMenuItem(title: "Open in Editor…") { [weak self] in self?.openEditor(row: row, column: column) })
            }
            menu.addItem(ClosureMenuItem(title: "Copy") { [weak self] in self?.copyRows(targets, as: .text) })
            menu.addItem(ClosureMenuItem(title: "Copy as CSV") { [weak self] in self?.copyRows(targets, as: .csv) })
            if onCopyAs != nil {
                menu.addItem(ClosureMenuItem(title: "Copy as SQL") { [weak self] in self?.copyRows(targets, as: .sql) })
            }
            menu.addItem(.separator())
            menu.addItem(ClosureMenuItem(title: "Export…") { [weak self] in self?.export(selected: targets) })
        }

        // MARK: Copy and export

        func copyRows(_ indices: [Int], as format: GridCopyFormat) {
            let indices = indices.filter { $0 < rows.count }.sorted()
            guard !indices.isEmpty else { return }
            if format != .text, let onCopyAs {
                onCopyAs(format, indices)
                return
            }
            let selected = indices.map { rows[$0] }
            let text = switch format {
            case .text: selected.map { $0.map { $0 ?? "" }.joined(separator: "\t") }.joined(separator: "\n")
            case .csv, .sql: rowsToCsv(columns: columns.map(\.name), rows: selected, header: true)
            }
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
        }

        /// From a table tab, its Export sheet; otherwise the rows this grid
        /// holds (query results), as CSV or JSON by the file name.
        func export(selected: [Int]) {
            if let onExport {
                onExport(selected)
                return
            }
            let panel = NSSavePanel()
            panel.nameFieldStringValue = "result.csv"
            panel.allowedContentTypes = [.commaSeparatedText, .json]
            panel.allowsOtherFileTypes = false
            panel.message = "Exports the rows shown. Name the file .json for JSON."
            guard panel.runModal() == .OK, let url = panel.url else { return }
            let names = columns.map(\.name)
            let text = url.pathExtension.lowercased() == "json"
                ? rowsToJson(columns: names, rows: rows)
                : rowsToCsv(columns: names, rows: rows, header: true)
            do {
                try text.write(to: url, atomically: true, encoding: .utf8)
            } catch {
                NSAlert(error: error).runModal()
            }
        }

        func tableView(_ tableView: NSTableView, shouldSelect tableColumn: NSTableColumn?) -> Bool { false }

        func tableViewSelectionDidChange(_ notification: Notification) {
            guard !isUpdating, let table else { return }
            let row = table.selectedRowIndexes.first
            onSelectRow?(row.flatMap { $0 < rows.count ? $0 : nil })
            onSelectRows?(table.selectedRowIndexes.filter { $0 < rows.count })
            // ↑ and ↓ keep the outlined column; a range of rows has no cell.
            if let cell = selectedCell {
                selectedCell = table.selectedRowIndexes.count == 1 ? (table.selectedRow, cell.column) : nil
            }
        }

        func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
            let rowView = tableView.makeView(withIdentifier: GridRowView.id, owner: nil) as? GridRowView ?? GridRowView()
            rowView.identifier = GridRowView.id
            rowView.table = tableView
            rowView.grid = self
            rowView.isNew = newRowIndex(row) != nil
            rowView.isDeleted = row < rows.count && (isDeleted?(row) ?? false)
            return rowView
        }

        /// The header's button: every column as wide as its longest loaded
        /// value, up to 200 characters.
        func fitColumns() {
            guard let table else { return }
            for (index, column) in columns.enumerated() {
                guard let tableColumn = table.tableColumn(withIdentifier: NSUserInterfaceItemIdentifier(String(index))) else { continue }
                let values = rows.map { $0.indices.contains(index) ? $0[index] : nil }
                let arrow: CGFloat = foreignKeys[column.name] != nil ? 18 : 0
                tableColumn.width = DataGrid.width(for: column, values: values, maxCharacters: 200) + arrow
            }
        }

        func tableView(_ tableView: NSTableView, didAdd rowView: NSTableRowView, forRow row: Int) {
            (rowView as? GridRowView)?.row = row
            rowView.needsDisplay = true
        }

        func tableViewColumnDidResize(_ notification: Notification) {
            table?.enumerateAvailableRowViews { rowView, _ in rowView.needsDisplay = true }
        }

        func tableView(_ tableView: NSTableView, sortDescriptorsDidChange oldDescriptors: [NSSortDescriptor]) {
            guard !isUpdating else { return }
            let sort = tableView.sortDescriptors.first.flatMap { descriptor in
                descriptor.key.map { RowSort(column: $0, ascending: descriptor.ascending) }
            }
            onSort?(sort)
        }
    }
}

/// Draws its own grid lines. NSTableView's grid fills the empty space below
/// the last row too; TablePlus stops at the data, and so do rows that draw them.
final class GridRowView: NSTableRowView {
    static let id = NSUserInterfaceItemIdentifier("gridRow")
    /// The click target for a cell's arrow or ⌃⌄, at its right edge.
    static let accessoryWidth: CGFloat = 18
    weak var table: NSTableView?
    weak var grid: DataGrid.Coordinator?
    var row = -1

    private static var accessoryImages: [String: NSImage] = [:]

    private static func accessoryImage(_ symbol: String) -> NSImage? {
        if let image = accessoryImages[symbol] { return image }
        let configuration = NSImage.SymbolConfiguration(pointSize: 9, weight: .medium)
            .applying(.init(hierarchicalColor: .tertiaryLabelColor))
        let image = NSImage(systemSymbolName: symbol, accessibilityDescription: nil)?.withSymbolConfiguration(configuration)
        accessoryImages[symbol] = image
        return image
    }

    /// The cells, drawn here rather than as one view each: a grid shows
    /// hundreds of cells, and views for them made switching tables slow.
    override func draw(_ dirtyRect: NSRect) {
        super.draw(dirtyRect)
        guard let table, let grid, row >= 0 else { return }
        let onAccent = isSelected && isEmphasized
        let font = AppFont.mono(size: AppSettings.shared.gridFontSize)
        let numberFont = AppFont.mono(size: AppSettings.shared.gridFontSize - 1)
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineBreakMode = .byTruncatingTail
        // Numbers right-aligned, as in TablePlus, so their digits line up.
        let numberParagraph = NSMutableParagraphStyle()
        numberParagraph.lineBreakMode = .byTruncatingTail
        numberParagraph.alignment = .right
        let lineHeight = ceil(font.ascender - font.descender + font.leading)
        let columns = table.columnIndexes(in: dirtyRect)
        for column in columns where column < table.numberOfColumns {
            let rect = table.rect(ofColumn: column)
            let cell = NSRect(x: rect.minX, y: 0, width: rect.width, height: bounds.height)
            var textRect = cell.insetBy(dx: 6, dy: 0)
            textRect.origin.y = (bounds.height - lineHeight) / 2
            textRect.size.height = lineHeight
            if column == 0 {
                let style = NSMutableParagraphStyle()
                style.alignment = .right
                (grid.numberText(row: row) as NSString).draw(with: textRect, options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine], attributes: [
                    .font: numberFont, .foregroundColor: onAccent ? NSColor.alternateSelectedControlTextColor : .tertiaryLabelColor, .paragraphStyle: style,
                ])
                continue
            }
            guard let paint = grid.paint(row: row, column: column - 1) else { continue }
            if let tint = paint.tint {
                tint.setFill()
                cell.insetBy(dx: 0, dy: 0).fill()
            }
            if paint.accessory != nil {
                textRect.size.width -= Self.accessoryWidth - 4
            }
            let color: NSColor = onAccent ? .alternateSelectedControlTextColor
                : paint.isPlaceholder ? .tertiaryLabelColor
                : paint.isDimmed ? .secondaryLabelColor : .labelColor
            (paint.text as NSString).draw(with: textRect, options: [.usesLineFragmentOrigin, .truncatesLastVisibleLine], attributes: [
                .font: font, .foregroundColor: color, .paragraphStyle: paint.isNumeric ? numberParagraph : paragraph,
            ])
            if let cell = grid.selectedCell, cell.row == row, cell.column == column - 1, !grid.isEditing(row: row, column: column - 1) {
                // The outlined cell, as TablePlus marks it.
                NSColor.systemRed.withAlphaComponent(0.85).setStroke()
                let outline = NSBezierPath(rect: cell.column >= 0 ? NSRect(x: rect.minX, y: 0, width: rect.width, height: bounds.height).insetBy(dx: 1, dy: 1) : .zero)
                outline.lineWidth = 1.5
                outline.stroke()
            }
            if let accessory = paint.accessory,
               let image = Self.accessoryImage(accessory == .foreignKey ? "arrow.right" : "chevron.up.chevron.down") {
                let size = image.size
                image.draw(in: NSRect(x: cell.maxX - Self.accessoryWidth + (14 - size.width) / 2, y: (bounds.height - size.height) / 2,
                                      width: size.width, height: size.height))
            }
        }
    }

    override var isFlipped: Bool { true }

    /// Light grey, close to TablePlus' grid.
    static let lineColor = NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            ? NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 0.08)
            : NSColor(srgbRed: 0.9, green: 0.9, blue: 0.9, alpha: 1)
    }

    var isNew = false {
        didSet { needsDisplay = true }
    }

    var isDeleted = false {
        didSet { needsDisplay = true }
    }

    override func drawBackground(in dirtyRect: NSRect) {
        super.drawBackground(in: dirtyRect)
        if isNew {
            NSColor.systemGreen.withAlphaComponent(0.12).setFill()
            bounds.fill()
        } else if isDeleted {
            NSColor.systemRed.withAlphaComponent(0.12).setFill()
            bounds.fill()
        }
        guard let table else { return }
        Self.lineColor.setFill()
        NSRect(x: 0, y: bounds.maxY - 1, width: bounds.width, height: 1).fill()
        for column in 0..<table.numberOfColumns {
            NSRect(x: table.rect(ofColumn: column).maxX - 1, y: 0, width: 1, height: bounds.height).fill()
        }
    }
}

/// A taller header, as in TablePlus, with the fit-columns button over the
/// row-number column.
final class GridHeaderView: NSTableHeaderView {
    var onFit: (() -> Void)?
    private let fitButton = NSButton()

    override init(frame: NSRect) {
        super.init(frame: NSRect(x: frame.minX, y: frame.minY, width: frame.width, height: 28))
        fitButton.bezelStyle = .accessoryBarAction
        fitButton.isBordered = false
        fitButton.image = NSImage(systemSymbolName: "arrow.left.and.right.square", accessibilityDescription: "Fit columns")
        fitButton.contentTintColor = .secondaryLabelColor
        fitButton.toolTip = "Expand the columns to fit the content"
        fitButton.target = self
        fitButton.action = #selector(fit)
        addSubview(fitButton)
    }

    @objc private func fit() {
        onFit?()
    }

    override func layout() {
        super.layout()
        // The number column is the first one.
        let numberColumn = (tableView?.numberOfColumns ?? 0) > 0 ? headerRect(ofColumn: 0).intersection(bounds) : .zero
        fitButton.frame = NSRect(x: numberColumn.midX - 11, y: (bounds.height - 22) / 2, width: 22, height: 22)
        fitButton.isHidden = tableView == nil || numberColumn.isEmpty
    }

    required init?(coder: NSCoder) {
        super.init(coder: coder)
    }

    override var frame: NSRect {
        get { super.frame }
        set { super.frame = NSRect(x: newValue.minX, y: newValue.minY, width: newValue.width, height: 28) }
    }
}

/// Adds ⌘C (the selected rows as tab-separated text, NULL as empty) and ⌫
/// (delete the selected rows).
final class GridTableView: NSTableView {
    var rows: [[String?]] = []
    var onDeleteKey: (([Int]) -> Void)?

    var onSpace: (() -> Void)?
    weak var grid: DataGrid.Coordinator?

    /// A click on a cell's arrow or ⌃⌄ acts on it; a double click types into
    /// the cell; anything else selects rows as usual.
    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        let row = row(at: point)
        let column = column(at: point) - 1
        // A click on the row number selects the row only.
        grid?.selectedCell = row >= 0 && column >= 0 ? (row, column) : nil
        if let grid, row >= 0, column >= 0 {
            let cell = frameOfCell(atColumn: column + 1, row: row)
            if point.x >= cell.maxX - GridRowView.accessoryWidth, grid.paint(row: row, column: column)?.accessory != nil {
                selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
                grid.performAccessory(row: row, column: column)
                return
            }
            if event.clickCount == 2, grid.prefersEditor(row: row, column: column) {
                grid.openEditor(row: row, column: column)
                return
            }
            if event.clickCount == 2, grid.canEdit(row: row, column: column) {
                grid.beginEditing(row: row, column: column)
                return
            }
        }
        super.mouseDown(with: event)
    }

    override func keyDown(with event: NSEvent) {
        let plain = event.modifierFlags.intersection(.deviceIndependentFlagsMask).subtracting([.numericPad, .function]).isEmpty
        if let grid, grid.editing == nil, plain, let cell = grid.selectedCell {
            switch event.keyCode {
            case 123, 124:  // ← →
                let next = cell.column + (event.keyCode == 123 ? -1 : 1)
                if next >= 0 && next < numberOfColumns - 1 {
                    grid.selectedCell = (cell.row, next)
                    scrollColumnToVisible(next + 1)
                }
                return
            case 36, 76:  // Return, Enter
                if grid.prefersEditor(row: cell.row, column: cell.column) {
                    grid.openEditor(row: cell.row, column: cell.column)
                } else {
                    grid.beginEditing(row: cell.row, column: cell.column)
                }
                return
            default:
                break
            }
        }
        // 49 is Space; while a cell is edited the field gets it.
        if event.keyCode == 49, grid?.editing == nil, event.modifierFlags.intersection(.deviceIndependentFlagsMask).isEmpty {
            onSpace?()
            return
        }
        // 51 is ⌫, 117 is ⌦. While a cell is being edited the field gets them.
        if [51, 117].contains(event.keyCode), grid?.editing == nil, !selectedRowIndexes.isEmpty {
            onDeleteKey?(Array(selectedRowIndexes))
        } else {
            super.keyDown(with: event)
        }
    }

    @objc func copyAsCSV(_ sender: Any?) {
        grid?.copyRows(Array(selectedRowIndexes), as: .csv)
    }

    @objc func copyAsSQL(_ sender: Any?) {
        grid?.copyRows(Array(selectedRowIndexes), as: .sql)
    }

    override func validateUserInterfaceItem(_ item: NSValidatedUserInterfaceItem) -> Bool {
        switch item.action {
        case #selector(copyAsCSV(_:)): return !selectedRowIndexes.isEmpty
        case #selector(copyAsSQL(_:)): return !selectedRowIndexes.isEmpty && grid?.onCopyAs != nil
        default: return super.validateUserInterfaceItem(item)
        }
    }

    @objc func copy(_ sender: Any?) {
        // An outlined cell in a single selected row: its value, as TablePlus.
        if let grid, let cell = grid.selectedCell, selectedRowIndexes.count <= 1, let text = grid.cellText(row: cell.row, column: cell.column) {
            NSPasteboard.general.clearContents()
            NSPasteboard.general.setString(text, forType: .string)
            return
        }
        let lines = selectedRowIndexes.compactMap { index -> String? in
            guard rows.indices.contains(index) else { return nil }
            return rows[index].map { $0 ?? "" }.joined(separator: "\t")
        }
        guard !lines.isEmpty else { return }
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(lines.joined(separator: "\n"), forType: .string)
    }
}

/// What one grid cell draws; made by the coordinator at draw time.
struct GridCellPaint {
    enum Accessory {
        /// Opens the referenced row.
        case foreignKey
        /// NULL, DEFAULT, dates, booleans and enum values.
        case valueMenu
    }

    let text: String
    /// NULL or DEFAULT, drawn grey.
    let isPlaceholder: Bool
    /// A row marked for delete.
    let isDimmed: Bool
    let tint: NSColor?
    let accessory: Accessory?
    var isNumeric = false

    /// What a draft shows as text; nil for NULL, DEFAULT and no draft.
    static func text(for draft: CellDraft?) -> String? {
        switch draft {
        case .value(let text): text
        case .function(let function): function.label
        case .null, .default, nil: nil
        }
    }
}


/// An NSMenuItem that runs a closure.
final class ClosureMenuItem: NSMenuItem {
    private let handler: () -> Void

    init(title: String, action: @escaping () -> Void) {
        handler = action
        super.init(title: title, action: #selector(run), keyEquivalent: "")
        target = self
    }

    required init(coder: NSCoder) {
        handler = {}
        super.init(coder: coder)
    }

    @objc private func run() {
        handler()
    }
}

extension SqlFunction {
    var label: String {
        switch self {
        case .now: "NOW()"
        case .currentDate: "CURRENT_DATE"
        case .currentTime: "CURRENT_TIME"
        }
    }
}

/// Dates as PostgreSQL prints and reads them.
enum DateText {
    private static func formatter(_ format: String) -> DateFormatter {
        let formatter = DateFormatter()
        formatter.locale = Locale(identifier: "en_US_POSIX")
        formatter.timeZone = .current
        formatter.dateFormat = format
        return formatter
    }

    private static let readers = [
        "yyyy-MM-dd HH:mm:ss.SSSSSSxx", "yyyy-MM-dd HH:mm:ss.SSSxx", "yyyy-MM-dd HH:mm:ssxx",
        "yyyy-MM-dd HH:mm:ss.SSSSSS", "yyyy-MM-dd HH:mm:ss", "yyyy-MM-dd",
    ].map(formatter)

    static func parse(_ text: String) -> Date? {
        readers.lazy.compactMap { $0.date(from: text) }.first
    }

    /// timestamptz carries the offset, so the value means the same instant
    /// whatever the server's time zone.
    static func format(_ date: Date, kind: ColumnKind) -> String {
        switch kind {
        case .date: formatter("yyyy-MM-dd").string(from: date)
        case .timestampTz: formatter("yyyy-MM-dd HH:mm:ssxxx").string(from: date)
        default: formatter("yyyy-MM-dd HH:mm:ss").string(from: date)
        }
    }
}

/// A popover with a calendar (and a time field for timestamps) and a Set button.
final class DatePickerController: NSViewController {
    private let picker = NSDatePicker()
    private let onSet: (Date) -> Void
    private var popover: NSPopover?

    init(date: Date, withTime: Bool, onSet: @escaping (Date) -> Void) {
        self.onSet = onSet
        super.init(nibName: nil, bundle: nil)
        picker.datePickerStyle = .clockAndCalendar
        picker.datePickerElements = withTime ? [.yearMonthDay, .hourMinuteSecond] : [.yearMonthDay]
        picker.dateValue = date
    }

    required init?(coder: NSCoder) {
        nil
    }

    override func loadView() {
        let set = NSButton(title: "Set", target: self, action: #selector(confirm))
        set.keyEquivalent = "\r"
        let cancel = NSButton(title: "Cancel", target: self, action: #selector(dismissPicker))
        cancel.keyEquivalent = "\u{1b}"
        let buttons = NSStackView(views: [cancel, set])
        let stack = NSStackView(views: [picker, buttons])
        stack.orientation = .vertical
        stack.alignment = .trailing
        stack.edgeInsets = NSEdgeInsets(top: 12, left: 12, bottom: 12, right: 12)
        view = stack
    }

    func show(relativeTo rect: NSRect, of view: NSView) {
        let popover = NSPopover()
        popover.contentViewController = self
        popover.behavior = .transient
        self.popover = popover
        popover.show(relativeTo: rect, of: view, preferredEdge: .maxY)
    }

    @objc private func confirm() {
        onSet(picker.dateValue)
        popover?.close()
    }

    @objc private func dismissPicker() {
        popover?.close()
    }
}

enum GridCopyFormat {
    /// Tab-separated, NULL as empty: ⌘C.
    case text
    case csv
    /// INSERT statements; table tabs only.
    case sql
}
