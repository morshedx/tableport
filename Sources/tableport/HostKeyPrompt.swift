import AppKit

/// Asks whether to trust an SSH server's key that ~/.ssh/known_hosts doesn't
/// have. Trusting stores the fingerprint with the connection only.
@MainActor
enum HostKeyPrompt {
    static func ask(host: String, fingerprint: String) -> Bool {
        let alert = NSAlert()
        alert.messageText = "Trust the SSH server \(host)?"
        alert.informativeText = """
            This is the first connection to this server. Its key fingerprint is:

            \(fingerprint)

            Compare it with the server's real fingerprint (for example \
            ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub on the server). \
            If they differ, someone may be intercepting the connection.
            """
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Cancel")
        alert.addButton(withTitle: "Trust and Connect")
        return alert.runModal() == .alertSecondButtonReturn
    }
}
