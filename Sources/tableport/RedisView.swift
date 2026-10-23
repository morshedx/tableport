import MacKit
import SwiftUI
import TablePortCore

/// A Redis connection's state: the logical database, the keys found so far,
/// the selected key's value and the console.
@MainActor
final class RedisModel: ObservableObject {
    let connection: SavedConnection
    let session: DatabaseSession

    @Published var databases: [RedisDatabase] = []
    @Published var database: UInt32
    @Published var search = ""
    @Published private(set) var keys: [RedisKey] = []
    /// SCAN's cursor; 0 once every key has been seen.
    @Published private(set) var cursor: UInt64 = 0
    @Published private(set) var isScanning = false
    @Published var keysError: String?

    @Published var selected: String?
    @Published private(set) var value: RedisValue?
    @Published var valueError: String?
    @Published var draft = ""
    @Published var tab: Tab = .value

    @Published var console: [ConsoleEntry] = []
    @Published var review: Review?
    @Published var editor: ItemEditor?

    enum Tab: String, CaseIterable {
        case value = "Value"
        case console = "Console"
    }

    struct ConsoleEntry: Identifiable {
        let id = UUID()
        let command: String
        let reply: String
        let failed: Bool
    }

    /// Changes waiting for the user to look at their commands and apply them.
    struct Review: Identifiable {
        let id = UUID()
        let title: String
        let changes: [RedisChange]
        let commands: [String]
    }

    /// The small form for adding or editing one element.
    struct ItemEditor: Identifiable {
        enum Mode {
            case field(original: String?)
            case listItem(index: Int64?)
            case member
            case scoredMember
            case ttl
            case rename
        }

        let id = UUID()
        let mode: Mode
        var first = ""
        var second = ""
    }

    static let itemLimit: UInt32 = 500
    private static let pageSize: UInt32 = 500
    private var scanTask: Task<Void, Never>?
    private var valueTask: Task<Void, Never>?

    init(connection: SavedConnection, session: DatabaseSession) {
        self.connection = connection
        self.session = session
        database = UInt32(connection.database.trimmingCharacters(in: .whitespaces)) ?? 0
    }

    /// A word is searched anywhere in key names; a glob (*, ?, [) is used as is.
    private var pattern: String {
        let text = search.trimmingCharacters(in: .whitespaces)
        if text.isEmpty { return "*" }
        return text.contains(where: { "*?[".contains($0) }) ? text : "*\(text)*"
    }

    func reload() {
        let session = session
        Task {
            do {
                databases = try await session.redisDatabases()
            } catch {
                keysError = error.displayMessage
            }
        }
        rescan()
    }

    func switchDatabase(to index: UInt32) {
        guard index != database else { return }
        database = index
        selected = nil
        value = nil
        rescan()
    }

    /// Starts the key list again from the first page.
    func rescan() {
        keys = []
        cursor = 0
        keysError = nil
        scan()
    }

    /// The next page of keys, until a page brings some or SCAN is done.
    func scan() {
        scanTask?.cancel()
        isScanning = true
        let session = session, database = database, pattern = pattern
        var cursor = cursor
        scanTask = Task {
            do {
                var found: [RedisKey] = []
                repeat {
                    let page = try await session.redisScan(database: database, pattern: pattern, cursor: cursor, count: Self.pageSize)
                    guard !Task.isCancelled else { return }
                    found += page.keys
                    cursor = page.cursor
                } while cursor != 0 && found.count < Int(Self.pageSize)
                keys += found
                self.cursor = cursor
            } catch {
                guard !Task.isCancelled else { return }
                keysError = error.displayMessage
            }
            isScanning = false
        }
    }

    func select(_ key: String?) {
        selected = key
        tab = .value
        loadValue()
    }

    func loadValue() {
        valueTask?.cancel()
        guard let key = selected else {
            value = nil
            return
        }
        let session = session, database = database
        valueTask = Task {
            do {
                let loaded = try await session.redisValue(database: database, key: key, limit: Self.itemLimit)
                guard !Task.isCancelled else { return }
                value = loaded
                draft = loaded.text ?? ""
                valueError = nil
            } catch {
                guard !Task.isCancelled else { return }
                value = nil
                valueError = error.displayMessage
            }
        }
    }

    /// Shows the changes' commands; Apply runs them.
    func propose(_ title: String, _ changes: [RedisChange]) {
        do {
            review = Review(title: title, changes: changes, commands: try redisChangeCommands(changes: changes))
        } catch {
            valueError = error.displayMessage
        }
    }

