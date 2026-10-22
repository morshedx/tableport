import AppKit
import os
import TablePortCore
import SwiftUI

/// The autocomplete catalog of one connection window. A reference, so the
/// editor reads the latest one at each keystroke without SwiftUI updates.
@MainActor
final class CompletionSource {
    static let logger = Logger(subsystem: Bundle.main.bundleIdentifier ?? "tableport", category: "Completion")
    var completer: Completer?
}

/// A plain NSTextView for SQL: JetBrains Mono, no smart quotes or spelling,
/// light keyword highlighting, and a red underline where the server found an
/// error.
struct SQLEditor: NSViewRepresentable {
    @Binding var text: String
    @Binding var selection: NSRange
    /// UTF-16 range to underline; nil clears it.
    let errorRange: NSRange?
    var completion: CompletionSource?

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSScrollView {
        // As NSTextView.scrollableTextView() builds it, with SQLTextView.
        let scroll = NSScrollView()
        scroll.hasVerticalScroller = true
        scroll.borderType = .noBorder
        let size = scroll.contentSize
        let textView = SQLTextView(frame: NSRect(origin: .zero, size: size))
        textView.minSize = NSSize(width: 0, height: size.height)
        textView.maxSize = NSSize(width: CGFloat.greatestFiniteMagnitude, height: CGFloat.greatestFiniteMagnitude)
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.autoresizingMask = [.width]
        textView.textContainer?.containerSize = NSSize(width: size.width, height: CGFloat.greatestFiniteMagnitude)
        textView.textContainer?.widthTracksTextView = true
        scroll.documentView = textView
        textView.delegate = context.coordinator
        textView.font = AppFont.mono(size: AppSettings.shared.editorFontSize)
        textView.isRichText = false
        textView.allowsUndo = true
        textView.isAutomaticQuoteSubstitutionEnabled = false
        textView.isAutomaticDashSubstitutionEnabled = false
        textView.isAutomaticTextReplacementEnabled = false
        textView.isAutomaticSpellingCorrectionEnabled = false
        textView.isContinuousSpellCheckingEnabled = false
        textView.isGrammarCheckingEnabled = false
        textView.textContainerInset = NSSize(width: 6, height: 8)
        // Only the background: the default selection also sets a text colour,
        // which hides the highlighting while text is selected.
        textView.selectedTextAttributes = [.backgroundColor: Coordinator.selectionColor]
        // New text gets the line height before the next highlight pass.
        textView.typingAttributes = Coordinator.baseAttributes()
        textView.string = text
        let ruler = LineNumberRuler(textView: textView)
        scroll.verticalRulerView = ruler
        scroll.hasVerticalRuler = true
        scroll.rulersVisible = true
        context.coordinator.textView = textView
        context.coordinator.highlight()
        context.coordinator.observeClosers(scroll: scroll)
        // A new or reopened query tab is for typing: put the cursor in it
        // once the editor is in its window, which happens after this returns.
        let focus = FocusOnJoin(target: textView)
        focus.frame = .zero
        scroll.addSubview(focus)
        return scroll
    }

    /// Switching tabs removes the editor without ending its editing, so its
    /// list must close here.
    static func dismantleNSView(_ scroll: NSScrollView, coordinator: Coordinator) {
        coordinator.closeList()
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        let coordinator = context.coordinator
        coordinator.parent = self
        guard let textView = coordinator.textView else { return }
        // Text set from outside, for example a query picked from history.
        if textView.string != text {
            textView.string = text
            coordinator.highlight()
            // Beautify sets a new selection along with the text.
            if NSMaxRange(selection) <= (text as NSString).length {
                textView.setSelectedRange(selection)
            }
            scroll.verticalRulerView?.needsDisplay = true
        }
        if coordinator.errorRange != errorRange {
            coordinator.errorRange = errorRange
            coordinator.highlight()
            if let errorRange { textView.scrollRangeToVisible(errorRange) }
        }
    }

