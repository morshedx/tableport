import AppKit
import TablePortCore
import MacKit
import SwiftUI

/// Create or edit a saved connection, with an optional SSH tunnel.
struct ConnectionForm: View {
    let target: FormTarget
    let store: ConnectionStore
    /// Closes the form's window.
    var onClose: () -> Void = {}

    @State private var name = ""
    @State private var host = "localhost"
    @State private var port = "5432"
    @State private var database = "postgres"
    @State private var user = "postgres"
    @State private var password = ""
    @State private var tag: ConnectionTag?
    @State private var tls: TlsMode = .prefer
    @State private var readOnly = false
    @State private var aiAccess = false
    @State private var aiAllDatabases = false
    @State private var showsPassword = false
    @State private var tab: FormTab = .connection

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

    init(target: FormTarget, store: ConnectionStore, onClose: @escaping () -> Void = {}) {
        self.target = target
        self.store = store
        self.onClose = onClose
        guard case .edit(let connection) = target else { return }
        _name = State(initialValue: connection.name)
        _host = State(initialValue: connection.host)
        _port = State(initialValue: String(connection.port))
        _database = State(initialValue: connection.database)
        _user = State(initialValue: connection.user)
        _tag = State(initialValue: connection.tag)
        _tls = State(initialValue: connection.tls)
        _readOnly = State(initialValue: connection.readOnly)
        _aiAccess = State(initialValue: connection.aiAccess)
        _aiAllDatabases = State(initialValue: connection.aiAllDatabases)
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

    private var isServerComplete: Bool {
        !trimmed(host).isEmpty && !trimmed(database).isEmpty && !trimmed(user).isEmpty && number(port) != nil
    }

    private var isSSHComplete: Bool {
        !useSSH || (!trimmed(sshHost).isEmpty && !trimmed(sshUser).isEmpty && number(sshPort) != nil
            && (sshAuth == .password || !trimmed(sshKeyPath).isEmpty))
    }

    private var isComplete: Bool { isServerComplete && isSSHComplete }

    private enum FormTab: String, CaseIterable {
        case connection = "Connection"
        case ssh = "SSH Tunnel"
        case access = "AI Access"

        /// The first tab holds everything a connection needs; the others
        /// are extras.
        var isOptional: Bool { self != .connection }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
                .padding(.horizontal, 22)
                .padding(.top, 20)
                .padding(.bottom, 14)
            VStack(alignment: .leading, spacing: 14) {
                urlBar
                tabBar
                Group {
                    switch tab {
                    case .connection: connectionTab
                    case .ssh: sshTab
                    case .access: accessTab
                    }
                }
                // The same height for every tab, so the sheet never grows
                // past the screen and the buttons don't jump.
                .frame(height: 386, alignment: .top)
                messages
            }
            .padding(.horizontal, 22)
            .padding(.bottom, 14)
            Divider()
            footer
                .padding(.horizontal, 22)
                .padding(.vertical, 14)
        }
        .frame(width: 520)
        .background(Theme.windowBg)
        .animation(.easeOut(duration: 0.15), value: useSSH)
        .animation(.easeOut(duration: 0.15), value: aiAccess)
    }

