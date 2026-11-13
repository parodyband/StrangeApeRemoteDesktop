import UIKit

/// evdev key codes (what the protocol sends; the host maps them).
enum KeyCode {
    static let esc: UInt16 = 1
    static let backspace: UInt16 = 14
    static let tab: UInt16 = 15
    static let enter: UInt16 = 28
    static let leftCtrl: UInt16 = 29
    static let leftShift: UInt16 = 42
    static let leftAlt: UInt16 = 56
    static let leftMeta: UInt16 = 125
    static let v: UInt16 = 47
    static let f1: UInt16 = 59
    static let home: UInt16 = 102
    static let up: UInt16 = 103
    static let pageUp: UInt16 = 104
    static let left: UInt16 = 105
    static let right: UInt16 = 106
    static let end: UInt16 = 107
    static let down: UInt16 = 108
    static let pageDown: UInt16 = 109
    static let delete: UInt16 = 111

    static func function(_ n: Int) -> UInt16 {
        n <= 10 ? f1 + UInt16(n - 1) : (n == 11 ? 87 : 88)
    }
}

/// Invisible first responder that brings up the iPhone keyboard and turns
/// typing into key presses on the host. Its input accessory is a bar of keys
/// the keyboard lacks (Esc, Tab, modifiers, arrows, F-keys). Modifiers are
/// sticky: tap Ctrl, then a letter.
final class KeyInputView: UIView, UIKeyInput {
    private weak var controller: ScreenViewController?
    private var modifiers: Set<UInt16> = []
    private var modifierButtons: [UInt16: UIButton] = [:]
    /// Keys a hardware keyboard holds down right now (for clean release).
    private var held: Set<UInt16> = []
    var hostPlatform = "" { didSet { relabel() } }

    init(controller: ScreenViewController) {
        self.controller = controller
        super.init(frame: CGRect(x: 0, y: 0, width: 1, height: 1))
        alpha = 0.01
    }

    required init?(coder: NSCoder) { fatalError() }

    override var canBecomeFirstResponder: Bool { true }

    // Plain keys: no autocorrect, no smart punctuation, no capitalization.
    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardType: UIKeyboardType = .asciiCapable
    var keyboardAppearance: UIKeyboardAppearance = .dark
    var returnKeyType: UIReturnKeyType = .default
    var hasText: Bool { true } // keeps Backspace enabled

    private var session: SardConnection? { controller?.session }

    func insertText(_ text: String) {
        session?.type(text, modifiers: Array(modifiers))
        clearModifiers()
    }

    func deleteBackward() {
        press(KeyCode.backspace)
    }

    /// Press and release `code` with the sticky modifiers held.
    func press(_ code: UInt16) {
        guard let s = session else { return }
        let mods = Array(modifiers)
        for m in mods { s.key(m, down: true) }
        s.key(code, down: true)
        s.key(code, down: false)
        for m in mods.reversed() { s.key(m, down: false) }
        clearModifiers()
    }

    private func clearModifiers() {
        guard !modifiers.isEmpty else { return }
        modifiers.removeAll()
        for (_, b) in modifierButtons { style(b, on: false) }
    }

