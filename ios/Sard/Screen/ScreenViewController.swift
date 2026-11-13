import AVFoundation
import SwiftUI
import UIKit

struct ScreenSessionView: UIViewControllerRepresentable {
    let host: Host
    let title: String
    let onClose: () -> Void

    func makeUIViewController(context: Context) -> ScreenViewController {
        ScreenViewController(host: host, title: title, onClose: onClose)
    }
    func updateUIViewController(_ vc: ScreenViewController, context: Context) {}
}

/// The video: a CAMetalLayer the Rust core draws into. Its drawable is the
/// video's pixel size; zoom and pan only move and scale the view.
final class MetalView: UIView {
    override class var layerClass: AnyClass { CAMetalLayer.self }
    var metalLayer: CAMetalLayer { layer as! CAMetalLayer }
    var pixelSize = CGSize(width: 16, height: 9) {
        didSet { if pixelSize != oldValue { metalLayer.drawableSize = pixelSize } }
    }
    override func layoutSubviews() {
        super.layoutSubviews()
        // UIKit may resize the drawable with the view; keep the video's size.
        if metalLayer.drawableSize != pixelSize { metalLayer.drawableSize = pixelSize }
    }
}

final class ScreenViewController: UIViewController, UIGestureRecognizerDelegate {
    private let host: Host
    private let hostTitle: String
    private let onClose: () -> Void
    private var connection: SardConnection?
    private let audio = AudioPlayer()

    // Video and viewport
    private let videoView = MetalView()
    private let cursorView = UIImageView()
    private var videoSize = CGSize(width: 1920, height: 1080) // host stream size
    private var zoom: CGFloat = 1
    private var pan: CGPoint = .zero

    // Pointer
    private var cursor = CGPoint(x: 0.5, y: 0.5) // normalized
    private var cursorMetadata = false
    private var cursorHot = CGPoint.zero
    private var cursorPixelSize = CGSize.zero
    private var hostCursorVisible = true
    private var lastLocalMove = Date.distantPast
    private var dragging = false
    private var pinching = false
    private var mode: PointerMode {
        PointerMode(rawValue: UserDefaults.standard.string(forKey: Pref.pointerMode) ?? "") ?? .trackpad
    }

    // Host
    private var hostPlatform = ""
    private var displays: [DisplayInfo] = []
    private var currentDisplay = 0
    private var fitted = UserDefaults.standard.bool(forKey: Pref.fitScreen)
    private var connected = false

    // Chrome
    private let toolbar = UIStackView()
    private let toolbarBackground = UIVisualEffectView(effect: UIBlurEffect(style: .systemUltraThinMaterialDark))
    private let statsLabel = UILabel()
    private let showToolbarButton = UIButton(type: .system)
    private let overlay = UIStackView()
    private let overlayLabel = UILabel()
    private let overlaySpinner = UIActivityIndicatorView(style: .large)
    private let reconnectButton = UIButton(type: .system)
    private let toast = UILabel()
    private var displaysButton: UIButton!
    private var fitButton: UIButton!
    private var modeButton: UIButton!
    private lazy var keyInput = KeyInputView(controller: self)

    init(host: Host, title: String, onClose: @escaping () -> Void) {
        self.host = host
        self.hostTitle = title
        self.onClose = onClose
        super.init(nibName: nil, bundle: nil)
        modalPresentationStyle = .fullScreen
    }

    required init?(coder: NSCoder) { fatalError() }

