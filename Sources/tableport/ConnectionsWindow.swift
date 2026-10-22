import MacKit
import SwiftUI

/// The launch window: app info and actions on the left, saved connections on
/// the right. Layout after TablePlus' welcome window.
struct ConnectionsView: View {
    @ObservedObject var store: ConnectionStore
    @ObservedObject var manager: ConnectionManager
    /// Connects, or brings the connection's open window forward.
    let open: (SavedConnection) -> Void
    let closeWindow: (SavedConnection.ID) -> Void
    @State private var search = ""
    @State private var selection: SavedConnection.ID?
    @State private var editing: FormTarget?
    @FocusState private var listFocused: Bool
    @FocusState private var searchFocused: Bool

    var body: some View {
        HStack(spacing: 0) {
            InfoPane(onCreate: { editing = .new })
                .frame(width: 250)
                .background(Theme.sidebarBg)
            Divider()
            Group {
                if store.connections.isEmpty {
                    EmptyConnections(onCreate: { editing = .new })
                } else {
                    connectionList
                }
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Theme.card)
        }
        .ignoresSafeArea()
        // Its own window rather than a sheet, so it can be moved.
        .onChange(of: editing?.id) { _, _ in
            guard let target = editing else { return }
            editing = nil
            ConnectionFormWindow.show(target, store: store, over: NSApp.keyWindow)
        }
        .onChange(of: manager.requestNewConnection, initial: true) { _, requested in
            guard requested else { return }
            manager.requestNewConnection = false
            editing = .new
        }
    }

    private var filtered: [SavedConnection] {
        let query = search.trimmingCharacters(in: .whitespaces)
        guard !query.isEmpty else { return store.connections }
        return store.connections.filter {
            $0.name.localizedCaseInsensitiveContains(query) || $0.host.localizedCaseInsensitiveContains(query)
        }
    }

    private var connectionList: some View {
        VStack(spacing: 0) {
            HStack(spacing: 8) {
                Button { editing = .new } label: {
                    Image(systemName: "plus").font(.system(size: 14, weight: .medium))
                }
                .buttonStyle(.plain)
                .foregroundStyle(Theme.inkSecondary)
                .help("Create Connection")
                TextField("Search for connection…", text: $search)
                    .focused($searchFocused)
                    // ↓ moves into the list; Return connects the top match.
                    .onKeyPress(.downArrow) {
                        guard let first = filtered.first else { return .ignored }
                        selection = first.id
                        listFocused = true
                        return .handled
                    }
                    .onSubmit {
                        if let first = filtered.first { open(first) }
                    }
                    .searchInput()
            }
            .padding(.horizontal, 14)
            .padding(.top, 14)
            .padding(.bottom, 8)
            // Type to search as soon as the window opens.
            .onAppear { searchFocused = true }

            // A plain stack, not a List: a focused List always draws its
            // selection in solid accent blue. Single click selects, double
            // click and Return connect, ↑↓ move, as before.
            ScrollViewReader { scroller in
                ScrollView {
                    LazyVStack(spacing: 2) {
                        ForEach(filtered) { connection in
                            row(connection)
                                .id(connection.id)
                        }
                    }
                    .padding(.horizontal, 8)
                    .padding(.bottom, 8)
                }
                .focusable()
                .focusEffectDisabled()
                .focused($listFocused)
                .onKeyPress(.downArrow) { moveSelection(by: 1, scroller); return .handled }
                .onKeyPress(.upArrow) { moveSelection(by: -1, scroller); return .handled }
                .onKeyPress(.return) {
                    guard let connection = connection(for: selection.map { [$0] } ?? []) else { return .ignored }
                    open(connection)
                    return .handled
                }
            }
            .overlay {
                if filtered.isEmpty {
                    Text("No connection matches “\(search)”.")
                        .font(Theme.caption)
                        .foregroundStyle(Theme.inkSecondary)
                }
            }
        }
    }

    private func row(_ connection: SavedConnection) -> some View {
        ConnectionRow(
            connection: connection,
            state: manager.states[connection.id],
            onCancel: { manager.cancel(connection.id) },
            onRetry: {
                manager.dismissFailure(connection.id)
                open(connection)
            }
        )
        .padding(.horizontal, 8)
        .padding(.vertical, 5)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 6)
                .fill(selection == connection.id ? Theme.ink.opacity(0.07) : Color.clear)
        )
        .contentShape(Rectangle())
        .onTapGesture(count: 2) { open(connection) }
        .simultaneousGesture(TapGesture().onEnded {
            selection = connection.id
            listFocused = true
        })
        // VoiceOver (and other assistive tools) connect with the press action.
        .accessibilityElement(children: .combine)
        .accessibilityAddTraits(.isButton)
        .accessibilityAction { open(connection) }
        .contextMenu {
            Button("Connect") { open(connection) }
            Button("Edit…") { editing = .edit(connection) }
            Button("Duplicate") { store.duplicate(connection) }
            Divider()
            Button("Delete…") { confirmDelete(connection) }
        }
    }

    private func moveSelection(by step: Int, _ scroller: ScrollViewProxy) {
        let ids = filtered.map(\.id)
        guard !ids.isEmpty else { return }
        let current = selection.flatMap { ids.firstIndex(of: $0) }
        let next = current.map { min(max($0 + step, 0), ids.count - 1) } ?? 0
        selection = ids[next]
        scroller.scrollTo(ids[next])
    }

    private func connection(for ids: Set<SavedConnection.ID>) -> SavedConnection? {
        guard ids.count == 1, let id = ids.first else { return nil }
        return store.connections.first { $0.id == id }
    }

    private func confirmDelete(_ connection: SavedConnection) {
        let alert = NSAlert()
        alert.messageText = "Delete “\(connection.name)”?"
        alert.informativeText = "The saved connection is removed. The database itself is not changed."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Delete").hasDestructiveAction = true
        alert.addButton(withTitle: "Cancel")
        if alert.runModal() == .alertFirstButtonReturn {
            manager.cancel(connection.id)
            closeWindow(connection.id)
            store.delete(connection)
            do {
                try Keychain.deleteAll(for: connection.id)
                try LocalData.store?.forgetConnection(connectionId: connection.id.uuidString)
            } catch {
                NSAlert(error: error).runModal()
            }
        }
    }
}

