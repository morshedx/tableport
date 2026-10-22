import AppKit
import Combine
import TablePortCore
import MacKit
import SwiftUI

/// One open connection: sidebar of schemas, tabs, and the session they use.
@MainActor
final class WorkspaceModel: ObservableObject {
    enum Tab: Identifiable, Hashable {
        case relation(Relation)
        case query(number: Int)

        var id: String {
            switch self {
            case .relation(let relation): "relation:\(relation.id)"
            case .query(let number): "query:\(number)"
            }
        }

        var title: String {
            switch self {
            case .relation(let relation): relation.name
            case .query(let number): "Query \(number)"
            }
        }
    }

    enum Load {
        case loading
        case loaded
        case failed(String)
    }

    let connection: SavedConnection
    @Published private(set) var session: DatabaseSession
    /// What the sidebar shows, in its own object: opening and closing tabs
    /// must not make SwiftUI rebuild a list of hundreds of tables.
    let sidebar = SidebarModel()
    /// Names for the query editor's autocomplete, loaded after the sidebar.
    let completion = CompletionSource()
    private var completionTask: Task<Void, Never>?
    var schemas: [SchemaContents] { sidebar.schemas }
    var load: Load { sidebar.load }
    @Published private(set) var databases: [String] = []
    @Published private(set) var switchError: String?
    @Published var tabs: [Tab] = []
    /// The quick open overlay (⌘P).
    @Published var showsQuickOpen = false
    @Published var showsNewTable = false

    @Published var selectedTab: Tab.ID? {
        didSet {
            // nil for a query tab or no tab, so the sidebar shows no table as
            // open and a click on the last one opens it again.
            if case .relation(let relation) = selectedTabValue {
                sidebar.openRelationID = relation.id
            } else {
                sidebar.openRelationID = nil
            }
        }
    }

    private var queryCount = 0
    /// Kept per tab, so switching tabs keeps the page, sort and filter.
    private var tableModels: [Tab.ID: TableTabModel] = [:]
    private var queryModels: [Tab.ID: QueryTabModel] = [:]
    lazy var queryHistory = QueryHistory(connectionID: connection.id)
    private var subscriptions = Set<AnyCancellable>()
    lazy var savedQueries = SavedQueries(connectionID: connection.id)
    private var loadTask: Task<Void, Never>?

    init(connection: SavedConnection, session: DatabaseSession) {
        self.connection = connection
        self.session = session
        reload()
        watchConnection()
    }

    // MARK: Connection lost

    enum LinkState: Equatable {
        case connected
        case lost(String)
        case reconnecting
    }

    @Published private(set) var linkState = LinkState.connected
    private var linkTimer: Timer?
    private var wakeObserver: NSObjectProtocol?