    func apply(_ review: Review) async throws {
        try await session.redisApply(database: database, changes: review.changes)
        self.review = nil
        let renamed = review.changes.compactMap { change -> String? in
            if case .renameKey(_, let newName) = change { return newName }
            return nil
        }.first
        let deleted = review.changes.contains { if case .deleteKey = $0 { true } else { false } }
        if deleted || renamed != nil {
            selected = renamed
            rescan()
        }
        loadValue()
        databases = (try? await session.redisDatabases()) ?? databases
    }

    func run(_ line: String) {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty else { return }
        let session = session, database = database
        Task {
            do {
                let reply = try await session.redisCommand(database: database, line: trimmed)
                console.append(ConsoleEntry(command: trimmed, reply: reply, failed: false))
            } catch {
                console.append(ConsoleEntry(command: trimmed, reply: error.displayMessage, failed: true))
            }
            // A command may have changed the selected key.
            if selected != nil { loadValue() }
        }
    }

    /// The change an item editor's form makes.
    func changes(for editor: ItemEditor) -> [RedisChange]? {
        guard let key = selected, let value else { return nil }
        let first = editor.first, second = editor.second
        switch editor.mode {
        case .field(let original):
            var changes: [RedisChange] = [.setField(key: key, field: first, value: second)]
            // A renamed field: the old one goes.
            if let original, original != first { changes.append(.deleteField(key: key, field: original)) }
            return first.isEmpty ? nil : changes
        case .listItem(let index):
            if let index { return [.setListItem(key: key, index: index, value: second)] }
            return [.pushListItem(key: key, value: second)]
        case .member:
            return first.isEmpty ? nil : [.addMember(key: key, member: first, score: nil)]
        case .scoredMember:
            guard !first.isEmpty, let score = Double(second.trimmingCharacters(in: .whitespaces)) else { return nil }
            return [.addMember(key: key, member: first, score: score)]
        case .ttl:
            let text = first.trimmingCharacters(in: .whitespaces)
            if text.isEmpty { return [.setTtl(key: key, seconds: nil)] }
            guard let seconds = Int64(text), seconds > 0 else { return nil }
            return [.setTtl(key: key, seconds: seconds)]
        case .rename:
            let name = first.trimmingCharacters(in: .whitespaces)
            return name.isEmpty || name == value.key ? nil : [.renameKey(key: key, newName: name)]
        }
    }
}

// MARK: - Views

/// Below the header: the key tree, and the selected key or the console.
struct RedisWorkspace: View {
    @ObservedObject var model: RedisModel

    var body: some View {
        HSplitView {
            RedisSidebar(model: model)
                .frame(minWidth: 200, idealWidth: 280, maxWidth: 460)
            VStack(spacing: 0) {
                Picker("View", selection: $model.tab) {
                    ForEach(RedisModel.Tab.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                }
                .pickerStyle(.segmented)
                .labelsHidden()
                .fixedSize()
                .padding(.vertical, 8)
                Divider()
                switch model.tab {
                case .value: RedisValueView(model: model)
                case .console: RedisConsole(model: model)
                }
            }
            .frame(minWidth: 380)
            .background(Theme.card)
        }
        .sheet(item: $model.review) { review in
            RedisReviewSheet(model: model, review: review)
        }
        .sheet(item: $model.editor) { editor in
            RedisItemSheet(model: model, editor: editor)
        }
    }
}

/// Keys grouped into folders by ":", as in "user:42:name".
private struct KeyNode: Identifiable {
    let id: String
    let label: String
    let key: RedisKey?
    var children: [KeyNode]?
    var count = 0

