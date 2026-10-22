import AppKit

/// A menu item that runs a closure and is enabled only while `isEnabled`
/// says so; AppKit asks before showing the menu and before a key equivalent.
final class CommandItem: NSMenuItem, NSMenuItemValidation {
    private let handler: () -> Void
    private let isEnabledCheck: () -> Bool
    private let titleProvider: (() -> String)?

    init(_ title: String, key: String = "", modifiers: NSEvent.ModifierFlags = .command,
         isEnabled: @escaping () -> Bool = { true }, title titleProvider: (() -> String)? = nil,
         action: @escaping () -> Void) {
        handler = action
        isEnabledCheck = isEnabled
        self.titleProvider = titleProvider
        super.init(title: title, action: #selector(run), keyEquivalent: key)
        keyEquivalentModifierMask = modifiers
        target = self
    }

    required init(coder: NSCoder) {
        handler = {}
        isEnabledCheck = { false }
        titleProvider = nil
        super.init(coder: coder)
    }

    @objc private func run() {
        handler()
    }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        if let titleProvider { menuItem.title = titleProvider() }
        return isEnabledCheck()
    }
}

/// The menu bar. Commands act on the key connection window and its open tab;
/// the buttons in the windows carry no shortcuts of their own, so each key
/// has one owner.
@MainActor
enum MainMenu {
    struct Actions {
        let workspace: () -> WorkspaceModel?
        let showConnections: () -> Void
        let newConnection: () -> Void
        let openSqliteFile: () -> Void
        let showSettings: () -> Void
    }

