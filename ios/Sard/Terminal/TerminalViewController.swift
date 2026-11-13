import SwiftTerm
import SwiftUI
import UIKit

struct TerminalSessionView: UIViewControllerRepresentable {
    let host: Host
    let title: String
    let onClose: () -> Void

    func makeUIViewController(context: Context) -> TerminalViewController {
        TerminalViewController(host: host, title: title, onClose: onClose)
    }
    func updateUIViewController(_ vc: TerminalViewController, context: Context) {}
}

/// Terminals on a paired computer, one tab per shell. Shells keep running
/// on the computer when the phone disconnects (backgrounded app, lost
/// network); on reconnect each tab reattaches and catches up.
final class TerminalViewController: UIViewController, TerminalViewDelegate {
    private final class Tab {
        var id: UInt32 = 0 // host shell id; 0 until opened
        var name = "shell"
        var title: String?
        let view: TerminalView
        var ended = false
        init(view: TerminalView) { self.view = view }
    }

    private let host: Host
    private let hostTitle: String
    private let onClose: () -> Void
    private var connection: SardConnection?
    private var tabs: [Tab] = []
    private var current: Tab?
    /// Open requests in flight: token → tab.
    private var pending: [UInt32: Tab] = [:]
    private var nextToken: UInt32 = 1
    private var connected = false

    private let container = UIView()
    private let tabBar = UIStackView()
    private let tabScroll = UIScrollView()
    private let overlay = UIStackView()
    private let overlayLabel = UILabel()
    private let overlaySpinner = UIActivityIndicatorView(style: .large)
    private let retryButton = UIButton(type: .system)

    private var fontSize: CGFloat {
        CGFloat(UserDefaults.standard.double(forKey: Pref.terminalFontSize).clamped(8, 24))
    }

