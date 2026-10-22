import TablePortCore
import MacKit
import AppKit
import SwiftUI

/// The connection window's sidebar, after TablePlus: Items, Queries and
/// History at the top; one schema's functions, tables and views as flat
/// sections; tables open to their columns; the schema picker at the bottom.
struct SidebarView: View {
    @ObservedObject var sidebar: SidebarModel
    /// For actions only; not observed, so tab changes don't redraw the list.
    let workspace: WorkspaceModel
    @ObservedObject var history: QueryHistory
    @ObservedObject var saved: SavedQueries
    @State private var mode: Mode = .items
    @State private var search = ""

    /// Lighter and calmer than MacKit's sidebar grey, close to TablePlus.
    static let background = Color(light: 0xF8F9FB, dark: 0x1F2023)

    enum Mode: String, CaseIterable {
        case items = "Items"
        case queries = "Queries"
        case history = "History"
    }

    var body: some View {
        VStack(spacing: 0) {
            Picker("Sidebar", selection: $mode) {
                ForEach(Mode.allCases, id: \.self) { Text($0.rawValue).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .controlSize(.small)
            .padding(.horizontal, 8)
            .padding(.top, 8)
            TextField(mode == .history ? "Search history…" : mode == .queries ? "Search saved queries…" : "Search for item…", text: $search)
                .searchInput()
                .padding(8)
            switch mode {
            case .items: items
            case .queries: queries
            case .history: historyList
            }
            Divider()
            bottomBar
        }
        .background(Self.background)
    }

    // MARK: Items

    private var query: String { search.trimmingCharacters(in: .whitespaces) }

    private func matches(_ name: String) -> Bool {
        query.isEmpty || name.localizedCaseInsensitiveContains(query)
    }

    private var schemaContents: SchemaContents? {
        sidebar.schemas.first { $0.name == sidebar.schema }
    }

    @ViewBuilder
    private var items: some View {
        switch sidebar.load {
        case .loading where sidebar.schemas.isEmpty:
            ProgressView().controlSize(.small).frame(maxHeight: .infinity)
        case .failed(let message):
            VStack(spacing: 8) {
                Text(message)
                    .font(Theme.caption)
                    .foregroundStyle(Theme.danger)
                    .multilineTextAlignment(.center)
                    .textSelection(.enabled)
                Button("Retry") { workspace.reload() }
            }
            .padding()
            .frame(maxHeight: .infinity)
        default:
            itemList
        }
    }

    private var itemList: some View {
        let relations = (schemaContents?.relations ?? []).filter { matches($0.name) }
        return SidebarOutline(
            content: SidebarOutline.Content(
                isSearching: !query.isEmpty,
                emptyMessage: query.isEmpty ? "This schema is empty." : "Nothing matches “\(query)”.",
                tables: relations.filter { [.table, .partitionedTable, .foreignTable].contains($0.kind) },
                views: relations.filter { $0.kind == .view },
                materialized: relations.filter { $0.kind == .materializedView },
                routines: (sidebar.routines[sidebar.schema] ?? []).filter { matches($0.name) },
                routinesError: sidebar.routinesError[sidebar.schema],
                columns: sidebar.columns
            ),
            openRelationID: sidebar.openRelationID,
            onOpen: { id in open(id) },
            onExpand: { relation in workspace.loadColumns(relation) },
            menuItems: { kind in SidebarActions.items(for: kind, workspace: workspace) }
        )
        .onChange(of: sidebar.schema, initial: true) { _, _ in workspace.loadRoutines() }
    }

    private func open(_ id: String?) {
        guard let id, id != sidebar.openRelationID else { return }
        let workspace = workspace
        // Next turn of the run loop: the row shows as selected at once, and
        // building the tab comes after.
        if id.hasPrefix("fn:"), let oid = UInt32(id.dropFirst(3)),
           let routine = sidebar.routines[sidebar.schema]?.first(where: { $0.oid == oid }) {
            DispatchQueue.main.async { workspace.open(routine) }
        } else if let relation = schemaContents?.relations.first(where: { $0.id == id }) {
            DispatchQueue.main.async { workspace.open(relation) }
        }
    }

    // MARK: Queries and History

    /// Saved queries, by name. A click opens one in a query tab.
    private var queries: some View {
        let items = saved.items.filter { matches($0.name) || matches($0.sql) }
        return List(items, id: \.id) { query in
            VStack(alignment: .leading, spacing: 2) {
                Label {
                    Text(query.name).font(.system(size: 12, weight: .medium)).lineLimit(1)
                } icon: {
                    Image(systemName: "doc.text").foregroundStyle(Theme.inkSecondary)
                }
                Text(query.sql.replacingOccurrences(of: "\\s+", with: " ", options: .regularExpression))
                    .font(AppFont.monoFont(size: 10))
                    .foregroundStyle(Theme.inkSecondary)
                    .lineLimit(1)
                    .padding(.leading, 22)
            }
            .contentShape(Rectangle())
            .onTapGesture { workspace.open(query) }
            .accessibilityElement(children: .combine)
            .accessibilityAddTraits(.isButton)
            .accessibilityAction(.default) { workspace.open(query) }
            .help(query.sql)
            .contextMenu {
                Button("Open") { workspace.open(query) }
                Button("Rename…") {
                    if let name = askQueryName(title: "Rename Query", current: query.name) {
                        saved.rename(query, to: name)
                    }
                }
                Divider()
                Button("Delete") { saved.delete(query) }
            }
        }
        .listStyle(.sidebar)
        .scrollContentBackground(.hidden)
        .overlay {
            if items.isEmpty {
                VStack(spacing: 6) {
                    Image(systemName: "bookmark").font(.system(size: 22, weight: .light))
                    Text(saved.items.isEmpty ? "No saved queries yet" : "Nothing matches “\(query)”.")
                        .font(.system(size: 12, weight: .semibold))
                    if saved.items.isEmpty {
                        Text("In a query tab, press ⌘S to save the query with a name.")
                            .font(Theme.caption)
                            .multilineTextAlignment(.center)
                    }
                }
                .foregroundStyle(Theme.inkSecondary)
                .padding()
            }
        }
    }

    private var historyList: some View {
        let items = history.items.filter { matches($0.sql) }
        return List(items) { item in
            VStack(alignment: .leading, spacing: 2) {
                Text(item.sql.replacingOccurrences(of: "\\s+", with: " ", options: .regularExpression))
                    .font(AppFont.monoFont(size: 11))
                    .lineLimit(2)
                Text("\(item.date.formatted(date: .omitted, time: .shortened)) · \(item.failed ? "failed" : item.elapsedMs.map { "\($0) ms" } ?? "")")
                    .font(Theme.meta)
                    .foregroundStyle(item.failed ? Theme.danger : Theme.inkSecondary)
            }
            .contentShape(Rectangle())
            .onTapGesture { workspace.newQuery(sql: item.sql) }
            .accessibilityElement(children: .combine)
            .accessibilityAddTraits(.isButton)
            .accessibilityAction(.default) { workspace.newQuery(sql: item.sql) }
            .help("Open in a new query tab")
            .contextMenu {
                Button("Open in New Tab") { workspace.newQuery(sql: item.sql) }
                Button("Copy SQL") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(item.sql, forType: .string)
                }
                Divider()
                Button("Clear History…") {
                    let alert = NSAlert()
                    alert.messageText = "Clear the query history of this connection?"
                    alert.addButton(withTitle: "Clear").hasDestructiveAction = true
                    alert.addButton(withTitle: "Cancel")
                    if alert.runModal() == .alertFirstButtonReturn { history.clear() }
                }
            }
        }
        .listStyle(.sidebar)
        .scrollContentBackground(.hidden)
        .overlay {
            if items.isEmpty {
                Text(history.items.isEmpty ? "Queries you run appear here." : "Nothing matches “\(query)”.")
                    .font(Theme.caption)
                    .foregroundStyle(Theme.inkSecondary)
            }
        }
    }

    // MARK: Bottom bar

    private var bottomBar: some View {
        HStack(spacing: 6) {
            Menu {
                Button("New Query Tab") { workspace.newQuery() }
                Button("New Table…") { workspace.showsNewTable = true }
                    .disabled(workspace.connection.readOnly)
                Button("Refresh") { workspace.reload() }
            } label: {
                Image(systemName: "plus")
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
            .help("New query tab, new table, refresh")
            Picker("Schema", selection: $sidebar.schema) {
                ForEach(sidebar.schemas) { schema in
                    Text(schema.name).tag(schema.name)
                }
            }
            .labelsHidden()
            .controlSize(.small)
            .help("The schema the sidebar lists")
        }
        .padding(.horizontal, 8)
        .frame(height: 32)
    }
}

/// Tables use Lucide's "sheet"; other kinds keep their SF Symbol. Grey,
/// and blue only for the open table.
struct RelationIcon: View {
    let kind: RelationKind
    var isActive = false
    var size: CGFloat = 14

    private var color: Color { isActive ? Theme.Tile.blue : Theme.inkSecondary }

    var body: some View {
        switch kind {
        case .table, .partitionedTable, .foreignTable:
            if let image = LucideIcon.image("sheet", size: size) {
                Image(nsImage: image)
                    .renderingMode(.template)
                    .foregroundStyle(color)
            } else {
                Image(systemName: kind.symbol).foregroundStyle(color)
            }
        case .view, .materializedView:
            Image(systemName: kind.symbol).foregroundStyle(color)
        }
    }
}

/// The Items list as an AppKit outline: only the visible rows exist and they
/// are reused, so opening a table no longer re-diffs every row of a large
/// schema as SwiftUI's List did.
struct SidebarOutline: NSViewRepresentable {
    struct Content: Equatable {
        var isSearching: Bool
        var emptyMessage: String
        var tables: [Relation]
        var views: [Relation]
        var materialized: [Relation]
        var routines: [Routine]
        var routinesError: String?
        var columns: [Relation.ID: [Column]]
    }

    let content: Content
    let openRelationID: Relation.ID?
    let onOpen: (String) -> Void
    let onExpand: (Relation) -> Void
    /// The right-click menu for a row.
    let menuItems: (Node.Kind) -> [NSMenuItem]

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeNSView(context: Context) -> NSScrollView {
        let outline = NSOutlineView()
        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("name"))
        column.resizingMask = .autoresizingMask
        outline.addTableColumn(column)
        outline.outlineTableColumn = column
        outline.headerView = nil
        outline.style = .plain
        outline.rowHeight = 24
        outline.indentationPerLevel = 10
        outline.intercellSpacing = NSSize(width: 0, height: 0)
        outline.backgroundColor = .clear
        outline.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        outline.autoresizesOutlineColumn = false
        outline.dataSource = context.coordinator
        outline.delegate = context.coordinator
        context.coordinator.outline = outline
        let menu = NSMenu()
        // Truncate and Drop are disabled on read-only connections.
        menu.autoenablesItems = false
        menu.delegate = context.coordinator
        outline.menu = menu

        let scroll = NSScrollView()
        scroll.documentView = outline
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.automaticallyAdjustsContentInsets = false
        scroll.contentInsets = NSEdgeInsets(top: 0, left: 0, bottom: 8, right: 0)
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        let coordinator = context.coordinator
        coordinator.onOpen = onOpen
        coordinator.onExpand = onExpand
        coordinator.menuItems = menuItems
        if coordinator.content != content {
            coordinator.apply(content)
        }
        coordinator.showOpen(openRelationID)
    }

    final class Node: NSObject {
        enum Kind {
            case section(String, count: Int)
            case relation(Relation)
            case column(Column)
            case routine(Routine)
            case message(String)
        }

        let id: String
        var kind: Kind
        var children: [Node] = []

        init(id: String, kind: Kind) {
            self.id = id
            self.kind = kind
        }

        var relation: Relation? {
            if case .relation(let relation) = kind { return relation }
            return nil
        }
    }

    @MainActor
    final class Coordinator: NSObject, NSOutlineViewDataSource, NSOutlineViewDelegate, NSMenuDelegate {
        weak var outline: NSOutlineView?
        var content: Content?
        var onOpen: ((String) -> Void)?
        var onExpand: ((Relation) -> Void)?
        var menuItems: ((Node.Kind) -> [NSMenuItem])?

        /// Built when it opens, for the row that was right-clicked.
        func menuNeedsUpdate(_ menu: NSMenu) {
            menu.removeAllItems()
            guard let outline, outline.clickedRow >= 0,
                  let node = outline.item(atRow: outline.clickedRow) as? Node else { return }
            for item in menuItems?(node.kind) ?? [] { menu.addItem(item) }
        }
        private var roots: [Node] = []
        /// Relation nodes are kept by ID so expansion and selection survive a
        /// rebuild of the tree, for example while typing a search.
        private var relationNodes: [Relation.ID: Node] = [:]
        private var sectionNodes: [String: Node] = [:]
        private var loadingNodes: [Relation.ID: Node] = [:]
        /// Functions start folded: tables are what people open most.
        private var collapsedSections: Set<String> = ["Functions"]
        private var expandedRelations: Set<Relation.ID> = []
        private var openID: Relation.ID?
        /// Set while the tree or the selection is changed from code.
        private var isApplying = false

        func apply(_ content: Content) {
            guard let outline else { return }
            let columnsChanged = self.content?.columns != content.columns
            let structureChanged = self.content.map {
                $0.tables != content.tables || $0.views != content.views || $0.materialized != content.materialized
                    || $0.routines != content.routines || $0.routinesError != content.routinesError
                    || $0.isSearching != content.isSearching
                    || $0.emptyMessage != content.emptyMessage
            } ?? true
            self.content = content
            isApplying = true
            defer { isApplying = false }
            if structureChanged {
                roots = buildRoots(content)
                outline.reloadData()
                for root in roots {
                    if case .section(let title, _) = root.kind, content.isSearching || !collapsedSections.contains(title) {
                        outline.expandItem(root)
                    }
                }
                for node in relationNodes.values where expandedRelations.contains(node.id) && outline.row(forItem: node) >= 0 {
                    outline.expandItem(node)
                }
                selectOpen()
            } else if columnsChanged {
                for node in relationNodes.values where expandedRelations.contains(node.id) {
                    let loaded = columnNodes(for: node.id)
                    if loaded.count != node.children.count {
                        node.children = loaded
                        outline.reloadItem(node, reloadChildren: true)
                    }
                }
            }
        }

        func showOpen(_ id: Relation.ID?) {
            guard id != openID else { return }
            let previous = openID
            openID = id
            // Only the two rows whose icon tint changes are redrawn.
            for changed in [previous, id].compactMap({ $0 }) {
                if let node = relationNodes[changed], let row = outline?.row(forItem: node), row >= 0 {
                    outline?.reloadData(forRowIndexes: IndexSet(integer: row), columnIndexes: IndexSet(integer: 0))
                }
            }
            isApplying = true
            selectOpen()
            isApplying = false
        }

        private func selectOpen() {
            guard let outline else { return }
            guard let openID, let node = relationNodes[openID], outline.row(forItem: node) >= 0 else { return }
            let row = outline.row(forItem: node)
            if outline.selectedRow != row {
                outline.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
            }
        }

        private func buildRoots(_ content: Content) -> [Node] {
            var roots: [Node] = []
            func section(_ title: String, _ relations: [Relation]) {
                guard !relations.isEmpty else { return }
                let node = sectionNode(title, count: relations.count)
                node.children = relations.map(relationNode)
                roots.append(node)
            }
            section("Tables", content.tables)
            section("Views", content.views)
            section("Materialized views", content.materialized)
            if !content.routines.isEmpty {
                let node = sectionNode("Functions", count: content.routines.count)
                node.children = content.routines.map { Node(id: "fn:\($0.oid)", kind: .routine($0)) }
                roots.append(node)
            } else if let error = content.routinesError {
                let node = sectionNode("Functions", count: 0)
                node.children = [Node(id: "fn:error", kind: .message("Couldn't list functions: \(error)"))]
                roots.append(node)
            }
            if roots.isEmpty {
                roots.append(Node(id: "message", kind: .message(content.emptyMessage)))
            }
            return roots
        }

        private func sectionNode(_ title: String, count: Int) -> Node {
            let node = sectionNodes[title] ?? Node(id: "section:\(title)", kind: .section(title, count: count))
            node.kind = .section(title, count: count)
            sectionNodes[title] = node
            return node
        }

        private func relationNode(_ relation: Relation) -> Node {
            let node = relationNodes[relation.id] ?? Node(id: relation.id, kind: .relation(relation))
            node.kind = .relation(relation)
            node.children = columnNodes(for: relation.id)
            relationNodes[relation.id] = node
            return node
        }

        private func columnNodes(for id: Relation.ID) -> [Node] {
            guard let columns = content?.columns[id] else { return [] }
            return columns.map { Node(id: "\(id)#\($0.name)", kind: .column($0)) }
        }

        // MARK: Data source

        func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
            guard let node = item as? Node else { return roots.count }
            if node.relation != nil && node.children.isEmpty { return 1 }  // "Loading…" until columns arrive
            return node.children.count
        }

        func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
            guard let node = item as? Node else { return roots[index] }
            if node.relation != nil && node.children.isEmpty {
                // Outline items must stay the same object between calls.
                if let loading = loadingNodes[node.id] { return loading }
                let loading = Node(id: "\(node.id)#loading", kind: .message("Loading…"))
                loadingNodes[node.id] = loading
                return loading
            }
            return node.children[index]
        }