    /// The three parts of the form; a dot marks one that is switched on or
    /// that still needs a field.
    private var tabBar: some View {
        HStack(spacing: 2) {
            ForEach(FormTab.allCases, id: \.self) { option in
                Button {
                    tab = option
                } label: {
                    HStack(spacing: 5) {
                        Text(option.rawValue)
                        if option.isOptional {
                            Text("optional").font(.system(size: 10)).foregroundStyle(Theme.inkSecondary.opacity(0.8))
                        }
                        if let dot = dot(for: option) {
                            Circle().fill(dot).frame(width: 6, height: 6)
                        }
                    }
                    .font(.system(size: 12, weight: tab == option ? .semibold : .regular))
                    .foregroundStyle(tab == option ? Theme.ink : Theme.inkSecondary)
                    .frame(maxWidth: .infinity)
                    .frame(height: 26)
                    .background(tab == option ? Theme.card : Color.clear, in: RoundedRectangle(cornerRadius: 6))
                    .shadow(color: .black.opacity(tab == option ? 0.08 : 0), radius: 1, y: 0.5)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityLabel(option.rawValue)
                .accessibilityAddTraits(tab == option ? [.isButton, .isSelected] : .isButton)
            }
        }
        .padding(2)
        .background(Theme.hairline, in: RoundedRectangle(cornerRadius: 8))
    }

    private func dot(for tab: FormTab) -> Color? {
        switch tab {
        case .connection:
            return isServerComplete ? nil : Theme.Tile.orange
        case .ssh:
            guard useSSH else { return nil }
            return isSSHComplete ? Theme.accent : Theme.Tile.orange
        case .access:
            return aiAccess ? Theme.accent : nil
        }
    }

    private var connectionTab: some View {
        VStack(alignment: .leading, spacing: 14) {
            FormCard("Connection") {
                FormRow("Name") {
                    TextField("Name", text: $name, prompt: Text("My database"))
                }
                FormRow("Tag") {
                    tagChips
                }
                SwitchRow("Read-only", hint: "Browse and query, never change data. The server refuses every write.",
                          isOn: $readOnly)
            }
            FormCard("Server") {
                FormRow("Host") {
                    HStack(spacing: 10) {
                        TextField("Host", text: $host, prompt: Text("localhost"))
                            // A URL pasted into Host fills the whole form instead.
                            .onChange(of: host) { _, text in
                                if text.hasPrefix("postgres://") || text.hasPrefix("postgresql://") { url = text }
                            }
                        Text("Port").foregroundStyle(Theme.inkSecondary)
                        TextField("Port", text: $port, prompt: Text("5432"))
                            .frame(width: 52)
                    }
                }
                FormRow("Database") {
                    TextField("Database", text: $database, prompt: Text("postgres"))
                }
                FormRow("User") {
                    TextField("User", text: $user, prompt: Text("postgres"))
                }
                FormRow("Password") {
                    HStack(spacing: 6) {
                        Group {
                            if showsPassword {
                                TextField("Password", text: $password, prompt: Text("none"))
                            } else {
                                SecureField("Password", text: $password, prompt: Text("none"))
                            }
                        }
                        Button {
                            showsPassword.toggle()
                        } label: {
                            Image(systemName: showsPassword ? "eye.slash" : "eye")
                                .foregroundStyle(Theme.inkSecondary)
                        }
                        .buttonStyle(.borderless)
                        .help(showsPassword ? "Hide the password" : "Show the password")
                    }
                }
                FormRow("TLS") {
                    Picker("TLS", selection: $tls) {
                        ForEach(TlsMode.allCases, id: \.self) { Text($0.label).tag($0) }
                    }
                    .labelsHidden()
                    .fixedSize()
                }
            }
        }
    }

    private var accessTab: some View {
        VStack(alignment: .leading, spacing: 14) {
            FormCard("AI tools") {
                SwitchRow("Allow AI access", hint: "Claude, Cursor and other AI tools can read it, never change it.",
                          isOn: $aiAccess)
                if aiAccess {
                    SwitchRow("Every database on this server",
                              hint: "With the same login, still read-only. For many databases on one server.",
                              isOn: $aiAllDatabases, indent: true)
                }
            }
        }
    }

    private var header: some View {
        HStack(spacing: 12) {
            ZStack {
                RoundedRectangle(cornerRadius: 10).fill(Color(red: 0.2, green: 0.4, blue: 0.62).opacity(0.12))
                if let logo = Bundle.resources?.image(forResource: "postgresql") {
                    Image(nsImage: logo).resizable().scaledToFit().padding(7)
                } else {
                    Image(systemName: "cylinder.split.1x2").font(.system(size: 18))
                }
            }
            .frame(width: 40, height: 40)
            VStack(alignment: .leading, spacing: 2) {
                Text(isNew ? "New Connection" : "Edit Connection")
                    .font(.system(size: 15, weight: .semibold))
                    .foregroundStyle(Theme.ink)
                Text(summary)
                    .font(Theme.caption)
                    .foregroundStyle(Theme.inkSecondary)
                    .lineLimit(1)
            }
            Spacer()
            if let tag {
                TagChip(tag: tag, selected: true)
            }
        }
    }

    /// "PostgreSQL · user@host:port/database", as the form stands.
    private var summary: String {
        let host = trimmed(self.host).isEmpty ? "host" : trimmed(self.host)
        let database = trimmed(self.database).isEmpty ? "database" : trimmed(self.database)
        return "PostgreSQL · \(trimmed(user))@\(host):\(port)/\(database)"
    }

    private var urlBar: some View {
        HStack(spacing: 8) {
            Image(systemName: "link")
                .foregroundStyle(Theme.inkSecondary)
            TextField("URL", text: $url, prompt: Text("Paste a postgres:// URL to fill the form"))
                .textFieldStyle(.roundedBorder)
                .onChange(of: url) { _, text in fill(from: text) }
        }
    }

    private var tagChips: some View {
        HStack(spacing: 6) {
            Button("None") { tag = nil }
                .buttonStyle(ChipButtonStyle(color: Theme.inkSecondary, selected: tag == nil))
            ForEach(ConnectionTag.allCases) { option in
                Button {
                    tag = option
                } label: {
                    HStack(spacing: 5) {
                        Circle().fill(option.color).frame(width: 7, height: 7)
                        Text(option.shortLabel)
                    }
                    .lineLimit(1)
                    .fixedSize()
                }
                .buttonStyle(ChipButtonStyle(color: option.color, selected: tag == option))
            }
        }
    }

    private var sshTab: some View {
        VStack(alignment: .leading, spacing: 14) {
            FormCard("SSH tunnel") {
                SwitchRow("Connect through SSH", hint: "Reach a server inside a private network through a bastion host.",
                          isOn: $useSSH)
            }
            if useSSH { sshFields }
        }
    }

    private var sshFields: some View {
        FormCard("Bastion") {
            if useSSH {
                FormRow("SSH host") {
                    HStack(spacing: 10) {
                        TextField("SSH host", text: $sshHost, prompt: Text("bastion.example.com"))
                        Text("Port").foregroundStyle(Theme.inkSecondary)
                        TextField("SSH port", text: $sshPort, prompt: Text("22"))
                            .frame(width: 52)
                    }
                }
                FormRow("SSH user") {
                    TextField("SSH user", text: $sshUser)
                }
                FormRow("Sign in") {
                    Picker("Sign in with", selection: $sshAuth) {
                        ForEach(SSHAuth.allCases, id: \.self) { Text($0.rawValue).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                    .fixedSize()
                }
                if sshAuth == .key {
                    FormRow("Key file") {
                        HStack(spacing: 8) {
                            TextField("Key file", text: $sshKeyPath)
                            Button("Choose…", action: chooseKeyFile).controlSize(.small)
                        }
                    }
                    FormRow("Passphrase") {
                        SecureField("Key passphrase", text: $sshPassphrase, prompt: Text("if the key has one"))
                    }
                } else {
                    FormRow("Password") {
                        SecureField("SSH password", text: $sshPassword)
                    }
                }
                Text("Host on the Connection tab is the database server as the SSH server sees it, often localhost.")
                    .font(Theme.meta)
                    .foregroundStyle(Theme.inkSecondary)
                    .padding(.horizontal, 12)
                    .padding(.vertical, 8)
            }
        }
    }

    @ViewBuilder
    private var messages: some View {
        // A failure can be long: it gets the full width. Progress and success
        // are short and sit next to Test.
        if case .failed = testState {
            testResult
                .lineLimit(3)
                .fixedSize(horizontal: false, vertical: true)
        }
        if let urlError {
            Label(urlError, systemImage: "exclamationmark.triangle")
                .font(Theme.caption)
                .foregroundStyle(Theme.danger)
        }
        if let saveError {
            Label(saveError, systemImage: "exclamationmark.triangle")
                .font(Theme.caption)
                .foregroundStyle(Theme.danger)
        }
    }

    private var footer: some View {
        HStack(spacing: 10) {
            Button(action: test) {
                Label("Test", systemImage: "bolt")
            }
            .disabled(!isComplete)
            switch testState {
            case .running, .succeeded:
                testResult.lineLimit(1)
            case .idle, .failed:
                EmptyView()
            }
            Spacer(minLength: 8)
            Button("Cancel") {
                testTask?.cancel()
                onClose()
            }
            .keyboardShortcut(.cancelAction)
            Button(isNew ? "Add Connection" : "Save", action: save)
                .keyboardShortcut(.defaultAction)
                .buttonStyle(.borderedProminent)
                .disabled(!isComplete || trimmed(name).isEmpty)
        }
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
            // Short, to fit beside Test: "Connected · 17.7"; the full version on hover.
            Label("Connected · \(version.split(separator: " ").first.map(String.init) ?? version)",
                  systemImage: "checkmark.circle.fill")
                .help("PostgreSQL \(version)")
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
            password: password.isEmpty ? nil : password, tls: tls, ssh: ssh, readOnly: readOnly,
            engine: .postgres
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
                // An SSH failure is fixed on the SSH tab: show it.
                if useSSH, error.displayMessage.localizedCaseInsensitiveContains("ssh") { tab = .ssh }
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
        connection.aiAccess = aiAccess
        connection.aiAllDatabases = aiAccess && aiAllDatabases
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
        onClose()
    }
}

/// A titled group of rows on a card, as in System Settings.
private struct FormCard<Content: View, Trailing: View>: View {
    let title: String
    let trailing: Trailing
    let content: Content

    init(_ title: String, @ViewBuilder trailing: () -> Trailing, @ViewBuilder content: () -> Content) {
        self.title = title
        self.trailing = trailing()
        self.content = content()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(title.uppercased())
                    .font(.system(size: 10.5, weight: .semibold))
                    .tracking(0.4)
                    .foregroundStyle(Theme.inkSecondary)
                Spacer()
                trailing.controlSize(.small)
            }
            .padding(.horizontal, 4)
            VStack(spacing: 0) {
                _VariadicView.Tree(DividedRows()) { content }
            }
            .background(Theme.card, in: RoundedRectangle(cornerRadius: 9))
            .overlay(RoundedRectangle(cornerRadius: 9).stroke(Theme.hairline))
        }
    }
}

extension FormCard where Trailing == EmptyView {
    init(_ title: String, @ViewBuilder content: () -> Content) {
        self.init(title, trailing: { EmptyView() }, content: content)
    }
}

/// Hairlines between a card's rows, inset from the left as in System Settings.
private struct DividedRows: _VariadicView_MultiViewRoot {
    func body(children: _VariadicView.Children) -> some View {
        let last = children.last?.id
        ForEach(children) { child in
            child
            if child.id != last {
                Divider().padding(.leading, 12)
            }
        }
    }
}

/// A label on the left and a borderless field on the right.
private struct FormRow<Field: View>: View {
    let label: String
    let field: Field