    @MainActor
    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: SQLEditor
        weak var textView: NSTextView?
        var errorRange: NSRange?
        private var popup: CompletionPopup?
        /// Where the word being completed starts, and the cursor the list is for.
        private var replaceStart = 0
        private var popupCursor = 0
        /// The last edit typed a letter or a `.` (or deleted one while the list
        /// was open): the list opens or follows it.
        private var editSuggests = false
        /// The list stays open while deleting, even with no word left.
        private var keepOpen = false

        init(_ parent: SQLEditor) {
            self.parent = parent
        }

        func textDidChange(_ notification: Notification) {
            guard let textView else { return }
            parent.text = textView.string
            // Typing makes the old error position meaningless.
            errorRange = nil
            highlight()
            textView.enclosingScrollView?.verticalRulerView?.needsDisplay = true
            if editSuggests {
                editSuggests = false
                showCompletions(explicit: keepOpen)
                keepOpen = false
            } else {
                hideCompletions()
            }
        }

        func textView(_ textView: NSTextView, shouldChangeTextIn range: NSRange, replacementString: String?) -> Bool {
            if let typed = replacementString, typed.count == 1, typingPair(typed, in: textView, range: range) {
                return false
            }
            let typed = replacementString ?? ""
            if typed.count == 1, let character = typed.unicodeScalars.first {
                editSuggests = CharacterSet.alphanumerics.contains(character) || character == "_" || character == "."
            } else {
                editSuggests = typed.isEmpty && popup?.isVisible == true
                keepOpen = editSuggests
            }
            return true
        }

        func textViewDidChangeSelection(_ notification: Notification) {
            guard let textView else { return }
            parent.selection = textView.selectedRange()
            shadeCurrentStatement()
            // A click or an arrow key elsewhere closes the list. Checked after
            // the edit that moved the cursor has updated the list.
            DispatchQueue.main.async { [weak self] in
                guard let self, let popup = self.popup, popup.isVisible, let textView = self.textView else { return }
                let selection = textView.selectedRange()
                if selection.length > 0 || selection.location != self.popupCursor { self.hideCompletions() }
            }
        }