        func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
            guard let node = item as? Node else { return false }
            switch node.kind {
            case .section, .relation: return true
            case .column, .routine, .message: return false
            }
        }

        // MARK: Delegate

        func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
            guard let node = item as? Node else { return nil }
            let cell = outlineView.makeView(withIdentifier: SidebarCell.id, owner: nil) as? SidebarCell ?? SidebarCell()
            switch node.kind {
            case .section(let title, let count):
                cell.show(title: title, detail: "(\(count))", style: .header)
            case .relation(let relation):
                cell.show(title: relation.name, icon: Self.icon(for: relation.kind, isOpen: relation.id == openID),
                          help: "\(relation.kind.label) \(relation.schema).\(relation.name)")
            case .column(let column):
                cell.show(title: column.name, detail: column.columnType, style: .column)
            case .routine(let routine):
                cell.show(title: routine.name, icon: Self.routineIcon,
                          help: "\(routine.isProcedure ? "Procedure" : "Function") \(routine.name)(\(routine.arguments))")
            case .message(let text):
                cell.show(title: text, help: text, style: .message)
            }
            return cell
        }

        func outlineView(_ outlineView: NSOutlineView, rowViewForItem item: Any) -> NSTableRowView? {
            outlineView.makeView(withIdentifier: SidebarRowView.id, owner: nil) as? SidebarRowView ?? SidebarRowView()
        }

        func outlineView(_ outlineView: NSOutlineView, shouldSelectItem item: Any) -> Bool {
            guard let node = item as? Node else { return false }
            switch node.kind {
            case .relation, .routine: return true
            case .section, .column, .message: return false
            }
        }

        func outlineViewSelectionDidChange(_ notification: Notification) {
            guard !isApplying, let outline, let node = outline.item(atRow: outline.selectedRow) as? Node else { return }
            onOpen?(node.id)
        }

        func outlineViewItemDidExpand(_ notification: Notification) {
            guard !isApplying, let node = notification.userInfo?["NSObject"] as? Node else { return }
            switch node.kind {
            case .section(let title, _): collapsedSections.remove(title)
            case .relation(let relation):
                expandedRelations.insert(relation.id)
                onExpand?(relation)
            default: break
            }
        }

        func outlineViewItemDidCollapse(_ notification: Notification) {
            guard !isApplying, let node = notification.userInfo?["NSObject"] as? Node else { return }
            switch node.kind {
            case .section(let title, _): collapsedSections.insert(title)
            case .relation(let relation): expandedRelations.remove(relation.id)
            default: break
            }
        }

        // MARK: Icons

        private static var icons: [String: NSImage] = [:]

        /// Grey icons, blue for the open table, as in the SwiftUI list before.
        static func icon(for kind: RelationKind, isOpen: Bool) -> NSImage? {
            let key = "\(kind.symbol)-\(isOpen)"
            if let icon = icons[key] { return icon }
            let color: NSColor = isOpen ? NSColor(srgbRed: 0x4A / 255, green: 0x88 / 255, blue: 0xF7 / 255, alpha: 1) : .secondaryLabelColor
            let base: NSImage?
            switch kind {
            case .table, .partitionedTable, .foreignTable:
                base = LucideIcon.image("sheet", size: 14) ?? NSImage(systemSymbolName: kind.symbol, accessibilityDescription: nil)
            case .view, .materializedView:
                base = NSImage(systemSymbolName: kind.symbol, accessibilityDescription: nil)?
                    .withSymbolConfiguration(.init(pointSize: 12, weight: .regular))
            }
            let icon = base.map { tinted($0, color) }
            icons[key] = icon
            return icon
        }

        static let routineIcon: NSImage? = NSImage(systemSymbolName: "function", accessibilityDescription: nil)
            .map { tinted($0.withSymbolConfiguration(.init(pointSize: 12, weight: .regular)) ?? $0, NSColor(srgbRed: 0xF5 / 255, green: 0xA0 / 255, blue: 0x33 / 255, alpha: 1)) }

        private static func tinted(_ image: NSImage, _ color: NSColor) -> NSImage {
            NSImage(size: image.size, flipped: false) { rect in
                image.draw(in: rect)
                color.set()
                rect.fill(using: .sourceAtop)
                return true
            }
        }
    }
}

