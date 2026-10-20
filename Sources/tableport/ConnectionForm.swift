import AppKit
import TablePortCore
import MacKit
import SwiftUI

/// Create or edit a saved connection, with an optional SSH tunnel.
struct ConnectionForm: View {
    let target: FormTarget
    let store: ConnectionStore
    @Environment(\.dismiss) private var dismiss

    @State private var name = ""
    @State private var host = "localhost"
    @State private var port = "5432"
    @State private var database = "postgres"
    @State private var user = "postgres"
    @State private var password = ""
    @State private var tag: ConnectionTag?
    @State private var tls: TlsMode = .prefer
    @State private var readOnly = false

    @State private var useSSH = false
    @State private var sshHost = ""
    @State private var sshPort = "22"
    @State private var sshUser = NSUserName()
    @State private var sshAuth: SSHAuth = .key
    @State private var sshPassword = ""
    @State private var sshKeyPath = "~/.ssh/id_ed25519"
    @State private var sshPassphrase = ""
    @State private var trustedFingerprint: String?

    @State private var url = ""
    @State private var urlError: String?
    @State private var testState: TestState = .idle
    @State private var testTask: Task<Void, Never>?
    @State private var saveError: String?

    private enum TestState {
        case idle
        case running
        case succeeded(String)
        case failed(String)
    }

    private enum SSHAuth: String, CaseIterable {
        case key = "Key file"
        case password = "Password"
    }

    init(target: FormTarget, store: ConnectionStore) {
        self.target = target
        self.store = store
        guard case .edit(let connection) = target else { return }
        _name = State(initialValue: connection.name)
        _host = State(initialValue: connection.host)
        _port = State(initialValue: String(connection.port))
        _database = State(initialValue: connection.database)
        _user = State(initialValue: connection.user)
        _tag = State(initialValue: connection.tag)
        _tls = State(initialValue: connection.tls)
        _readOnly = State(initialValue: connection.readOnly)
        if let ssh = connection.ssh {
            _useSSH = State(initialValue: true)
            _sshHost = State(initialValue: ssh.host)
            _sshPort = State(initialValue: String(ssh.port))
            _sshUser = State(initialValue: ssh.user)
            _sshAuth = State(initialValue: ssh.keyPath == nil ? .password : .key)
            _sshKeyPath = State(initialValue: ssh.keyPath ?? "~/.ssh/id_ed25519")
            _trustedFingerprint = State(initialValue: ssh.trustedFingerprint)
        }
        do {
            _password = State(initialValue: try Keychain.secret(.databasePassword, for: connection.id) ?? "")
            _sshPassword = State(initialValue: try Keychain.secret(.sshPassword, for: connection.id) ?? "")
            _sshPassphrase = State(initialValue: try Keychain.secret(.sshKeyPassphrase, for: connection.id) ?? "")
        } catch {
            _saveError = State(initialValue: error.localizedDescription)
        }
    }

    private func number(_ text: String) -> Int? {
        Int(text).flatMap { (1...65535).contains($0) ? $0 : nil }
    }

