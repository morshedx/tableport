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