/// One sidebar row: icon and name, or a section header, or a column with its
/// type. Frame layout only; rows are made and reused while scrolling.
final class SidebarCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("sidebarCell")

    enum Style {
        case item, header, column, message
    }

    private let icon = NSImageView()
    private let title = NSTextField(labelWithString: "")
    private let detail = NSTextField(labelWithString: "")
    private var style = Style.item

    init() {
        super.init(frame: .zero)
        identifier = Self.id
        for field in [title, detail] {
            field.lineBreakMode = .byTruncatingTail
            field.cell?.usesSingleLineMode = true
            addSubview(field)
        }
        icon.imageScaling = .scaleProportionallyDown
        addSubview(icon)
        textField = title
        imageView = icon
    }

    required init?(coder: NSCoder) {
        nil
    }

    override var canBecomeKeyView: Bool { false }

    func show(title text: String, detail detailText: String = "", icon image: NSImage? = nil, help: String? = nil, style: Style = .item) {
        self.style = style
        title.stringValue = text
        detail.stringValue = detailText
        detail.isHidden = detailText.isEmpty
        icon.image = image
        icon.isHidden = image == nil
        toolTip = help
        switch style {
        case .item:
            title.font = AppFont.mono(size: 12)
            title.textColor = .labelColor
        case .header:
            title.font = .systemFont(ofSize: 11, weight: .semibold)
            title.textColor = NSColor.labelColor.withAlphaComponent(0.7)
            detail.font = .systemFont(ofSize: 11, weight: .semibold)
            detail.textColor = .secondaryLabelColor
        case .column:
            title.font = AppFont.mono(size: 11)
            title.textColor = .labelColor
            detail.font = AppFont.mono(size: 10)
            detail.textColor = .secondaryLabelColor
        case .message:
            title.font = .systemFont(ofSize: 12)
            title.textColor = .secondaryLabelColor
        }
        needsLayout = true
    }

    override func layout() {
        super.layout()
        let height = bounds.height
        func place(_ field: NSTextField, x: CGFloat, width: CGFloat) {
            let fieldHeight = ceil(field.intrinsicContentSize.height)
            field.frame = NSRect(x: x, y: (height - fieldHeight) / 2, width: max(width, 0), height: fieldHeight)
        }
        var x: CGFloat = 2
        if !icon.isHidden {
            icon.frame = NSRect(x: x, y: (height - 16) / 2, width: 16, height: 16)
            x += 22
        }
        switch style {
        case .header:
            // Measured from the text itself: a reused cell's intrinsic size can
            // still be the old font's.
            let titleWidth = ceil(title.attributedStringValue.size().width) + 4
            place(title, x: x, width: titleWidth)
            place(detail, x: x + titleWidth + 4, width: bounds.width - x - titleWidth - 8)
        case .column:
            let detailWidth = min(ceil(detail.intrinsicContentSize.width), bounds.width * 0.45)
            place(detail, x: bounds.width - detailWidth - 8, width: detailWidth)
            place(title, x: x, width: bounds.width - x - detailWidth - 14)
        case .item, .message:
            place(title, x: x, width: bounds.width - x - 8)
        }
    }
}

