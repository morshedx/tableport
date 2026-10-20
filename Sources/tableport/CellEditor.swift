import AppKit
import TablePortCore

/// The cell editor: a popover with a large text area for long text and JSON,
/// with Format JSON, Set NULL, Cancel and Save (⌘↩). Read-only cells open
/// it to view the whole value.
final class CellEditorController: NSViewController, NSPopoverDelegate {
    /// The popover's width, padding included.
    static let width: CGFloat = 552
    private static let padding: CGFloat = 16

    struct Content {
        let title: String
        /// nil while the whole value is still loading.
        var text: String?
        let isNull: Bool
        /// Why the cell can't be changed, or nil when it can.
        var readOnlyReason: String?
    }

    private var content: Content
    private let onSave: (CellDraft) -> Void
    private let textView = EscapeTextView()
    private let status = NSTextField(labelWithString: "")
    private let formatButton = NSButton()
    private let nullButton = NSButton()
    private let saveButton = NSButton()
    private var popover: NSPopover?

    init(content: Content, onSave: @escaping (CellDraft) -> Void) {
        self.content = content
        self.onSave = onSave
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) {
        nil
    }

    override func loadView() {
        let title = NSTextField(labelWithString: content.title)
        title.font = .systemFont(ofSize: 12, weight: .semibold)
        title.lineBreakMode = .byTruncatingMiddle

        textView.onEscape = { [weak self] in self?.cancelEditing() }
        textView.font = AppFont.mono(size: AppSettings.shared.editorFontSize)
        textView.isRichText = false
        textView.allowsUndo = true
        textView.isAutomaticQuoteSubstitutionEnabled = false
        textView.isAutomaticDashSubstitutionEnabled = false
        textView.isAutomaticTextReplacementEnabled = false
        textView.isAutomaticSpellingCorrectionEnabled = false
        textView.isContinuousSpellCheckingEnabled = false
        textView.textContainerInset = NSSize(width: 4, height: 6)
        textView.isVerticallyResizable = true
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true
        let scroll = NSScrollView()
        scroll.documentView = textView
        scroll.hasVerticalScroller = true
        scroll.borderType = .bezelBorder
        scroll.translatesAutoresizingMaskIntoConstraints = false

        status.font = .systemFont(ofSize: 11)
        status.textColor = .secondaryLabelColor
        status.lineBreakMode = .byTruncatingTail

        configure(formatButton, "Format JSON", #selector(formatJSON))
        configure(nullButton, "Set NULL", #selector(setNull))
        let cancel = NSButton()
        configure(cancel, "Cancel", #selector(cancelEditing))
        cancel.keyEquivalent = "\u{1b}"
        configure(saveButton, "Save", #selector(save))
        saveButton.keyEquivalent = "\r"
        saveButton.keyEquivalentModifierMask = [.command]
        saveButton.toolTip = "Save (⌘↩)"

        // Format and Set NULL on the left, Cancel and Save at the right edge.
        let buttons = NSStackView()
        for view in [formatButton, nullButton, status] { buttons.addView(view, in: .leading) }
        for view in [cancel, saveButton] { buttons.addView(view, in: .trailing) }
        buttons.setHuggingPriority(.defaultHigh, for: .vertical)
        status.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        let stack = NSStackView(views: [title, scroll, buttons])
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 8
        stack.translatesAutoresizingMaskIntoConstraints = false
        // Padding by constraints: the popover sizes its content from the
        // fitting size, which left the stack's own edge insets out.
        let container = NSView()
        container.addSubview(stack)
        let padding = Self.padding
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: container.leadingAnchor, constant: padding),
            stack.trailingAnchor.constraint(equalTo: container.trailingAnchor, constant: -padding),
            stack.topAnchor.constraint(equalTo: container.topAnchor, constant: 12),
            stack.bottomAnchor.constraint(equalTo: container.bottomAnchor, constant: -padding),
            scroll.widthAnchor.constraint(equalToConstant: Self.width - 2 * padding),
            scroll.heightAnchor.constraint(equalToConstant: 300),
            buttons.widthAnchor.constraint(equalTo: scroll.widthAnchor),
        ])
        view = container
        apply()
    }

    private func configure(_ button: NSButton, _ title: String, _ action: Selector) {
        button.title = title
        button.bezelStyle = .push
        button.controlSize = .small
        button.target = self
        button.action = action
    }

    /// Fills the text and turns controls on or off for the current content.
    private func apply() {
        let editable = content.readOnlyReason == nil && content.text != nil
        textView.string = content.text ?? ""
        textView.isEditable = editable
        textView.textColor = content.isNull && content.text?.isEmpty == true ? .tertiaryLabelColor : .labelColor
        nullButton.isHidden = !editable
        saveButton.isHidden = !editable
        formatButton.isEnabled = content.text != nil
        status.stringValue = content.text == nil ? "Loading the whole value…"
            : content.readOnlyReason ?? (content.isNull ? "NULL. Typing sets a value." : "")
    }

    /// The whole value arrived (it was shortened in the grid).
    func show(text: String?, readOnlyReason: String? = nil) {
        content.text = text ?? ""
        if let readOnlyReason { content.readOnlyReason = readOnlyReason }
        if isViewLoaded { apply() }
    }

    func show(relativeTo rect: NSRect, of view: NSView, preferredEdge: NSRectEdge = .maxY) {
        let popover = NSPopover()
        popover.contentViewController = self
        // Not transient: a click elsewhere must not throw away typing.
        popover.behavior = .applicationDefined
        popover.delegate = self
        self.popover = popover
        popover.show(relativeTo: rect, of: view, preferredEdge: preferredEdge)
        view.window?.makeFirstResponder(textView)
    }

    @objc private func formatJSON() {
        if let formatted = formatJson(text: textView.string) {
            // Through the text system, so ⌘Z undoes it.
            if textView.shouldChangeText(in: NSRange(location: 0, length: (textView.string as NSString).length), replacementString: formatted) {
                textView.string = formatted
                textView.didChangeText()
            }
            status.stringValue = ""
        } else {
            status.stringValue = "This is not valid JSON."
        }
    }

    @objc private func setNull() {
        onSave(.null)
        popover?.close()
    }

    @objc private func save() {
        // Save without a change is Cancel: the editor's whole value would
        // otherwise count as an edit of a value the grid holds shortened.
        if textView.string != content.text || content.isNull {
            onSave(.value(text: textView.string))
        }
        popover?.close()
    }

    @objc private func cancelEditing() {
        popover?.close()
    }
}

/// Esc closes the editor. NSTextView binds Esc to word completion
/// (`complete:`), so the Cancel button's Esc never fired.
private final class EscapeTextView: NSTextView {
    var onEscape: (() -> Void)?

    override func complete(_ sender: Any?) {
        onEscape?()
    }

    override func cancelOperation(_ sender: Any?) {
        onEscape?()
    }
}