    /// Checks every few seconds whether the connection has ended (the server
    /// closed it, or keepalives found it dead), and pings after the Mac
    /// wakes, when a connection is most often gone.
    private func watchConnection() {
        linkTimer = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { [weak self] _ in
            MainActor.assumeIsolated {
                guard let self, self.linkState == .connected, self.session.isClosed() else { return }
                self.linkState = .lost("The server closed the connection.")
            }
        }
        wakeObserver = NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.didWakeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.checkConnection() }
        }
    }

    /// A round trip now; a failure shows the banner.
    func checkConnection() {
        guard linkState == .connected else { return }
        let session = session
        Task {
            do {
                try await session.ping()
            } catch {
                if linkState == .connected { linkState = .lost(error.displayMessage) }
            }
        }
    }

    /// The banner's Reconnect: a new connection in the same session, so the
    /// tabs, their pending changes and the query text all stay. Then the
    /// sidebar and every table tab load again.
    func reconnect() {
        guard linkState != .reconnecting else { return }
        linkState = .reconnecting
        let session = session
        Task {
            do {
                try await session.reconnect()
                linkState = .connected
                reload()
                for model in tableModels.values { model.reload() }
            } catch {
                linkState = .lost(error.displayMessage)
            }
        }
    }

    var relationCount: Int { schemas.reduce(0) { $0 + $1.relations.count } }

    /// "user@host:port/database" for the status bar.
    var endpointText: String {
        let settings = session.settings()
        let endpoint = "\(settings.user)@\(settings.host):\(settings.port)/\(settings.database)"
        return settings.ssh.map { "\(endpoint) via SSH \($0.user)@\($0.host):\($0.port)" } ?? endpoint
    }

    func reload() {
        loadTask?.cancel()
        sidebar.load = .loading
        let session = session
        loadTask = Task {
            do {
                async let schemas = session.schemas()
                async let databases = session.databases()
                let (loadedSchemas, loadedDatabases) = try await (schemas, databases)
                guard !Task.isCancelled else { return }
                let sidebar = self.sidebar
                sidebar.schemas = loadedSchemas
                self.databases = loadedDatabases
                sidebar.load = .loaded
                if !loadedSchemas.contains(where: { $0.name == sidebar.schema }) {
                    sidebar.schema = loadedSchemas.first { $0.name == "public" }?.name ?? loadedSchemas.first?.name ?? ""
                }
                sidebar.routines = [:]
                sidebar.columns = [:]
                self.loadRoutines()
                self.loadCompleter()
            } catch {
                guard !Task.isCancelled else { return }
                self.sidebar.load = .failed(error.displayMessage)
            }
        }
    }

    /// Reads the catalog for autocomplete. Without it the editor still works,
    /// with no suggestions, so a failure is only logged.
    private func loadCompleter() {
        completionTask?.cancel()
        let session = session
        let completion = completion
        completionTask = Task {
            do {
                let completer = try await session.completer()
                guard !Task.isCancelled else { return }
                completion.completer = completer
            } catch {
                CompletionSource.logger.error("Could not load autocomplete names: \(error.displayMessage, privacy: .public)")
            }
        }
    }

    /// Opens a new session on another database of the same server. The
    /// tabs belong to the old database, so they close.
    /// `then` runs once the new database is open (restoring a session).
    func switchDatabase(to name: String, then: (() -> Void)? = nil) {
        guard name != session.settings().database else { return }
        guard DiscardPrompt.confirm(changeCount: changeCount) else { return }
        var settings = session.settings()
        settings.database = name
        switchError = nil
        Task {
            do {
                let newSession = try await DatabaseSession.connect(settings: settings)
                let oldSession = session
                session = newSession
                tabs = []
                tableModels = [:]
                // The cached pages belong to the old database.
                pageCache = [:]
                pageCacheOrder = []
                queryModels = [:]
                selectedTab = nil
                reload()
                oldSession.close()
                then?()
            } catch {
                switchError = "Could not open \(name): \(error.displayMessage)"
            }
        }
    }

    func open(_ relation: Relation) {
        let tab = Tab.relation(relation)
        if !tabs.contains(tab) { tabs.append(tab) }
        selectedTab = tab.id
    }

    func tableModel(for relation: Relation) -> TableTabModel {
        let id = Tab.relation(relation).id
        if let model = tableModels[id] { return model }
        let model = TableTabModel(
            relation: relation, session: session, cached: pageCache[relation.id],
            isProduction: connection.tag == .production, isReadOnly: connection.readOnly
        )
        model.onCache = { [weak self] page in self?.cachePage(page, for: relation.id) }
        tableModels[id] = model
        return model
    }

    /// First pages of the tables seen last, newest at the end. A closed
    /// table that opens again shows these rows while fresh ones load.
    private var pageCache: [Relation.ID: CachedPage] = [:]
    private var pageCacheOrder: [Relation.ID] = []
    private static let pageCacheSize = 10

    private func cachePage(_ page: CachedPage, for id: Relation.ID) {
        pageCache[id] = page
        pageCacheOrder.removeAll { $0 == id }
        pageCacheOrder.append(id)
        while pageCacheOrder.count > Self.pageCacheSize {
            pageCache[pageCacheOrder.removeFirst()] = nil
        }
    }

    func queryModel(for tab: Tab) -> QueryTabModel {
        if let model = queryModels[tab.id] { return model }
        let model = QueryTabModel(session: session, isProduction: connection.tag == .production, history: queryHistory,
                                  completion: completion)
        queryModels[tab.id] = model
        return model
    }

    /// Follows a foreign key: the referenced table, filtered to the value.
    func follow(_ key: ForeignKey, value: String) {
        let relation = key.referencedRelation
        let filter = RowFilter(target: .column(name: key.referencedColumn), operator: .equals, value: value)
        let id = Tab.relation(relation).id
        if let model = tableModels[id] {
            model.apply(filter)
        } else {
            tableModels[id] = TableTabModel(
                relation: relation, session: session, filter: filter,
                isProduction: connection.tag == .production, isReadOnly: connection.readOnly
            )
        }
        open(relation)
    }

    var selectedTabValue: Tab? { tabs.first { $0.id == selectedTab } }

    /// The model of the open table tab, if one is open.
    var currentTableModel: TableTabModel? {
        guard let tab = selectedTabValue, case .relation = tab else { return nil }
        return tableModels[tab.id]
    }

    var currentQueryModel: QueryTabModel? {
        guard let tab = selectedTabValue, case .query = tab else { return nil }
        return queryModels[tab.id]
    }

    /// Returns false when there was no tab to close.
    func closeSelectedTab() -> Bool {
        guard let tab = selectedTabValue else { return false }
        close(tab)
        return true
    }

    func selectTab(offset: Int) {
        guard !tabs.isEmpty else { return }
        let index = tabs.firstIndex { $0.id == selectedTab } ?? 0
        selectedTab = tabs[(index + offset + tabs.count) % tabs.count].id
    }

    /// A tab's name in the tab bar: a saved query shows its own name.
    func title(for tab: Tab) -> String {
        if case .query = tab, let name = queryModels[tab.id]?.savedQuery?.name { return name }
        return tab.title
    }

    var isProduction: Bool { connection.tag == .production }

    /// Opens a table on its Structure tab.
    func openStructure(_ relation: Relation) {
        open(relation)
        tableModel(for: relation).view = .structure
    }

    /// Opens a table and its export sheet.
    func export(_ relation: Relation) {
        open(relation)
        let table = tableModel(for: relation)
        table.view = .data
        table.isExportSheetShown = true
    }

    /// Runs a schema or data change from the sidebar (Truncate, Drop,
    /// Refresh), then reloads what it touched: the sidebar, and the open tab
    /// of `refreshing`; a dropped relation's tab closes.
    func runChange(_ sql: String, refreshing: Relation? = nil, dropping: Relation? = nil) {
        let session = session
        Task {
            do {
                try await session.execute(sql: sql)
                if let dropping { close(.relation(dropping)) }
                if let refreshing, tableModels[Tab.relation(refreshing).id] != nil {
                    pageCache[refreshing.id] = nil
                    tableModel(for: refreshing).reload()
                }
                reload()
            } catch {
                let alert = NSAlert()
                alert.messageText = "The change failed"
                alert.informativeText = error.displayMessage
                alert.alertStyle = .warning
                if let window = NSApp.keyWindow ?? NSApp.mainWindow {
                    alert.beginSheetModal(for: window) { _ in }
                } else {
                    alert.runModal()
                }
            }
        }
    }

    /// Opens a saved query; one already open is brought forward instead.
    func open(_ saved: SavedQuery) {
        if let (id, _) = queryModels.first(where: { $0.value.savedQuery?.id == saved.id }) {
            selectedTab = id
            return
        }
        newQuery(sql: saved.sql)
        if let id = selectedTab { queryModels[id]?.savedQuery = saved }
    }

    func newQuery(sql: String = "") {
        queryCount += 1
        let tab = Tab.query(number: queryCount)
        let model = QueryTabModel(session: session, isProduction: connection.tag == .production, history: queryHistory,
                                  completion: completion)
        model.sql = sql
        // The tab bar shows a saved query's name.
        model.$savedQuery.dropFirst().receive(on: RunLoop.main).sink { [weak self] _ in self?.objectWillChange.send() }.store(in: &subscriptions)
        queryModels[tab.id] = model
        tabs.append(tab)
        selectedTab = tab.id
    }

    func loadRoutines() {
        let schema = sidebar.schema
        guard !schema.isEmpty, sidebar.routines[schema] == nil else { return }
        let session = session
        Task {
            do {
                sidebar.routines[schema] = try await session.routines(schema: schema)
                sidebar.routinesError[schema] = nil
            } catch {
                // Shown in the Functions section, so a missing list is explained.
                sidebar.routinesError[schema] = error.displayMessage
            }
        }
    }

    func loadColumns(_ relation: Relation) {
        guard sidebar.columns[relation.id] == nil else { return }
        let session = session
        Task {
            if let columns = try? await session.columns(relation: relation) {
                sidebar.columns[relation.id] = columns
            }
        }
    }

    /// Opens a function's CREATE OR REPLACE statement in a new query tab.
    func open(_ routine: Routine) {
        let session = session
        Task {
            do {
                newQuery(sql: try await session.routineDefinition(oid: routine.oid))
            } catch {
                newQuery(sql: "-- \(error.displayMessage)")
            }
        }
    }

    // MARK: Session restore

    /// This window as it should reopen on the next launch.
    func savedWindow() -> SavedWindow {
        let saved: [SavedWindow.Tab] = tabs.map { tab in
            switch tab {
            case .relation(let relation):
                return .relation(schema: relation.schema, name: relation.name, kind: relation.kind.savedName)
            case .query:
                let model = queryModels[tab.id]
                return .query(sql: model?.sql ?? "", savedQueryID: model?.savedQuery?.id)
            }
        }
        return SavedWindow(connectionID: connection.id, database: session.settings().database, schema: sidebar.schema,
                           tabs: saved, selectedTab: tabs.firstIndex { $0.id == selectedTab })
    }

    /// Reopens a saved window's database, schema and tabs.
    func restore(_ saved: SavedWindow) {
        if saved.database != session.settings().database {
            switchDatabase(to: saved.database) { [weak self] in self?.restoreTabs(saved) }
        } else {
            restoreTabs(saved)
        }
    }

    private func restoreTabs(_ saved: SavedWindow) {
        // Kept if the schema still exists once the sidebar has loaded.
        sidebar.schema = saved.schema
        for tab in saved.tabs {
            switch tab {
            case .relation(let schema, let name, let kind):
                open(Relation(schema: schema, name: name, kind: RelationKind(savedName: kind) ?? .table))
            case .query(let sql, let savedQueryID):
                newQuery(sql: sql)
                if let savedQueryID, let id = selectedTab,
                   let query = savedQueries.items.first(where: { $0.id == savedQueryID }) {
                    queryModels[id]?.savedQuery = query
                }
            }
        }
        if let index = saved.selectedTab, tabs.indices.contains(index) {
            selectedTab = tabs[index].id
        }
    }

    /// Uncommitted edits in all table tabs of this window.
    var changeCount: Int {
        tableModels.values.reduce(0) { $0 + $1.changeCount }
    }

    func close(_ tab: Tab) {
        guard let index = tabs.firstIndex(of: tab) else { return }
        guard DiscardPrompt.confirm(changeCount: tableModels[tab.id]?.changeCount ?? 0) else { return }
        tabs.remove(at: index)
        tableModels[tab.id] = nil
        queryModels[tab.id] = nil
        if selectedTab == tab.id {
            selectedTab = tabs.indices.contains(index) ? tabs[index].id : tabs.last?.id
        }
    }

    func closeSession() async {
        linkTimer?.invalidate()
        if let wakeObserver { NSWorkspace.shared.notificationCenter.removeObserver(wakeObserver) }
        loadTask?.cancel()
        session.close()
    }
}