/// A light, rounded selection, as the SwiftUI sidebar had, not solid blue.
final class SidebarRowView: NSTableRowView {
    static let id = NSUserInterfaceItemIdentifier("sidebarRow")

    override init(frame: NSRect) {
        super.init(frame: frame)
        identifier = Self.id
    }

    required init?(coder: NSCoder) {
        nil
    }

    /// The selection is light grey, so text keeps its normal colors; the
    /// default turns it white for a solid blue selection.
    override var interiorBackgroundStyle: NSView.BackgroundStyle { .normal }

    override func drawSelection(in dirtyRect: NSRect) {
        guard selectionHighlightStyle != .none else { return }
        NSColor.labelColor.withAlphaComponent(isEmphasized ? 0.1 : 0.07).setFill()
        NSBezierPath(roundedRect: bounds.insetBy(dx: 6, dy: 1), xRadius: 5, yRadius: 5).fill()
    }
}

/// The sidebar's right-click menus. Truncate and Drop show their SQL and ask
/// first; on a production connection the name must be typed.
@MainActor
enum SidebarActions {
    static func items(for kind: SidebarOutline.Node.Kind, workspace: WorkspaceModel) -> [NSMenuItem] {
        let canWrite = !workspace.connection.readOnly
        switch kind {
        case .relation(let relation):
            let isTable = [.table, .partitionedTable, .foreignTable].contains(relation.kind)
            var items: [NSMenuItem] = [
                ClosureMenuItem(title: "Open") { workspace.open(relation) },
                ClosureMenuItem(title: "Open Structure") { workspace.openStructure(relation) },
                ClosureMenuItem(title: isTable ? "Query This Table" : "Query This View") {
                    workspace.newQuery(sql: "SELECT * FROM \(qualifiedName(relation: relation, engine: workspace.session.engine()))\nLIMIT 100;")
                },
                .separator(),
                ClosureMenuItem(title: "Copy Name") { copy(relation.name) },
                ClosureMenuItem(title: "Copy Qualified Name") { copy(qualifiedName(relation: relation, engine: workspace.session.engine())) },
                ClosureMenuItem(title: "Export…") { workspace.export(relation) },
            ]
            if relation.kind == .materializedView {
                items.append(.separator())
                items.append(enabled(canWrite, ClosureMenuItem(title: "Refresh Data") {
                    workspace.runChange(refreshMaterializedViewSql(relation: relation), refreshing: relation)
                }))
            }
            items.append(.separator())
            if relation.kind == .table || relation.kind == .partitionedTable {
                items.append(enabled(canWrite, ClosureMenuItem(title: "Truncate…") { confirmTruncate(relation, workspace) }))
            }
            items.append(enabled(canWrite, ClosureMenuItem(title: "Drop…") { confirmDrop(relation, workspace) }))
            return items
        case .column(let column):
            return [ClosureMenuItem(title: "Copy Name") { copy(column.name) }]
        case .routine(let routine):
            return [
                ClosureMenuItem(title: "Open Definition") { workspace.open(routine) },
                ClosureMenuItem(title: "Copy Name") { copy(routine.name) },
                .separator(),
                enabled(canWrite, ClosureMenuItem(title: "Drop…") {
                    DangerConfirm.run(
                        title: "Drop \(routine.isProcedure ? "procedure" : "function") \(routine.name)?",
                        typeToConfirm: workspace.isProduction ? routine.name : nil,
                        options: ["Cascade"], button: "Drop",
                        sql: { options in dropRoutineSql(routine: routine, cascade: options[0]) }
                    ) { sql, _ in workspace.runChange(sql) }
                }),
            ]
        case .section(let title, _) where title == "Tables":
            return [enabled(canWrite, ClosureMenuItem(title: "New Table…") { workspace.showsNewTable = true })]
        case .section, .message:
            return []
        }
    }