        func textView(_ textView: NSTextView, doCommandBy selector: Selector) -> Bool {
            if selector == #selector(NSResponder.deleteBackward(_:)), deleteEmptyPair(in: textView) {
                return true
            }
            if let popup, popup.isVisible {
                switch selector {
                case #selector(NSResponder.moveUp(_:)): popup.move(by: -1)
                case #selector(NSResponder.moveDown(_:)): popup.move(by: 1)
                case #selector(NSResponder.scrollPageUp(_:)), #selector(NSResponder.pageUp(_:)): popup.move(by: -CompletionPopup.visibleRows)
                case #selector(NSResponder.scrollPageDown(_:)), #selector(NSResponder.pageDown(_:)): popup.move(by: CompletionPopup.visibleRows)
                case #selector(NSResponder.insertNewline(_:)), #selector(NSResponder.insertTab(_:)): acceptCompletion()
                case #selector(NSResponder.cancelOperation(_:)), #selector(NSResponder.complete(_:)): hideCompletions()
                // Deleting edits the word: the edit refreshes the list.
                case #selector(NSResponder.deleteBackward(_:)), #selector(NSResponder.deleteForward(_:)),
                     #selector(NSResponder.deleteWordBackward(_:)):
                    return false
                default:
                    hideCompletions()
                    return false
                }
                return true
            }
            // Esc asks for the list, as in Xcode, wherever the cursor is.
            if selector == #selector(NSResponder.complete(_:)) || selector == #selector(NSResponder.cancelOperation(_:)) {
                showCompletions(explicit: true)
                return true
            }
            return false
        }

        func textDidEndEditing(_ notification: Notification) {
            hideCompletions()
        }

        /// Scrolling the SQL or leaving the window closes the list.
        func observeClosers(scroll: NSScrollView) {
            let center = NotificationCenter.default
            scroll.contentView.postsBoundsChangedNotifications = true
            center.addObserver(self, selector: #selector(closeList), name: NSView.boundsDidChangeNotification,
                               object: scroll.contentView)
            center.addObserver(self, selector: #selector(windowResigned(_:)), name: NSWindow.didResignKeyNotification,
                               object: nil)
        }

        @objc func closeList() {
            hideCompletions()
        }

        @objc private func windowResigned(_ notification: Notification) {
            if (notification.object as? NSWindow) == textView?.window { hideCompletions() }
        }

        // MARK: - Pairs

        private static let pairs: [String: String] = ["(": ")", "'": "'", "\"": "\""]
        private static let closers: Set<String> = [")", "'", "\""]

        /// Typing an opening quote or bracket adds its closer, with the cursor
        /// between; typing a closer that is already next steps over it; with
        /// a selection, the pair wraps it. True when this handled the typing.
        private func typingPair(_ typed: String, in textView: NSTextView, range: NSRange) -> Bool {
            // Only real typing: not undo, paste or the text set from outside.
            guard textView.selectedRange() == range, !textView.hasMarkedText() else { return false }
            let text = textView.string as NSString
            let next = range.location + range.length < text.length
                ? text.substring(with: NSRange(location: range.location + range.length, length: 1)) : nil
            let previous = range.location > 0 ? text.substring(with: NSRange(location: range.location - 1, length: 1)) : nil

            if range.length == 0, Self.closers.contains(typed), next == typed {
                textView.setSelectedRange(NSRange(location: range.location + 1, length: 0))
                return true
            }
            guard let closer = Self.pairs[typed] else { return false }
            if range.length > 0 {
                let selected = text.substring(with: range)
                textView.insertText(typed + selected + closer, replacementRange: range)
                textView.setSelectedRange(NSRange(location: range.location + 1, length: range.length))
                return true
            }
            // Next to a word, the character is most likely meant alone: the
            // `'` of it's, or a bracket typed before existing text.
            let isWordCharacter = { (character: String?) in
                character.map { $0.rangeOfCharacter(from: .alphanumerics) != nil || $0 == "_" } ?? false
            }
            if isWordCharacter(next) { return false }
            if typed != "(" && isWordCharacter(previous) { return false }
            textView.insertText(typed + closer, replacementRange: range)
            textView.setSelectedRange(NSRange(location: range.location + 1, length: 0))
            return true
        }

        /// Backspace between an empty pair, `'|'` or `(|)`, deletes both.
        private func deleteEmptyPair(in textView: NSTextView) -> Bool {
            let selection = textView.selectedRange()
            let text = textView.string as NSString
            guard selection.length == 0, selection.location > 0, selection.location < text.length else { return false }
            let pair = text.substring(with: NSRange(location: selection.location - 1, length: 2))
            guard Self.pairs.contains(where: { $0.key + $0.value == pair }) else { return false }
            textView.insertText("", replacementRange: NSRange(location: selection.location - 1, length: 2))
            return true
        }

        // MARK: - Autocomplete

        private func showCompletions(explicit: Bool) {
            guard let textView, let completer = parent.completion?.completer else { return hideCompletions() }
            let selection = textView.selectedRange()
            guard selection.length == 0 else { return hideCompletions() }
            let result = completer.complete(sql: textView.string, cursor: UInt32(clamping: selection.location), explicit: explicit,
                                            keywordCase: AppSettings.shared.keywordCase)
            guard !result.items.isEmpty else { return hideCompletions() }
            replaceStart = Int(result.replaceStart)
            popupCursor = selection.location
            let popup = self.popup ?? CompletionPopup { [weak self] in self?.acceptCompletion() }
            self.popup = popup
            // Screen coordinates of the word's start, on its line.
            let anchor = textView.firstRect(forCharacterRange: NSRange(location: replaceStart, length: 0), actualRange: nil)
            popup.show(result.items, at: anchor, in: textView.window)
        }

        private func hideCompletions() {
            popup?.hide()
        }

        private func acceptCompletion() {
            guard let textView, let item = popup?.selectedItem else { return hideCompletions() }
            let cursor = textView.selectedRange().location
            hideCompletions()
            let text = textView.string as NSString
            guard replaceStart <= cursor, cursor <= text.length else { return }
            // A keyword is followed by more SQL: add the space, unless one is
            // already there.
            var insert = item.insertText
            let nextIsSpace = cursor < text.length
                && CharacterSet.whitespacesAndNewlines.contains(UnicodeScalar(text.character(at: cursor)) ?? " ")
            if item.kind == .keyword && !nextIsSpace { insert += " " }
            // Through the text view, so it is one undo step like typing.
            textView.insertText(insert, replacementRange: NSRange(location: replaceStart, length: cursor - replaceStart))
        }

        /// Shades the statement that Run Current would run, as TablePlus
        /// does. Temporary attributes, so undo and the text stay untouched.
        func shadeCurrentStatement() {
            guard let textView, let layoutManager = textView.layoutManager else { return }
            let length = (textView.string as NSString).length
            layoutManager.removeTemporaryAttribute(.backgroundColor, forCharacterRange: NSRange(location: 0, length: length))
            let selection = textView.selectedRange()
            guard selection.length == 0,
                  let range = statementRange(sql: textView.string, cursor: UInt32(clamping: selection.location)),
                  Int(range.end) <= length else { return }
            layoutManager.addTemporaryAttribute(
                .backgroundColor, value: Self.statementShade,
                forCharacterRange: NSRange(location: Int(range.start), length: Int(range.end - range.start)))
        }

        /// A light grey selection, as TablePlus has, so the syntax colours
        /// stay readable on it.
        static let selectionColor = NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
                ? NSColor.white.withAlphaComponent(0.25)
                : NSColor.black.withAlphaComponent(0.2)
        }

        private static let statementShade = NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
                ? NSColor.white.withAlphaComponent(0.06)
                : NSColor.black.withAlphaComponent(0.05)
        }

        /// The font, and lines taller than the text with the text centred,
        /// as in TablePlus: the statement shade and the selection then fill
        /// the whole line instead of a strip behind the letters.
        static func baseAttributes() -> [NSAttributedString.Key: Any] {
            let font = AppFont.mono(size: AppSettings.shared.editorFontSize)
            let textHeight = ceil(font.ascender - font.descender + font.leading)
            let lineHeight = ceil(textHeight * 1.45)
            let paragraph = NSMutableParagraphStyle()
            paragraph.minimumLineHeight = lineHeight
            paragraph.maximumLineHeight = lineHeight
            return [
                .font: font,
                .foregroundColor: NSColor.labelColor,
                .paragraphStyle: paragraph,
                // The extra height goes above the text; this centres it.
                .baselineOffset: (lineHeight - textHeight) / 2,
            ]
        }

        func highlight() {
            guard let storage = textView?.textStorage else { return }
            let text = storage.string as NSString
            let all = NSRange(location: 0, length: text.length)
            storage.beginEditing()
            storage.setAttributes(Self.baseAttributes(), range: all)
            for (pattern, color) in Self.rules {
                pattern.enumerateMatches(in: storage.string, range: all) { match, _, _ in
                    if let range = match?.range { storage.addAttribute(.foregroundColor, value: color, range: range) }
                }
            }
            if let errorRange, NSMaxRange(errorRange) <= text.length {
                storage.addAttributes([
                    .underlineStyle: NSUnderlineStyle.thick.rawValue,
                    .underlineColor: NSColor.systemRed,
                ], range: errorRange)
            }
            storage.endEditing()
            shadeCurrentStatement()
        }

        /// Later rules win, so strings and comments come last and override
        /// keywords inside them.
        private static let rules: [(NSRegularExpression, NSColor)] = {
            let keywords = [
                "select", "from", "where", "and", "or", "not", "in", "is", "null", "as", "on", "join", "left", "right",
                "inner", "outer", "full", "cross", "group", "by", "order", "having", "limit", "offset", "insert", "into",
                "values", "update", "set", "delete", "returning", "create", "table", "index", "view", "drop", "alter",
                "add", "column", "primary", "key", "foreign", "references", "unique", "default", "with", "union", "all",
                "distinct", "case", "when", "then", "else", "end", "begin", "commit", "rollback", "asc", "desc", "like",
                "ilike", "between", "exists", "true", "false", "explain", "analyze", "truncate", "cascade", "grant",
            ]
            func rule(_ pattern: String, _ color: NSColor, _ options: NSRegularExpression.Options = []) -> (NSRegularExpression, NSColor)? {
                (try? NSRegularExpression(pattern: pattern, options: options)).map { ($0, color) }
            }
            return [
                rule("\\b(\(keywords.joined(separator: "|")))\\b", .systemBlue, .caseInsensitive),
                rule("\\b\\d+(\\.\\d+)?\\b", .systemOrange),
                rule("'(?:[^']|'')*'", .systemGreen),
                rule("--[^\\n]*", .secondaryLabelColor),
                rule("/\\*[\\s\\S]*?\\*/", .secondaryLabelColor),
            ].compactMap { $0 }
        }()
    }
}