    init(_ label: String, @ViewBuilder field: () -> Field) {
        self.label = label
        self.field = field()
    }

    var body: some View {
        HStack(spacing: 12) {
            Text(label)
                .foregroundStyle(Theme.ink)
                .frame(width: 84, alignment: .leading)
            // The system field style, so the focused field shows macOS' own focus ring.
            field
                .textFieldStyle(.roundedBorder)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, 12)
        .frame(minHeight: 38)
    }
}

/// A setting that is on or off, with a line saying what it does.
private struct SwitchRow: View {
    let title: String
    let hint: String
    @Binding var isOn: Bool
    var indent = false

    init(_ title: String, hint: String, isOn: Binding<Bool>, indent: Bool = false) {
        self.title = title
        self.hint = hint
        _isOn = isOn
        self.indent = indent
    }

    var body: some View {
        HStack(spacing: 12) {
            if indent {
                Image(systemName: "arrow.turn.down.right")
                    .font(.system(size: 11))
                    .foregroundStyle(Theme.inkSecondary)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(title).foregroundStyle(Theme.ink)
                Text(hint)
                    .font(Theme.meta)
                    .foregroundStyle(Theme.inkSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 12)
            Toggle(title, isOn: $isOn)
                .toggleStyle(ThemeToggleStyle())
                .labelsHidden()
        }
        .padding(.horizontal, 12)
        .padding(.leading, indent ? 12 : 0)
        .padding(.vertical, 9)
    }
}

/// A connection's tag as a small coloured capsule.
private struct TagChip: View {
    let tag: ConnectionTag
    let selected: Bool