/// The sidebar's state. Separate from WorkspaceModel so that tab changes
/// don't redraw the sidebar's (possibly very long) list.
@MainActor
final class SidebarModel: ObservableObject {
    @Published var schemas: [SchemaContents] = []
    @Published var load: WorkspaceModel.Load = .loading
    /// The schema the sidebar lists, chosen at its bottom.
    @Published var schema = ""
    /// Functions and procedures by schema, loaded when a schema is shown.
    @Published var routines: [String: [Routine]] = [:]
    /// Why a schema's functions could not be listed.
    @Published var routinesError: [String: String] = [:]
    /// Columns of tables expanded in the sidebar.
    @Published var columns: [Relation.ID: [Column]] = [:]
    /// The table of the open tab, for the sidebar's highlight.
    @Published var openRelationID: Relation.ID?
}

/// The NSWindow of one connection. Closing it closes the session.
@MainActor
final class MainWindowController: NSObject, NSWindowDelegate {
    let model: WorkspaceModel
    private let window: NSWindow
    var onClose: (() -> Void)?

    init(connection: SavedConnection, session: DatabaseSession) {
        model = WorkspaceModel(connection: connection, session: session)
        window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1200, height: 760),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        super.init()
        window.title = connection.name
        window.titleVisibility = .hidden
        window.titlebarAppearsTransparent = true
        window.isReleasedWhenClosed = false
        // The grid adds hundreds of cell views per table; with this on, AppKit
        // rebuilds the Tab-key order for each one.
        window.autorecalculatesKeyViewLoop = false
        window.contentMinSize = NSSize(width: 720, height: 420)
        let host = NSHostingController(rootView: WorkspaceView(model: model))
        host.sizingOptions = []
        window.contentViewController = host
        window.setContentSize(NSSize(width: 1200, height: 760))
        window.delegate = self
        // Each connection keeps its own size and place.
        let autosaveName = "MainWindow-\(connection.id.uuidString)"
        if !window.setFrameUsingName(autosaveName) { window.center() }
        window.setFrameAutosaveName(autosaveName)
    }

    var isKey: Bool { window.isKeyWindow }

    func show() {
        NSApp.activate()
        window.makeKeyAndOrderFront(nil)
        centerTrafficLights()
    }

    /// AppKit lays the title bar out again on resize and full screen, which
    /// puts the buttons back at the top, so this runs after each of those.
    private func centerTrafficLights() {
        guard let close = window.standardWindowButton(.closeButton),
              let container = close.superview?.superview else { return }
        container.frame = NSRect(
            x: container.frame.minX, y: window.frame.height - headerHeight,
            width: container.frame.width, height: headerHeight)
        for type in [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton] {
            guard let button = window.standardWindowButton(type) else { continue }
            button.setFrameOrigin(NSPoint(x: button.frame.minX, y: (headerHeight - button.frame.height) / 2))
        }
    }

    func windowDidResize(_ notification: Notification) {
        centerTrafficLights()
    }

    func windowDidExitFullScreen(_ notification: Notification) {
        centerTrafficLights()
    }

    func close() {
        window.close()
    }

    /// Set while the app quits: the quit already asked about changes.
    var skipsDiscardPrompt = false

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        skipsDiscardPrompt || DiscardPrompt.confirm(changeCount: model.changeCount)
    }

    func windowWillClose(_ notification: Notification) {
        let model = model
        Task { await model.closeSession() }
        onClose?()
    }
}