/// The autocomplete list under the cursor, after Xcode's: a letter badge
/// for the kind, the name, and the schema or table in grey at the right. A
/// child window of the editor's window that never takes the focus, so
/// typing goes on.
@MainActor
final class CompletionPopup: NSObject, NSTableViewDataSource, NSTableViewDelegate {
    static let visibleRows = 10
    private static let rowHeight: CGFloat = 24
    private static let padding: CGFloat = 5
    /// Room around the list for its shadow, inside the window.
    private static let shadowMargin: CGFloat = 14

    private let panel: NSPanel
    private let background = NSView()
    private let table = ClickThroughTable()
    private var items: [CompletionItem] = []
    private let onAccept: () -> Void

    init(onAccept: @escaping () -> Void) {
        self.onAccept = onAccept
        panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 300, height: 100),
                        styleMask: [.borderless, .nonactivatingPanel], backing: .buffered, defer: true)
        super.init()
        panel.isOpaque = false
        panel.backgroundColor = .clear
        // The window shadow gives a borderless panel a dark rim; a layer
        // shadow is softer.
        panel.hasShadow = false
        // The editor closes the list when its window stops being key.
        panel.hidesOnDeactivate = false

        let column = NSTableColumn(identifier: NSUserInterfaceItemIdentifier("item"))
        column.resizingMask = .autoresizingMask
        table.addTableColumn(column)
        table.headerView = nil
        table.style = .plain
        table.rowHeight = Self.rowHeight
        table.intercellSpacing = .zero
        table.backgroundColor = .clear
        table.gridStyleMask = []
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.action = #selector(clicked)

        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.scrollerStyle = .overlay
        scroll.automaticallyAdjustsContentInsets = false
        scroll.contentInsets = NSEdgeInsets(top: Self.padding, left: 0, bottom: Self.padding, right: 0)

        // The editor's own background, so the list reads as part of it.
        background.wantsLayer = true
        background.layer?.cornerRadius = 8
        background.layer?.masksToBounds = true
        background.layer?.borderWidth = 1
        scroll.frame = background.bounds
        scroll.autoresizingMask = [.width, .height]
        background.addSubview(scroll)

        // The shadow sits on a holder: a layer that clips its corners can't
        // cast one.
        // Sized like the panel first: the list's frame is inset from it.
        let holder = NSView(frame: NSRect(origin: .zero, size: panel.frame.size))
        holder.wantsLayer = true
        holder.layer?.shadowOpacity = 0.12
        holder.layer?.shadowRadius = 10
        holder.layer?.shadowOffset = CGSize(width: 0, height: -3)
        let margin = Self.shadowMargin
        background.frame = holder.bounds.insetBy(dx: margin, dy: margin)
        background.autoresizingMask = [.width, .height]
        holder.addSubview(background)
        panel.contentView = holder
        updateColors()
    }

    /// Layer colours don't follow the appearance by themselves.
    private func updateColors() {
        guard let layer = background.layer else { return }
        let dark = background.effectiveAppearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
        background.effectiveAppearance.performAsCurrentDrawingAppearance {
            layer.backgroundColor = NSColor.textBackgroundColor.cgColor
        }
        // A light grey hairline, not the darker system separator.
        layer.borderColor = (dark ? NSColor(white: 1, alpha: 0.12) : NSColor(white: 0.86, alpha: 1)).cgColor
        panel.contentView?.layer?.shadowColor = NSColor.black.cgColor
    }

    var isVisible: Bool { panel.isVisible }

    var selectedItem: CompletionItem? {
        items.indices.contains(table.selectedRow) ? items[table.selectedRow] : nil
    }

    /// `anchor`: the screen rect of the word's start, one line high.
    func show(_ items: [CompletionItem], at anchor: NSRect, in window: NSWindow?) {
        self.items = items
        table.reloadData()
        table.selectRowIndexes(IndexSet(integer: 0), byExtendingSelection: false)
        table.scrollRowToVisible(0)
        if let window { panel.appearance = window.effectiveAppearance }
        updateColors()

        let height = CGFloat(min(items.count, Self.visibleRows)) * Self.rowHeight + Self.padding * 2
        // The names line up with the typed word: the badge sits to its left.
        var frame = NSRect(x: anchor.minX - CompletionCell.textInset, y: anchor.minY - height - 4,
                           width: Self.width(for: items), height: height)
        if let screen = window?.screen ?? NSScreen.main {
            let area = screen.visibleFrame
            // No room below the line: above it.
            if frame.minY < area.minY { frame.origin.y = anchor.maxY + 4 }
            frame.origin.x = min(max(frame.minX, area.minX + 4), area.maxX - frame.width - 4)
        }
        panel.setFrame(frame.insetBy(dx: -Self.shadowMargin, dy: -Self.shadowMargin), display: true)
        if panel.parent == nil, let window { window.addChildWindow(panel, ordered: .above) }
        panel.orderFront(nil)
    }

    /// Wide enough for the longest name and its detail, within limits.
    private static func width(for items: [CompletionItem]) -> CGFloat {
        let nameFont = CompletionCell.nameFont
        let detailFont = CompletionCell.detailFont
        let widest = items.prefix(200).map { item in
            (item.label as NSString).size(withAttributes: [.font: nameFont]).width
                + (item.detail as NSString).size(withAttributes: [.font: detailFont]).width
        }.max() ?? 0
        return min(max(widest + CompletionCell.textInset + 40, 240), 520)
    }

    func hide() {
        guard panel.isVisible else { return }
        panel.parent?.removeChildWindow(panel)
        panel.orderOut(nil)
    }

    func move(by delta: Int) {
        guard !items.isEmpty else { return }
        let row = min(max(table.selectedRow + delta, 0), items.count - 1)
        table.selectRowIndexes(IndexSet(integer: row), byExtendingSelection: false)
        table.scrollRowToVisible(row)
    }

    @objc private func clicked() {
        guard table.clickedRow >= 0 else { return }
        table.selectRowIndexes(IndexSet(integer: table.clickedRow), byExtendingSelection: false)
        onAccept()
    }

    func numberOfRows(in tableView: NSTableView) -> Int { items.count }

    func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
        CompletionRowView()
    }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let cell = tableView.makeView(withIdentifier: CompletionCell.id, owner: nil) as? CompletionCell ?? CompletionCell()
        if items.indices.contains(row) { cell.show(items[row]) }
        return cell
    }
}