    static func tree(_ keys: [RedisKey]) -> [KeyNode] {
        final class Folder {
            var folders: [String: Folder] = [:]
            var leaves: [RedisKey] = []
            var count = 0
        }
        let root = Folder()
        for key in keys {
            let parts = key.name.split(separator: ":", omittingEmptySubsequences: false).map(String.init)
            var folder = root
            folder.count += 1
            for part in parts.dropLast() {
                let next = folder.folders[part] ?? Folder()
                folder.folders[part] = next
                folder = next
                folder.count += 1
            }
            folder.leaves.append(key)
        }
        func nodes(_ folder: Folder, path: String) -> [KeyNode] {
            let folders = folder.folders.sorted { $0.key < $1.key }.map { name, child in
                let id = path + name + ":"
                return KeyNode(id: "folder:" + id, label: name, key: nil, children: nodes(child, path: id), count: child.count)
            }
            let leaves = folder.leaves.sorted { $0.name < $1.name }.map { key in
                KeyNode(id: "key:" + key.name, label: String(key.name.dropFirst(path.count)), key: key, children: nil)
            }
            return folders + leaves
        }
        return nodes(root, path: "")
    }
}

private struct RedisSidebar: View {
    @ObservedObject var model: RedisModel

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 6) {
                TextField("Search keys", text: $model.search, prompt: Text("Search keys, or a pattern such as user:*"))
                    .textFieldStyle(.roundedBorder)
                    .onSubmit { model.rescan() }
                Button {
                    model.reload()
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.borderless)
                .help("Reload the keys")
            }
            .padding(8)
            Divider()
            if let error = model.keysError {
                Text(error).font(Theme.caption).foregroundStyle(Theme.danger).padding(10)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            List(selection: Binding(get: { model.selected.map { "key:" + $0 } }, set: { id in
                model.select(id.flatMap { $0.hasPrefix("key:") ? String($0.dropFirst(4)) : nil })
            })) {
                OutlineGroup(KeyNode.tree(model.keys), children: \.children) { node in
                    KeyRow(node: node).tag(node.id)
                }
                if model.cursor != 0 {
                    Button(model.isScanning ? "Loading…" : "Load more keys") { model.scan() }
                        .buttonStyle(.link)
                        .disabled(model.isScanning)
                }
            }
            .listStyle(.sidebar)
            // The list's own material is darker than the SQL sidebar, more so in a background window.
            .scrollContentBackground(.hidden)
            .overlay {
                if model.keys.isEmpty && !model.isScanning && model.keysError == nil {
                    Text(model.search.isEmpty ? "No keys in this database" : "No keys match")
                        .font(Theme.caption)
                        .foregroundStyle(Theme.inkSecondary)
                }
            }
            Divider()
            HStack {
                Text("\(model.keys.count)\(model.cursor != 0 ? "+" : "") keys")
                Spacer()
                Menu {
                    ForEach(model.databases, id: \.index) { database in
                        Button {
                            model.switchDatabase(to: database.index)
                        } label: {
                            if database.index == model.database {
                                Label("db\(database.index) · \(database.keys) keys", systemImage: "checkmark")
                            } else {
                                Text("db\(database.index) · \(database.keys) keys")
                            }
                        }
                    }
                } label: {
                    Text("db\(model.database)")
                }
                .fixedSize()
                .help("Logical database")
            }
            .font(Theme.meta)
            .foregroundStyle(Theme.inkSecondary)
            .padding(.horizontal, 10)
            .frame(height: 30)
        }
        .background(SidebarView.background)
    }
}

private struct KeyRow: View {
    let node: KeyNode

    var body: some View {
        HStack(spacing: 6) {
            if let key = node.key {
                KindBadge(kind: key.kind)
                Text(node.label.isEmpty ? "(empty)" : node.label)
                    .font(AppFont.monoFont(size: 12))
                    .lineLimit(1)
            } else {
                Image(systemName: "folder").foregroundStyle(Theme.inkSecondary).frame(width: 30)
                Text(node.label.isEmpty ? "(empty)" : node.label)
                    .font(AppFont.monoFont(size: 12))
                    .lineLimit(1)
                Spacer()
                Text("\(node.count)").font(Theme.meta).foregroundStyle(Theme.inkSecondary)
            }
        }
    }
}

/// The value type as a short coloured tag, as in other Redis tools.
private struct KindBadge: View {
    let kind: RedisKeyKind

    private var label: (String, Color) {
        switch kind {
        case .string: ("STR", Theme.Tile.blue)
        case .hash: ("HASH", Theme.Tile.purple)
        case .list: ("LIST", Theme.Tile.green)
        case .set: ("SET", Theme.Tile.orange)
        case .sortedSet: ("ZSET", Theme.Tile.red)
        case .stream: ("STRM", Theme.Tile.teal)
        case .json: ("JSON", Theme.Tile.blue)
        case .other: ("?", Theme.Tile.gray)
        }
    }

    var body: some View {
        Text(label.0)
            .font(.system(size: 9, weight: .bold, design: .rounded))
            .foregroundStyle(label.1)
            .frame(width: 30, height: 16)
            .background(RoundedRectangle(cornerRadius: 4).fill(label.1.opacity(0.14)))
    }
}