    override var prefersStatusBarHidden: Bool { true }
    override var prefersHomeIndicatorAutoHidden: Bool { true }
    override var preferredScreenEdgesDeferringSystemGestures: UIRectEdge { .all }
    override var canBecomeFirstResponder: Bool { true }

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        view.clipsToBounds = true
        view.addSubview(videoView)
        videoView.isUserInteractionEnabled = false
        cursorView.isHidden = true
        view.addSubview(cursorView)
        view.addSubview(keyInput)
        setUpGestures()
        setUpChrome()
        let nc = NotificationCenter.default
        nc.addObserver(self, selector: #selector(didEnterBackground), name: UIApplication.didEnterBackgroundNotification, object: nil)
        nc.addObserver(self, selector: #selector(willEnterForeground), name: UIApplication.willEnterForegroundNotification, object: nil)
        connect()
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        becomeFirstResponder() // hardware keyboard
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        layoutVideo()
    }

    override func viewWillTransition(to size: CGSize, with coordinator: UIViewControllerTransitionCoordinator) {
        super.viewWillTransition(to: size, with: coordinator)
        coordinator.animate(alongsideTransition: { _ in
            self.zoom = 1
            self.pan = .zero
            self.layoutVideo()
        })
    }

    // MARK: connection

    private func fitSize() -> CGSize? {
        guard fitted else { return nil }
        let size = TextSize(rawValue: UserDefaults.standard.integer(forKey: Pref.textSize)) ?? .medium
        let b = view.window?.windowScene?.screen.bounds ?? UIScreen.main.bounds
        let aspect = max(b.width, b.height) / max(1, min(b.width, b.height))
        return CGSize(width: size.desktopWidth, height: (size.desktopWidth / aspect).rounded())
    }

    private func connect() {
        teardown()
        connected = false
        showOverlay("Connecting to \(hostTitle)…", spinner: true, retry: false)
        var opts = SardScreenOptions()
        opts.max_width = 2560
        opts.max_height = 1600
        opts.max_fps = UInt16(UIScreen.main.maximumFramesPerSecond)
        if let fit = fitSize() {
            opts.fit_width = UInt16(fit.width)
            opts.fit_height = UInt16(fit.height)
        }
        opts.audio = UserDefaults.standard.bool(forKey: Pref.playAudio)
        let c = SardConnection(key: host.key, mode: .screen, options: opts)
        c.onEvent = { [weak self] in self?.handle($0) }
        c.onCursorShape = { [weak self] _, image, hot in
            guard let self else { return }
            self.cursorView.image = image
            self.cursorHot = hot
            self.cursorPixelSize = CGSize(width: image.size.width * image.scale, height: image.size.height * image.scale)
            self.layoutCursor()
        }
        c.onCursorPos = { [weak self] p, visible in
            guard let self else { return }
            self.hostCursorVisible = visible
            // Our own moves echo back late; don't let them fight the finger.
            if Date().timeIntervalSince(self.lastLocalMove) > 0.4 { self.cursor = p }
            self.layoutCursor()
        }
        connection = c
    }

    private func teardown() {
        audio.stop() // before the session goes: the audio thread reads it
        if let c = connection {
            c.releaseAll()
            c.setLayer(nil)
            c.close()
        }
        connection = nil
    }

    private func handle(_ e: SessionEvent) {
        switch e {
        case .connected(_, let via):
            connected = true
            hideOverlay()
            connection?.setLayer(videoView.metalLayer)
            showToast(via == "internet" ? "Connected over the internet" : "Connected")
        case .failed(let error):
            showOverlay("Couldn't connect to \(hostTitle).\n\(error)", spinner: false, retry: true)
        case .disconnected(let reason):
            audio.stop()
            connected = false
            showOverlay("Disconnected: \(reason)", spinner: false, retry: true)
        case .status(let text):
            showToast(text.prefix(1).uppercased() + text.dropFirst())
        case .stream(let w, let h, let platform, let metadata):
            hostPlatform = platform
            cursorMetadata = metadata
            videoSize = CGSize(width: w, height: h)
            keyInput.hostPlatform = platform
            layoutVideo()
        case .video(let w, let h):
            videoView.pixelSize = CGSize(width: w, height: h)
            if videoSize.width == 0 { videoSize = CGSize(width: w, height: h) }
            layoutVideo()
        case .displays(let current, let list):
            displays = list
            currentDisplay = current
            updateDisplaysMenu()
        case .stats(let fps, let mbps, let latency, _):
            statsLabel.text = latency >= 0
                ? String(format: "%d fps · %.0f ms · %.1f Mbps", fps, latency, mbps)
                : String(format: "%d fps · %.1f Mbps", fps, mbps)
        case .audio:
            if let h = connection?.rawHandle { audio.start(session: h) }
        case .clipboard(let text):
            UIPasteboard.general.string = text
        default:
            break
        }
    }

    @objc private func didEnterBackground() {
        teardown()
    }

    @objc private func willEnterForeground() {
        connect()
    }

    // MARK: layout

    /// Where the video sits at zoom 1 (aspect fit).
    private var baseRect: CGRect {
        AVMakeRect(aspectRatio: videoSize.width > 0 ? videoSize : CGSize(width: 16, height: 9), insideRect: view.bounds)
    }

    private func layoutVideo() {
        let base = baseRect
        let b = view.bounds
        let w = base.width * zoom, h = base.height * zoom
        var x = b.midX - w / 2 + pan.x, y = b.midY - h / 2 + pan.y
        x = w > b.width ? min(0, max(b.width - w, x)) : b.midX - w / 2
        y = h > b.height ? min(0, max(b.height - h, y)) : b.midY - h / 2
        pan = CGPoint(x: x - (b.midX - w / 2), y: y - (b.midY - h / 2))
        videoView.frame = CGRect(x: x, y: y, width: w, height: h)
        layoutCursor()
    }

    private func layoutCursor() {
        let f = videoView.frame
        guard cursorMetadata, hostCursorVisible, connected, cursorView.image != nil, f.width > 0 else {
            cursorView.isHidden = true
            return
        }
        // Host cursor pixels → points: follow the video's scale, but keep it
        // findable on a phone.
        let hostWidth = displays.first(where: { $0.id == currentDisplay }).map { CGFloat($0.width) } ?? videoSize.width
        var scale = f.width / max(1, hostWidth)
        let minHeight: CGFloat = 16, maxHeight: CGFloat = 40
        if cursorPixelSize.height * scale < minHeight { scale = minHeight / max(1, cursorPixelSize.height) }
        if cursorPixelSize.height * scale > maxHeight { scale = maxHeight / max(1, cursorPixelSize.height) }
        let p = CGPoint(x: f.minX + cursor.x * f.width, y: f.minY + cursor.y * f.height)
        cursorView.frame = CGRect(x: p.x - cursorHot.x * scale, y: p.y - cursorHot.y * scale,
                                  width: cursorPixelSize.width * scale, height: cursorPixelSize.height * scale)
        cursorView.isHidden = false
    }

    /// Keep the pointer on screen while zoomed in.
    private func followCursor() {
        guard zoom > 1.01 else { return }
        let f = videoView.frame
        let p = CGPoint(x: f.minX + cursor.x * f.width, y: f.minY + cursor.y * f.height)
        let inset = view.bounds.insetBy(dx: 60, dy: 60)
        var dx: CGFloat = 0, dy: CGFloat = 0
        if p.x < inset.minX { dx = inset.minX - p.x } else if p.x > inset.maxX { dx = inset.maxX - p.x }
        if p.y < inset.minY { dy = inset.minY - p.y } else if p.y > inset.maxY { dy = inset.maxY - p.y }
        if dx != 0 || dy != 0 {
            pan.x += dx
            pan.y += dy
            layoutVideo()
        }
    }

    /// A point in the view → normalized video coordinates.
    private func normalized(_ p: CGPoint) -> CGPoint {
        let f = videoView.frame
        return CGPoint(x: min(1, max(0, (p.x - f.minX) / max(1, f.width))),
                       y: min(1, max(0, (p.y - f.minY) / max(1, f.height))))
    }

    // MARK: gestures

    private func setUpGestures() {
        let tap = UITapGestureRecognizer(target: self, action: #selector(onTap(_:)))
        let twoTap = UITapGestureRecognizer(target: self, action: #selector(onTwoFingerTap(_:)))
        twoTap.numberOfTouchesRequired = 2
        let pan1 = UIPanGestureRecognizer(target: self, action: #selector(onPan(_:)))
        pan1.maximumNumberOfTouches = 1
        let pan2 = UIPanGestureRecognizer(target: self, action: #selector(onTwoFingerPan(_:)))
        pan2.minimumNumberOfTouches = 2
        pan2.maximumNumberOfTouches = 2
        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(onPinch(_:)))
        let hold = UILongPressGestureRecognizer(target: self, action: #selector(onHold(_:)))
        hold.minimumPressDuration = 0.35
        for g in [tap, twoTap, pan1, pan2, pinch, hold] as [UIGestureRecognizer] {
            g.delegate = self
            view.addGestureRecognizer(g)
        }
        pan2.allowedScrollTypesMask = .continuous // trackpads / mice on iPad
    }

    func gestureRecognizer(_ g: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        // Pinch and two-finger pan together: zoom and move the view.
        (g is UIPinchGestureRecognizer && other is UIPanGestureRecognizer)
            || (g is UIPanGestureRecognizer && other is UIPinchGestureRecognizer)
    }

    func gestureRecognizer(_ g: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        // Not on the toolbar / overlay buttons.
        !(touch.view is UIControl) && connected
    }

    private func moveCursor(by d: CGPoint, velocity v: CGPoint) {
        let f = videoView.frame
        guard f.width > 0 else { return }
        // Pointer acceleration: slow moves are precise, fast ones cover ground.
        let speed = 1 + min(2.5, hypot(v.x, v.y) / 900)
        cursor.x = min(1, max(0, cursor.x + d.x * speed / f.width))
        cursor.y = min(1, max(0, cursor.y + d.y * speed / f.height))
        lastLocalMove = Date()
        connection?.moveMouse(cursor)
        layoutCursor()
        followCursor()
    }

    private func setCursor(_ p: CGPoint) {
        cursor = normalized(p)
        lastLocalMove = Date()
        connection?.moveMouse(cursor)
        layoutCursor()
    }

    private func click(_ button: UInt8) {
        connection?.button(button, down: true, at: cursor)
        connection?.button(button, down: false, at: cursor)
    }

    @objc private func onTap(_ g: UITapGestureRecognizer) {
        if mode == .touch { setCursor(g.location(in: view)) }
        click(1)
    }

    @objc private func onTwoFingerTap(_ g: UITapGestureRecognizer) {
        if mode == .touch { setCursor(g.location(in: view)) }
        click(2)
    }

    @objc private func onPan(_ g: UIPanGestureRecognizer) {
        switch mode {
        case .trackpad:
            let d = g.translation(in: view)
            g.setTranslation(.zero, in: view)
            moveCursor(by: d, velocity: g.velocity(in: view))
        case .touch:
            // Drag with the left button, from where the finger went down.
            let p = g.location(in: view)
            switch g.state {
            case .began:
                setCursor(p)
                connection?.button(1, down: true, at: cursor)
                dragging = true
            case .changed:
                setCursor(p)
            default:
                setCursor(p)
                connection?.button(1, down: false, at: cursor)
                dragging = false
            }
        }
    }

    private var lastHoldPoint = CGPoint.zero

    @objc private func onHold(_ g: UILongPressGestureRecognizer) {
        let p = g.location(in: view)
        switch (mode, g.state) {
        case (.trackpad, .began):
            // Hold, then drag: press the left button and move with the finger.
            lastHoldPoint = p
            connection?.button(1, down: true, at: cursor)
            dragging = true
            UIImpactFeedbackGenerator(style: .medium).impactOccurred()
        case (.trackpad, .changed):
            moveCursor(by: CGPoint(x: p.x - lastHoldPoint.x, y: p.y - lastHoldPoint.y), velocity: .zero)
            lastHoldPoint = p
        case (.trackpad, _):
            if dragging {
                connection?.button(1, down: false, at: cursor)
                dragging = false
            }
        case (.touch, .began):
            // Touch mode: hold to right-click.
            setCursor(p)
            click(2)
            UIImpactFeedbackGenerator(style: .medium).impactOccurred()
        default:
            break
        }
    }

    @objc private func onTwoFingerPan(_ g: UIPanGestureRecognizer) {
        let d = g.translation(in: view)
        g.setTranslation(.zero, in: view)
        if pinching { return } // the pinch moves the view itself
        if zoom > 1.01 && mode == .touch {
            pan.x += d.x
            pan.y += d.y
            layoutVideo()
            return
        }
        // Content follows the fingers, like scrolling on the phone.
        connection?.scroll(dx: d.x * 2, dy: d.y * 2)
    }

    private var pinchAnchor = CGPoint.zero

    @objc private func onPinch(_ g: UIPinchGestureRecognizer) {
        let p = g.location(in: view)
        switch g.state {
        case .began:
            pinching = true
            pinchAnchor = p
        case .changed:
            let newZoom = min(5, max(1, zoom * g.scale))
            let factor = newZoom / zoom
            g.scale = 1
            // Zoom around the fingers.
            let b = view.bounds
            let c = CGPoint(x: b.midX + pan.x, y: b.midY + pan.y)
            pan.x = (c.x - p.x) * factor + p.x - b.midX + (p.x - pinchAnchor.x)
            pan.y = (c.y - p.y) * factor + p.y - b.midY + (p.y - pinchAnchor.y)
            pinchAnchor = p
            zoom = newZoom
            layoutVideo()
        default:
            pinching = false
            if zoom < 1.05 {
                zoom = 1
                pan = .zero
                UIView.animate(withDuration: 0.2) { self.layoutVideo() }
            }
        }
    }

    // MARK: keyboard

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !keyInput.handlePresses(presses, down: true) { super.pressesBegan(presses, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !keyInput.handlePresses(presses, down: false) { super.pressesEnded(presses, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        if !keyInput.handlePresses(presses, down: false) { super.pressesCancelled(presses, with: event) }
    }

    var session: SardConnection? { connection }

    // MARK: chrome

    private func button(_ symbol: String, _ label: String, action: Selector?) -> UIButton {
        let b = UIButton(type: .system)
        b.setImage(UIImage(systemName: symbol, withConfiguration: UIImage.SymbolConfiguration(pointSize: 17, weight: .semibold)), for: .normal)
        b.tintColor = .white
        b.accessibilityLabel = label
        if let action { b.addTarget(self, action: action, for: .touchUpInside) }
        b.widthAnchor.constraint(equalToConstant: 40).isActive = true
        b.heightAnchor.constraint(equalToConstant: 40).isActive = true
        return b
    }

    private func setUpChrome() {
        toolbarBackground.layer.cornerRadius = 22
        toolbarBackground.clipsToBounds = true
        toolbarBackground.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(toolbarBackground)

        toolbar.axis = .horizontal
        toolbar.alignment = .center
        toolbar.spacing = 2
        toolbar.translatesAutoresizingMaskIntoConstraints = false
        toolbarBackground.contentView.addSubview(toolbar)

        let close = button("xmark", "Disconnect", action: #selector(closeTapped))
        let keyboard = button("keyboard", "Keyboard", action: #selector(keyboardTapped))
        displaysButton = button("display.2", "Screens", action: nil)
        displaysButton.showsMenuAsPrimaryAction = true
        displaysButton.isHidden = true
        fitButton = button("arrow.down.right.and.arrow.up.left", "Fit Screen to Phone", action: #selector(fitTapped))
        modeButton = button("cursorarrow.rays", "Pointer Mode", action: #selector(modeTapped))
        let more = button("ellipsis.circle", "More", action: nil)
        more.showsMenuAsPrimaryAction = true
        more.menu = UIMenu(children: [
            UIAction(title: "Paste from iPhone", image: UIImage(systemName: "doc.on.clipboard")) { [weak self] _ in self?.pasteFromPhone() },
            UIAction(title: "Refresh Picture", image: UIImage(systemName: "arrow.clockwise")) { [weak self] _ in self?.connection?.requestKeyframe() },
            UIAction(title: "Release All Keys", image: UIImage(systemName: "keyboard.badge.ellipsis")) { [weak self] _ in self?.connection?.releaseAll() },
            UIAction(title: "Hide Toolbar", image: UIImage(systemName: "chevron.up")) { [weak self] _ in self?.setToolbar(visible: false) },
        ])
        let hide = button("chevron.up", "Hide Toolbar", action: #selector(hideToolbarTapped))
        statsLabel.font = .monospacedDigitSystemFont(ofSize: 11, weight: .medium)
        statsLabel.textColor = UIColor(white: 1, alpha: 0.75)
        statsLabel.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        for v in [close, keyboard, displaysButton!, fitButton!, modeButton!, more, statsLabel, hide] as [UIView] {
            toolbar.addArrangedSubview(v)
        }
        toolbar.setCustomSpacing(10, after: modeButton)
        toolbar.setCustomSpacing(10, after: more)
        toolbar.setCustomSpacing(8, after: statsLabel)

        showToolbarButton.setImage(UIImage(systemName: "chevron.compact.down"), for: .normal)
        showToolbarButton.tintColor = UIColor(white: 1, alpha: 0.8)
        showToolbarButton.backgroundColor = UIColor(white: 0, alpha: 0.35)
        showToolbarButton.layer.cornerRadius = 14
        showToolbarButton.accessibilityLabel = "Show Toolbar"
        showToolbarButton.addTarget(self, action: #selector(showToolbarTapped), for: .touchUpInside)
        showToolbarButton.translatesAutoresizingMaskIntoConstraints = false
        showToolbarButton.isHidden = true
        view.addSubview(showToolbarButton)

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
        rc.title = "Reconnect"
        rc.cornerStyle = .capsule
        reconnectButton.configuration = rc
        reconnectButton.addTarget(self, action: #selector(reconnectTapped), for: .touchUpInside)
        for v in [overlaySpinner, overlayLabel, reconnectButton] { overlay.addArrangedSubview(v) }
        view.addSubview(overlay)

        toast.textColor = .white
        toast.font = .preferredFont(forTextStyle: .subheadline)
        toast.backgroundColor = UIColor(white: 0.1, alpha: 0.85)
        toast.layer.cornerRadius = 12
        toast.clipsToBounds = true
        toast.textAlignment = .center
        toast.numberOfLines = 2
        toast.alpha = 0
        toast.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(toast)

        let g = view.safeAreaLayoutGuide
        NSLayoutConstraint.activate([
            toolbarBackground.topAnchor.constraint(equalTo: g.topAnchor, constant: 6),
            toolbarBackground.centerXAnchor.constraint(equalTo: g.centerXAnchor),
            toolbarBackground.leadingAnchor.constraint(greaterThanOrEqualTo: g.leadingAnchor, constant: 8),
            toolbar.topAnchor.constraint(equalTo: toolbarBackground.contentView.topAnchor, constant: 2),
            toolbar.bottomAnchor.constraint(equalTo: toolbarBackground.contentView.bottomAnchor, constant: -2),
            toolbar.leadingAnchor.constraint(equalTo: toolbarBackground.contentView.leadingAnchor, constant: 6),
            toolbar.trailingAnchor.constraint(equalTo: toolbarBackground.contentView.trailingAnchor, constant: -6),
            showToolbarButton.topAnchor.constraint(equalTo: g.topAnchor, constant: 2),
            showToolbarButton.centerXAnchor.constraint(equalTo: g.centerXAnchor),
            showToolbarButton.widthAnchor.constraint(equalToConstant: 56),
            showToolbarButton.heightAnchor.constraint(equalToConstant: 28),
            overlay.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            overlay.centerYAnchor.constraint(equalTo: view.centerYAnchor),
            overlay.leadingAnchor.constraint(greaterThanOrEqualTo: g.leadingAnchor, constant: 32),
            toast.bottomAnchor.constraint(equalTo: g.bottomAnchor, constant: -24),
            toast.centerXAnchor.constraint(equalTo: g.centerXAnchor),
            toast.leadingAnchor.constraint(greaterThanOrEqualTo: g.leadingAnchor, constant: 24),
            toast.heightAnchor.constraint(greaterThanOrEqualToConstant: 36),
        ])
        updateFitButton()
        updateModeButton()
    }

    private func setToolbar(visible: Bool) {
        UIView.animate(withDuration: 0.2) {
            self.toolbarBackground.alpha = visible ? 1 : 0
            self.showToolbarButton.alpha = visible ? 0 : 1
        } completion: { _ in
            self.toolbarBackground.isHidden = !visible
            self.showToolbarButton.isHidden = visible
        }
        toolbarBackground.isHidden = false
        showToolbarButton.isHidden = false
    }

    private func updateDisplaysMenu() {
        displaysButton.isHidden = displays.count < 2
        displaysButton.menu = UIMenu(title: "Screens", children: displays.map { d in
            UIAction(title: d.name, subtitle: "\(d.width)×\(d.height)",
                     state: d.id == currentDisplay ? .on : .off) { [weak self] _ in
                self?.connection?.selectDisplay(d.id)
            }
        })
    }

    private func updateFitButton() {
        fitButton.tintColor = fitted ? .systemBlue : .white
        fitButton.accessibilityValue = fitted ? "On" : "Off"
    }

    private func updateModeButton() {
        modeButton.setImage(UIImage(systemName: mode == .trackpad ? "cursorarrow.rays" : "hand.point.up.left"), for: .normal)
        modeButton.accessibilityValue = mode.label
    }

    private func showOverlay(_ text: String, spinner: Bool, retry: Bool) {
        overlayLabel.text = text
        overlaySpinner.isHidden = !spinner
        if spinner { overlaySpinner.startAnimating() } else { overlaySpinner.stopAnimating() }
        reconnectButton.isHidden = !retry
        overlay.isHidden = false
        videoView.alpha = 0.3
        cursorView.isHidden = true
    }

    private func hideOverlay() {
        overlay.isHidden = true
        videoView.alpha = 1
    }

    private var toastWork: DispatchWorkItem?

    private func showToast(_ text: String) {
        toast.text = "  \(text)  "
        UIView.animate(withDuration: 0.2) { self.toast.alpha = 1 }
        toastWork?.cancel()
        let w = DispatchWorkItem { [weak self] in UIView.animate(withDuration: 0.4) { self?.toast.alpha = 0 } }
        toastWork = w
        DispatchQueue.main.asyncAfter(deadline: .now() + 2.5, execute: w)
    }

    @objc private func closeTapped() {
        NotificationCenter.default.removeObserver(self)
        _ = keyInput.resignFirstResponder()
        teardown()
        onClose()
    }

    @objc private func keyboardTapped() {
        if keyInput.isFirstResponder {
            _ = keyInput.resignFirstResponder()
            becomeFirstResponder()
        } else {
            keyInput.becomeFirstResponder()
        }
    }

    @objc private func fitTapped() {
        fitted.toggle()
        updateFitButton()
        connection?.fitDisplay(fitSize())
        zoom = 1
        pan = .zero
        layoutVideo()
    }

    @objc private func modeTapped() {
        let next: PointerMode = mode == .trackpad ? .touch : .trackpad
        UserDefaults.standard.set(next.rawValue, forKey: Pref.pointerMode)
        updateModeButton()
        showToast(next == .trackpad ? "Trackpad: drag to move, tap to click" : "Touch: tap where you want to click")
    }

    @objc private func hideToolbarTapped() { setToolbar(visible: false) }
    @objc private func showToolbarTapped() { setToolbar(visible: true) }
    @objc private func reconnectTapped() { connect() }

    private func pasteFromPhone() {
        guard let text = UIPasteboard.general.string, let c = connection else { return }
        c.setClipboard(text)
        // Then paste it there: Cmd+V on a Mac, Ctrl+V elsewhere.
        let modifier: UInt16 = hostPlatform == "macos" ? KeyCode.leftMeta : KeyCode.leftCtrl
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) {
            c.key(modifier, down: true)
            c.key(KeyCode.v, down: true)
            c.key(KeyCode.v, down: false)
            c.key(modifier, down: false)
        }
    }
}