enum FormTarget: Identifiable {
    case new
    case edit(SavedConnection)

    var id: String {
        switch self {
        case .new: "new"
        case .edit(let connection): connection.id.uuidString
        }
    }
}

private struct InfoPane: View {
    let onCreate: () -> Void

    private var version: String {
        let info = Bundle.main.infoDictionary
        let short = info?["CFBundleShortVersionString"] as? String ?? "–"
        let build = info?["CFBundleVersion"] as? String ?? "–"
        return "Version \(short) (\(build))"
    }

    var body: some View {
        VStack(spacing: 0) {
            Spacer().frame(height: 44)
            AppIconImage(size: 96)
            Text(AppInfo.name)
                .font(.system(size: 26, weight: .bold))
                .foregroundStyle(Theme.ink)
                .padding(.top, 8)
            Text(version)
                .font(Theme.caption)
                .foregroundStyle(Theme.inkSecondary)
                .padding(.top, 4)

            Spacer()

            VStack(spacing: 8) {
                // Backup and restore come in phase 2 of the plan.
                PaneButton(title: "Backup Database", symbol: "square.and.arrow.up", action: {})
                    .disabled(true)
                PaneButton(title: "Restore Database", symbol: "square.and.arrow.down", action: {})
                    .disabled(true)
                PaneButton(title: "Create Connection", symbol: "plus.circle", action: onCreate)
            }
            .padding(.horizontal, 18)
            .padding(.bottom, 18)
        }
    }
}

private struct PaneButton: View {
    let title: String
    let symbol: String
    let action: () -> Void
    @Environment(\.isEnabled) private var isEnabled

    var body: some View {
        Button(action: action) {
            HStack(spacing: 10) {
                Image(systemName: symbol).frame(width: 18)
                Text(title)
                Spacer()
            }
            .font(Theme.body)
            .foregroundStyle(Theme.ink)
            .padding(.horizontal, 12)
            .frame(height: 38)
            .background(
                RoundedRectangle(cornerRadius: Theme.radiusLarge)
                    .fill(Theme.card)
                    .overlay(RoundedRectangle(cornerRadius: Theme.radiusLarge).stroke(Theme.hairline))
            )
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .opacity(isEnabled ? 1 : 0.45)
    }
}

private struct ConnectionRow: View {
    let connection: SavedConnection
    let state: ConnectionManager.State?
    let onCancel: () -> Void
    let onRetry: () -> Void
    // Selected rows sit on the accent color, where tag and error colors are unreadable.
    @Environment(\.backgroundProminence) private var prominence

    private var onAccent: Bool { prominence == .increased }

    private func tint(_ color: Color) -> Color {
        onAccent ? .white.opacity(0.9) : color
    }

    var body: some View {
        HStack(spacing: 10) {
            PostgresLogo(size: 26)
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 4) {
                    Text(connection.name)
                        .foregroundStyle(onAccent ? .white : Theme.ink)
                    if let tag = connection.tag {
                        Text("(\(tag.label))").foregroundStyle(tint(tag.color))
                    }
                }
                .font(Theme.body)
                subtitle
                    .font(Theme.meta)
                    .lineLimit(1)
                    .truncationMode(.tail)
            }
            Spacer(minLength: 8)
            trailing
        }
        .padding(.vertical, 3)
    }

    @ViewBuilder
    private var subtitle: some View {
        switch state {
        case .connected(let version):
            Text("Connected · PostgreSQL \(version)").foregroundStyle(tint(Theme.successInk))
        case .failed(let message):
            Text(message).foregroundStyle(tint(Theme.danger)).help(message)
        case .connecting, nil:
            Text(connection.subtitle).foregroundStyle(tint(Theme.inkSecondary))
        }
    }

    @ViewBuilder
    private var trailing: some View {
        switch state {
        case .connecting:
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Button("Cancel", action: onCancel).controlSize(.small)
            }
        case .failed:
            Button("Retry", action: onRetry).controlSize(.small)
        case .connected, nil:
            EmptyView()
        }
    }
}

private struct EmptyConnections: View {
    let onCreate: () -> Void

    var body: some View {
        VStack(spacing: 10) {
            Image(systemName: "cylinder.split.1x2")
                .font(.system(size: 36, weight: .light))
                .foregroundStyle(Theme.inkSecondary)
            Text("No connections yet")
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Theme.ink)
            Text("Create a connection to a PostgreSQL server.")
                .font(Theme.caption)
                .foregroundStyle(Theme.inkSecondary)
            Button("Create Connection", action: onCreate)
                .buttonStyle(PrimaryButtonStyle())
                .fixedSize()
                .padding(.top, 6)
        }
        .padding(24)
    }
}

extension View {
    /// The one look for every search and value field in the app: the
    /// filter bar's small rounded field with the mono font.
    func searchInput() -> some View {
        textFieldStyle(.roundedBorder)
            .controlSize(.small)
            .font(AppFont.monoFont(size: 12))
    }
}
