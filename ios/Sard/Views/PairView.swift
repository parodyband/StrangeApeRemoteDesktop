import SwiftUI

struct PairView: View {
    @EnvironmentObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @State private var address = ""
    @State private var code = ""
    @State private var busy = false
    @State private var error: String?
    @FocusState private var field: Field?

    private enum Field { case address, code }

    private var digits: String { code.filter(\.isNumber) }
    private var canPair: Bool { !address.trimmingCharacters(in: .whitespaces).isEmpty && digits.count == 6 && !busy }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Button {
                        dismiss()
                        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4) { model.scanRequested = true }
                    } label: {
                        Label("Scan Pairing Code Instead", systemImage: "qrcode.viewfinder")
                    }
                }
                Section {
                    TextField("Address, e.g. 192.168.1.20", text: $address)
                        .keyboardType(.numbersAndPunctuation)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .focused($field, equals: .address)
                        .submitLabel(.next)
                        .onSubmit { field = .code }
                    TextField("6-digit code", text: $code)
                        .keyboardType(.numberPad)
                        .textContentType(.oneTimeCode)
                        .focused($field, equals: .code)
                        .font(.title3.monospacedDigit())
                } footer: {
                    Text("On the computer, choose **Pair a New Computer…** in the Sard menu. The phone must be on the same network as the computer to pair; after that, Connect Anywhere can reach it from elsewhere.")
                }
                if let error {
                    Section {
                        Label(error, systemImage: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                    }
                }
            }
            .navigationTitle("Pair a Computer")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    if busy {
                        ProgressView()
                    } else {
                        Button("Pair") { pair() }.disabled(!canPair)
                    }
                }
            }
            .onAppear { field = .address }
        }
    }

    private func pair() {
        busy = true
        error = nil
        let addr = address.trimmingCharacters(in: .whitespaces)
        Task {
            let r = await model.pair(address: addr, code: digits)
            busy = false
            switch r {
            case .success: dismiss()
            case .failure(let e): error = e.message
            }
        }
    }
}
