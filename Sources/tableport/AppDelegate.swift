import AppKit
import TablePortCore
import MacKit
import MacKitUpdates

/// User-facing text reads the product name from CFBundleName (mackit.toml's
/// `name`), so a rename is one change there.
enum AppInfo {
    static let name = Bundle.main.object(forInfoDictionaryKey: "CFBundleName") as? String
        ?? ProcessInfo.processInfo.processName
}

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private let store = ConnectionStore()
    private let manager = ConnectionManager()
    private var mainWindows: [SavedConnection.ID: MainWindowController] = [:]
    private lazy var settingsWindow = makeSettingsWindow()
    private lazy var connectionsWindow = HostedWindow(
        size: NSSize(width: 700, height: 480),
        autosaveName: "ConnectionsWindow"
    ) { [unowned self] in
        ConnectionsView(
            store: store,
            manager: manager,
            open: { [unowned self] connection in open(connection) },
            closeWindow: { [unowned self] id in mainWindows[id]?.close() }
        )
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        SingleInstance.claim()
        _ = Updater.shared  // starts Sparkle's daily check  // another copy (DMG, .build) quits
        AppFont.register()
        AppSettings.shared.applyAppearance()
        // A regular window app: stay in the Dock when every window is closed,
        // which HostedWindow would otherwise undo for menu-bar apps.
        DockPresence.keepsDock = { true }
        NSApp.setActivationPolicy(.regular)
        NSApp.mainMenu = MainMenu.make(.init(
            workspace: { [unowned self] in
                mainWindows.values.first { $0.isKey }?.model
            },
            showConnections: { [unowned self] in connectionsWindow.show() },
            newConnection: { [unowned self] in
                connectionsWindow.show()
                manager.requestNewConnection = true
            },
            showSettings: { [unowned self] in showSettings() }
        ))
        manager.onConnected = { [unowned self] connection, session in
            showMainWindow(connection: connection, session: session)
        }
        manager.trustHostKey = { [unowned self] connection, host, fingerprint in
            guard HostKeyPrompt.ask(host: host, fingerprint: fingerprint), var ssh = connection.ssh else { return nil }
            ssh.trustedFingerprint = fingerprint
            var trusted = connection
            trusted.ssh = ssh
            store.update(trusted)
            return trusted
        }
        connectionsWindow.show()
    }

    /// One warning for all windows, then quit without asking again.
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        let changeCount = mainWindows.values.reduce(0) { $0 + $1.model.changeCount }
        guard DiscardPrompt.confirm(changeCount: changeCount) else { return .terminateCancel }
        for window in mainWindows.values { window.skipsDiscardPrompt = true }
        return .terminateNow
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag { connectionsWindow.show() }
        return true
    }

    /// Brings an open connection forward, or starts connecting.
    private func open(_ connection: SavedConnection) {
        if let window = mainWindows[connection.id] {
            window.show()
        } else {
            manager.connect(connection)
        }
    }

    private func showMainWindow(connection: SavedConnection, session: DatabaseSession) {
        let controller = MainWindowController(connection: connection, session: session)
        controller.onClose = { [unowned self] in
            mainWindows[connection.id] = nil
            manager.windowClosed(connection.id)
            // As in TablePlus: with no connection open, the launch window returns.
            if mainWindows.isEmpty { connectionsWindow.show() }
        }
        mainWindows[connection.id] = controller
        controller.show()
        connectionsWindow.close()
    }

    private func showSettings() {
        settingsWindow.show()
    }

}