    /// Hardware keyboard: real key downs and ups. Returns true if handled.
    func handlePresses(_ presses: Set<UIPress>, down: Bool) -> Bool {
        var handled = false
        for p in presses {
            guard let key = p.key else { continue }
            let code = sard_hid_to_evdev(UInt32(key.keyCode.rawValue))
            guard code != 0 else { continue }
            if down { held.insert(code) } else { held.remove(code) }
            session?.key(code, down: down)
            handled = true
        }
        return handled
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !handlePresses(presses, down: true) { super.pressesBegan(presses, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !handlePresses(presses, down: false) { super.pressesEnded(presses, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !handlePresses(presses, down: false) { super.pressesCancelled(presses, with: event) }
    }

    // MARK: accessory bar

    private lazy var bar: UIView = makeBar()
    override var inputAccessoryView: UIView? { bar }

    private func style(_ b: UIButton, on: Bool) {
        var c = b.configuration ?? .gray()
        c.baseBackgroundColor = on ? .systemBlue : UIColor(white: 0.32, alpha: 1)
        c.baseForegroundColor = .white
        b.configuration = c
    }

    private func makeKey(_ title: String, symbol: String? = nil, action: @escaping () -> Void) -> UIButton {
        var c = UIButton.Configuration.gray()
        c.cornerStyle = .medium
        c.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 10, bottom: 6, trailing: 10)
        if let symbol {
            c.image = UIImage(systemName: symbol, withConfiguration: UIImage.SymbolConfiguration(pointSize: 14, weight: .semibold))
        } else {
            c.attributedTitle = AttributedString(title, attributes: AttributeContainer([.font: UIFont.systemFont(ofSize: 15, weight: .medium)]))
        }
        let b = UIButton(configuration: c, primaryAction: UIAction { _ in action() })
        b.accessibilityLabel = title
        style(b, on: false)
        return b
    }

    private func modifierLabel(_ code: UInt16) -> String {
        let mac = hostPlatform == "macos"
        switch code {
        case KeyCode.leftCtrl: return mac ? "⌃ control" : "Ctrl"
        case KeyCode.leftAlt: return mac ? "⌥ option" : "Alt"
        case KeyCode.leftMeta: return mac ? "⌘ cmd" : (hostPlatform == "windows" ? "⊞ Win" : "Super")
        default: return "⇧ Shift"
        }
    }

    private func relabel() {
        for (code, b) in modifierButtons {
            var c = b.configuration
            c?.attributedTitle = AttributedString(modifierLabel(code), attributes: AttributeContainer([.font: UIFont.systemFont(ofSize: 15, weight: .medium)]))
            b.configuration = c
        }
    }

    private func makeBar() -> UIView {
        let bar = UIInputView(frame: CGRect(x: 0, y: 0, width: 320, height: 48), inputViewStyle: .keyboard)
        let scroll = UIScrollView()
        scroll.showsHorizontalScrollIndicator = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        bar.addSubview(scroll)
        let row = UIStackView()
        row.axis = .horizontal
        row.spacing = 6
        row.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(row)

        row.addArrangedSubview(makeKey("Hide Keyboard", symbol: "keyboard.chevron.compact.down") { [weak self] in
            _ = self?.resignFirstResponder()
            _ = self?.controller?.becomeFirstResponder()
        })
        row.addArrangedSubview(makeKey("esc") { [weak self] in self?.press(KeyCode.esc) })
        row.addArrangedSubview(makeKey("tab") { [weak self] in self?.press(KeyCode.tab) })
        for code in [KeyCode.leftCtrl, KeyCode.leftAlt, KeyCode.leftMeta, KeyCode.leftShift] {
            let b = makeKey(modifierLabel(code)) { [weak self] in self?.toggle(code) }
            modifierButtons[code] = b
            row.addArrangedSubview(b)
        }
        let keys: [(String, String?, UInt16)] = [
            ("Left", "arrow.left", KeyCode.left), ("Up", "arrow.up", KeyCode.up),
            ("Down", "arrow.down", KeyCode.down), ("Right", "arrow.right", KeyCode.right),
            ("del", "delete.right", KeyCode.delete), ("home", nil, KeyCode.home), ("end", nil, KeyCode.end),
            ("pg up", nil, KeyCode.pageUp), ("pg dn", nil, KeyCode.pageDown),
        ]
        for (title, symbol, code) in keys {
            row.addArrangedSubview(makeKey(title, symbol: symbol) { [weak self] in self?.press(code) })
        }
        for n in 1...12 {
            row.addArrangedSubview(makeKey("F\(n)") { [weak self] in self?.press(KeyCode.function(n)) })
        }

        NSLayoutConstraint.activate([
            scroll.leadingAnchor.constraint(equalTo: bar.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: bar.trailingAnchor),
            scroll.topAnchor.constraint(equalTo: bar.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: bar.bottomAnchor),
            row.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor, constant: 8),
            row.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor, constant: -8),
            row.centerYAnchor.constraint(equalTo: scroll.frameLayoutGuide.centerYAnchor),
            row.heightAnchor.constraint(equalToConstant: 36),
        ])
        return bar
    }

    private func toggle(_ code: UInt16) {
        if modifiers.contains(code) { modifiers.remove(code) } else { modifiers.insert(code) }
        if let b = modifierButtons[code] { style(b, on: modifiers.contains(code)) }
    }

    override func resignFirstResponder() -> Bool {
        // Don't leave anything pressed on the host.
        for code in held { session?.key(code, down: false) }
        held.removeAll()
        return super.resignFirstResponder()
    }
}