struct WorkspaceView: View {
    @ObservedObject var model: WorkspaceModel

    var body: some View {
        VStack(spacing: 0) {
            if model.connection.tag == .production {
                // Production is never subtle.
                Rectangle().fill(Theme.danger).frame(height: 3)
            }
            HeaderBar(model: model)
            Divider()
            ConnectionLostBanner(model: model)
            HSplitView {
                SidebarView(sidebar: model.sidebar, workspace: model, history: model.queryHistory, saved: model.savedQueries)
                    .frame(minWidth: 180, idealWidth: 240, maxWidth: 420)
                VStack(spacing: 0) {
                    TabBar(model: model)
                    Divider()
                    TabContent(model: model)
                }
                .frame(minWidth: 360)
            }
            Divider()
            StatusBar(model: model, sidebar: model.sidebar)
        }
        .background(Theme.windowBg)
        .ignoresSafeArea(edges: .top)
        .sheet(isPresented: $model.showsNewTable) {
            NewTableSheet(workspace: model)
        }
        .overlay(alignment: .top) {
            if model.showsQuickOpen {
                ZStack(alignment: .top) {
                    // A click outside closes it.
                    Color.black.opacity(0.06)
                        .contentShape(Rectangle())
                        .onTapGesture { model.showsQuickOpen = false }
                    QuickOpenView(model: model, sidebar: model.sidebar, saved: model.savedQueries)
                        .padding(.top, 70)
                }
            }
        }
    }
}

