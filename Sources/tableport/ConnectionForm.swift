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
    /// Chosen in the header for a new connection; an edit keeps its own.
    @State private var engine: Engine = .postgres
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

    @AppStorage(OracleClient.directoryKey) private var oracleClientDirectory = ""
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

    init(target: FormTarget, store: ConnectionStore, engine: Engine = .postgres, onClose: @escaping () -> Void = {}) {
        self.target = target
        self.store = store
        self.onClose = onClose
        _engine = State(initialValue: engine)
        if engine.isSqlite || engine == .firestore {
            // A file or a URL has no server defaults; Firestore's host is the emulator.
            _host = State(initialValue: "")
            _database = State(initialValue: "")
            _user = State(initialValue: "")
        }
        guard case .edit(let connection) = target else { return }
        _engine = State(initialValue: connection.engine)
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

    /// A MongoDB connection string names its own hosts and ports.
    /// A MongoDB connection string or an Oracle connect descriptor names its
    /// own hosts and ports, and an Oracle descriptor its service.
    private var isConnectionString: Bool {
        (engine == .mongo && host.contains("://")) || (engine == .oracle && trimmed(host).hasPrefix("("))
    }

    private var serverPort: Int? { isConnectionString ? 0 : number(port) }

    private func number(_ text: String) -> Int? {
        Int(text).flatMap { (1...65535).contains($0) ? $0 : nil }
    }

    private var isServerComplete: Bool {
        // Firestore needs the project, and a key unless it is the emulator.
        if engine == .firestore { return !trimmed(user).isEmpty && (!trimmed(password).isEmpty || !trimmed(host).isEmpty) }
        // D1 needs the account, the database and a token.
        if engine == .d1 { return ![user, database, password].contains { trimmed($0).isEmpty } }
        if engine.isSqlite { return !trimmed(database).isEmpty }
        // MySQL shows every database of the server; naming one is optional.
        let hasDatabase = fields.databaseIsOptional || isConnectionString || !trimmed(database).isEmpty
        // Redis logs in as its default user, and MongoDB may need no login.
        let hasUser = fields.userIsOptional || !trimmed(user).isEmpty
        return !trimmed(host).isEmpty && hasDatabase && hasUser && serverPort != nil
    }

    private var isSSHComplete: Bool {
        !useSSH || (!trimmed(sshHost).isEmpty && !trimmed(sshUser).isEmpty && number(sshPort) != nil
            && (sshAuth == .password || !trimmed(sshKeyPath).isEmpty))
    }

    private var isComplete: Bool { isServerComplete && isSSHComplete }

    private enum FormTab: String, CaseIterable {
        case connection = "Connection"
        /// Oracle only: the folder of Oracle's client library.
        case client = "Instant Client"
        case ssh = "SSH Tunnel"
        case access = "AI Access"

        /// Connection (and Oracle's Instant Client) hold what a connection
        /// needs; the others are extras.
        var isOptional: Bool { self == .ssh || self == .access }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
                .padding(.horizontal, 22)
                .padding(.top, 20)
                .padding(.bottom, 14)
            VStack(alignment: .leading, spacing: 14) {
                // URLs are for servers; a file is chosen on the Connection tab.
                // Cassandra has no connection URL.
                if fields.urlPrompt != nil { urlBar }
                tabBar
                Group {
                    switch tab {
                    case .connection: connectionTab
                    case .client: OracleClientCard()
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
            ForEach(tabs, id: \.self) { option in
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

    /// A file needs no SSH tunnel; only Oracle needs a client library.
    private var tabs: [FormTab] {
        // A file, and the HTTP engines, need no SSH tunnel.
        if engine.isSqlite || engine == .firestore { return [.connection, .access] }
        if engine == .oracle { return [.connection, .client, .ssh, .access] }
        return [.connection, .ssh, .access]
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
        case .client:
            return oracleClientDirectory.isEmpty ? Theme.Tile.orange : nil
        }
    }

    @ViewBuilder
    private var connectionTab: some View {
        switch engine {
        case .sqlite: sqliteTab
        case .libsql: libsqlTab
        case .d1: d1Tab
        case .firestore: firestoreTab
        case .postgres, .mysql, .redis, .mongo, .mssql, .cassandra, .oracle: serverTab
        }
    }

    /// A SQLite file: the connection's name and tag, and the file.
    private var sqliteTab: some View {
        VStack(alignment: .leading, spacing: 14) {
            FormCard("Connection") {
                FormRow("Name") {
                    TextField("Name", text: $name, prompt: Text("My database"))
                }
                FormRow("Tag") {
                    tagChips
                }
                SwitchRow("Read-only", hint: "Browse and query, never change data. SQLite opens the file read-only.",
                          isOn: $readOnly)
            }
            FormCard("Database file") {
                FormRow("File") {
                    HStack(spacing: 8) {
                        TextField("File", text: $database, prompt: Text("~/data/app.sqlite"))
                        Button("Choose…", action: chooseDatabaseFile)
                    }
                }
            }
        }
    }

    /// A libSQL server or Turso database: its URL and auth token.
    /// Firestore: the project, the database, and a service account's key,
    /// or the emulator's address instead of a key.
    private var firestoreTab: some View {
        VStack(alignment: .leading, spacing: 14) {
            FormCard("Connection") {
                FormRow("Name") {
                    TextField("Name", text: $name, prompt: Text("My database"))
                }
                FormRow("Tag") {
                    tagChips
                }
                SwitchRow("Read-only", hint: "Browse and query, never change data. TablePort runs only reads.", isOn: $readOnly)
            }
            FormCard("Database") {
                FormRow("Project ID") {
                    TextField("Project ID", text: $user, prompt: Text("from the key, or my-project"))
                }
                FormRow("Database") {
                    TextField("Database", text: $database, prompt: Text("(default)"))
                }
                FormRow("Key") {
                    HStack(spacing: 6) {
                        Text(firestoreKeyLabel)
                            .foregroundStyle(password.isEmpty ? Theme.inkSecondary : Theme.ink)
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .frame(maxWidth: .infinity, alignment: .leading)
                        Button("Choose…", action: chooseFirestoreKey)
                    }
                }
                FormRow("Emulator") {
                    TextField("Emulator", text: $host, prompt: Text("none, or 127.0.0.1:8080"))
                }
            }
            Text("Create a key under IAM → Service accounts → Keys in the Google Cloud console. TablePort keeps it in the Keychain. The emulator needs no key.")
                .font(Theme.meta)
                .foregroundStyle(Theme.inkSecondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    /// The key's service account, as its JSON names it.
    private var firestoreKeyLabel: String {
        guard !password.isEmpty else { return "none" }
        let key = (try? JSONSerialization.jsonObject(with: Data(password.utf8))) as? [String: Any]
        return key?["client_email"] as? String ?? "a key that isn't a service account's"
    }

    private func chooseFirestoreKey() {
        let panel = NSOpenPanel()
        panel.message = "Choose the service account's JSON key"
        panel.allowedContentTypes = [.json]
        guard panel.runModal() == .OK, let url = panel.url, let text = try? String(contentsOf: url, encoding: .utf8) else { return }
        password = text
        // The key names its project.
        if trimmed(user).isEmpty,
           let key = (try? JSONSerialization.jsonObject(with: Data(text.utf8))) as? [String: Any],
           let project = key["project_id"] as? String {
            user = project
        }
    }

    /// Cloudflare D1: the account and database IDs, and an API token.
    private var d1Tab: some View {
        VStack(alignment: .leading, spacing: 14) {
            FormCard("Connection") {
                FormRow("Name") {
                    TextField("Name", text: $name, prompt: Text("My database"))
                }
                FormRow("Tag") {
                    tagChips
                }
                SwitchRow("Read-only", hint: "Browse and query, never change data. TablePort refuses every write.",
                          isOn: $readOnly)
            }
            FormCard("Database") {
                FormRow("Account ID") {
                    TextField("Account ID", text: $user, prompt: Text("32 hex characters"))
                }
                FormRow("Database ID") {
                    TextField("Database ID", text: $database, prompt: Text("xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"))
                }
                FormRow("API token") {
                    HStack(spacing: 6) {
                        Group {
                            if showsPassword {
                                TextField("API token", text: $password, prompt: Text("with D1 Edit, or D1 Read"))
                            } else {
                                SecureField("API token", text: $password, prompt: Text("with D1 Edit, or D1 Read"))
                            }
                        }
                        Button {
                            showsPassword.toggle()
                        } label: {
                            Image(systemName: showsPassword ? "eye.slash" : "eye")
                                .foregroundStyle(Theme.inkSecondary)
                        }
                        .buttonStyle(.borderless)
                        .help(showsPassword ? "Hide the token" : "Show the token")
                    }
                }
            }
            Text("The IDs are on the database's page in the Cloudflare dashboard, or from wrangler d1 list. Create the token under My Profile → API Tokens.")
                .font(Theme.meta)
                .foregroundStyle(Theme.inkSecondary)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private var libsqlTab: some View {
        VStack(alignment: .leading, spacing: 14) {
            FormCard("Connection") {
                FormRow("Name") {
                    TextField("Name", text: $name, prompt: Text("My database"))
                }
                FormRow("Tag") {
                    tagChips
                }
                SwitchRow("Read-only", hint: "Browse and query, never change data. TablePort refuses every write.",
                          isOn: $readOnly)
            }
            FormCard("Database") {
                FormRow("URL") {
                    TextField("URL", text: $database, prompt: Text("libsql://my-db-org.turso.io"))
                }
                FormRow("Token") {
                    HStack(spacing: 6) {
                        Group {
                            if showsPassword {
                                TextField("Token", text: $password, prompt: Text("none, for a local server"))
                            } else {
                                SecureField("Token", text: $password, prompt: Text("none, for a local server"))
                            }
                        }
                        Button {
                            showsPassword.toggle()
                        } label: {
                            Image(systemName: showsPassword ? "eye.slash" : "eye")
                                .foregroundStyle(Theme.inkSecondary)
                        }
                        .buttonStyle(.borderless)
                        .help(showsPassword ? "Hide the token" : "Show the token")
                    }
                }
            }
            Text("Turso: create a token with turso db tokens create <database>.")
                .font(Theme.meta)
                .foregroundStyle(Theme.inkSecondary)
        }
    }

    private var serverTab: some View {
        VStack(alignment: .leading, spacing: 14) {
            FormCard("Connection") {
                FormRow("Name") {
                    TextField("Name", text: $name, prompt: Text("My database"))
                }
                FormRow("Tag") {
                    tagChips
                }
                SwitchRow("Read-only", hint: fields.readOnlyHint, isOn: $readOnly)
            }
            FormCard("Server") {
                FormRow("Host") {
                    HStack(spacing: 10) {
                        TextField("Host", text: $host, prompt: Text("localhost"))
                            // A URL pasted into Host fills the whole form instead.
                            .onChange(of: host) { _, text in
                                if ["postgres://", "postgresql://", "mysql://", "mariadb://", "redis://", "rediss://", "mongodb://", "mongodb+srv://",
                                    "sqlserver://", "jdbc:sqlserver://"].contains(where: text.hasPrefix)
                                    || ["server=", "data source="].contains(where: text.lowercased().hasPrefix) { url = text }
                            }
                        if !isConnectionString {
                            Text("Port").foregroundStyle(Theme.inkSecondary)
                            TextField("Port", text: $port, prompt: Text(Self.defaults(engine).port))
                                .frame(width: 52)
                        }
                    }
                }
                FormRow(fields.databaseLabel) {
                    TextField(fields.databaseLabel, text: $database, prompt: Text(fields.databasePrompt))
                }
                FormRow("User") {
                    TextField("User", text: $user, prompt: Text(fields.userPrompt))
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
                if aiAccess && fields.offersEveryDatabase {
                    SwitchRow("Every database on this server",
                              hint: "With the same login, still read-only. For many databases on one server.",
                              isOn: $aiAllDatabases, indent: true)
                }
            }
            if let warning = fields.loginWarning {
                loginAccessWarning(warning)
            }
            if engine == .firestore {
                Label("For AI access, a service account with the Cloud Datastore Viewer role only is safest.", systemImage: "info.circle")
                    .font(Theme.meta)
                    .foregroundStyle(Theme.inkSecondary)
            }
            if engine == .d1 {
                Label("For AI access, a token with D1 Read only is safest.", systemImage: "info.circle")
                    .font(Theme.meta)
                    .foregroundStyle(Theme.inkSecondary)
            }
            if engine == .libsql {
                Label("For AI access, a read-only token is safest: turso db tokens create <database> --read-only.",
                      systemImage: "info.circle")
                    .font(Theme.meta)
                    .foregroundStyle(Theme.inkSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    /// A MySQL or Cassandra login reads every database or keyspace it has
    /// rights to; TablePort can't keep an AI tool to one, so the user must
    /// know before turning it on.
    private func loginAccessWarning(_ warning: (title: String, detail: String)) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(Theme.warning)
                .font(.system(size: 15))
            VStack(alignment: .leading, spacing: 4) {
                Text(warning.title)
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Theme.ink)
                Text(warning.detail)
                    .font(Theme.meta)
                    .foregroundStyle(Theme.inkSecondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(Theme.warning.opacity(0.12), in: RoundedRectangle(cornerRadius: Theme.radiusItem))
        .overlay(RoundedRectangle(cornerRadius: Theme.radiusItem).stroke(Theme.warning.opacity(0.35)))
    }

    private var header: some View {
        HStack(spacing: 12) {
            ZStack {
                RoundedRectangle(cornerRadius: 10).fill(Color(red: 0.2, green: 0.4, blue: 0.62).opacity(0.12))
                EngineLogo(engine: engine, size: 26)
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
            if isNew {
                Picker("Database", selection: $engine) {
                    ForEach([Engine.postgres, .mysql, .mssql, .oracle, .sqlite, .libsql, .d1, .redis, .mongo, .firestore, .cassandra], id: \.self) { engine in
                        Text(engine == .mysql ? "MySQL / MariaDB" : engine == .libsql ? "libSQL / Turso" : engine.label).tag(engine)
                    }
                }
                .labelsHidden()
                .fixedSize()
                .help("Database")
            }
        }
        .onChange(of: engine) { old, engine in switchEngine(from: old, to: engine) }
    }

    private var fields: EngineFields { EngineFields.of(engine) }

    /// Port, user and database a new connection starts with.
    private static func defaults(_ engine: Engine) -> (port: String, user: String, database: String) {
        switch engine {
        case .postgres: ("5432", "postgres", "postgres")
        case .mysql: ("3306", "root", "")
        case .redis: ("6379", "", "0")
        case .mongo: ("27017", "", "")
        case .mssql: ("1433", "sa", "")
        case .cassandra: ("9042", "", "")
        case .oracle: ("1521", "system", "FREEPDB1")
        case .sqlite, .libsql, .d1, .firestore: ("", "", "")
        }
    }

    /// Swaps one engine's defaults for another's, keeping what the user typed.
    private func switchEngine(from old: Engine, to engine: Engine) {
        testTask?.cancel()
        testState = .idle
        let before = Self.defaults(old)
        let after = Self.defaults(engine)
        if !tabs.contains(tab) { tab = .connection }
        // Firestore's host is the emulator's: "localhost" there would mean the emulator.
        if engine.isSqlite || engine == .firestore {
            if host == "localhost" { host = "" }
            if tab == .ssh { tab = .connection }
        } else if trimmed(host).isEmpty {
            host = "localhost"
        }
        if port == before.port || trimmed(port).isEmpty { port = after.port }
        // Redis servers rarely speak TLS unless told to; trying first costs a second.
        if engine == .redis && tls == .prefer { tls = .disable }
        if user == before.user || trimmed(user).isEmpty { user = after.user }
        // A file's path is no server database, and back.
        // A file's path or a URL is no server database, and back.
        if database == before.database || database.contains("/") || old.isSqlite { database = after.database }
    }

    /// "PostgreSQL · user@host:port/database", or "SQLite · file", as the form stands.
    private var summary: String {
        if engine == .sqlite {
            let file = trimmed(database).isEmpty ? "choose a file" : (trimmed(database) as NSString).lastPathComponent
            return "SQLite · \(file)"
        }
        if engine == .firestore {
            if !trimmed(host).isEmpty { return "Firestore emulator · \(trimmed(host))" }
            return "Firestore · \(trimmed(user).isEmpty ? "project ID" : trimmed(user))"
        }
        if engine == .d1 {
            return "Cloudflare D1 · \(trimmed(database).isEmpty ? "database ID" : trimmed(database))"
        }
        if engine == .libsql {
            let host = URL(string: trimmed(database))?.host ?? (trimmed(database).isEmpty ? "enter a URL" : trimmed(database))
            return "libSQL · \(host)"
        }
        let host = trimmed(self.host).isEmpty ? "host" : trimmed(self.host)
        if engine == .mysql {
            let database = trimmed(self.database).isEmpty ? "" : "/\(trimmed(self.database))"
            return "MySQL · \(trimmed(user))@\(host):\(port)\(database)"
        }
        if engine == .redis {
            let user = trimmed(self.user).isEmpty ? "" : "\(trimmed(self.user))@"
            return "Redis · \(user)\(host):\(port)/\(trimmed(database).isEmpty ? "0" : trimmed(database))"
        }
        if engine == .oracle {
            // A connect descriptor shows its host only.
            let shown = isConnectionString ? SavedConnection.displayHost(of: host) : "\(host):\(port)/\(trimmed(database))"
            return "Oracle · \(trimmed(user))@\(shown)"
        }
        if engine == .cassandra {
            let user = trimmed(self.user).isEmpty ? "" : "\(trimmed(self.user))@"
            return "Cassandra · \(user)\(host):\(port)\(trimmed(database).isEmpty ? "" : "/\(trimmed(database))")"
        }
        if engine == .mssql {
            let database = trimmed(self.database).isEmpty ? "" : "/\(trimmed(self.database))"
            return "SQL Server · \(trimmed(user))@\(host):\(port)\(database)"
        }
        if engine == .mongo {
            // A connection string shows its hosts only.
            let shown = host.contains("://") ? SavedConnection.displayHost(of: host) : "\(host):\(port)"
            return "MongoDB · \(shown)\(trimmed(database).isEmpty ? "" : "/\(trimmed(database))")"
        }
        let database = trimmed(self.database).isEmpty ? "database" : trimmed(self.database)
        return "PostgreSQL · \(trimmed(user))@\(host):\(port)/\(database)"
    }

    private var urlBar: some View {
        HStack(spacing: 8) {
            Image(systemName: "link")
                .foregroundStyle(Theme.inkSecondary)
            TextField("URL", text: $url, prompt: Text(fields.urlPrompt ?? ""))
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
            // Short, to fit beside Test: "Connected · 17.7" or "Opened · SQLite 3.50"; the full version on hover.
            Label(engine == .sqlite ? "Opened · \(version)"
                  // "SQL Server 2022 (16.0.4135.4)" → "2022"; "Azure SQL 12.0.2000.8" → "Azure SQL".
                  : engine == .mssql ? "Connected · \(version.hasPrefix("Azure") ? "Azure SQL" : version.split(separator: " ").dropFirst(2).first.map(String.init) ?? version)"
                  // "MySQL 8.4.13" → "8.4.13"; the server's name is in the header.
                  : engine == .mysql || engine == .libsql || engine == .redis || engine == .mongo || engine == .cassandra || engine == .oracle ? "Connected · \(version.split(separator: " ").last.map(String.init) ?? version)"
                  : "Connected · \(version.split(separator: " ").first.map(String.init) ?? version)",
                  systemImage: "checkmark.circle.fill")
                .help(engine == .postgres ? "PostgreSQL \(version)" : version)
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
            // The URL's scheme picks the engine; its defaults then give way to the URL's parts.
            if isNew && parsed.engine != engine { engine = parsed.engine }
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

    private func chooseDatabaseFile() {
        let panel = NSOpenPanel()
        panel.message = "Choose a SQLite database file"
        panel.canChooseDirectories = false
        if panel.runModal() == .OK, let url = panel.url {
            database = url.path
            if trimmed(name).isEmpty { name = url.deletingPathExtension().lastPathComponent }
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
        if engine == .firestore {
            return ConnectionSettings(host: trimmed(host), port: 0, database: trimmed(database), user: trimmed(user),
                                      password: password.isEmpty ? nil : password, tls: .verifyFull, ssh: nil, readOnly: readOnly,
                                      engine: .firestore)
        }
        if engine == .d1 {
            return ConnectionSettings(host: "", port: 0, database: trimmed(database), user: trimmed(user),
                                      password: password.isEmpty ? nil : password, tls: .verifyFull, ssh: nil, readOnly: readOnly,
                                      engine: .d1)
        }
        if engine == .libsql {
            return ConnectionSettings(host: "", port: 0, database: trimmed(database), user: "",
                                      password: password.isEmpty ? nil : password, tls: .disable, ssh: nil, readOnly: readOnly,
                                      engine: .libsql)
        }
        if engine == .sqlite {
            return ConnectionSettings(host: "", port: 0, database: (trimmed(database) as NSString).expandingTildeInPath, user: "",
                                      password: nil, tls: .disable, ssh: nil, readOnly: readOnly, engine: .sqlite)
        }
        guard let port = serverPort else { return nil }
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
            engine: engine
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
        if engine == .sqlite { return saveFile() }
        if engine == .libsql || engine == .d1 || engine == .firestore { return saveRemote() }
        guard let port = serverPort else { return }
        var connection = SavedConnection(
            name: trimmed(name), host: trimmed(host), port: port,
            database: trimmed(database), user: trimmed(user), tag: tag, tls: tls
        )
        connection.engine = engine
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

    /// A libSQL connection: the URL, and the token in the Keychain.
    private func saveRemote() {
        // D1 keeps its account ID as the user, Firestore its project and the
        // emulator as host; libSQL has neither.
        var connection = SavedConnection(name: trimmed(name), host: engine == .firestore ? trimmed(host) : "", port: 0, database: trimmed(database),
                                         user: engine == .libsql ? "" : trimmed(user), tag: tag, tls: engine == .libsql ? .disable : .verifyFull)
        connection.engine = engine
        connection.readOnly = readOnly
        connection.aiAccess = aiAccess
        if case .edit(let existing) = target { connection.id = existing.id }
        do {
            try Keychain.setSecret(password, .databasePassword, for: connection.id)
        } catch {
            saveError = error.localizedDescription
            return
        }
        if isNew { store.add(connection) } else { store.update(connection) }
        testTask?.cancel()
        onClose()
    }

    /// A SQLite connection: the file and the options; no passwords.
    private func saveFile() {
        let path = URL(fileURLWithPath: (trimmed(database) as NSString).expandingTildeInPath).standardizedFileURL.path
        var connection = SavedConnection(name: trimmed(name), host: "", port: 0, database: path, user: "",
                                         tag: tag, tls: .disable)
        connection.engine = .sqlite
        connection.readOnly = readOnly
        connection.aiAccess = aiAccess
        if case .edit(let existing) = target { connection.id = existing.id }
        if isNew { store.add(connection) } else { store.update(connection) }
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
        let close: () -> Void = { [weak panel] in panel?.close() }
        let host = NSHostingController(rootView: ConnectionForm(target: target, store: store, onClose: close))
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

/// What the connection form shows for one engine: its own labels, hints and
/// prompts only. One exhaustive switch, so a new engine can't fall back on
/// another engine's text.
private struct EngineFields {
    var databaseLabel = "Database"
    var databasePrompt = ""
    var databaseIsOptional = true
    var userPrompt = "none"
    var userIsOptional = true
    /// None: the engine has no connection URL, and the URL bar is hidden.
    var urlPrompt: String?
    var readOnlyHint = "Browse and query, never change data. The server refuses every write."
    /// AI access may cover every database on the server.
    var offersEveryDatabase = false
    /// Shown on the AI Access tab when a login reads more than its database.
    var loginWarning: (title: String, detail: String)?

    static func of(_ engine: Engine) -> EngineFields {
        switch engine {
        case .postgres:
            EngineFields(databasePrompt: "postgres", databaseIsOptional: false, userPrompt: "postgres", userIsOptional: false,
                         urlPrompt: "Paste a postgres:// URL to fill the form", offersEveryDatabase: true)
        case .mysql:
            EngineFields(databasePrompt: "optional: every database is shown", userPrompt: "root", userIsOptional: false,
                         urlPrompt: "Paste a mysql:// URL to fill the form",
                         loginWarning: ("AI tools can read every database this login can read",
                                        "MySQL has no way to keep a login to one database. To limit what AI tools see, connect with a MySQL user that has rights only to the databases you want to share."))
        case .mssql:
            // SQL Server has no read-only session to ask for.
            EngineFields(databasePrompt: "optional: the login's default database", userPrompt: "sa", userIsOptional: false,
                         urlPrompt: "Paste a connection string (Server=…) or sqlserver:// URL to fill the form",
                         readOnlyHint: "Browse and query, never change data. TablePort refuses writes and rolls back every query.",
                         offersEveryDatabase: true)
        case .redis:
            EngineFields(databasePrompt: "0", userPrompt: "default", urlPrompt: "Paste a redis:// URL to fill the form",
                         readOnlyHint: "Browse and read, never change data. TablePort runs only commands that read.")
        case .mongo:
            // MongoDB has no read-only session; TablePort checks each command.
            EngineFields(databasePrompt: "optional: every database is shown", urlPrompt: "Paste a mongodb:// URL to fill the form",
                         readOnlyHint: "Browse and query, never change data. TablePort runs only commands that read.",
                         offersEveryDatabase: true)
        case .oracle:
            // No URL format: a whole connect descriptor goes in Host instead.
            EngineFields(databaseLabel: "Service", databasePrompt: "FREEPDB1", databaseIsOptional: false, userPrompt: "system",
                         userIsOptional: false,
                         readOnlyHint: "Browse and query, never change data. Queries run in read-only transactions; TablePort refuses other statements.")
        case .cassandra:
            // CQL has no URL, no read-only session and no transactions.
            EngineFields(databaseLabel: "Keyspace", databasePrompt: "optional: every keyspace is shown",
                         readOnlyHint: "Browse and query, never change data. TablePort runs only statements that read.",
                         loginWarning: ("AI tools can read every keyspace this login can read",
                                        "A CQL query can name any keyspace. To limit what AI tools see, connect with a Cassandra role that has rights only to the keyspaces you want to share."))
        case .sqlite, .libsql, .d1, .firestore:
            // Their own tabs, without server fields or a URL bar.
            EngineFields(databaseIsOptional: false)
        }
    }
}

/// Oracle needs Oracle's own client library, which users install
/// themselves: TablePort loads it from the folder chosen here, once per
/// launch, and remembers the folder for later launches.
struct OracleClientCard: View {
    @AppStorage(OracleClient.directoryKey) private var directory = ""
    @State private var status: Result<String, Error>?

    var body: some View {
        FormCard("Oracle Instant Client") {
            FormRow("Folder") {
                HStack(spacing: 6) {
                    Text(directory.isEmpty ? "Not chosen" : (directory as NSString).abbreviatingWithTildeInPath)
                        .foregroundStyle(directory.isEmpty ? Theme.inkSecondary : Theme.ink)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    Button("Choose…", action: choose)
                }
            }
            VStack(alignment: .leading, spacing: 6) {
                switch status {
                case .success(let version):
                    Label("Instant Client \(version) is loaded.", systemImage: "checkmark.circle.fill")
                        .foregroundStyle(Theme.success)
                case .failure(let error):
                    Label(error.displayMessage, systemImage: "xmark.circle.fill")
                        .foregroundStyle(Theme.danger)
                        .fixedSize(horizontal: false, vertical: true)
                case nil:
                    Text("TablePort talks to Oracle through Oracle's Instant Client. Download the Basic or Basic Light package for macOS, unpack it, and choose its folder. TablePort remembers it for every Oracle connection.")
                        .foregroundStyle(Theme.inkSecondary)
                        .fixedSize(horizontal: false, vertical: true)
                    Link("Instant Client downloads on oracle.com", destination: URL(string: "https://www.oracle.com/database/technologies/instant-client/downloads.html")!)
                }
            }
            .font(Theme.meta)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
        }
        .onAppear { if !directory.isEmpty { status = OracleClient.load(directory) } }
    }

    private func choose() {
        let panel = NSOpenPanel()
        panel.message = "Choose the Instant Client folder (it holds libclntsh.dylib)"
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        if panel.runModal() == .OK, let url = panel.url {
            directory = url.path
            status = OracleClient.load(url.path)
        }
    }
}

/// Loads Instant Client for the core; loading again is harmless.
enum OracleClient {
    static let directoryKey = "oracleClientDirectory"

    @discardableResult
    static func load(_ directory: String) -> Result<String, Error> {
        Result { try loadOracleClient(directory: directory) }
    }

    /// At launch, the folder chosen before, so Oracle connections open at once.
    static func loadSaved() {
        guard let directory = UserDefaults.standard.string(forKey: directoryKey), !directory.isEmpty else { return }
        load(directory)
    }
}