    static func make(_ actions: Actions) -> NSMenu {
        let workspace = actions.workspace
        let table = { workspace()?.currentTableModel }
        let query = { workspace()?.currentQueryModel }

        let app = NSMenu()
        app.addItem(withTitle: "About \(AppInfo.name)", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        app.addItem(.separator())
        app.addItem(CommandItem("Settings…", key: ",", action: actions.showSettings))
        app.addItem(.separator())
        app.addItem(withTitle: "Hide \(AppInfo.name)", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        app.addItem(withTitle: "Hide Others", action: #selector(NSApplication.hideOtherApplications(_:)), keyEquivalent: "h")
            .keyEquivalentModifierMask = [.command, .option]
        app.addItem(withTitle: "Show All", action: #selector(NSApplication.unhideAllApplications(_:)), keyEquivalent: "")
        app.addItem(.separator())
        app.addItem(withTitle: "Quit \(AppInfo.name)", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")

        let file = NSMenu(title: "File")
        file.addItem(CommandItem("New Connection…", key: "n", action: actions.newConnection))
        file.addItem(CommandItem("Open SQLite File…", key: "o", action: actions.openSqliteFile))
        file.addItem(CommandItem("New Query Tab", key: "t", isEnabled: { workspace() != nil }) { workspace()?.newQuery() })
        file.addItem(CommandItem("New Table…", isEnabled: { workspace().map { !$0.connection.readOnly } ?? false }) {
            workspace()?.showsNewTable = true
        })
        file.addItem(CommandItem("Open Anything…", key: "p", isEnabled: { workspace() != nil }) {
            workspace().map { $0.showsQuickOpen.toggle() }
        })
        file.addItem(.separator())
        file.addItem(CommandItem("Export…", key: "e", modifiers: [.command, .shift], isEnabled: { table() != nil }) {
            guard let table = table() else { return }
            table.view = .data
            table.isExportSheetShown = true
        })
        file.addItem(.separator())
        // ⌘W closes the open tab first, as in TablePlus and browsers; the
        // window goes when it has no tab left.
        file.addItem(CommandItem("Close Tab", key: "w", isEnabled: { NSApp.keyWindow != nil }) {
            if workspace()?.closeSelectedTab() != true { NSApp.keyWindow?.performClose(nil) }
        })
        file.addItem(withTitle: "Close Window", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "W")

        let edit = NSMenu(title: "Edit")
        // Standard actions go to the first responder, so text fields, the
        // query editor and the grid get them without extra wiring.
        edit.addItem(withTitle: "Undo", action: Selector(("undo:")), keyEquivalent: "z")
        edit.addItem(withTitle: "Redo", action: Selector(("redo:")), keyEquivalent: "Z")
        edit.addItem(.separator())
        edit.addItem(withTitle: "Cut", action: #selector(NSText.cut(_:)), keyEquivalent: "x")
        edit.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        // Sent to the grid that has the focus.
        edit.addItem(withTitle: "Copy as CSV", action: #selector(GridTableView.copyAsCSV(_:)), keyEquivalent: "")
        edit.addItem(withTitle: "Copy as SQL", action: #selector(GridTableView.copyAsSQL(_:)), keyEquivalent: "")
        edit.addItem(withTitle: "Paste", action: #selector(NSText.paste(_:)), keyEquivalent: "v")
        edit.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")

        let view = NSMenu(title: "View")
        view.addItem(CommandItem("Refresh", key: "r", isEnabled: { workspace() != nil }) {
            workspace()?.reload()
            table()?.reload()
        })
        view.addItem(.separator())
        view.addItem(CommandItem("Data", key: "1", modifiers: [.command, .option], isEnabled: { table() != nil }) {
            table()?.view = .data
        })
        view.addItem(CommandItem("Structure", key: "2", modifiers: [.command, .option], isEnabled: { table() != nil }) {
            table()?.view = .structure
        })
        view.addItem(.separator())
        let hasTabs = { (workspace()?.tabs.count ?? 0) > 1 }
        let next: () -> Void = { workspace()?.selectTab(offset: 1) }
        let previous: () -> Void = { workspace()?.selectTab(offset: -1) }
        view.addItem(CommandItem("Next Tab", key: "\t", modifiers: .control, isEnabled: hasTabs, action: next))
        // Shift turns Tab into back-tab, which is what the key event carries.
        view.addItem(CommandItem("Previous Tab", key: "\u{19}", modifiers: [.control, .shift], isEnabled: hasTabs, action: previous))
        // ⇧⌘] and ⇧⌘[ as well, as in Safari; hidden items still take their keys.
        for (key, action) in [("]", next), ("[", previous)] {
            let item = CommandItem(key == "]" ? "Next Tab" : "Previous Tab", key: key, modifiers: [.command, .shift], isEnabled: hasTabs, action: action)
            item.isHidden = true
            item.allowsKeyEquivalentWhenHidden = true
            view.addItem(item)
        }

        let queryMenu = NSMenu(title: "Query")
        queryMenu.addItem(CommandItem("Run Current", key: "\r", isEnabled: { query().map { !$0.isRunning } ?? false }) {
            query()?.run(.current)
        })
        queryMenu.addItem(CommandItem("Run All", key: "\r", modifiers: [.command, .shift], isEnabled: { query().map { !$0.isRunning } ?? false }) {
        queryMenu.addItem(CommandItem("Save Query As…", key: "s", modifiers: [.command, .shift], isEnabled: { query().map { !$0.sql.isEmpty } ?? false }) {
            if let query = query(), let saved = workspace()?.savedQueries { query.save(to: saved, asNew: true) }
        })
            query()?.run(.all)
        })
        // ⌘I is Beautify in a query tab and Add Row in a table tab, as in
        // TablePlus. One item for both: a disabled item with the same key
        // would swallow it.
        queryMenu.addItem(CommandItem("Beautify / Add Row", key: "i", isEnabled: {
            query() != nil || (table().map { $0.canAddRows && !$0.columns.isEmpty } ?? false)
        }, title: { query() != nil ? "Beautify" : "Add Row" }) {
            if let query = query() {
                query.beautify()
            } else {
                table()?.view = .data
                table()?.addRow()
            }
        })
        queryMenu.addItem(CommandItem("Cancel", key: ".", isEnabled: { query()?.isRunning ?? false }) { query()?.cancel() })

        let data = NSMenu(title: "Data")
        data.addItem(CommandItem("Filter", key: "f", isEnabled: { table() != nil }) {
            table()?.focusFilters()
        })
        data.addItem(.separator())
        data.addItem(CommandItem("Add Row  ⌘I", isEnabled: { table().map { $0.canAddRows && !$0.columns.isEmpty } ?? false }) {
            table()?.view = .data
            table()?.addRow()
        })
        data.addItem(.separator())
        data.addItem(CommandItem("Review Changes…", key: "S", isEnabled: { (table()?.changeCount ?? 0) > 0 }) {
            table()?.isReviewing = true
        })
        // ⌘S commits a table tab's changes and saves a query tab's query.
        // One item for both: a disabled item with the same key would
        // swallow it.
        data.addItem(CommandItem("Commit Changes", key: "s", isEnabled: {
            if let query = query() { return !query.sql.isEmpty }
            return table().map { $0.changeCount > 0 && !$0.isCommitting } ?? false
        }, title: { query() != nil ? "Save Query" : "Commit Changes" }) {
            if let query = query(), let saved = workspace()?.savedQueries {
                query.save(to: saved)
            } else {
                table()?.commit()
            }
        })
        data.addItem(CommandItem("Discard Changes", isEnabled: { (table()?.changeCount ?? 0) > 0 }) {
            table()?.discardChanges()
        })

        let window = NSMenu(title: "Window")
        window.addItem(withTitle: "Minimize", action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        window.addItem(withTitle: "Zoom", action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        window.addItem(.separator())
        window.addItem(CommandItem("Connections", key: "0", action: actions.showConnections))

        let menu = NSMenu()
        for submenu in [app, file, edit, view, queryMenu, data, window] {
            let item = NSMenuItem()
            item.submenu = submenu
            menu.addItem(item)
        }
        NSApp.windowsMenu = window
        return menu
    }
}