    private var isComplete: Bool {
        let database = !trimmed(host).isEmpty && !trimmed(self.database).isEmpty && !trimmed(user).isEmpty
            && number(port) != nil
        let ssh = !useSSH || (!trimmed(sshHost).isEmpty && !trimmed(sshUser).isEmpty && number(sshPort) != nil
            && (sshAuth == .password || !trimmed(sshKeyPath).isEmpty))
        return database && ssh
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text(isNew ? "New Connection" : "Edit Connection")
                .font(.system(size: 15, weight: .semibold))

            Form {
                TextField("URL", text: $url, prompt: Text("postgres://user:password@host:5432/database"))
                    .onChange(of: url) { _, text in fill(from: text) }
                TextField("Name", text: $name, prompt: Text("local-db"))
                Picker("Tag", selection: $tag) {
                    Text("None").tag(ConnectionTag?.none)
                    ForEach(ConnectionTag.allCases) { tag in
                        Text(tag.label.capitalized).tag(Optional(tag))
                    }
                }
                TextField("Host", text: $host)
                    // A URL pasted into Host fills the whole form instead.
                    .onChange(of: host) { _, text in
                        if text.hasPrefix("postgres://") || text.hasPrefix("postgresql://") {
                            url = text
                        }
                    }
                TextField("Port", text: $port)
                TextField("Database", text: $database)
                TextField("User", text: $user)
                SecureField("Password", text: $password)
                Picker("TLS", selection: $tls) {
                    ForEach(TlsMode.allCases, id: \.self) { mode in
                        Text(mode.label).tag(mode)
                    }
                }
                Toggle("Read-only: the server refuses all changes", isOn: $readOnly)
                Toggle("Connect through SSH", isOn: $useSSH)
                if useSSH {
                    TextField("SSH host", text: $sshHost, prompt: Text("bastion.example.com"))
                    TextField("SSH port", text: $sshPort)
                    TextField("SSH user", text: $sshUser)
                    Picker("Sign in with", selection: $sshAuth) {
                        ForEach(SSHAuth.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    if sshAuth == .key {
                        HStack {
                            TextField("Key file", text: $sshKeyPath)
                            Button("Choose…", action: chooseKeyFile)
                        }
                        SecureField("Key passphrase", text: $sshPassphrase, prompt: Text("if the key has one"))
                    } else {
                        SecureField("SSH password", text: $sshPassword)
                    }
                }
            }
            .formStyle(.columns)

            if let urlError {
                Text(urlError)
                    .font(Theme.caption)
                    .foregroundStyle(Theme.danger)
            }

            if useSSH {
                Text("Host is the database server as the SSH server sees it, often localhost.")
                    .font(Theme.caption)
                    .foregroundStyle(Theme.inkSecondary)
            }

            testResult

            if let saveError {
                Text(saveError)
                    .font(Theme.caption)
                    .foregroundStyle(Theme.danger)
            }

            HStack {
                Button("Test", action: test)
                    .disabled(!isComplete)
                Spacer()
                Button("Cancel") {
                    testTask?.cancel()
                    dismiss()
                }
                .keyboardShortcut(.cancelAction)
                Button("Save", action: save)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!isComplete || trimmed(name).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 460)
    }

    @ViewBuilder
    private var testResult: some View {
        switch testState {
        case .idle:
            EmptyView()
        case .running:
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                Text("Connecting…").foregroundStyle(Theme.inkSecondary)
            }
            .font(Theme.caption)
        case .succeeded(let version):
            Label("Connected · PostgreSQL \(version)", systemImage: "checkmark.circle.fill")
                .font(Theme.caption)
                .foregroundStyle(Theme.successInk)
        case .failed(let message):
            Label(message, systemImage: "xmark.octagon.fill")
                .font(Theme.caption)
                .foregroundStyle(Theme.danger)
                .textSelection(.enabled)
        }
    }

    private var isNew: Bool {
        if case .new = target { return true }
        return false
    }

    private func trimmed(_ text: String) -> String {
        text.trimmingCharacters(in: .whitespaces)
    }

    /// Fills the fields a `postgres://` URL gives; the rest keep their values.
    private func fill(from text: String) {
        let text = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else {
            urlError = nil
            return
        }
        do {
            let parsed = try parseConnectionUrl(url: text)
            if let value = parsed.host { host = value }
            if let value = parsed.port { port = String(value) }
            if let value = parsed.database { database = value }
            if let value = parsed.user { user = value }
            if let value = parsed.password { password = value }
            if let value = parsed.tls { tls = value }
            if trimmed(name).isEmpty, let value = parsed.host {
                name = parsed.database.map { "\($0) @ \(value)" } ?? value
            }
            urlError = nil
        } catch {
            urlError = error.displayMessage
        }
    }

    private func chooseKeyFile() {
        let panel = NSOpenPanel()
        panel.message = "Choose an SSH private key"
        panel.showsHiddenFiles = true
        panel.directoryURL = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".ssh")
        if panel.runModal() == .OK, let url = panel.url {
            sshKeyPath = url.path
        }
    }

    /// The form's values as the core's settings, secrets included.
    private func settings() -> ConnectionSettings? {
        guard let port = number(port) else { return nil }
        var ssh: SshSettings?
        if useSSH, let sshPort = number(sshPort) {
            let byKey = sshAuth == .key
            ssh = SshSettings(
                host: trimmed(sshHost), port: UInt16(sshPort), user: trimmed(sshUser),
                password: byKey ? nil : sshPassword,
                privateKeyPath: byKey ? trimmed(sshKeyPath) : nil,
                keyPassphrase: byKey && !sshPassphrase.isEmpty ? sshPassphrase : nil,
                trustedFingerprint: trustedFingerprint
            )
        }
        return ConnectionSettings(
            host: trimmed(host), port: UInt16(port), database: trimmed(database), user: trimmed(user),
            password: password.isEmpty ? nil : password, tls: tls, ssh: ssh, readOnly: readOnly
        )
    }

    private func test() {
        guard let settings = settings() else { return }
        testTask?.cancel()
        testState = .running
        testTask = Task {
            do {
                let version = try await testConnection(settings: settings)
                guard !Task.isCancelled else { return }
                testState = .succeeded(version)
            } catch {
                guard !Task.isCancelled else { return }
                if let (host, fingerprint) = error.unknownHostKey {
                    testState = .idle
                    if HostKeyPrompt.ask(host: host, fingerprint: fingerprint) {
                        trustedFingerprint = fingerprint
                        test()
                    }
                    return
                }
                testState = .failed(error.displayMessage)
            }
        }
    }

    private func save() {
        guard let port = number(port) else { return }
        var connection = SavedConnection(
            name: trimmed(name), host: trimmed(host), port: port,
            database: trimmed(database), user: trimmed(user), tag: tag, tls: tls
        )
        connection.readOnly = readOnly
        if useSSH, let sshPort = number(sshPort) {
            connection.ssh = SavedSSH(
                host: trimmed(sshHost), port: sshPort, user: trimmed(sshUser),
                keyPath: sshAuth == .key ? trimmed(sshKeyPath) : nil,
                trustedFingerprint: trustedFingerprint
            )
        }
        if case .edit(let existing) = target { connection.id = existing.id }
        do {
            try Keychain.setSecret(password, .databasePassword, for: connection.id)
            let byKey = useSSH && sshAuth == .key
            try Keychain.setSecret(useSSH && !byKey ? sshPassword : "", .sshPassword, for: connection.id)
            try Keychain.setSecret(byKey ? sshPassphrase : "", .sshKeyPassphrase, for: connection.id)
        } catch {
            saveError = error.localizedDescription
            return
        }
        if isNew {
            store.add(connection)
        } else {
            store.update(connection)
        }
        testTask?.cancel()
        dismiss()
    }
}