    init(host: Host, title: String, onClose: @escaping () -> Void) {
        self.host = host
        self.hostTitle = title
        self.onClose = onClose
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { fatalError() }

    override var preferredStatusBarStyle: UIStatusBarStyle { .lightContent }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        setUpChrome()
        let nc = NotificationCenter.default
        nc.addObserver(self, selector: #selector(didEnterBackground), name: UIApplication.didEnterBackgroundNotification, object: nil)
        nc.addObserver(self, selector: #selector(willEnterForeground), name: UIApplication.willEnterForegroundNotification, object: nil)
        connect()
    }

    // MARK: connection

    private func connect() {
        connection?.close()
        connected = false
        pending.removeAll()
        showOverlay("Connecting to \(hostTitle)…", spinner: true, retry: false)
        let c = SardConnection(key: host.key, mode: .terminal)
        c.onEvent = { [weak self] in self?.handle($0) }
        c.onShellData = { [weak self] id, data in
            guard let self else { return }
            if let tab = self.tabs.first(where: { $0.id == id }) {
                tab.view.feed(byteArray: ArraySlice(data))
            }
            // Shown: the computer may send more.
            self.connection?.ackShell(id)
        }
        connection = c
    }

    private func handle(_ e: SessionEvent) {
        switch e {
        case .connected:
            connected = true
            hideOverlay()
        case .failed(let error):
            showOverlay("Couldn't connect to \(hostTitle).\n\(error)", spinner: false, retry: true)
        case .disconnected(let reason):
            connected = false
            showOverlay("Disconnected: \(reason)\nYour terminals keep running on \(hostTitle).", spinner: false, retry: true)
        case .shells(let list):
            reconcile(with: list)
        case .shellOpened(let token, let id, let name, let resumed):
            guard let tab = pending.removeValue(forKey: token) else { return }
            if resumed {
                // The computer replays recent output: start from a clean screen.
                tab.view.getTerminal().resetToInitialState()
            }
            tab.id = id
            tab.name = name
            tab.ended = false
            rebuildTabBar()
        case .shellRefused(let token, let reason):
            if let tab = pending.removeValue(forKey: token) {
                remove(tab)
            }
            showOverlay(reason, spinner: false, retry: true)
        case .shellExit(let id, let message):
            guard let tab = tabs.first(where: { $0.id == id }) else { return }
            tab.ended = true
            tab.view.feed(text: "\r\n\u{1b}[2m[\(message)]\u{1b}[0m\r\n")
            rebuildTabBar()
        case .status(let text):
            showOverlay(text, spinner: false, retry: true)
        default:
            break
        }
    }

    /// The computer's list of our shells (sent on every connect): reattach
    /// open tabs, add tabs for shells we didn't show, start one if none.
    private func reconcile(with list: [ShellInfo]) {
        let ids = Set(list.map(\.id))
        for tab in tabs where tab.id != 0 && !tab.ended {
            if ids.contains(tab.id) {
                open(tab, id: tab.id)
            } else {
                tab.ended = true
                tab.view.feed(text: "\r\n\u{1b}[2m[this terminal ended while you were away]\u{1b}[0m\r\n")
            }
        }
        for info in list where !tabs.contains(where: { $0.id == info.id }) {
            let tab = makeTab()
            tab.id = info.id
            tab.name = info.name
            open(tab, id: info.id)
        }
        if tabs.allSatisfy(\.ended) {
            newShell()
        }
        rebuildTabBar()
        if current == nil || current?.ended == true, let live = tabs.last(where: { !$0.ended }) {
            select(live)
        }
    }

    private func open(_ tab: Tab, id: UInt32) {
        let token = nextToken
        nextToken += 1
        pending[token] = tab
        container.layoutIfNeeded()
        let t = tab.view.getTerminal()
        connection?.openShell(token: token, id: id, cols: t.cols, rows: t.rows)
    }

    @objc private func newShell() {
        let tab = makeTab()
        select(tab)
        open(tab, id: 0)
        rebuildTabBar()
    }

    private func makeTab() -> Tab {
        let tv = TerminalView(frame: container.bounds, font: UIFont.monospacedSystemFont(ofSize: fontSize, weight: .regular))
        tv.terminalDelegate = self
        tv.nativeBackgroundColor = .black
        tv.nativeForegroundColor = UIColor(white: 0.92, alpha: 1)
        tv.optionAsMetaKey = true
        tv.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        tv.isHidden = true
        container.addSubview(tv)
        let tab = Tab(view: tv)
        tabs.append(tab)
        return tab
    }

    private func select(_ tab: Tab) {
        current = tab
        for t in tabs { t.view.isHidden = t !== tab }
        tab.view.frame = container.bounds
        _ = tab.view.becomeFirstResponder()
        rebuildTabBar()
    }

    private func remove(_ tab: Tab) {
        tab.view.removeFromSuperview()
        tabs.removeAll { $0 === tab }
        if current === tab {
            current = nil
            if let last = tabs.last { select(last) }
        }
        rebuildTabBar()
    }

    private func closeTab(_ tab: Tab) {
        if !tab.ended && tab.id != 0 { connection?.closeShell(tab.id) }
        remove(tab)
        if tabs.isEmpty && connected { newShell() }
    }

    @objc private func didEnterBackground() {
        // iOS suspends the app soon; the shells keep running on the computer.
        connection?.close()
        connection = nil
    }

    @objc private func willEnterForeground() {
        connect()
    }

    // MARK: TerminalViewDelegate

    private func tab(for view: TerminalView) -> Tab? { tabs.first { $0.view === view } }

    func send(source: TerminalView, data: ArraySlice<UInt8>) {
        guard let tab = tab(for: source), tab.id != 0, !tab.ended else { return }
        connection?.writeShell(tab.id, data)
    }

    func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
        guard let tab = tab(for: source), tab.id != 0, !tab.ended else { return }
        connection?.resizeShell(tab.id, cols: newCols, rows: newRows)
    }

    func setTerminalTitle(source: TerminalView, title: String) {
        tab(for: source)?.title = title.isEmpty ? nil : title
        rebuildTabBar()
    }

    func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
    func scrolled(source: TerminalView, position: Double) {}
    func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}