private struct RedisValueView: View {
    @ObservedObject var model: RedisModel

    var body: some View {
        if let value = model.value {
            VStack(alignment: .leading, spacing: 0) {
                header(value)
                Divider()
                content(value)
            }
        } else if let error = model.valueError {
            Text(error).font(Theme.caption).foregroundStyle(Theme.danger).padding()
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        } else {
            Text(model.selected == nil ? "Select a key" : "Loading…")
                .font(Theme.caption)
                .foregroundStyle(Theme.inkSecondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
    }

    private func header(_ value: RedisValue) -> some View {
        HStack(spacing: 10) {
            KindBadge(kind: value.kind)
            Text(value.key)
                .font(AppFont.monoFont(size: 13))
                .textSelection(.enabled)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer()
            Text(ttlText(value.ttlSeconds)).font(Theme.meta).foregroundStyle(Theme.inkSecondary)
            Text(lengthText(value)).font(Theme.meta).foregroundStyle(Theme.inkSecondary)
            Button("TTL…") { model.editor = .init(mode: .ttl, first: value.ttlSeconds.map(String.init) ?? "") }
            Button("Rename…") { model.editor = .init(mode: .rename, first: value.key) }
            Button {
                deleteKey(value.key)
            } label: {
                Image(systemName: "trash")
            }
            .help("Delete the key")
            Button {
                model.loadValue()
            } label: {
                Image(systemName: "arrow.clockwise")
            }
            .help("Reload the value")
        }
        .controlSize(.small)
        .padding(.horizontal, 12)
        .frame(height: 40)
    }

    @ViewBuilder
    private func content(_ value: RedisValue) -> some View {
        switch value.kind {
        case .string:
            VStack(alignment: .leading, spacing: 8) {
                TextEditor(text: $model.draft)
                    .font(AppFont.monoFont(size: 12))
                    .scrollContentBackground(.hidden)
                    .padding(6)
                    .background(Theme.windowBg, in: RoundedRectangle(cornerRadius: 6))
                HStack {
                    if let formatted = formatJson(text: model.draft), formatted != model.draft {
                        Button("Format JSON") { model.draft = formatted }
                    }
                    Spacer()
                    Button("Revert") { model.draft = value.text ?? "" }
                        .disabled(model.draft == (value.text ?? ""))
                    Button("Save") { model.propose("Save \(value.key)", [.setString(key: value.key, value: model.draft)]) }
                        .keyboardShortcut("s", modifiers: .command)
                        .disabled(model.draft == (value.text ?? ""))
                }
                .controlSize(.small)
            }
            .padding(12)
        case .json:
            ScrollView {
                Text(formatJson(text: value.text ?? "") ?? value.text ?? "")
                    .font(AppFont.monoFont(size: 12))
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(12)
            }
        case .other:
            Text("TablePort doesn't show \(value.typeName) values yet. Use the console.")
                .font(Theme.caption)
                .foregroundStyle(Theme.inkSecondary)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .hash, .list, .set, .sortedSet, .stream:
            ItemsTable(model: model, value: value)
        }
    }

    private func deleteKey(_ key: String) {
        let model = model
        DangerConfirm.run(
            title: "Delete the key \(key)?",
            typeToConfirm: model.connection.tag == .production ? key : nil,
            options: [],
            button: "Delete",
            sql: { _ in (try? redisChangeCommands(changes: [.deleteKey(key: key)]))?.joined(separator: "\n") ?? "" }
        ) { _, _ in
            Task {
                do {
                    try await model.apply(.init(title: "", changes: [.deleteKey(key: key)], commands: []))
                } catch {
                    model.valueError = error.displayMessage
                }
            }
        }
    }

    private func ttlText(_ seconds: Int64?) -> String {
        guard let seconds else { return "No expiry" }
        let formatter = DateComponentsFormatter()
        formatter.allowedUnits = [.day, .hour, .minute, .second]
        formatter.unitsStyle = .abbreviated
        formatter.maximumUnitCount = 2
        return "Expires in " + (formatter.string(from: TimeInterval(seconds)) ?? "\(seconds)s")
    }

    private func lengthText(_ value: RedisValue) -> String {
        switch value.kind {
        case .string: "\(value.length) bytes"
        case .json, .other: ""
        default: "\(value.length) items"
        }
    }
}

/// Hash fields, list elements, set and sorted set members, stream entries.
private struct ItemsTable: View {
    @ObservedObject var model: RedisModel
    let value: RedisValue

    private struct Row: Identifiable {
        let id: Int
        let item: RedisItem
    }

    private var rows: [Row] { value.items.enumerated().map { Row(id: $0.offset, item: $0.element) } }
    @State private var selection: Set<Int> = []

    var body: some View {
        VStack(spacing: 0) {
            Table(rows, selection: $selection) {
                TableColumn(firstTitle) { row in
                    Text(row.item.field ?? row.item.value).font(AppFont.monoFont(size: 12)).textSelection(.enabled)
                }
                TableColumn(secondTitle) { row in
                    Text(secondText(row.item)).font(AppFont.monoFont(size: 12)).textSelection(.enabled)
                }
            }
            .contextMenu(forSelectionType: Int.self) { ids in
                if let id = ids.first, rows.indices.contains(id), value.kind != .stream {
                    let item = rows[id].item
                    if value.kind == .hash || value.kind == .list || value.kind == .sortedSet {
                        Button("Edit…") { edit(item) }
                    }
                    Button("Remove") { remove(item) }
                }
            } primaryAction: { ids in
                if let id = ids.first, rows.indices.contains(id), value.kind != .stream { edit(rows[id].item) }
            }
            Divider()
            HStack {
                if UInt64(value.items.count) < value.length {
                    Text("Showing the first \(value.items.count) of \(value.length)")
                        .font(Theme.meta)
                        .foregroundStyle(Theme.inkSecondary)
                }
                Spacer()
                if value.kind != .stream {
                    Button(addTitle) { add() }
                }
            }
            .controlSize(.small)
            .padding(.horizontal, 12)
            .frame(height: 34)
        }
    }

    /// A score as Redis prints it: 640, not 640.0.
    private static func score(_ value: Double) -> String {
        value.rounded() == value && abs(value) < 1e15 ? String(Int64(value)) : String(value)
    }

    private var firstTitle: String {
        switch value.kind {
        case .hash: "field"
        case .list: "index"
        case .stream: "id"
        default: "member"
        }
    }

    private var secondTitle: String {
        switch value.kind {
        case .sortedSet: "score"
        case .set: ""
        case .stream: "fields"
        default: "value"
        }
    }

    private var addTitle: String {
        switch value.kind {
        case .hash: "Add Field"
        case .list: "Append"
        default: "Add Member"
        }
    }

    private func secondText(_ item: RedisItem) -> String {
        switch value.kind {
        case .sortedSet: item.score.map(Self.score) ?? ""
        case .set: ""
        case .hash, .list, .stream: item.field == nil ? "" : item.value
        default: item.value
        }
    }

    private func add() {
        switch value.kind {
        case .hash: model.editor = .init(mode: .field(original: nil))
        case .list: model.editor = .init(mode: .listItem(index: nil))
        case .sortedSet: model.editor = .init(mode: .scoredMember)
        default: model.editor = .init(mode: .member)
        }
    }

    private func edit(_ item: RedisItem) {
        switch value.kind {
        case .hash: model.editor = .init(mode: .field(original: item.field), first: item.field ?? "", second: item.value)
        case .list: model.editor = .init(mode: .listItem(index: item.field.flatMap { Int64($0) }), first: item.field ?? "", second: item.value)
        case .sortedSet: model.editor = .init(mode: .scoredMember, first: item.value, second: item.score.map(Self.score) ?? "")
        default: break
        }
    }

    private func remove(_ item: RedisItem) {
        let key = value.key
        switch value.kind {
        case .hash: model.propose("Remove a field", [.deleteField(key: key, field: item.field ?? "")])
        case .list: model.propose("Remove an element", [.removeListItem(key: key, value: item.value)])
        case .set: model.propose("Remove a member", [.removeMember(key: key, member: item.value, sorted: false)])
        case .sortedSet: model.propose("Remove a member", [.removeMember(key: key, member: item.value, sorted: true)])
        default: break
        }
    }
}

/// The commands a change runs; Apply runs them in one MULTI/EXEC.
private struct RedisReviewSheet: View {
    @ObservedObject var model: RedisModel
    let review: RedisModel.Review
    @State private var error: String?
    @State private var isApplying = false
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(review.title).font(.headline)
            Text(review.commands.joined(separator: "\n"))
                .font(AppFont.monoFont(size: 12))
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(8)
                .background(Theme.windowBg, in: RoundedRectangle(cornerRadius: 6))
            if model.connection.tag == .production {
                Label("Production connection: this changes live data.", systemImage: "exclamationmark.triangle")
                    .font(Theme.meta)
                    .foregroundStyle(Theme.danger)
            }
            if let error {
                Text(error).font(Theme.caption).foregroundStyle(Theme.danger).textSelection(.enabled)
            }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button(isApplying ? "Applying…" : "Apply") {
                    isApplying = true
                    Task {
                        do {
                            try await model.apply(review)
                        } catch {
                            self.error = error.displayMessage
                        }
                        isApplying = false
                    }
                }
                .keyboardShortcut(.defaultAction)
                .disabled(isApplying)
            }
        }
        .padding(20)
        .frame(width: 460)
    }
}

