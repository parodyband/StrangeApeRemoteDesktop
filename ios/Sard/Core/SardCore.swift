import Foundation
import UIKit

/// A paired computer (from the Rust core's `sard_hosts`).
struct Host: Identifiable, Decodable, Hashable {
    let key: String
    let name: String
    let addr: String
    let fingerprint: String
    var id: String { key }
}

/// Thin Swift face of the Rust core (crates/mobile, `sard.h`).
enum SardCore {
    static let configDirectory: URL = {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        let dir = base.appendingPathComponent("sard", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        // Keys and paired computers: readable only while the phone is unlocked
        // after first unlock (background reconnects still work).
        try? (dir as NSURL).setResourceValue(
            URLFileProtection.completeUntilFirstUserAuthentication, forKey: .fileProtectionKey)
        return dir
    }()

    static func start(deviceName: String) {
        sard_init(configDirectory.path, deviceName)
        Transport.install()
    }

    /// Take ownership of a JSON string from the core.
    static func take(_ p: UnsafeMutablePointer<CChar>?) -> [String: Any] {
        guard let p else { return [:] }
        defer { sard_free(p) }
        let data = Data(bytes: p, count: strlen(p))
        return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
    }

    static func hosts() -> [Host] {
        guard let p = sard_hosts() else { return [] }
        defer { sard_free(p) }
        let data = Data(bytes: p, count: strlen(p))
        return (try? JSONDecoder().decode([Host].self, from: data)) ?? []
    }

    static var fingerprint: String {
        take(sard_identity())["fingerprint"] as? String ?? ""
    }

    static var anywhereEnabled: Bool {
        take(sard_anywhere())["enabled"] as? Bool ?? false
    }
}

/// Events from a session, already on the main thread.
enum SessionEvent {
    case connected(name: String, via: String)
    case failed(String)
    case disconnected(String)
    case status(String)
    case stream(width: Int, height: Int, hostPlatform: String, cursorMetadata: Bool)
    case video(width: Int, height: Int)
    case displays(current: Int, list: [DisplayInfo])
    case stats(fps: Int, mbps: Double, latencyMs: Double, rttMs: Double)
    case audio
    case clipboard(String)
    case shells([ShellInfo])
    case shellOpened(token: UInt32, id: UInt32, name: String, resumed: Bool)
    case shellRefused(token: UInt32, reason: String)
    case shellExit(id: UInt32, message: String)
}

struct DisplayInfo: Identifiable, Hashable {
    let id: Int
    let name: String
    let width: Int
    let height: Int
}

struct ShellInfo: Identifiable, Hashable {
    let id: UInt32
    let name: String
    let attached: Bool
    let ageSeconds: Int
}

/// Receives the core's callbacks (on its threads) and forwards them to the
/// session on the main queue. Retained by the core until the session closes.
private final class CallbackBox {
    weak var session: SardConnection?
    init(_ s: SardConnection) { session = s }
}

/// One session with a paired computer: a screen or a terminal.
final class SardConnection {
    enum Mode { case screen, terminal }

    let mode: Mode
    private(set) var handle: OpaquePointer?
    private var box: Unmanaged<CallbackBox>?

    var onEvent: ((SessionEvent) -> Void)?
    var onShellData: ((UInt32, Data) -> Void)?
    var onCursorShape: ((UInt32, UIImage, CGPoint) -> Void)?
    var onCursorPos: ((CGPoint, Bool) -> Void)?

    init(key: String, mode: Mode, options: SardScreenOptions = SardScreenOptions()) {
        self.mode = mode
        let box = Unmanaged.passRetained(CallbackBox(self))
        self.box = box
        let callbacks = SardCallbacks(
            ctx: box.toOpaque(),
            event: { ctx, json in
                guard let ctx, let json else { return }
                let s = String(cString: json)
                let box = Unmanaged<CallbackBox>.fromOpaque(ctx).takeUnretainedValue()
                DispatchQueue.main.async { box.session?.handle(json: s) }
            },
            shell_data: { ctx, id, data, len in
                guard let ctx, let data else { return }
                let bytes = Data(bytes: data, count: len)
                let box = Unmanaged<CallbackBox>.fromOpaque(ctx).takeUnretainedValue()
                DispatchQueue.main.async { box.session?.onShellData?(id, bytes) }
            },
            cursor_shape: { ctx, serial, w, h, hx, hy, rgba in
                guard let ctx, let rgba, w > 0, h > 0 else { return }
                let data = Data(bytes: rgba, count: Int(w) * Int(h) * 4)
                guard let image = SardConnection.image(rgba: data, width: Int(w), height: Int(h)) else { return }
                let box = Unmanaged<CallbackBox>.fromOpaque(ctx).takeUnretainedValue()
                let hot = CGPoint(x: Int(hx), y: Int(hy))
                DispatchQueue.main.async { box.session?.onCursorShape?(serial, image, hot) }
            },
            cursor_pos: { ctx, x, y, visible in
                guard let ctx else { return }
                let box = Unmanaged<CallbackBox>.fromOpaque(ctx).takeUnretainedValue()
                DispatchQueue.main.async { box.session?.onCursorPos?(CGPoint(x: CGFloat(x), y: CGFloat(y)), visible) }
            })
        handle = sard_connect(key, mode == .screen ? UInt32(SARD_MODE_SCREEN) : UInt32(SARD_MODE_TERMINAL),
                              options, callbacks)
    }

    deinit { close() }

    /// Disconnect. Waiting for the core's threads can take a moment (a
    /// connection attempt in progress), so it happens off the main thread.
    func close() {
        guard let h = handle else { return }
        handle = nil
        let box = self.box
        self.box = nil
        DispatchQueue.global(qos: .userInitiated).async {
            sard_session_close(h)
            box?.release()
        }
    }