private struct HeaderBar: View {
    @ObservedObject var model: WorkspaceModel

    var body: some View {
        HStack(spacing: 2) {
            // Room for the traffic lights, which sit over the content.
            Spacer().frame(width: 70)
            HeaderChip {
                PostgresLogo(size: headerIconSize)
            } label: {
                Text(model.connection.name)
                if let tag = model.connection.tag {
                    Text(tag.label)
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(tag.color)
                        .padding(.horizontal, 5)
                        .frame(height: 16)
                        .background(Capsule().fill(tag.color.opacity(0.12)))
                }
            }
            .help(model.connection.name)
            Text("/")
                .font(.system(size: 13))
                .foregroundStyle(Theme.inkSecondary.opacity(0.5))
            Menu {
                ForEach(model.databases, id: \.self) { name in
                    Button {
                        model.switchDatabase(to: name)
                    } label: {
                        if name == model.session.settings().database {
                            Label(name, systemImage: "checkmark")
                        } else {
                            Text(name)
                        }
                    }
                }
            } label: {
                HeaderChip(isButton: true) {
                    headerLucide("database")
                } label: {
                    Text(model.session.settings().database)
                    Image(systemName: "chevron.down")
                        .font(.system(size: 9, weight: .semibold))
                        .foregroundStyle(Theme.inkSecondary)
                }
            }
            .menuStyle(.button)
            .buttonStyle(.plain)
            .menuIndicator(.hidden)
            .focusEffectDisabled()
            .fixedSize()
            .help("Database")
            if model.connection.readOnly {
                HeaderChip {
                    headerSymbol("lock.fill")
                } label: {
                    Text("Read-only").foregroundStyle(Theme.inkSecondary)
                }
                .background(Capsule().fill(Theme.control))
                .padding(.leading, 6)
                .help("The server refuses all changes on this connection")
            }
            if let switchError = model.switchError {
                Text(switchError)
                    .font(Theme.caption)
                    .foregroundStyle(Theme.danger)
                    .lineLimit(1)
                    .padding(.leading, 6)
            }
            Spacer()
            HeaderIconButton(symbol: "arrow.clockwise", help: "Refresh (⌘R)", action: model.reload)
            HeaderIconButton(symbol: "square.and.pencil", help: "New query tab (⌘T)") { model.newQuery() }
        }
        .padding(.horizontal, 10)
        .frame(height: headerHeight)
        .overlay(alignment: .bottom) { ActivityBar(workspace: model) }
    }
}

/// A thin moving bar along the header's bottom edge while the open tab loads
/// rows, commits or runs a query, as TablePlus shows progress in its title.
private struct ActivityBar: View {
    @ObservedObject var workspace: WorkspaceModel

