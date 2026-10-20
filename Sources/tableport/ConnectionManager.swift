import TablePortCore
import Foundation

/// Connection attempts from the Connections window. A successful attempt
/// hands its session to `onConnected`, which opens the main window; the row
/// stays "connected" until `windowClosed` is called.
@MainActor
final class ConnectionManager: ObservableObject {
    enum State {
        case connecting
        case connected(serverVersion: String)
        case failed(message: String)
    }

    @Published private(set) var states: [SavedConnection.ID: State] = [:]
    /// Set by File › New Connection; the Connections window opens the form.
    @Published var requestNewConnection = false
    private var tasks: [SavedConnection.ID: Task<Void, Never>] = [:]

    var onConnected: ((SavedConnection, DatabaseSession) -> Void)?

    /// Asks the user to trust an unknown SSH host key. Returns the
    /// connection with the fingerprint saved, or nil when not trusted.
    var trustHostKey: ((SavedConnection, _ host: String, _ fingerprint: String) -> SavedConnection?)?

    func connect(_ connection: SavedConnection) {
        if case .connecting = states[connection.id] { return }
        let settings: ConnectionSettings
        do {
            settings = try connection.settings()
            Keychain.resealOnce(for: connection.id)
        } catch {
            states[connection.id] = .failed(message: error.displayMessage)
            return
        }
        states[connection.id] = .connecting
        tasks[connection.id] = Task { [weak self] in
            do {
                let session = try await DatabaseSession.connect(settings: settings)
                guard !Task.isCancelled, let self else {
                    session.close()
                    return
                }
                self.states[connection.id] = .connected(serverVersion: session.serverVersion())
                self.tasks[connection.id] = nil
                self.onConnected?(connection, session)
            } catch {
                guard !Task.isCancelled, let self else { return }
                self.tasks[connection.id] = nil
                if let (host, fingerprint) = error.unknownHostKey {
                    self.states[connection.id] = nil
                    if let trusted = self.trustHostKey?(connection, host, fingerprint) {
                        self.connect(trusted)
                    }
                    return
                }
                self.states[connection.id] = .failed(message: error.displayMessage)
            }
        }
    }

    func cancel(_ id: SavedConnection.ID) {
        tasks.removeValue(forKey: id)?.cancel()
        states[id] = nil
    }

    /// The main window of this connection closed its session.
    func windowClosed(_ id: SavedConnection.ID) {
        states[id] = nil
    }

    /// Clears a failure so the row shows its normal state again.
    func dismissFailure(_ id: SavedConnection.ID) {
        if case .failed = states[id] { states[id] = nil }
    }
}