    private static func confirmTruncate(_ relation: Relation, _ workspace: WorkspaceModel) {
        DangerConfirm.run(
            title: "Delete every row of \(relation.name)?",
            typeToConfirm: workspace.isProduction ? relation.name : nil,
            // SQLite has neither: its DELETE has no such options.
            options: workspace.session.engine() == .sqlite ? []
                : ["Restart identity (reset sequences)", "Cascade (also tables that reference it)"],
            button: "Truncate",
            sql: { options in
                truncateSql(relation: relation, restartIdentity: options.first ?? false, cascade: options.dropFirst().first ?? false,
                            engine: workspace.session.engine())
            }
        ) { sql, _ in workspace.runChange(sql, refreshing: relation) }
    }

    private static func confirmDrop(_ relation: Relation, _ workspace: WorkspaceModel) {
        DangerConfirm.run(
            title: "Drop \(relation.name)?",
            typeToConfirm: workspace.isProduction ? relation.name : nil,
            options: workspace.session.engine() == .sqlite ? [] : ["Cascade (also objects that depend on it)"],
            button: "Drop",
            sql: { options in dropRelationSql(relation: relation, cascade: options.first ?? false, engine: workspace.session.engine()) }
        ) { sql, _ in workspace.runChange(sql, dropping: relation) }
    }