    var body: some View {
        if let table = workspace.currentTableModel {
            TableActivity(model: table)
        } else if let query = workspace.currentQueryModel {
            QueryActivity(model: query)
        }
    }
}

private struct TableActivity: View {
    @ObservedObject var model: TableTabModel

    var body: some View {
        IndeterminateBar(isActive: model.isLoading || model.isCommitting || model.isExporting)
    }
}

private struct QueryActivity: View {
    @ObservedObject var model: QueryTabModel

    var body: some View {
        IndeterminateBar(isActive: model.isRunning)
    }
}

private struct IndeterminateBar: View {
    let isActive: Bool

    var body: some View {
        // Paused while idle, so it costs nothing between loads.
        TimelineView(.animation(paused: !isActive)) { context in
            GeometryReader { geometry in
                let width = geometry.size.width * 0.3
                let phase = context.date.timeIntervalSinceReferenceDate.truncatingRemainder(dividingBy: 1.2) / 1.2
                Capsule()
                    .fill(Color.accentColor)
                    .frame(width: width, height: 2)
                    .offset(x: -width + (geometry.size.width + width) * phase)
            }
        }
        .frame(height: 2)
        .clipped()
        .opacity(isActive ? 1 : 0)
        .animation(.easeOut(duration: 0.2), value: isActive)
        .allowsHitTesting(false)
        .accessibilityHidden(true)
    }
}

/// The header replaces the title bar; the traffic lights are moved to its
/// middle so they line up with its items.
private let headerHeight: CGFloat = 44

/// One header item: a 16 pt icon and 13 pt text on one baseline, in a
/// 26 pt tall capsule that shades on hover when it is a button.
private struct HeaderChip<Icon: View, Content: View>: View {
    var isButton = false
    @ViewBuilder let icon: Icon
    @ViewBuilder let label: Content
    @State private var isHovered = false

    var body: some View {
        HStack(spacing: 6) {
            icon.frame(width: headerIconSize, height: headerIconSize)
            HStack(spacing: 5) { label }
        }
        .font(.system(size: 13, weight: .medium))
        .foregroundStyle(Theme.ink)
        .padding(.horizontal, 8)
        .frame(height: 26)
        .background(Capsule().fill(isButton && isHovered ? Theme.control : Color.clear))
        .contentShape(Capsule())
        .onHover { isHovered = $0 }
    }
}

private let headerIconSize: CGFloat = 16

/// A Lucide icon in the header's blue, the same size as the other icons.
@MainActor @ViewBuilder
private func headerLucide(_ name: String) -> some View {
    if let image = LucideIcon.image(name, size: headerIconSize) {
        Image(nsImage: image)
            .renderingMode(.template)
            .foregroundStyle(Color(red: 0.20, green: 0.40, blue: 0.57))
    } else {
        headerSymbol("cylinder")
    }
}

/// An SF Symbol drawn at the same visual size as the Postgres logo, in the
/// logo's blue so the two icons read as one set.
private func headerSymbol(_ name: String) -> some View {
    Image(systemName: name)
        .font(.system(size: 14, weight: .regular))
        .foregroundStyle(Color(red: 0.20, green: 0.40, blue: 0.57))
}

/// The header's icon-only buttons: same 16 pt icon, 26 pt square hover.
private struct HeaderIconButton: View {
    let symbol: String
    let help: String
    let action: () -> Void
    @State private var isHovered = false

    var body: some View {
        Button(action: action) {
            Image(systemName: symbol)
                .font(.system(size: 13, weight: .regular))
                .foregroundStyle(Theme.inkSecondary)
                .frame(width: 26, height: 26)
                .background(RoundedRectangle(cornerRadius: 6).fill(isHovered ? Theme.control : Color.clear))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .focusEffectDisabled()
        .onHover { isHovered = $0 }
        .help(help)
    }
}

/// Tabs share the width, as in Safari and TablePlus; past their minimum the
/// bar scrolls, and the open tab is always scrolled into view.
private struct TabBar: View {
    @ObservedObject var model: WorkspaceModel

    /// Wide enough to read a name; more tabs than fit make the bar scroll.
    private static let minWidth: CGFloat = 160
    private static let maxWidth: CGFloat = 220