/// Clicks reach the list although its window is never the key window.
private final class ClickThroughTable: NSTableView {
    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }
}

/// The selected row: a rounded accent fill inside the list's margin, as a
/// menu shows it, although the list's window is never key.
private final class CompletionRowView: NSTableRowView {
    override var isEmphasized: Bool {
        get { true }
        set {}
    }

    override func drawSelection(in dirtyRect: NSRect) {
        NSColor.controlAccentColor.setFill()
        NSBezierPath(roundedRect: bounds.insetBy(dx: 5, dy: 1), xRadius: 5, yRadius: 5).fill()
    }
}

private final class CompletionCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("completion")
    static let nameFont = AppFont.mono(size: 12)
    static let detailFont = NSFont.systemFont(ofSize: 11)
    /// From the cell's left edge to the name: the margin and the badge.
    static let textInset: CGFloat = 36

    private let badge = NSImageView()
    private let name = NSTextField(labelWithString: "")
    private let detail = NSTextField(labelWithString: "")

    init() {
        super.init(frame: .zero)
        identifier = Self.id
        name.font = Self.nameFont
        name.lineBreakMode = .byTruncatingTail
        detail.font = Self.detailFont
        detail.lineBreakMode = .byTruncatingHead
        detail.alignment = .right
        textField = name
        for view in [badge, name, detail] {
            view.translatesAutoresizingMaskIntoConstraints = false
            addSubview(view)
        }
        name.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        detail.setContentHuggingPriority(.required, for: .horizontal)
        NSLayoutConstraint.activate([
            badge.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 12),
            badge.centerYAnchor.constraint(equalTo: centerYAnchor),
            badge.widthAnchor.constraint(equalToConstant: 16),
            badge.heightAnchor.constraint(equalToConstant: 16),
            name.leadingAnchor.constraint(equalTo: leadingAnchor, constant: Self.textInset),
            name.centerYAnchor.constraint(equalTo: centerYAnchor),
            detail.leadingAnchor.constraint(greaterThanOrEqualTo: name.trailingAnchor, constant: 16),
            detail.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -14),
            detail.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
        backgroundStyle = .normal
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not used")
    }

    override var backgroundStyle: NSView.BackgroundStyle {
        didSet {
            let selected = backgroundStyle == .emphasized
            name.textColor = selected ? .white : .labelColor
            detail.textColor = selected ? NSColor.white.withAlphaComponent(0.75) : .tertiaryLabelColor
        }
    }

    func show(_ item: CompletionItem) {
        name.stringValue = item.label
        detail.stringValue = item.detail
        badge.image = Self.badge(for: item.kind)
    }

    private static var badges: [String: NSImage] = [:]

    /// A rounded square in the kind's colour with its letter, as Xcode marks
    /// its suggestions.
    static func badge(for kind: CompletionKind) -> NSImage {
        let (letter, color): (String, NSColor) = switch kind {
        case .keyword: ("K", .systemGray)
        case .table: ("T", .systemBlue)
        case .view: ("V", .systemTeal)
        case .column: ("C", .systemOrange)
        case .function: ("F", .systemPurple)
        case .schema: ("S", .systemGreen)
        }
        if let cached = badges[letter] { return cached }
        let image = NSImage(size: NSSize(width: 16, height: 16), flipped: false) { rect in
            color.setFill()
            NSBezierPath(roundedRect: rect, xRadius: 4, yRadius: 4).fill()
            let attributes: [NSAttributedString.Key: Any] = [
                .font: NSFont.systemFont(ofSize: 10, weight: .bold),
                .foregroundColor: NSColor.white,
            ]
            let size = (letter as NSString).size(withAttributes: attributes)
            (letter as NSString).draw(at: NSPoint(x: rect.midX - size.width / 2, y: rect.midY - size.height / 2),
                                      withAttributes: attributes)
            return true
        }
        badges[letter] = image
        return image
    }
}