    private static func copy(_ text: String) {
        NSPasteboard.general.clearContents()
        NSPasteboard.general.setString(text, forType: .string)
    }

    private static func enabled(_ isEnabled: Bool, _ item: NSMenuItem) -> NSMenuItem {
        item.isEnabled = isEnabled
        return item
    }
}

/// A confirmation for a change that can't be undone: the SQL it will run,
/// options that change that SQL, and on production a name to type before
/// the button works. A sheet on the key window, so it opens where the user
/// is; `confirmed` gets the SQL to run and the options chosen, and isn't
/// called on Cancel.
@MainActor
enum DangerConfirm {
    static func run(title: String, typeToConfirm: String?, options: [String], button: String,
                    sql: @escaping ([Bool]) -> String, confirmed: @escaping (String, [Bool]) -> Void) {
        let parent = NSApp.keyWindow ?? NSApp.mainWindow
        let panel = NSPanel(contentRect: .zero, styleMask: [.titled, .fullSizeContentView], backing: .buffered, defer: false)
        panel.titlebarAppearsTransparent = true
        let close = { [weak panel, weak parent] (response: NSApplication.ModalResponse) in
            guard let panel else { return }
            if let parent { parent.endSheet(panel, returnCode: response) } else { NSApp.stopModal(withCode: response) }
        }
        let sheet = DangerSheet(title: title, typeToConfirm: typeToConfirm, options: options, button: button, sql: sql) { result in
            close(result == nil ? .cancel : .OK)
            if let (statements, chosen) = result { confirmed(statements, chosen) }
        }
        let host = NSHostingController(rootView: sheet)
        host.sizingOptions = .preferredContentSize
        // A sheet shows no title bar; no room is kept for one.
        host.safeAreaRegions = []
        panel.contentViewController = host
        panel.setContentSize(host.view.fittingSize)
        if let parent {
            parent.beginSheet(panel)
        } else {
            NSApp.runModal(for: panel)
            panel.close()
        }
    }
}