    private func handle(json: String) {
        guard let data = json.data(using: .utf8),
              let o = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let type = o["type"] as? String else { return }
        let int = { (k: String) in (o[k] as? NSNumber)?.intValue ?? 0 }
        let dbl = { (k: String) in (o[k] as? NSNumber)?.doubleValue ?? 0 }
        let str = { (k: String) in o[k] as? String ?? "" }
        let event: SessionEvent?
        switch type {
        case "connected": event = .connected(name: str("name"), via: str("via"))
        case "failed": event = .failed(str("error"))
        case "disconnected": event = .disconnected(str("reason"))
        case "status": event = .status(str("text"))
        case "stream":
            event = .stream(width: int("width"), height: int("height"), hostPlatform: str("host_platform"),
                            cursorMetadata: o["cursor_metadata"] as? Bool ?? false)
        case "video": event = .video(width: int("width"), height: int("height"))
        case "displays":
            let list = (o["list"] as? [[String: Any]] ?? []).map {
                DisplayInfo(id: ($0["id"] as? NSNumber)?.intValue ?? 0, name: $0["name"] as? String ?? "",
                            width: ($0["width"] as? NSNumber)?.intValue ?? 0,
                            height: ($0["height"] as? NSNumber)?.intValue ?? 0)
            }
            event = .displays(current: int("current"), list: list)
        case "stats":
            event = .stats(fps: int("fps"), mbps: dbl("mbps"), latencyMs: dbl("latency_ms"), rttMs: dbl("rtt_ms"))
        case "audio": event = .audio
        case "clipboard": event = .clipboard(str("text"))
        case "shells":
            let list = (o["list"] as? [[String: Any]] ?? []).map {
                ShellInfo(id: ($0["id"] as? NSNumber)?.uint32Value ?? 0, name: $0["name"] as? String ?? "",
                          attached: $0["attached"] as? Bool ?? false,
                          ageSeconds: ($0["age_s"] as? NSNumber)?.intValue ?? 0)
            }
            event = .shells(list)
        case "shell_opened":
            event = .shellOpened(token: UInt32(int("token")), id: UInt32(int("id")), name: str("name"),
                                 resumed: o["resumed"] as? Bool ?? false)
        case "shell_refused": event = .shellRefused(token: UInt32(int("token")), reason: str("reason"))
        case "shell_exit": event = .shellExit(id: UInt32(int("id")), message: str("message"))
        default: event = nil
        }
        if let event { onEvent?(event) }
    }

    private static func image(rgba: Data, width: Int, height: Int) -> UIImage? {
        guard let provider = CGDataProvider(data: rgba as CFData) else { return nil }
        let info = CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue)
        guard let cg = CGImage(width: width, height: height, bitsPerComponent: 8, bitsPerPixel: 32,
                               bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: info,
                               provider: provider, decode: nil, shouldInterpolate: true, intent: .defaultIntent)
        else { return nil }
        return UIImage(cgImage: cg)
    }

    // MARK: input (screen sessions)

    func setLayer(_ layer: CAMetalLayer?) {
        guard let h = handle else { return }
        if let layer {
            sard_session_set_layer(h, Unmanaged.passUnretained(layer).toOpaque())
        } else {
            sard_session_set_layer(h, nil)
        }
    }
    func moveMouse(_ p: CGPoint) { if let h = handle { sard_mouse_move(h, Float(p.x), Float(p.y)) } }
    func button(_ b: UInt8, down: Bool, at p: CGPoint) {
        if let h = handle { sard_mouse_button(h, b, down, Float(p.x), Float(p.y)) }
    }
    func scroll(dx: CGFloat, dy: CGFloat) { if let h = handle { sard_scroll(h, Float(dx), Float(dy)) } }
    func key(_ code: UInt16, down: Bool) { if let h = handle { sard_key(h, code, down) } }
    func type(_ text: String, modifiers: [UInt16]) {
        guard let h = handle else { return }
        modifiers.withUnsafeBufferPointer { sard_type_text(h, text, $0.baseAddress, $0.count) }
    }
    func releaseAll() { if let h = handle { sard_release_all(h) } }
    func selectDisplay(_ id: Int) { if let h = handle { sard_select_display(h, UInt32(id)) } }
    func fitDisplay(_ size: CGSize?) {
        guard let h = handle else { return }
        sard_fit_display(h, UInt16(size?.width ?? 0), UInt16(size?.height ?? 0))
    }
    func requestKeyframe() { if let h = handle { sard_request_keyframe(h) } }
    func setClipboard(_ text: String) { if let h = handle { sard_set_clipboard(h, text) } }
    var rawHandle: OpaquePointer? { handle }

    // MARK: terminal

    func openShell(token: UInt32, id: UInt32, cols: Int, rows: Int) {
        if let h = handle { sard_shell_open(h, token, id, UInt16(clamping: cols), UInt16(clamping: rows)) }
    }
    func writeShell(_ id: UInt32, _ bytes: ArraySlice<UInt8>) {
        guard let h = handle else { return }
        bytes.withUnsafeBufferPointer { sard_shell_write(h, id, $0.baseAddress, $0.count) }
    }
    func resizeShell(_ id: UInt32, cols: Int, rows: Int) {
        if let h = handle { sard_shell_resize(h, id, UInt16(clamping: cols), UInt16(clamping: rows)) }
    }
    func ackShell(_ id: UInt32, count: UInt32 = 1) { if let h = handle { sard_shell_ack(h, id, count) } }
    func closeShell(_ id: UInt32) { if let h = handle { sard_shell_close(h, id) } }
    func listShells() { if let h = handle { sard_shell_list(h) } }
}