/// Adds or edits one element, sets the TTL, or renames the key.
private struct RedisItemSheet: View {
    @ObservedObject var model: RedisModel
    @State var editor: RedisModel.ItemEditor
    @Environment(\.dismiss) private var dismiss

    private var labels: (title: String, first: String?, second: String?) {
        switch editor.mode {
        case .field(let original): (original == nil ? "Add Field" : "Edit Field", "Field", "Value")
        case .listItem(let index): (index == nil ? "Append to the List" : "Edit Element \(index ?? 0)", nil, "Value")
        case .member: ("Add Member", "Member", nil)
        case .scoredMember: ("Member and Score", "Member", "Score")
        case .ttl: ("Time to Live", "Seconds (empty: never expires)", nil)
        case .rename: ("Rename Key", "New name", nil)
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(labels.title).font(.headline)
            Form {
                if let first = labels.first {
                    TextField(first, text: $editor.first)
                }
                if let second = labels.second {
                    TextField(second, text: $editor.second, axis: .vertical)
                        .lineLimit(1...8)
                }
            }
            .font(AppFont.monoFont(size: 12))
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Review…") {
                    guard let changes = model.changes(for: editor) else { return }
                    dismiss()
                    model.propose(labels.title, changes)
                }
                .keyboardShortcut(.defaultAction)
                .disabled(model.changes(for: editor) == nil)
            }
        }
        .padding(20)
        .frame(width: 420)
    }
}

