import AppKit
import TablePortCore
import SwiftUI

/// A plain NSTextView for SQL: JetBrains Mono, no smart quotes or spelling,
/// light keyword highlighting, and a red underline where the server found an
/// error.
struct SQLEditor: NSViewRepresentable {
    @Binding var text: String
    @Binding var selection: NSRange
    /// UTF-16 range to underline; nil clears it.
    let errorRange: NSRange?

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSScrollView {
        let scroll = NSTextView.scrollableTextView()
        guard let textView = scroll.documentView as? NSTextView else { return scroll }
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
        textView.string = text
        let ruler = LineNumberRuler(textView: textView)
        scroll.verticalRulerView = ruler
        scroll.hasVerticalRuler = true
        scroll.rulersVisible = true
        context.coordinator.textView = textView
        context.coordinator.highlight()
        return scroll
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
        }

        func textViewDidChangeSelection(_ notification: Notification) {
            guard let textView else { return }
            parent.selection = textView.selectedRange()
            shadeCurrentStatement()
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

        private static let statementShade = NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
                ? NSColor.white.withAlphaComponent(0.06)
                : NSColor.black.withAlphaComponent(0.04)
        }

        func highlight() {
            guard let storage = textView?.textStorage else { return }
            let text = storage.string as NSString
            let all = NSRange(location: 0, length: text.length)
            storage.beginEditing()
            storage.setAttributes([.font: AppFont.mono(size: AppSettings.shared.editorFontSize), .foregroundColor: NSColor.labelColor], range: all)
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