/// The SQL text view. Lines are taller than the text (see
/// `baseAttributes`); the cursor stays the height of the text, centred.
final class SQLTextView: NSTextView {
    override func drawInsertionPoint(in rect: NSRect, color: NSColor, turnedOn flag: Bool) {
        super.drawInsertionPoint(in: Self.textHeight(rect), color: color, turnedOn: flag)
    }

    static func textHeight(_ rect: NSRect) -> NSRect {
        let font = AppFont.mono(size: AppSettings.shared.editorFontSize)
        let height = min(rect.height, ceil(font.ascender - font.descender + 2))
        return NSRect(x: rect.minX, y: rect.midY - height / 2, width: rect.width, height: height)
    }
}

/// Makes `target` the first responder when this view joins a window.
private final class FocusOnJoin: NSView {
    private weak var target: NSView?

    init(target: NSView) {
        self.target = target
        super.init(frame: .zero)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not used")
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard let window else { return }
        // After SwiftUI has finished placing the views, or it takes the focus back.
        DispatchQueue.main.async { [weak self] in
            guard let target = self?.target, target.window === window else { return }
            window.makeFirstResponder(target)
        }
    }
}

/// Line numbers to the left of the SQL, as in TablePlus. Numbers count
/// lines of text, not wrapped visual lines.
final class LineNumberRuler: NSRulerView {
    private weak var textView: NSTextView?

