import AppKit

// Top-level code here is `@MainActor`-isolated under Swift 6; `delegate` has
// static storage, so it outlives `NSApplication`'s weak `delegate` ref.
// `tableport mcp`: an AI tool's MCP connection, not the app.
if CommandLine.arguments.dropFirst().first == "mcp" {
    exit(McpCommand.run())
}

let app = NSApplication.shared
// Before anything reads settings, connections or passwords.
CodenameMigration.run()
let delegate = AppDelegate()
app.delegate = delegate
app.run()
