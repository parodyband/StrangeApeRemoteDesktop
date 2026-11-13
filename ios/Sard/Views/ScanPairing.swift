import SwiftUI
import VisionKit

/// Live camera scanner for a computer's pairing QR code.
struct QRScannerView: UIViewControllerRepresentable {
    let onCode: (String) -> Void

    static var isAvailable: Bool {
        DataScannerViewController.isSupported && DataScannerViewController.isAvailable
    }

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let vc = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced,
            recognizesMultipleItems: false,
            isHighFrameRateTrackingEnabled: false,
            isHighlightingEnabled: true)
        vc.delegate = context.coordinator
        try? vc.startScanning()
        return vc
    }

    func updateUIViewController(_ vc: DataScannerViewController, context: Context) {}

    func makeCoordinator() -> Coordinator { Coordinator(onCode: onCode) }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let onCode: (String) -> Void
        private var done = false
        init(onCode: @escaping (String) -> Void) { self.onCode = onCode }

        func dataScanner(_ scanner: DataScannerViewController, didAdd items: [RecognizedItem], allItems: [RecognizedItem]) {
            for item in items {
                guard !done, case .barcode(let code) = item, let s = code.payloadStringValue,
                      s.lowercased().hasPrefix("sard://pair") else { continue }
                done = true
                scanner.stopScanning()
                UINotificationFeedbackGenerator().notificationOccurred(.success)
                onCode(s)
            }
        }
    }
}

/// Scan sheet: the camera, with a hint.
struct ScanSheet: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            ZStack(alignment: .bottom) {
                if QRScannerView.isAvailable {
                    QRScannerView { link in
                        dismiss()
                        model.scannedLink = link
                    }
                    .ignoresSafeArea()
                    Text("Point at the code in the computer's “Pair a New Computer” window.")
                        .font(.callout)
                        .padding(12)
                        .background(.ultraThinMaterial, in: RoundedRectangle(cornerRadius: 12))
                        .padding(24)
                } else {
                    ContentUnavailableView(
                        "Camera Unavailable",
                        systemImage: "camera",
                        description: Text("Allow Sard to use the camera in Settings, or enter the address and code instead."))
                }
            }
            .navigationTitle("Scan Pairing Code")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
            }
        }
    }
}

/// Pairing from a scanned code (in-app scanner or the iPhone's Camera).
struct LinkPairView: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    let link: String
    @State private var name = ""
    @State private var error: String?
    @State private var done = false

    var body: some View {
        VStack(spacing: 22) {
            Spacer()
            if let error {
                Image(systemName: "exclamationmark.triangle.fill").font(.system(size: 48)).foregroundStyle(.orange)
                Text("Couldn't Pair").font(.title2.bold())
                Text(error).multilineTextAlignment(.center).foregroundStyle(.secondary)
                Button("Try Again") { pair() }.buttonStyle(.borderedProminent)
            } else if done {
                Image(systemName: "checkmark.circle.fill").font(.system(size: 56)).foregroundStyle(.green)
                Text("Paired with \(name)").font(.title2.bold()).multilineTextAlignment(.center)
            } else {
                ProgressView().controlSize(.large)
                Text(name.isEmpty ? "Pairing…" : "Pairing with \(name)…").font(.title3.weight(.semibold))
                Text("If the phone asks to find devices on your local network, tap Allow.")
                    .font(.callout).multilineTextAlignment(.center).foregroundStyle(.secondary)
            }
            Spacer()
            Button(done ? "Done" : "Cancel") { dismiss() }.padding(.bottom)
        }
        .padding(32)
        .onAppear {
            let info = SardCore.take(sard_parse_pair_link(link))
            name = info["name"] as? String ?? ""
            if info["ok"] as? Bool != true {
                error = info["error"] as? String ?? "That isn't a Sard pairing code."
            } else {
                pair()
            }
        }
    }

    private func pair() {
        error = nil
        Task {
            switch await model.pair(link: link) {
            case .success(let n):
                name = n
                done = true
                UINotificationFeedbackGenerator().notificationOccurred(.success)
                try? await Task.sleep(for: .seconds(1.2))
                dismiss()
            case .failure(let e):
                error = e.message
            }
        }
    }
}