    var body: some View {
        HStack(spacing: 5) {
            Circle().fill(tag.color).frame(width: 7, height: 7)
            Text(tag.label.capitalized)
        }
        .lineLimit(1)
        .fixedSize()
        .font(.system(size: 11, weight: .medium))
        .padding(.horizontal, 8)
        .padding(.vertical, 3)
        .background(tag.color.opacity(0.14), in: Capsule())
        .foregroundStyle(tag.color)
    }
}

/// The tag choices: a capsule, filled in the tag's colour when chosen.
private struct ChipButtonStyle: ButtonStyle {
    let color: Color
    let selected: Bool

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 11, weight: .medium))
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(selected ? color.opacity(0.16) : Color.clear, in: Capsule())
            .overlay(Capsule().stroke(selected ? color.opacity(0.55) : Theme.hairline))
            .foregroundStyle(selected ? color : Theme.inkSecondary)
            .opacity(configuration.isPressed ? 0.7 : 1)
            .contentShape(Capsule())
    }
}

private extension ConnectionTag {
    /// Short enough for the tag chips to fit on one row.
    var shortLabel: String {
        switch self {
        case .local: "Local"
        case .development: "Dev"
        case .staging: "Staging"
        case .production: "Prod"
        }
    }
}

/// New and Edit Connection in a window of their own: it can be moved, and
/// it opens over the window that asked for it. One at a time.
@MainActor
enum ConnectionFormWindow {
    private static var window: NSWindow?

