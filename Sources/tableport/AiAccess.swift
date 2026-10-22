import Combine
import Darwin
import Foundation
import os
import TablePortCore

/// The app's side of AI access (MCP): the connections the user allowed, their
/// settings with the Keychain password, and a log of what AI tools ran. The
/// server itself is in the core; it calls this from its own threads.
final class AiAccess: McpHost, @unchecked Sendable {
    static let socketURL: URL = {
        let support = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return support.appendingPathComponent(Bundle.main.bundleIdentifier ?? "tableport", isDirectory: true)
            .appendingPathComponent("mcp.sock")
    }()

    /// What an AI tool's configuration starts: this app's own executable,
    /// so the installed copy answers for itself.
    static var command: String { Bundle.main.executablePath ?? "/Applications/TablePort.app/Contents/MacOS/tableport" }

    private static let logger = Logger(subsystem: Bundle.main.bundleIdentifier ?? "tableport", category: "AiAccess")
    private let allowed = OSAllocatedUnfairLock<[SavedConnection]>(initialState: [])
    @MainActor let log: AiAccessLog
    @MainActor private var server: McpServer?
    @MainActor private var subscription: AnyCancellable?

    @MainActor
    init() {
        log = AiAccessLog()
    }

    @MainActor
    func start(store: ConnectionStore) {
        subscription = store.$connections.sink { [allowed] connections in
            allowed.withLock { $0 = connections.filter(\.aiAccess) }
        }
        do {
            try FileManager.default.createDirectory(at: Self.socketURL.deletingLastPathComponent(), withIntermediateDirectories: true)
            let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "dev"
            server = try startMcpServer(socketPath: Self.socketURL.path, version: version, host: self)
        } catch {
            Self.logger.error("AI access could not start: \(error.displayMessage, privacy: .public)")
        }
    }

    @MainActor
    func stop() {
        server?.stop()
        server = nil
    }

    func connections() -> [McpConnection] {
        allowed.withLock { $0 }.map {
            McpConnection(id: $0.id.uuidString, name: $0.name, database: $0.database,
                          allDatabases: $0.aiAllDatabases, tag: $0.tag?.rawValue)
        }
    }

    func settings(connectionId: String) -> ConnectionSettings? {
        guard let connection = allowed.withLock({ $0.first { $0.id.uuidString == connectionId } }) else { return nil }
        do {
            return try connection.settings()
        } catch {
            Self.logger.error("AI access could not read \(connection.name, privacy: .public)'s settings: \(error.localizedDescription, privacy: .public)")
            return nil
        }
    }

    func record(entry: McpLogEntry) {
        DispatchQueue.main.async { [log] in
            MainActor.assumeIsolated { log.add(entry) }
        }
    }
}

/// The last AI tool calls, newest first, for Settings › AI access.
@MainActor
final class AiAccessLog: ObservableObject {
    struct Entry: Identifiable {
        let id = UUID()
        let date: Date
        let call: McpLogEntry
    }

    @Published private(set) var entries: [Entry] = []

    func add(_ call: McpLogEntry) {
        entries.insert(Entry(date: Date(), call: call), at: 0)
        if entries.count > 200 { entries.removeLast(entries.count - 200) }
    }
}

/// `tableport mcp`, started by an AI tool: opens TablePort in the background
/// when it isn't running, then passes messages through to its socket.
enum McpCommand {
    static func run() -> Int32 {
        let path = AiAccess.socketURL.path
        if !canConnect(path) {
            let open = Process()
            open.executableURL = URL(fileURLWithPath: "/usr/bin/open")
            // -g: in the background, so the user's work keeps the focus.
            open.arguments = ["-g", Bundle.main.bundlePath]
            do {
                try open.run()
                open.waitUntilExit()
            } catch {
                FileHandle.standardError.write(Data("Could not open TablePort: \(error.localizedDescription)\n".utf8))
                return 1
            }
            for _ in 0..<75 where !canConnect(path) {
                usleep(200_000)
            }
        }
        return runMcpBridge(socketPath: path)
    }

    /// Whether the app answers on the socket; a file left by a crash doesn't.
    private static func canConnect(_ path: String) -> Bool {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return false }
        defer { close(fd) }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        guard bytes.count < MemoryLayout.size(ofValue: address.sun_path) else { return false }
        withUnsafeMutableBytes(of: &address.sun_path) { buffer in
            buffer.copyBytes(from: bytes)
        }
        let length = socklen_t(MemoryLayout<sockaddr_un>.size)
        return withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) { connect(fd, $0, length) == 0 }
        }
    }
}