/// The confirmation itself: a header with the warning beside the title,
/// the SQL, the options, and on production the name to type.
private struct DangerSheet: View {
    let title: String
    let typeToConfirm: String?
    let options: [String]
    let button: String
    let sql: ([Bool]) -> String
    /// The SQL and options on confirm; nil on Cancel.
    let finish: (((String, [Bool]))?) -> Void

    @State private var chosen: [Bool]
    @State private var typed = ""

    init(title: String, typeToConfirm: String?, options: [String], button: String,
         sql: @escaping ([Bool]) -> String, finish: @escaping (((String, [Bool]))?) -> Void) {
        self.title = title
        self.typeToConfirm = typeToConfirm
        self.options = options
        self.button = button
        self.sql = sql
        self.finish = finish
        _chosen = State(initialValue: options.map { _ in false })
    }

    private static let maxSQLHeight: CGFloat = 220

    /// The SQL's height when wrapped to `width`, up to the most shown
    /// before it scrolls. Monospaced, so lines wrap at a fixed count.
    private static func height(of sql: String, width: CGFloat) -> CGFloat {
        let font = AppFont.mono(size: 12)
        let characterWidth = ("M" as NSString).size(withAttributes: [.font: font]).width
        let perLine = max(1, Int(width / max(characterWidth, 1)))
        let lines = sql.split(separator: "\n", omittingEmptySubsequences: false)
            .reduce(0) { $0 + max(1, ($1.count + perLine - 1) / perLine) }
        let lineHeight = ceil(font.ascender - font.descender + font.leading)
        return min(CGFloat(lines) * lineHeight + 16, maxSQLHeight)
    }