    var body: some View {
        HStack(spacing: 0) {
            GeometryReader { geometry in
                let count = CGFloat(max(model.tabs.count, 1))
                let width = min(max(geometry.size.width / count, Self.minWidth), Self.maxWidth)
                TabStrip(
                    selectedIndex: model.tabs.firstIndex { $0.id == model.selectedTab },
                    tabWidth: width + 1
                ) {
                    HStack(spacing: 0) {
                        ForEach(model.tabs) { tab in
                            TabButton(
                                tab: tab,
                                title: model.title(for: tab),
                                isSelected: model.selectedTab == tab.id,
                                onSelect: { model.selectedTab = tab.id },
                                onClose: { model.close(tab) }
                            )
                            .frame(width: width)
                            Divider()
                        }
                    }
                    .frame(height: 30)
                }
            }
            Button {
                model.newQuery()
            } label: {
                Image(systemName: "plus").frame(width: 28, height: 28)
            }
            .buttonStyle(.borderless)
            .help("New query tab")
            // Every open tab, for the ones scrolled out of view.
            Menu {
                ForEach(model.tabs) { tab in
                    Button {
                        model.selectedTab = tab.id
                    } label: {
                        if model.selectedTab == tab.id {
                            Label(model.title(for: tab), systemImage: "checkmark")
                        } else {
                            Text(model.title(for: tab))
                        }
                    }
                }
            } label: {
                Image(systemName: "chevron.down")
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .fixedSize()
            .padding(.trailing, 8)
            .disabled(model.tabs.isEmpty)
            .help("All open tabs")
        }
        .frame(height: 30)
        .background(Theme.sidebarBg)
    }
}

/// The tab row in an AppKit scroll view: a mouse wheel or a vertical
/// trackpad scroll moves the tabs sideways (SwiftUI's horizontal ScrollView
/// takes only sideways scrolling), and the selected tab is kept in view.
private struct TabStrip<Content: View>: NSViewRepresentable {
    let selectedIndex: Int?
    /// One tab plus its divider.
    let tabWidth: CGFloat
    @ViewBuilder let content: () -> Content

    func makeNSView(context: Context) -> WheelScrollView {
        let scroll = WheelScrollView()
        scroll.hasHorizontalScroller = false
        scroll.hasVerticalScroller = false
        scroll.drawsBackground = false
        scroll.horizontalScrollElasticity = .allowed
        scroll.verticalScrollElasticity = .none
        scroll.documentView = NSHostingView(rootView: leading())
        return scroll
    }

    func updateNSView(_ scroll: WheelScrollView, context: Context) {
        guard let host = scroll.documentView as? NSHostingView<AnyView> else { return }
        host.rootView = leading()
        let size = host.fittingSize
        host.frame = NSRect(x: 0, y: 0, width: max(size.width, scroll.contentSize.width), height: scroll.contentSize.height)
        if let selectedIndex, selectedIndex != context.coordinator.shownIndex {
            context.coordinator.shownIndex = selectedIndex
            let rect = NSRect(x: CGFloat(selectedIndex) * tabWidth, y: 0, width: tabWidth, height: 1)
            // After layout, so a just-added tab exists to scroll to.
            DispatchQueue.main.async { host.scrollToVisible(rect) }
        }
    }

    /// Pinned left: the host can be wider than the tabs, and SwiftUI would
    /// centre them.
    private func leading() -> AnyView {
        AnyView(content().frame(maxWidth: .infinity, alignment: .leading))
    }

    func makeCoordinator() -> Coordinator { Coordinator() }

    final class Coordinator {
        var shownIndex: Int?
    }
}

final class WheelScrollView: NSScrollView {
    override func scrollWheel(with event: NSEvent) {
        // Mostly-vertical scrolling becomes sideways; sideways stays as is.
        guard abs(event.scrollingDeltaY) > abs(event.scrollingDeltaX),
              let document = documentView else {
            super.scrollWheel(with: event)
            return
        }
        let step = event.hasPreciseScrollingDeltas ? event.scrollingDeltaY : event.scrollingDeltaY * 12
        let maxX = max(0, document.frame.width - contentView.bounds.width)
        let x = min(max(contentView.bounds.origin.x - step, 0), maxX)
        contentView.scroll(to: NSPoint(x: x, y: contentView.bounds.origin.y))
        reflectScrolledClipView(contentView)
    }
}

private struct TabButton: View {
    let tab: WorkspaceModel.Tab
    let title: String
    let isSelected: Bool
    let onSelect: () -> Void
    let onClose: () -> Void
    @State private var hovering = false

