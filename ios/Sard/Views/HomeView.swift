import SwiftUI

struct HomeView: View {
    @EnvironmentObject var model: AppModel
    @State private var showPair = false
    @State private var showSettings = false
    @State private var showScan = false
    #if DEBUG
    @State private var autoOpen: AutoOpen?
    #endif

    var body: some View {
        NavigationStack {
            Group {
                if model.hosts.isEmpty {
                    EmptyState(scan: { showScan = true }, pair: { showPair = true })
                } else {
                    List {
                        Section {
                            ForEach(model.hosts) { h in
                                NavigationLink(value: h) { HostRow(host: h) }
                            }
                        } footer: {
                            Text(model.anywhereEnabled
                                 ? "Connect Anywhere is on: your computers are reachable from any network."
                                 : "Your computers are reachable on the same network as this phone.")
                        }
                    }
                }
            }
            .navigationTitle("Sard")
            .navigationDestination(for: Host.self) { HostView(host: $0) }
            .toolbar {
                ToolbarItem(placement: .topBarLeading) {
                    Button { showSettings = true } label: { Image(systemName: "gearshape") }
                        .accessibilityLabel("Settings")
                }
                ToolbarItem(placement: .topBarTrailing) {
                    Menu {
                        Button { showScan = true } label: { Label("Scan Pairing Code", systemImage: "qrcode.viewfinder") }
                        Button { showPair = true } label: { Label("Enter Address and Code", systemImage: "keyboard") }
                    } label: {
                        Image(systemName: "plus")
                    }
                    .accessibilityLabel("Pair a computer")
                }
            }
            .sheet(isPresented: $showPair) { PairView() }
            .fullScreenCover(isPresented: $showScan) { ScanSheet() }
            .onChange(of: model.scanRequested) { _, wanted in
                if wanted {
                    showScan = true
                    model.scanRequested = false
                }
            }
            .sheet(item: Binding(get: { model.scannedLink.map(ScannedLink.init) },
                                 set: { model.scannedLink = $0?.link })) { LinkPairView(link: $0.link) }
            #if DEBUG
            // Development: `-SardAutoOpen screen|terminal` opens the first
            // computer straight away (simulator runs without tapping).
            .fullScreenCover(item: $autoOpen) { target in
                if target.terminal {
                    TerminalSessionView(host: target.host, title: model.displayName(target.host)) { autoOpen = nil }
                        .ignoresSafeArea(.container, edges: .bottom)
                } else {
                    ScreenSessionView(host: target.host, title: model.displayName(target.host)) { autoOpen = nil }
                        .ignoresSafeArea()
                }
            }
            .onAppear {
                if let mode = UserDefaults.standard.string(forKey: "SardAutoOpen"), let h = model.hosts.first {
                    autoOpen = AutoOpen(host: h, terminal: mode == "terminal")
                }
            }
            #endif
            .sheet(isPresented: $showSettings) { SettingsView() }
            .refreshable { model.reload() }
        }
    }
}

private struct HostRow: View {
    @EnvironmentObject var model: AppModel
    let host: Host

    var body: some View {
        HStack(spacing: 14) {
            Image(systemName: "desktopcomputer")
                .font(.title2)
                .foregroundStyle(.tint)
                .frame(width: 36)
            VStack(alignment: .leading, spacing: 2) {
                Text(model.displayName(host)).font(.headline)
                Text(host.addr).font(.caption).foregroundStyle(.secondary)
            }
        }
        .padding(.vertical, 4)
    }
}

private struct ScannedLink: Identifiable {
    let link: String
    var id: String { link }
}

private struct EmptyState: View {
    let scan: () -> Void
    let pair: () -> Void

    var body: some View {
        VStack(spacing: 18) {
            Image(systemName: "desktopcomputer.and.arrow.down")
                .font(.system(size: 54))
                .foregroundStyle(.tint)
            Text("Pair a computer").font(.title2.bold())
            Text("On the computer, open the Sard menu and choose **Pair a New Computer…**, then scan the code it shows.")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
            Button(action: scan) {
                Label("Scan Pairing Code", systemImage: "qrcode.viewfinder")
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            Button("Enter Address and Code Instead", action: pair)
                .font(.callout)
        }
        .padding(32)
        .frame(maxWidth: 460)
    }
}

#if DEBUG
private struct AutoOpen: Identifiable {
    let host: Host
    let terminal: Bool
    var id: String { host.key + (terminal ? "/t" : "/s") }
}
#endif