    var body: some View {
        let statements = sql(chosen)
        // Long SQL (a SQLite rebuild repeats the whole table) gets more width, then scrolls.
        let width: CGFloat = statements.split(separator: "\n").count > 6 ? 460 : 320
        let code = Text(SQLSyntax.highlighted(statements))
            .font(AppFont.monoFont(size: 12))
            .textSelection(.enabled)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(8)
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 12) {
                Image(nsImage: NSImage(named: NSImage.cautionName) ?? NSImage())
                    .resizable()
                    .frame(width: 40, height: 40)
                VStack(alignment: .leading, spacing: 4) {
                    Text(title).font(.system(size: 13, weight: .semibold))
                    Text("This can't be undone. TablePort will run:")
                        .font(.system(size: 12))
                        .foregroundStyle(.secondary)
                }
                .fixedSize(horizontal: false, vertical: true)
            }
            ScrollView { code }
                .frame(height: Self.height(of: statements, width: width - 40 - 16))
                .background(Color(nsColor: .quaternarySystemFill), in: RoundedRectangle(cornerRadius: 6))
            ForEach(options.indices, id: \.self) { index in
                Toggle(options[index], isOn: $chosen[index])
            }
            if let typeToConfirm {
                Text("This is a production connection. Type \(typeToConfirm) to confirm.")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
                TextField(typeToConfirm, text: $typed)
                    .textFieldStyle(.roundedBorder)
            }
            HStack {
                Spacer()
                // Return cancels, as in a stock critical alert.
                Button("Cancel") { finish(nil) }
                    .keyboardShortcut(.defaultAction)
                Button(button, role: .destructive) { finish((statements, chosen)) }
                    .foregroundStyle(Theme.danger)
                    .disabled(typeToConfirm.map { typed != $0 } ?? false)
            }
        }
        .padding(20)
        .frame(width: width)
    }
}