    var body: some View {
        HStack(spacing: 6) {
            switch tab {
            case .relation(let relation):
                RelationIcon(kind: relation.kind, isActive: isSelected, size: 13)
            case .query:
                Image(systemName: "doc.text").foregroundStyle(Theme.inkSecondary)
            }
            Text(title)
                .lineLimit(1)
                .truncationMode(.middle)
                .frame(maxWidth: .infinity, alignment: .leading)
                .help(title)
            Button(action: onClose) {
                Image(systemName: "xmark").font(.system(size: 9, weight: .semibold))
            }
            .buttonStyle(.borderless)
            .opacity(hovering || isSelected ? 1 : 0)
            .help("Close tab")
        }
        .font(Theme.caption)
        .foregroundStyle(isSelected ? Theme.ink : Theme.inkSecondary)
        .padding(.horizontal, 12)
        .frame(height: 30)
        .background(isSelected ? Theme.card : Color.clear)
        .contentShape(Rectangle())
        .onTapGesture(perform: onSelect)
        .onHover { hovering = $0 }
        // A tap gesture is invisible to VoiceOver; expose the tab as a button.
        .accessibilityElement(children: .contain)
        .accessibilityLabel(title)
        .accessibilityAddTraits(isSelected ? [.isButton, .isSelected] : .isButton)
        .accessibilityAction(.default, onSelect)
    }
}

private struct TabContent: View {
    @ObservedObject var model: WorkspaceModel

    var body: some View {
        Group {
            switch model.tabs.first(where: { $0.id == model.selectedTab }) {
            case .relation(let relation):
                // No .id per table: one grid stays in the window and gets the
                // new tab's data. A new grid on every switch made AppKit
                // rebuild the window's key view loop and every cell (~100 ms+).
                TableTabView(model: model.tableModel(for: relation), onFollow: model.follow)
            case .query(let number):
                QueryTabView(model: model.queryModel(for: .query(number: number)), history: model.queryHistory)
                    .id(WorkspaceModel.Tab.query(number: number).id)
            case nil:
                Placeholder(symbol: "sidebar.left", title: "No table open", detail: "Select a table in the sidebar, or press ⌘T for a query.")
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Theme.card)
    }
}

private struct Placeholder: View {
    let symbol: String
    let title: String
    let detail: String

    var body: some View {
        VStack(spacing: 8) {
            Image(systemName: symbol)
                .font(.system(size: 30, weight: .light))
                .foregroundStyle(Theme.inkSecondary)
            Text(title).font(.system(size: 14, weight: .semibold)).foregroundStyle(Theme.ink)
            Text(detail).font(Theme.caption).foregroundStyle(Theme.inkSecondary)
        }
    }
}

private struct StatusBar: View {
    @ObservedObject var model: WorkspaceModel
    @ObservedObject var sidebar: SidebarModel

    var body: some View {
        HStack(spacing: 6) {
            Circle().fill(Theme.success).frame(width: 7, height: 7)
            Text("Connected · PostgreSQL \(model.session.serverVersion())")
            Text("·")
            Text(model.endpointText)
                .lineLimit(1)
                .truncationMode(.middle)
            Spacer()
            if case .loaded = model.load {
                Text("\(model.relationCount) relations in \(model.schemas.count) schemas")
            }
        }
        .font(Theme.meta)
        .foregroundStyle(Theme.inkSecondary)
        .padding(.horizontal, 12)
        .frame(height: 24)
    }
}

extension RelationKind {
    var symbol: String {
        switch self {
        case .table, .partitionedTable, .foreignTable: "tablecells"
        case .view: "eye"
        case .materializedView: "square.stack.3d.up"
        }
    }

    var label: String {
        switch self {
        case .table: "Table"
        case .partitionedTable: "Partitioned table"
        case .view: "View"
        case .materializedView: "Materialized view"
        case .foreignTable: "Foreign table"
        }
    }
}

/// Shown while the connection is gone: why, and Reconnect. The tabs stay
/// open behind it, pending changes included.
private struct ConnectionLostBanner: View {
    @ObservedObject var model: WorkspaceModel

    var body: some View {
        switch model.linkState {
        case .connected:
            EmptyView()
        case .lost(let reason):
            banner(reason: reason)
        case .reconnecting:
            banner(reason: nil)
        }
    }

    private func banner(reason: String?) -> some View {
        HStack(spacing: 10) {
            Image(systemName: "bolt.horizontal.circle.fill")
                .foregroundStyle(Theme.danger)
            VStack(alignment: .leading, spacing: 1) {
                Text(reason == nil ? "Reconnecting…" : "Connection lost")
                    .font(.system(size: 12, weight: .semibold))
                if let reason {
                    Text(reason)
                        .font(Theme.caption)
                        .foregroundStyle(Theme.inkSecondary)
                        .lineLimit(2)
                        .textSelection(.enabled)
                }
            }
            Spacer()
            Text("Your tabs and unsaved changes are kept.")
                .font(Theme.caption)
                .foregroundStyle(Theme.inkSecondary)
            if reason == nil {
                ProgressView().controlSize(.small)
            } else {
                Button("Reconnect", action: model.reconnect)
                    .keyboardShortcut("r", modifiers: [.command, .shift])
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .background(Theme.danger.opacity(0.08))
        .overlay(alignment: .bottom) { Divider() }
    }
}