    init(textView: NSTextView) {
        self.textView = textView
        super.init(scrollView: textView.enclosingScrollView, orientation: .verticalRuler)
        clientView = textView
        ruleThickness = 40
        NotificationCenter.default.addObserver(
            self, selector: #selector(redraw), name: NSView.boundsDidChangeNotification,
            object: textView.enclosingScrollView?.contentView)
    }

    required init(coder: NSCoder) {
        fatalError("init(coder:) is not used")
    }

    @objc private func redraw() {
        needsDisplay = true
    }

    override func drawHashMarksAndLabels(in rect: NSRect) {
        guard let textView, let layoutManager = textView.layoutManager,
              let container = textView.textContainer else { return }
        NSColor.textBackgroundColor.setFill()
        bounds.fill()

        let text = textView.string as NSString
        let font = AppFont.mono(size: max(AppSettings.shared.editorFontSize - 2, 9))
        let attributes: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: NSColor.tertiaryLabelColor]
        let inset = textView.textContainerInset.height
        let originY = convert(NSPoint.zero, from: textView).y

        func draw(_ number: Int, lineRect: NSRect) {
            let label = "\(number)" as NSString
            let size = label.size(withAttributes: attributes)
            let y = originY + inset + lineRect.minY + (lineRect.height - size.height) / 2
            label.draw(at: NSPoint(x: ruleThickness - size.width - 8, y: y), withAttributes: attributes)
        }

        let visible = textView.visibleRect
        let glyphs = layoutManager.glyphRange(forBoundingRect: visible, in: container)
        let characters = layoutManager.characterRange(forGlyphRange: glyphs, actualGlyphRange: nil)
        var number = 1
        if characters.location > 0 {
            let before = text.substring(to: characters.location) as NSString
            number += before.components(separatedBy: "\n").count - 1
        }
        var index = characters.location
        while index < NSMaxRange(characters) {
            let line = text.lineRange(for: NSRange(location: index, length: 0))
            let glyph = layoutManager.glyphIndexForCharacter(at: line.location)
            draw(number, lineRect: layoutManager.lineFragmentRect(forGlyphAt: glyph, effectiveRange: nil))
            number += 1
            index = NSMaxRange(line)
        }
        // The empty line after a final newline has no glyphs of its own.
        if NSMaxRange(characters) == text.length,
           text.length == 0 || text.character(at: text.length - 1) == 10 {
            draw(number, lineRect: layoutManager.extraLineFragmentRect)
        }
    }
}