    func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {
        if let url = URL(string: link), ["http", "https"].contains(url.scheme?.lowercased() ?? "") {
            UIApplication.shared.open(url)
        }
    }

    func bell(source: TerminalView) {
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
    }

    func clipboardCopy(source: TerminalView, content: Data) {
        if let s = String(data: content, encoding: .utf8) { UIPasteboard.general.string = s }
    }

    // MARK: chrome

    private func setUpChrome() {
        let top = UIView()
        top.backgroundColor = UIColor(white: 0.08, alpha: 1)
        top.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(top)

        let close = chromeButton("xmark", "Close") { [weak self] in self?.closeTapped() }
        let add = chromeButton("plus", "New Terminal") { [weak self] in self?.newShell() }
        let font = chromeButton("textformat.size", "Text Size", action: nil)
        font.showsMenuAsPrimaryAction = true
        font.menu = UIMenu(children: [
            UIAction(title: "Larger Text", image: UIImage(systemName: "plus.magnifyingglass")) { [weak self] _ in self?.changeFont(by: 1) },
            UIAction(title: "Smaller Text", image: UIImage(systemName: "minus.magnifyingglass")) { [weak self] _ in self?.changeFont(by: -1) },
        ])

        tabScroll.showsHorizontalScrollIndicator = false
        tabScroll.translatesAutoresizingMaskIntoConstraints = false
        tabBar.axis = .horizontal
        tabBar.spacing = 6
        tabBar.translatesAutoresizingMaskIntoConstraints = false
        tabScroll.addSubview(tabBar)

        let row = UIStackView(arrangedSubviews: [close, tabScroll, add, font])
        row.axis = .horizontal
        row.spacing = 6
        row.alignment = .center
        row.translatesAutoresizingMaskIntoConstraints = false
        top.addSubview(row)

        container.backgroundColor = .black
        container.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(container)

        overlay.axis = .vertical
        overlay.alignment = .center
        overlay.spacing = 16
        overlay.translatesAutoresizingMaskIntoConstraints = false
        overlayLabel.numberOfLines = 0
        overlayLabel.textAlignment = .center
        overlayLabel.textColor = .white
        overlayLabel.font = .preferredFont(forTextStyle: .headline)
        overlaySpinner.color = .white
        var rc = UIButton.Configuration.filled()
        rc.title = "Try Again"
        rc.cornerStyle = .capsule
        retryButton.configuration = rc
        retryButton.addAction(UIAction { [weak self] _ in self?.connect() }, for: .touchUpInside)
        for v in [overlaySpinner, overlayLabel, retryButton] { overlay.addArrangedSubview(v) }
        view.addSubview(overlay)

        let g = view.safeAreaLayoutGuide
        NSLayoutConstraint.activate([
            top.topAnchor.constraint(equalTo: view.topAnchor),
            top.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            top.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            row.topAnchor.constraint(equalTo: g.topAnchor, constant: 4),
            row.bottomAnchor.constraint(equalTo: top.bottomAnchor, constant: -4),
            row.leadingAnchor.constraint(equalTo: g.leadingAnchor, constant: 8),
            row.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -8),
            row.heightAnchor.constraint(equalToConstant: 36),
            tabBar.leadingAnchor.constraint(equalTo: tabScroll.contentLayoutGuide.leadingAnchor),
            tabBar.trailingAnchor.constraint(equalTo: tabScroll.contentLayoutGuide.trailingAnchor),
            tabBar.topAnchor.constraint(equalTo: tabScroll.contentLayoutGuide.topAnchor),
            tabBar.bottomAnchor.constraint(equalTo: tabScroll.contentLayoutGuide.bottomAnchor),
            tabBar.heightAnchor.constraint(equalTo: tabScroll.frameLayoutGuide.heightAnchor),
            tabScroll.heightAnchor.constraint(equalToConstant: 32),
            container.topAnchor.constraint(equalTo: top.bottomAnchor),
            container.leadingAnchor.constraint(equalTo: g.leadingAnchor),
            container.trailingAnchor.constraint(equalTo: g.trailingAnchor),
            // Above the keyboard when it's up.
            container.bottomAnchor.constraint(equalTo: view.keyboardLayoutGuide.topAnchor),
            overlay.centerXAnchor.constraint(equalTo: container.centerXAnchor),
            overlay.centerYAnchor.constraint(equalTo: container.centerYAnchor),
            overlay.leadingAnchor.constraint(greaterThanOrEqualTo: g.leadingAnchor, constant: 32),
        ])
    }

    private func chromeButton(_ symbol: String, _ label: String, action: (() -> Void)?) -> UIButton {
        let b = UIButton(type: .system)
        b.setImage(UIImage(systemName: symbol, withConfiguration: UIImage.SymbolConfiguration(pointSize: 16, weight: .semibold)), for: .normal)
        b.tintColor = .white
        b.accessibilityLabel = label
        if let action { b.addAction(UIAction { _ in action() }, for: .touchUpInside) }
        b.widthAnchor.constraint(equalToConstant: 36).isActive = true
        b.heightAnchor.constraint(equalToConstant: 36).isActive = true
        return b
    }

    private func rebuildTabBar() {
        for v in tabBar.arrangedSubviews { v.removeFromSuperview() }
        for tab in tabs {
            var c = UIButton.Configuration.filled()
            c.cornerStyle = .capsule
            c.contentInsets = NSDirectionalEdgeInsets(top: 4, leading: 12, bottom: 4, trailing: 12)
            let title = tab.title ?? tab.name
            c.attributedTitle = AttributedString(tab.ended ? "\(title) (ended)" : title,
                                                 attributes: AttributeContainer([.font: UIFont.systemFont(ofSize: 13, weight: .medium)]))
            c.baseBackgroundColor = tab === current ? .systemGreen.withAlphaComponent(0.85) : UIColor(white: 0.22, alpha: 1)
            c.baseForegroundColor = .white
            let b = UIButton(configuration: c, primaryAction: UIAction { [weak self] _ in self?.select(tab) })
            b.titleLabel?.lineBreakMode = .byTruncatingTail
            b.widthAnchor.constraint(lessThanOrEqualToConstant: 200).isActive = true
            b.menu = UIMenu(children: [
                UIAction(title: tab.ended ? "Remove Tab" : "Close Terminal", image: UIImage(systemName: "xmark"),
                         attributes: tab.ended ? [] : .destructive) { [weak self] _ in self?.closeTab(tab) },
            ])
            tabBar.addArrangedSubview(b)
        }
    }

    private func changeFont(by delta: CGFloat) {
        let size = (fontSize + delta).clamped(8, 24)
        UserDefaults.standard.set(Double(size), forKey: Pref.terminalFontSize)
        for t in tabs { t.view.font = UIFont.monospacedSystemFont(ofSize: size, weight: .regular) }
    }

    private func showOverlay(_ text: String, spinner: Bool, retry: Bool) {
        overlayLabel.text = text
        overlaySpinner.isHidden = !spinner
        if spinner { overlaySpinner.startAnimating() } else { overlaySpinner.stopAnimating() }
        retryButton.isHidden = !retry
        overlay.isHidden = false
        container.alpha = 0.35
    }

    private func hideOverlay() {
        overlay.isHidden = true
        container.alpha = 1
    }

    private func closeTapped() {
        NotificationCenter.default.removeObserver(self)
        for t in tabs { _ = t.view.resignFirstResponder() }
        // Closing the screen leaves the shells running on the computer (they
        // come back next time); close a tab to end its shell.
        connection?.close()
        connection = nil
        onClose()
    }
}

extension Comparable {
    func clamped(_ lo: Self, _ hi: Self) -> Self { min(max(self, lo), hi) }
}
