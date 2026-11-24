import AVFoundation
import SwiftUI
import VisionKit

/// QR scanner for the pairing URI.
///
/// `DataScannerViewController` is used when available because it is the only
/// camera based scanner Apple allows to be embedded without shipping a custom
/// capture pipeline; a manual paste field is always offered as a fallback.
struct PairingScannerView: View {
    @Environment(\.dismiss) private var dismiss
    var onScanned: (String) -> Void

    @State private var manual = ""
    @State private var error: String?
    @State private var showManual = false

    var body: some View {
        VStack(spacing: 16) {
            if DataScannerViewController.isSupported && DataScannerViewController.isAvailable && !showManual {
                ScannerRepresentable { code in
                    guard code.hasPrefix("waystone://") else {
                        error = "That QR code is not a Waystone pairing code."
                        return
                    }
                    onScanned(code)
                    dismiss()
                }
            } else {
                ContentUnavailableView(
                    "Camera unavailable",
                    systemImage: "camera.fill",
                    description: Text("Scanning needs a camera. Paste the pairing URI instead.")
                )
            }

            if showManual {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Paste the pairing URI from the panel")
                        .font(.footnote)
                        .foregroundStyle(.secondary)
                    TextField("waystone://pair?ep=…", text: $manual, axis: .vertical)
                        .textFieldStyle(.roundedBorder)
                        .autocorrectionDisabled()
                        .textInputAutocapitalization(.never)
                        .font(.system(.footnote, design: .monospaced))
                    Button("Add device") {
                        let trimmed = manual.trimmingCharacters(in: .whitespacesAndNewlines)
                        guard trimmed.hasPrefix("waystone://pair?") else {
                            error = "That does not look like a pairing URI."
                            return
                        }
                        onScanned(trimmed)
                        dismiss()
                    }
                    .buttonStyle(.borderedProminent)
                }
                .padding(.horizontal)
            }

            if let error {
                Text(error)
                    .font(.footnote)
                    .foregroundStyle(.orange)
                    .padding(.horizontal)
            }

            Button(showManual ? "Use the camera" : "Paste the URI instead") {
                showManual.toggle()
            }
            .font(.footnote)
            .padding(.bottom)
        }
        .navigationTitle("Scan pairing code")
        .navigationBarTitleDisplayMode(.inline)
    }
}

/// Bridges `DataScannerViewController` into SwiftUI.
private struct ScannerRepresentable: UIViewControllerRepresentable {
    var onCode: (String) -> Void

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let controller = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced,
            recognizesMultipleItems: false,
            isHighFrameRateTrackingEnabled: false,
            isPinchToZoomEnabled: true,
            isGuidanceEnabled: true,
            isHighlightingEnabled: true
        )
        controller.delegate = context.coordinator
        return controller
    }

    func updateUIViewController(_ controller: DataScannerViewController, context: Context) {
        try? controller.startScanning()
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(onCode: onCode)
    }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        private let onCode: (String) -> Void
        private var handled = false

        init(onCode: @escaping (String) -> Void) {
            self.onCode = onCode
        }

        func dataScanner(_ scanner: DataScannerViewController, didAdd items: [RecognizedItem], allItems: [RecognizedItem]) {
            guard !handled else { return }
            for item in items {
                if case .barcode(let barcode) = item,
                    let value = barcode.payloadStringValue {
                    handled = true
                    onCode(value)
                    return
                }
            }
        }
    }
}
