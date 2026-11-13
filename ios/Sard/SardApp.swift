import SwiftUI

@main
struct SardApp: App {
    @StateObject private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            HomeView()
                .environmentObject(model)
                .preferredColorScheme(.dark)
        }
    }
}

/// Settings keys (UserDefaults / @AppStorage).
enum Pref {
    static let deviceName = "deviceName"
    static let fitScreen = "fitScreen"
    static let textSize = "textSize"
    static let playAudio = "playAudio"
    static let pointerMode = "pointerMode"
    static let terminalFontSize = "terminalFontSize"
    static let names = "hostNames"
}

/// How big the host's desktop is made when fitting it to the phone
/// (logical width; the height follows the phone's shape).
enum TextSize: Int, CaseIterable, Identifiable {
    case small = 0, medium = 1, large = 2
    var id: Int { rawValue }
    var desktopWidth: CGFloat { [1600, 1280, 1024][rawValue] }
    var label: String { ["Small", "Medium", "Large"][rawValue] }
}

enum PointerMode: String, CaseIterable, Identifiable {
    case trackpad, touch
    var id: String { rawValue }
    var label: String { self == .trackpad ? "Trackpad" : "Touch" }
}

@MainActor
final class AppModel: ObservableObject {
    @Published private(set) var hosts: [Host] = []
    @Published private(set) var anywhereEnabled = false
    /// The user's own names for computers, by key.
    @Published private var names: [String: String] = [:]

    init() {
        let defaults = UserDefaults.standard
        defaults.register(defaults: [
            Pref.fitScreen: true,
            Pref.textSize: TextSize.medium.rawValue,
            Pref.playAudio: true,
            Pref.pointerMode: PointerMode.trackpad.rawValue,
            Pref.terminalFontSize: 13.0,
        ])
        if defaults.string(forKey: Pref.deviceName) == nil {
            defaults.set(UIDevice.current.name, forKey: Pref.deviceName)
        }
        names = defaults.dictionary(forKey: Pref.names) as? [String: String] ?? [:]
        SardCore.start(deviceName: defaults.string(forKey: Pref.deviceName) ?? "iPhone")
        reload()
    }

    func reload() {
        hosts = SardCore.hosts()
        anywhereEnabled = SardCore.anywhereEnabled
    }

    func displayName(_ h: Host) -> String {
        names[h.key].flatMap { $0.isEmpty ? nil : $0 } ?? h.name
    }

    func rename(_ h: Host, to name: String) {
        names[h.key] = name.trimmingCharacters(in: .whitespaces)
        UserDefaults.standard.set(names, forKey: Pref.names)
    }

    /// Pair with the computer at `address` (blocking work off the main thread).
    func pair(address: String, code: String) async -> Result<String, PairError> {
        let r = await Task.detached { SardCore.take(sard_pair(address, code)) }.value
        reload()
        if r["ok"] as? Bool == true { return .success(r["name"] as? String ?? address) }
        return .failure(PairError(message: r["error"] as? String ?? "Pairing failed."))
    }

    func forget(_ h: Host) {
        _ = sard_forget(h.key)
        names[h.key] = nil
        UserDefaults.standard.set(names, forKey: Pref.names)
        reload()
    }

    func setAddress(_ h: Host, _ address: String) -> Bool {
        let ok = sard_set_address(h.key, address)
        reload()
        return ok
    }

    func setAnywhere(token: String) -> String? {
        let r = SardCore.take(sard_set_anywhere(token))
        reload()
        return r["ok"] as? Bool == true ? nil : (r["error"] as? String ?? "Couldn't save the invite code.")
    }

    func setDeviceName(_ name: String) {
        UserDefaults.standard.set(name, forKey: Pref.deviceName)
        sard_set_device_name(name)
    }
}

struct PairError: Error {
    let message: String
}
