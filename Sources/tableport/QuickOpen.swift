import MacKit
import SwiftUI
import TablePortCore

/// Quick open (⌘P): type part of a name to open a table, view, function or
/// saved query. Matching and ranking happen in the core (`rankNames`).
struct QuickOpenView: View {
    @ObservedObject var model: WorkspaceModel
    @ObservedObject var sidebar: SidebarModel
    @ObservedObject var saved: SavedQueries

    @State private var query = ""
    @State private var selection = 0
    @FocusState private var focused: Bool

    private static let maxResults = 50
    private static let rowHeight: CGFloat = 30

    private struct Item: Identifiable {
        enum Target {
            case relation(Relation)
            case routine(Routine)
            case saved(SavedQuery)
        }

        let id: String
        let name: String
        let detail: String
        let target: Target
    }

    /// Everything that can be opened: relations of every schema, the
    /// functions loaded so far, and the saved queries.
    private var items: [Item] {
        var items: [Item] = []
        for schema in sidebar.schemas {
            for relation in schema.relations {
                items.append(Item(id: "r:\(relation.id)", name: relation.name, detail: relation.schema, target: .relation(relation)))
            }
        }
        for (schema, routines) in sidebar.routines.sorted(by: { $0.key < $1.key }) {
            for routine in routines {
                items.append(Item(id: "f:\(routine.oid)", name: routine.name, detail: "\(schema) · \(routine.arguments)",
                                  target: .routine(routine)))
            }
        }
        for query in saved.items {
            items.append(Item(id: "q:\(query.id)", name: query.name, detail: "saved query", target: .saved(query)))
        }
        return items
    }

    private var results: [Item] {
        let all = items
        return rankNames(query: query, names: all.map(\.name))
            .prefix(Self.maxResults)
            .compactMap { all.indices.contains(Int($0)) ? all[Int($0)] : nil }
    }

    var body: some View {
        let results = results
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Image(systemName: "magnifyingglass")
                    .foregroundStyle(Theme.inkSecondary)
                TextField("Open a table, view, function or saved query", text: $query)
                    .textFieldStyle(.plain)
                    .font(.system(size: 16))
                    .focused($focused)
                    .onSubmit { open(results) }
                    .onKeyPress(.downArrow) { move(1, in: results); return .handled }
                    .onKeyPress(.upArrow) { move(-1, in: results); return .handled }
                    .onKeyPress(.escape) { close(); return .handled }
            }
            .padding(.horizontal, 14)
            .frame(height: 46)

            if !results.isEmpty {
                Divider()
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(spacing: 0) {
                            ForEach(Array(results.enumerated()), id: \.element.id) { index, item in
                                row(item, selected: index == selection)
                                    .contentShape(Rectangle())
                                    .onTapGesture {
                                        selection = index
                                        open(results)
                                    }
                            }
                        }
                        .padding(5)
                    }
                    .frame(height: min(CGFloat(results.count), 10) * Self.rowHeight + 10)
                    .onChange(of: selection) { _, row in
                        if results.indices.contains(row) { proxy.scrollTo(results[row].id) }
                    }
                }
            } else if !query.isEmpty {
                Divider()
                Text("Nothing matches “\(query)”")
                    .font(Theme.caption)
                    .foregroundStyle(Theme.inkSecondary)
                    .frame(maxWidth: .infinity)
                    .frame(height: 44)
            }
        }
        .frame(width: 580)
        .background(Theme.card)
        .clipShape(RoundedRectangle(cornerRadius: 12))
        .overlay(RoundedRectangle(cornerRadius: 12).stroke(Theme.hairline))
        .shadow(color: .black.opacity(0.18), radius: 24, y: 8)
        .onAppear { focused = true }
        .onChange(of: query) { selection = 0 }
    }

    private func row(_ item: Item, selected: Bool) -> some View {
        HStack(spacing: 10) {
            icon(item.target)
                .frame(width: 16)
                .foregroundStyle(selected ? .white : Theme.inkSecondary)
            Text(item.name)
                .font(AppFont.monoFont(size: 13))
                .foregroundStyle(selected ? .white : Theme.ink)
                .lineLimit(1)
            Spacer(minLength: 12)
            Text(item.detail)
                .font(Theme.meta)
                .foregroundStyle(selected ? .white.opacity(0.8) : Theme.inkSecondary)
                .lineLimit(1)
                .truncationMode(.middle)
        }
        .padding(.horizontal, 10)
        .frame(height: Self.rowHeight)
        .background(selected ? Theme.accent : .clear, in: RoundedRectangle(cornerRadius: 6))
    }

    @ViewBuilder
    private func icon(_ target: Item.Target) -> some View {
        switch target {
        case .relation(let relation):
            if relation.kind == .view || relation.kind == .materializedView {
                Image(systemName: "eye")
            } else if let sheet = LucideIcon.image("sheet", size: 14) {
                Image(nsImage: sheet).renderingMode(.template)
            } else {
                Image(systemName: "tablecells")
            }
        case .routine:
            Image(systemName: "function")
        case .saved:
            Image(systemName: "doc.text")
        }
    }

    private func move(_ delta: Int, in results: [Item]) {
        guard !results.isEmpty else { return }
        selection = min(max(selection + delta, 0), results.count - 1)
    }

    private func open(_ results: [Item]) {
        guard results.indices.contains(selection) else { return }
        let target = results[selection].target
        close()
        switch target {
        case .relation(let relation): model.open(relation)
        case .routine(let routine): model.open(routine)
        case .saved(let query): model.open(query)
        }
    }

    private func close() {
        model.showsQuickOpen = false
    }
}
