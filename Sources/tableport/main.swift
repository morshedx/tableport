import AppKit

// Top-level code here is `@MainActor`-isolated under Swift 6; `delegate` has
// static storage, so it outlives `NSApplication`'s weak `delegate` ref.
let app = NSApplication.shared
// Before anything reads settings, connections or passwords.
CodenameMigration.run()
let delegate = AppDelegate()
app.delegate = delegate
app.run()
