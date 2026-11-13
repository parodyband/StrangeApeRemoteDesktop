import SwiftUI

struct HostView: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    let host: Host
    @State private var showScreen = false
    @State private var showTerminal = false
    @State private var confirmForget = false
    @State private var editingAddress = false
    @State private var newAddress = ""
    @State private var editingName = false
    @State private var newName = ""

    var body: some View {
        List {
            Section {
                HStack(spacing: 12) {
                    BigButton(title: "Screen", systemImage: "display", tint: .blue) { showScreen = true }
                    BigButton(title: "Terminal", systemImage: "apple.terminal", tint: .green) { showTerminal = true }
                }
                .listRowInsets(EdgeInsets())
                .listRowBackground(Color.clear)
            } footer: {
                Text("Terminal needs **Allow Terminal Access** turned on in the computer's Sard menu (Settings).")
            }
            Section("This Computer") {
                Button {
                    newName = model.displayName(host)
                    editingName = true
                } label: {
                    LabeledContent("Name", value: model.displayName(host))
                }
                .tint(.primary)
                Button {
                    newAddress = host.addr
                    editingAddress = true
                } label: {
                    LabeledContent("Address", value: host.addr)
                }
                .tint(.primary)
                LabeledContent("Fingerprint", value: host.fingerprint)
                    .font(.footnote.monospaced())
            }
            Section {
                Button("Forget This Computer", role: .destructive) { confirmForget = true }
            }
        }
        .navigationTitle(model.displayName(host))
        .fullScreenCover(isPresented: $showScreen) {
            ScreenSessionView(host: host, title: model.displayName(host)) { showScreen = false }
                .ignoresSafeArea()
        }
        .fullScreenCover(isPresented: $showTerminal) {
            TerminalSessionView(host: host, title: model.displayName(host)) { showTerminal = false }
                .ignoresSafeArea(.container, edges: .bottom)
        }
        .confirmationDialog("Forget \(model.displayName(host))?", isPresented: $confirmForget, titleVisibility: .visible) {
            Button("Forget", role: .destructive) {
                model.forget(host)
                dismiss()
            }
        } message: {
            Text("You'll need to pair again to connect to it.")
        }
        .alert("Address", isPresented: $editingAddress) {
            TextField("192.168.1.20", text: $newAddress)
                .keyboardType(.numbersAndPunctuation)
                .textInputAutocapitalization(.never)
            Button("Save") { _ = model.setAddress(host, newAddress) }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Where this phone finds the computer on your network.")
        }
        .alert("Name", isPresented: $editingName) {
            TextField(host.name, text: $newName)
            Button("Save") { model.rename(host, to: newName) }
            Button("Cancel", role: .cancel) {}
        }
    }
}

private struct BigButton: View {
    let title: String
    let systemImage: String
    let tint: Color
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            VStack(spacing: 10) {
                Image(systemName: systemImage).font(.system(size: 34, weight: .medium))
                Text(title).font(.headline)
            }
            .frame(maxWidth: .infinity, minHeight: 120)
            .foregroundStyle(.white)
            .background(tint.gradient, in: RoundedRectangle(cornerRadius: 18, style: .continuous))
        }
        .buttonStyle(.plain)
    }
}
