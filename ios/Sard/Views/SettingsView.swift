import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @AppStorage(Pref.fitScreen) private var fitScreen = true
    @AppStorage(Pref.textSize) private var textSize = TextSize.medium.rawValue
    @AppStorage(Pref.playAudio) private var playAudio = true
    @AppStorage(Pref.pointerMode) private var pointerMode = PointerMode.trackpad.rawValue
    @AppStorage(Pref.terminalFontSize) private var terminalFontSize = 13.0
    @State private var deviceName = UserDefaults.standard.string(forKey: Pref.deviceName) ?? ""
    @State private var invite = ""
    @State private var anywhereError: String?

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    TextField("Name", text: $deviceName)
                        .onSubmit { model.setDeviceName(deviceName) }
                } header: {
                    Text("This Phone")
                } footer: {
                    Text("Shown on your computers. Fingerprint: \(SardCore.fingerprint)")
                }

                Section {
                    Toggle("Fit Screen to Phone", isOn: $fitScreen)
                    Picker("Text Size", selection: $textSize) {
                        ForEach(TextSize.allCases) { Text($0.label).tag($0.rawValue) }
                    }
                    .disabled(!fitScreen)
                    Picker("Pointer", selection: $pointerMode) {
                        ForEach(PointerMode.allCases) { Text($0.label).tag($0.rawValue) }
                    }
                    Toggle("Play Sound", isOn: $playAudio)
                } header: {
                    Text("Screen")
                } footer: {
                    Text("Fit Screen to Phone lowers the computer's resolution while you're connected, so text is readable here, and puts it back when you leave. Trackpad: drag to move the pointer, tap to click, two-finger tap to right-click, hold then drag to drag. Touch: tap where you want to click.")
                }

                Section("Terminal") {
                    Stepper("Font Size: \(Int(terminalFontSize))", value: $terminalFontSize, in: 8...24)
                }

                Section {
                    if model.anywhereEnabled {
                        Label("On", systemImage: "globe").foregroundStyle(.green)
                        Button("Turn Off", role: .destructive) { anywhereError = model.setAnywhere(token: "") }
                    } else {
                        SecureField("Invite code", text: $invite)
                        Button("Turn On") { anywhereError = model.setAnywhere(token: invite) }
                            .disabled(invite.trimmingCharacters(in: .whitespaces).isEmpty)
                    }
                    if let anywhereError {
                        Text(anywhereError).foregroundStyle(.orange)
                    }
                } header: {
                    Text("Connect Anywhere (Invite Only)")
                } footer: {
                    Text("Reach your computers from any network. Use the same invite code as on your computers.")
                }

                Section {
                    LabeledContent("Version", value: Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "")
                }
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") {
                        model.setDeviceName(deviceName)
                        dismiss()
                    }
                }
            }
        }
    }
}