/// Commands and their replies, as in redis-cli.
private struct RedisConsole: View {
    @ObservedObject var model: RedisModel
    @State private var line = ""
    @State private var history: [String] = []
    @State private var historyIndex: Int?
    @FocusState private var focused: Bool

    var body: some View {
        VStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        if model.console.isEmpty {
                            Text("Run a Redis command, such as SCAN 0 MATCH user:* or HGETALL user:1.")
                                .font(Theme.caption)
                                .foregroundStyle(Theme.inkSecondary)
                        }
                        ForEach(model.console) { entry in
                            VStack(alignment: .leading, spacing: 3) {
                                Text("db\(model.database)> \(entry.command)").foregroundStyle(Theme.accent)
                                Text(entry.reply).foregroundStyle(entry.failed ? Theme.danger : Theme.ink)
                            }
                            .font(AppFont.monoFont(size: 12))
                            .textSelection(.enabled)
                            .id(entry.id)
                        }
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(12)
                }
                .onChange(of: model.console.count) {
                    if let last = model.console.last { proxy.scrollTo(last.id, anchor: .bottom) }
                }
            }
            Divider()
            HStack(spacing: 6) {
                Text("db\(model.database)>").font(AppFont.monoFont(size: 12)).foregroundStyle(Theme.inkSecondary)
                TextField("Command", text: $line, prompt: Text("GET key"))
                    .textFieldStyle(.plain)
                    .font(AppFont.monoFont(size: 12))
                    .focused($focused)
                    .onSubmit {
                        model.run(line)
                        if !line.isEmpty { history.append(line) }
                        historyIndex = nil
                        line = ""
                    }
                    .onKeyPress(.upArrow) { step(-1); return .handled }
                    .onKeyPress(.downArrow) { step(1); return .handled }
            }
            .padding(.horizontal, 12)
            .frame(height: 36)
        }
        .onAppear { focused = true }
    }

    /// Up and Down walk the commands run before, as in a shell.
    private func step(_ delta: Int) {
        guard !history.isEmpty else { return }
        let index = min(max((historyIndex ?? history.count) + delta, 0), history.count)
        historyIndex = index
        line = index < history.count ? history[index] : ""
    }
}
