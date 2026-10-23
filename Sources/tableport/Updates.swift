import MacKitUpdates

/// Sparkle from MacKitUpdates. The free edition's feed is the appcast on the
/// newest GitHub release of morshedx/tableport (Info.plist's SUFeedURL),
/// shown inline on Settings › Updates.
extension Updater {
    static let shared = Updater(appName: AppInfo.name)
}

/// What changed in each version. Settings › Updates shows the running
/// version's. Add the next version's entry before releasing it.
enum ReleaseNotes {
    static let notes = VersionNotes([
        "0.1.0-beta.5": [
            "More databases: MySQL and MariaDB, SQLite files, libSQL and Turso, Redis, MongoDB, Apache Cassandra and Google Cloud Firestore.",
            "In preview: SQL Server and Azure SQL, Oracle Database (with Oracle Instant Client) and Cloudflare D1. They are not yet tested against live servers; please report problems.",
            "Redis: a key tree, viewers for every value type, and a console. MongoDB: collections as tables and mongo shell commands. Firestore: Firebase SDK queries in the editor.",
            "Each connection form shows only the options of its database.",
            "AI access: let AI tools such as Claude Code read the connections you choose, read-only, from Settings › AI access.",
            "Open Anything (⌘P), table actions in the sidebar, a row panel, table structure editing and New Table.",
            "TablePort reopens your last connections and tabs at launch.",
            "Settings › About › Licences lists the open-source libraries TablePort uses.",
        ],
        "0.1.0-beta.4": [
            "Autocomplete in the query editor: tables after FROM and JOIN, columns of the tables in the statement, alias. and CTE columns, keywords and functions. Letters in order match too: sl finds SELECT.",
            "Settings › Editor › Keyword case: suggested keywords in uppercase (default), lowercase or as typed.",
            "Quotes and brackets close themselves, with the cursor between them.",
            "Selected SQL keeps its syntax colours on a light grey selection, and lines are a little taller.",
            "A new query tab (⌘T) puts the cursor in the editor.",
        ],
        "0.1.0-beta.3": [
            "Numbers are right-aligned in the data grid and in query results, as in TablePlus, so their digits line up.",
        ],
        "0.1.0-beta.2": [
            "Settings › Updates has a second switch: Check for updates automatically, on by default.",
            "The Settings window can be resized, and it opens without a focus ring until you press Tab.",
            "Settings › About › Report a Problem opens the bug report form on GitHub.",
        ],
        "0.1.0-beta.1": [
            "First public beta. TablePort is free and open source (Apache-2.0).",
            "PostgreSQL connections with TLS, SSH tunnels, read-only mode and colour tags; passwords stay in the macOS Keychain.",
            "A fast data grid: sort, filter, edit cells, add and delete rows, review the SQL and commit it in one transaction.",
            "A query editor with Run Current, Run All, Beautify, line numbers, history and saved queries.",
            "Copy as CSV or SQL, and export tables to CSV, JSON or SQL.",
            "A cell editor for long text and JSON, and a banner with Reconnect when the connection drops.",
        ],
    ])
}