    static func show(_ target: FormTarget, store: ConnectionStore, over parent: NSWindow?) {
        window?.close()
        let panel = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 520, height: 574),
                             styleMask: [.titled, .closable, .fullSizeContentView], backing: .buffered, defer: false)
        panel.titlebarAppearsTransparent = true
        panel.titleVisibility = .hidden
        panel.title = target.isNew ? "New Connection" : "Edit Connection"
        panel.isMovableByWindowBackground = true
        panel.isReleasedWhenClosed = false
        // The form has its own Cancel (Esc); the title bar buttons would sit on its header.
        for button in [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton] {
            panel.standardWindowButton(button)?.isHidden = true
        }
        let form = ConnectionForm(target: target, store: store) { [weak panel] in panel?.close() }
        let host = NSHostingController(rootView: form)
        host.sizingOptions = .preferredContentSize
        // No room kept for the hidden title bar: the form's header is the top.
        host.safeAreaRegions = []
        panel.contentViewController = host
        panel.setContentSize(host.view.fittingSize)
        place(panel, over: parent)
        panel.makeKeyAndOrderFront(nil)
        window = panel
    }

    /// Centred over `parent`, a little below its top, and inside its screen.
    private static func place(_ panel: NSWindow, over parent: NSWindow?) {
        guard let parent, let screen = parent.screen ?? NSScreen.main else {
            panel.center()
            return
        }
        let size = panel.frame.size
        let area = screen.visibleFrame
        var origin = NSPoint(x: parent.frame.midX - size.width / 2, y: parent.frame.maxY - 40 - size.height)
        origin.x = min(max(origin.x, area.minX + 8), area.maxX - size.width - 8)
        origin.y = min(max(origin.y, area.minY + 8), area.maxY - size.height - 8)
        panel.setFrameOrigin(origin)
    }
}

extension FormTarget {
    var isNew: Bool {
        if case .new = self { return true }
        return false
    }
}
